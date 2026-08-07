//! Plan executor: runs a validated [`CompiledPlan`] against an engine
//! transaction. Shared by the autocommit paths on [`crate::Database`] and the
//! interactive [`crate::WriteSession`] via the [`TxnCtx`] abstraction.

use crate::trigger::{CompiledTrigger, WriteRules};
use crate::ExecResult;
use mpedb_core::{FoldOpts, FoldStop, ReadTxn, WriteTxn};
use mpedb_sql::{
    AccessPath, AggCall, Aggregation, CompiledPlan, ConflictProbe, InsertSource, Join, JoinKind,
    CompoundPlan, GroupKey, OrderOver, PlanOnConflict, PlanStmt, Projection, RowMap, RowSide,
    LimitVal, Mask, RowPrune, SelectPlan, SetOp, SortDir, SubBody, SubPlan,
};
use mpedb_types::{
    exact_float_as_int, exact_int_as_float, keycode, Accum, Collation, DefaultExpr, Error,
    HostColls, OrderColl,
    ExprProgram, HostFns, KeyBound, KeyPart, Result, Schema, TableDef, Value,
};
use std::cmp::Ordering;
use std::sync::Arc;
use std::collections::BinaryHeap;

std::thread_local! {
    /// The rowid the most recent INSERT statement assigned/used, for the
    /// C-API's `sqlite3_last_insert_rowid`. Recorded per inserted row into a
    /// rowid-alias (INTEGER PRIMARY KEY) table by [`record_last_insert_rowid`],
    /// so the last row of a multi-row insert wins. Read (and cleared) by
    /// [`take_last_insert_rowid`] immediately after the statement returns, on
    /// the same thread that executed it — every write path (`Database::query`,
    /// `WriteSession::query`, the group-commit leader) runs `exec_stmt`
    /// synchronously in the caller's thread, so this needs no synchronization.
    static LAST_INSERT_ROWID: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// The value a column's DEFAULT contributes when an INSERT omits it.
///
/// One function for the three insert paths (plan executor, INSERT…SELECT, and
/// the ring's fast prepare) so a default cannot mean one thing on one path and
/// another on the next. `now_micros` is the STATEMENT instant, read ONCE per
/// statement by the caller and passed to every row: every row of one INSERT
/// must carry the same instant, which is what sqlite does and what a test
/// comparing two rows of one statement depends on. It used to be TWO clock
/// parameters, and callers filled the second with a fresh read per row — two
/// rows of one INSERT straddling a millisecond then carried different
/// instants, a wrong answer a loaded box reproduces and an idle one hides.
/// One parameter makes the per-row read unwritable.
pub(crate) fn default_cell(
    default: Option<&DefaultExpr>,
    now_micros: i64,
    host: Option<&dyn HostFns>,
) -> Result<Value> {
    Ok(match default {
        Some(DefaultExpr::Const(v)) => v.clone(),
        Some(DefaultExpr::Now) => Value::Timestamp(now_micros),
        // An instant-dependent expression default is EVALUATED here, once per
        // statement's worth of rows like the keyword forms — the instant sits
        // in parameter slot 0, which is the only slot a default can have (it
        // takes no user parameters).
        // `host` is what lets a DEFAULT call a host-registered UDF, which
        // sqlite resolves per INSERT (see `fold_default_expr`). Without it the
        // program would error on `Instr::HostCall` and the column would fall to
        // NULL — a wrong answer where an error belongs, which is why the
        // resolver is threaded here rather than defaulted away.
        Some(DefaultExpr::Expr(d)) => d.program.eval_host(
            &[],
            &[Value::Text(mpedb_types::sqlite_now_string(now_micros))],
            host,
        )?,
        Some(k @ (DefaultExpr::CurrentTimestamp | DefaultExpr::CurrentDate | DefaultExpr::CurrentTime)) => {
            let (ts, date, time) = mpedb_types::sqlite_now_parts(now_micros);
            Value::Text(match k {
                DefaultExpr::CurrentDate => date,
                DefaultExpr::CurrentTime => time,
                _ => ts,
            })
        }
        None => Value::Null,
    })
}

/// Record the rowid of a row just inserted into a rowid-alias table (facade hook
/// for `sqlite3_last_insert_rowid`). Called from the INSERT loop after each
/// successful `insert_row`, so the final call reflects the last inserted row.
pub(crate) fn record_last_insert_rowid(rowid: i64) {
    LAST_INSERT_ROWID.with(|c| c.set(Some(rowid)));
}

thread_local! {
    /// Violations a `DEFERRABLE INITIALLY DEFERRED` key produced, held until
    /// COMMIT (#194). Thread-local for the same reason `LAST_INSERT_ROWID` is:
    /// every write path runs `exec_stmt` synchronously in the caller's thread,
    /// and the engine's writer lock means one write transaction per thread at a
    /// time — so this cannot be two transactions' lists at once.
    static FK_DEFERRED: std::cell::RefCell<Vec<crate::fk::Deferred>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Hold a deferred violation over to COMMIT.
pub(crate) fn push_fk_deferred(d: Vec<crate::fk::Deferred>) {
    if d.is_empty() {
        return;
    }
    FK_DEFERRED.with(|c| c.borrow_mut().extend(d));
}

/// Take the held-over violations — COMMIT re-probes them, ROLLBACK drops them.
pub(crate) fn take_fk_deferred() -> Vec<crate::fk::Deferred> {
    FK_DEFERRED.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// Take (read and clear) the rowid assigned by the last INSERT executed on this
/// thread, or `None` if the last statement inserted nothing into a rowid-alias
/// table. The C-API shim calls this once after each statement and updates its
/// per-connection `last_insert_rowid` only when a value is present — matching
/// sqlite, where a non-insert statement leaves `last_insert_rowid` unchanged.
pub fn take_last_insert_rowid() -> Option<i64> {
    LAST_INSERT_ROWID.with(|c| c.take())
}

mod aggregate;
mod fts;
mod gather;
mod parallel;
mod recursive;
mod window;

pub(crate) use gather::{range_bounds, resolve_part, RawBound};
/// See [`crate::parallel_folds_engaged`].
pub(crate) fn parallel_folds_engaged() -> u64 {
    parallel::ENGAGED.load(std::sync::atomic::Ordering::Relaxed)
}
use aggregate::exec_aggregate;
use gather::{cmp_rows, gather_joined, gather_rows, gather_topk, sort_rows};

mod ctx;
mod dml;
mod params;
mod select;

pub(crate) use ctx::{ChargeMode, ReadCtx, TxnCtx, WriteCtx};
use ctx::{base_row_collations, output_collations};
pub(crate) use dml::fire_row_triggers;
use dml::exec_stmt_rest;
pub(crate) use params::coerce_params;
pub(crate) use select::{
    exec_pk_point_hot, exec_stmt, exec_stmt_triggered, try_build_pk_point_hot, PkPointHot,
    MAX_TRIGGER_DEPTH,
};
use select::{
    correlated_survivors, dml_survivors, exec_compound, exec_select, exec_select_leveled,
    run_subplan, select_output_columns, subplan_value,
};

/// The `which` attribution (#74) for a table `id` in one of the exec-layer
/// budget bumps. Built lazily, only on the abort path.
fn table_name(schema: &Schema, id: u32) -> String {
    schema
        .table(id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| format!("table #{id}"))
}


/// Resolve a LIMIT bound for THIS execution (format 62): a literal passes
/// through; a parameter reads its bound value. sqlite semantics for the
/// resolved integer (differentially confirmed against the bundled oracle):
/// a negative LIMIT means no bound, and NULL is refused loudly ("datatype
/// mismatch"). The compiler typed the slot int64, so any other value shape
/// was already rejected by the execute-time parameter type check.
pub(crate) fn resolve_limit(v: Option<LimitVal>, params: &[Value]) -> Result<Option<u64>> {
    match v {
        None => Ok(None),
        Some(LimitVal::Lit(n)) => Ok(Some(n)),
        Some(LimitVal::Param(i)) => match params.get(i as usize) {
            Some(Value::Int(n)) if *n < 0 => Ok(None),
            Some(Value::Int(n)) => Ok(Some(*n as u64)),
            Some(Value::Null) => Err(Error::Bind(
                "datatype mismatch: LIMIT/OFFSET parameter is NULL".into(),
            )),
            _ => Err(internal("LIMIT parameter slot is not an integer")),
        },
    }
}

/// The paired form, carrying sqlite's EVALUATION ORDER: LIMIT resolves
/// first, and an exact 0 short-circuits the statement before OFFSET is even
/// looked at — `LIMIT 0 OFFSET NULL` is an empty result, not a datatype
/// error (differentially pinned in mpedb-testkit/tests/limit_param.rs).
pub(crate) fn resolve_limit_offset(
    limit: Option<LimitVal>,
    offset: Option<LimitVal>,
    params: &[Value],
) -> Result<(Option<u64>, u64)> {
    let l = resolve_limit(limit, params)?;
    if l == Some(0) {
        return Ok((Some(0), 0));
    }
    Ok((l, resolve_offset(offset, params)?))
}

/// The OFFSET twin: absent and negative both mean "skip nothing" (sqlite),
/// NULL is the same loud refusal as LIMIT.
pub(crate) fn resolve_offset(v: Option<LimitVal>, params: &[Value]) -> Result<u64> {
    match v {
        None => Ok(0),
        Some(LimitVal::Lit(n)) => Ok(n),
        Some(LimitVal::Param(i)) => match params.get(i as usize) {
            Some(Value::Int(n)) => Ok((*n).max(0) as u64),
            Some(Value::Null) => Err(Error::Bind(
                "datatype mismatch: LIMIT/OFFSET parameter is NULL".into(),
            )),
            _ => Err(internal("OFFSET parameter slot is not an integer")),
        },
    }
}

fn internal(msg: &str) -> Error {
    Error::Internal(format!("validated plan out of bounds: {msg}"))
}

/// True when `e` is a constraint error that the engine's row mutators
/// (`insert_row`/`update_by_pk`) raise from their pre-checks, strictly
/// *before* mutating any tree: a call that failed this way left the
/// transaction untouched. Anything else (DbFull, Corrupt, Internal, Io, ...)
/// can fire mid-mutation and must be treated as a possible partial effect.
/// **§6.5 classification-oracle closure.** On an RLS-enabled table, collapse the
/// constraint-violation variants into one opaque rejection.
///
/// `rls` is `with_check.is_some()`, which is exact rather than a proxy: the
/// planner emits `with_check` for a write iff RLS is enabled on the target
/// (`write_check` returns `None` otherwise), so no plan-format flag is needed.
///
/// MUST be applied AFTER `precheck_failure` has decided `partial`: that function
/// matches on the very variants being collapsed, so normalizing first would make
/// it report a partial effect where the row never landed.
fn hide_constraint_variant(e: Error, table: &str, rls: bool) -> Error {
    if !rls {
        return e;
    }
    match e {
        Error::PrimaryKeyViolation { .. }
        | Error::UniqueViolation { .. }
        | Error::CheckViolation { .. } => Error::WriteRejected {
            table: table.to_string(),
        },
        other => other,
    }
}

/// Fill the OUTPUT-COLUMN name into a `||` decode error rising out of a
/// projection slot (§8): the C-API shapes CPython's exact message from it
/// (`Could not decode to UTF-8 column '<name>' with text '…'`), and the
/// projection is the one place that knows what the column is called.
fn name_decode_error(e: Error, name: &str) -> Error {
    match e {
        Error::NonUtf8Concat { column: None, text } => Error::NonUtf8Concat {
            column: Some(name.to_string()),
            text,
        },
        other => other,
    }
}

fn precheck_failure(e: &Error) -> bool {
    matches!(
        e,
        Error::TypeMismatch(_)
            | Error::NotNullViolation { .. }
            | Error::CheckViolation { .. }
            | Error::UniqueViolation { .. }
            | Error::PrimaryKeyViolation { .. }
    )
}

// Active nested-derived working table while `exec_derived` runs an outer scan
// whose statement node is NOT itself `PlanStmt::Derived` (format 58 compound
// arms). Single-threaded per execute; cleared on the way out.
thread_local! {
    static ACTIVE_WORKING_TABLE: std::cell::RefCell<Option<TableDef>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) fn with_working_table_def<R>(def: TableDef, f: impl FnOnce() -> R) -> R {
    ACTIVE_WORKING_TABLE.with(|c| {
        let prev = c.replace(Some(def));
        let out = f();
        c.replace(prev);
        out
    })
}

fn table_def<'a>(
    schema: &'a Schema,
    plan: &'a CompiledPlan,
    table: u32,
) -> Result<std::borrow::Cow<'a, TableDef>> {
    use std::borrow::Cow;
    // FROM-less SELECT: the DUAL sentinel resolves to the shared zero-column
    // def — every downstream width/name computation degrades correctly over
    // zero columns, and the gather never reaches a TxnCtx call.
    if table == mpedb_sql::DUAL_TABLE {
        return Ok(Cow::Borrowed(mpedb_sql::dual_def()));
    }
    // The working table resolves to the synthetic def of the active derived /
    // recursive CTE. Nested Derived compound arms (format 58) install theirs
    // via [`with_working_table_def`] for the outer scan; top-level
    // PlanStmt::Derived / RecursiveCte carry theirs on the statement node.
    // `FROM generate_series(…)`: one column, generated. The def is static —
    // unlike the CTE working table, a series' shape does not depend on which
    // statement is running.
    if table == mpedb_sql::SERIES_TABLE {
        return Ok(Cow::Borrowed(mpedb_sql::series_def()));
    }
    if table == mpedb_sql::CTE_TABLE {
        if let Some(def) = ACTIVE_WORKING_TABLE.with(|c| c.borrow().clone()) {
            return Ok(Cow::Owned(def));
        }
        return match &plan.stmt {
            PlanStmt::RecursiveCte(rc) => Ok(Cow::Owned(rc.cte_def())),
            PlanStmt::Derived(dp) => Ok(Cow::Owned(dp.derived_def())),
            _ => Err(internal("CTE working table outside a recursive CTE / derived table")),
        };
    }
    schema
        .table(table)
        .map(Cow::Borrowed)
        .ok_or_else(|| internal("table id out of range"))
}

/// Microseconds since the Unix epoch, captured once per execute() call.
fn now_micros() -> i64 {
    // Via `crate::os` so the wasm32 build reads the HOST's clock; a direct
    // `SystemTime::now()` panics there. See `os::wall_clock_micros`.
    mpedb_core::wall_clock_micros()
}
