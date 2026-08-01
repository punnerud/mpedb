//! Physical planning: decompose WHERE into AND-conjuncts, extract the access
//! path (PK point > PK range > secondary unique point > full scan), compute
//! the residual filter, elide provably redundant ORDER BY, and compute the
//! precomputed footprint (design/DESIGN.md §7.3).

use crate::ast::{self, BinOp};
use std::collections::{BTreeMap, BTreeSet};

/// The statistics the planner reads, as closures — so the SQL crate keeps
/// depending only on `mpedb-types`, and a caller with no database
/// (`mpedb_sql::prepare`) passes a zero source.
///
/// This is the `CostSource` widening design/DESIGN-MPEE-SOLVER.md §9.0 names
/// (design/DESIGN-MPEE-GENERAL.md §3 rule 1): every statistic reaches only the
/// MPEE join solver ([`mpee`]), and every one is consumed as a coarse log2
/// bucket — see DESIGN-MPEE-SOLVER.md §2.1/§6 for why that quantization is what
/// keeps content-hashed plan identity stable across commits.
pub struct CostSource<'a> {
    /// The catalog's transactionally-exact per-table row count.
    pub row_count: &'a dyn Fn(u32) -> u64,
    /// log2 magnitude bucket of the distinct-key count of `(table_id,
    /// index_no)`'s full key, from the last `ANALYZE`-style pass — `None` when
    /// never analyzed (or stale), which prices exactly as before the pass
    /// existed. `index_no` is engine numbering: 0 = PK tree, secondary =
    /// position + 1. Deterministic between analyze runs by construction: the
    /// value is a stored record, not a live estimate.
    pub index_ndv_bucket: &'a dyn Fn(u32, u32) -> Option<u32>,
    /// Whether the workload MODEL marks `table_id` columnar (scan-heavy) — the
    /// stable, gen-gated signal that column segments exist for its scans (stage
    /// C, DESIGN-COLUMNAR §7.1). It prices a full-column `sum`/`avg` off the
    /// packed segment rather than the index tree: the segment reads bits per
    /// value where the tree reads whole `(value ‖ pk)` entries, so on a columnar
    /// table the segment wins and `agg_index_choice` declines the index. NOT
    /// live segment presence — that changes as segments are built/dropped and
    /// would re-hash plans; the model signal is stable and bumps `schema_gen`
    /// when it changes, keeping plan identity coherent. Defaults to "never
    /// columnar" (`false`), which prices exactly as before this signal existed.
    pub columnar: &'a dyn Fn(u32) -> bool,
}

/// How the cost source is threaded through the planner. The alias (rather than
/// `&CostSource` at ~20 call sites) is what let the seam widen from a bare
/// row-count closure without touching a signature.
pub type RowCountFn<'a> = &'a CostSource<'a>;

/// A cost source for callers that have no catalog: every table unknown.
/// The solver then still runs — its decisive term (cartesian-step count) is
/// purely structural — but cannot rank tables by size.
pub const NO_ROW_COUNTS: RowCountFn<'static> =
    &CostSource { row_count: &|_| 0, index_ndv_bucket: &|_, _| None, columnar: &|_| false };

/// What a `plan_*` helper hands back: the statement plan, the inferred parameter
/// types, the session-context keys it referenced (in reserved-slot order), and
/// the subset of those keys that are `IN` list slots (§2.6 — those have no
/// scalar type, so the type-inference guard skips them).
// (stmt, param_types, context_keys, list_keys, out_types).
// `out_types` = the caller-visible output columns' types (order-junk excluded);
// `None` = unpinned (a bare NULL item). Only compound planning consumes it —
// DML producers return an empty vec.
type PlannedStmt = (
    PlanStmt,
    Vec<Option<ColumnType>>,
    Vec<String>,
    BTreeSet<String>,
    Vec<Option<ColumnType>>,
    Vec<SubPlan>,
);
use crate::binder::{
    compile_program, declared_collation, peel_collate, peel_order_collate, BExpr, Binder,
    HostUdfSet, Scope, Ty,
};

/// Resolved ORDER BY keys: `(column index into the sorted tuple, direction +
/// NULL placement, collation)`. The collation is [`Collation::Binary`] for a
/// plain `ORDER BY`.
pub(crate) type OrderKeys = Vec<(u16, crate::plan::SortDir, mpedb_types::OrderColl)>;
use crate::plan::{
    render_program, AccessPath, AggCall, Aggregation, CompiledPlan, ConflictProbe, Frame,
    CompoundArm, DerivedPlan, FrameBound, FrameMode, InsertSource, CompoundPlan, GroupKey, Join, JoinKind, OrderOver,
    PlanOnConflict, PlanStmt, PolicyStamp, Projection, RecursiveCtePlan, SelectPlan, SubBody,
    LimitVal, SubPlan, SubPlanKind, WinInt, WindowFunc, WindowSpec, CTE_TABLE,
    MAX_PLAN_SUBPLANS,
};
#[allow(unused_imports)]
use crate::plan::{FtsQuery, FtsTerm};
use crate::policy::{PolicyCatalog, TablePolicies};
use mpedb_types::{exact_float_as_int, BareGroupBy, Collation, ExprProgram, ColumnType, Error, Footprint, Instr, KeyAccess, KeyBound, KeyPart, PolicyCmd, Result, Schema,
    TableDef, TableSet, Value,};

mod access;
mod aggregate;
mod derived;
mod footprint;
mod fts;
mod join;
mod mpee;
pub use mpee::magnitude;
pub mod sequence;
mod partial;
mod prune;
mod recursive;
mod select;
mod subquery;
mod window;
mod atoms;
mod compound;
mod conflict;
mod dml;
mod helpers;
mod rls;

#[cfg(test)]
pub(crate) mod tests;

pub use prune::{row_prune, Mask, RowPrune};

pub(crate) use footprint::compute_footprint;
use access::extract_access;
use aggregate::{contains_agg, plan_aggregate_select};
use join::plan_join_select;
use recursive::plan_recursive_cte;
use select::plan_select;
use window::{contains_window, plan_window_select};
use atoms::{and_all, as_atom, as_col_cmp, max_col, rebuild_residual, split_and, Atom};
use compound::plan_compound;
use conflict::{plan_on_conflict, plan_returning};
use dml::{plan_delete, plan_insert, plan_update, push_plan_const};
pub(crate) use helpers::{conflict_probe, conflict_probe_opt};
use helpers::{bind_err, owned_subplan_total, register_limit_params};
use helpers::{reject_correlated_in_aggregate, total_subplans};
pub use helpers::secondary_indexes;
use rls::{and, merge_and, read_policy, write_check};

/// A recursive CTE's working table in name-resolution scope, present only while
/// planning the RECURSIVE TERM and the OUTER statement of a `WITH RECURSIVE`
/// (design/DESIGN-CTE-RECURSIVE.md). `None` for every ordinary statement. The
/// `def` carries the [`CTE_TABLE`] sentinel id and the CTE's columns, so a
/// `FROM <name>` reference binds to the working table instead of the schema.
#[derive(Clone, Copy)]
pub(super) struct CteRef<'a> {
    pub name: &'a str,
    pub def: &'a TableDef,
}

/// Bind and plan one parsed statement into a [`CompiledPlan`].
pub(crate) fn plan_statement(
    stmt: &ast::Stmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    // GROUP BY strictness dialect (COMPAT.md). Threaded to every aggregate
    // planning site — including subqueries and CTEs — because a bare column can
    // appear at any nesting depth, and a postgres-mode database must refuse it
    // everywhere. Copy, so it rides alongside `catalog` without ceremony.
    mode: BareGroupBy,
    // Host-registered scalar UDFs in scope (design/DESIGN-UDF.md). Threaded to
    // every binder-construction site alongside `mode`, for the same reason: a
    // UDF call can appear at any nesting depth.
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
) -> Result<CompiledPlan> {
    let mut consts: Vec<Value> = Vec::new();
    let txn = |p: PlanStmt| {
        (p, vec![None; n_params as usize], Vec::new(), BTreeSet::new(), Vec::new(), Vec::new())
    };
    let (plan_stmt, param_types, context_keys, list_keys, _out_types, subplans) = match stmt {
        ast::Stmt::Begin => txn(PlanStmt::Begin),
        ast::Stmt::Commit => txn(PlanStmt::Commit),
        ast::Stmt::Rollback => txn(PlanStmt::Rollback),
        ast::Stmt::Savepoint(n) => txn(PlanStmt::Savepoint(n.clone())),
        ast::Stmt::Release(n) => txn(PlanStmt::Release(n.clone())),
        ast::Stmt::RollbackTo(n) => txn(PlanStmt::RollbackTo(n.clone())),
        // A surviving derived table (`FROM (SELECT …) t` the Stage-B flattener
        // could not splice) is MATERIALIZED — legal only here, at the top level.
        ast::Stmt::Select(s) if s.from_derived.is_some() => {
            derived::plan_derived_select(s, schema, n_params, catalog, mode, host_udfs, row_count, &mut consts)?
        }
        ast::Stmt::Select(s) => {
            plan_select(s, schema, n_params, catalog, mode, host_udfs, row_count, &mut consts, None, &[])?
        }
        ast::Stmt::Compound(c) => {
            plan_compound(c, schema, n_params, catalog, mode, host_udfs, row_count, &mut consts)?
        }
        ast::Stmt::RecursiveCte(rc) => {
            plan_recursive_cte(rc, schema, n_params, catalog, mode, host_udfs, row_count, &mut consts)?
        }
        ast::Stmt::Insert(s) => {
            plan_insert(s, schema, n_params, catalog, mode, host_udfs, row_count, &mut consts)?
        }
        ast::Stmt::Update(s) => {
            plan_update(s, schema, n_params, catalog, mode, host_udfs, row_count, &mut consts)?
        }
        ast::Stmt::Delete(s) => {
            plan_delete(s, schema, n_params, catalog, mode, host_udfs, row_count, &mut consts)?
        }
    };
    let mut param_types = param_types;
    register_limit_params(&plan_stmt, &subplans, &mut param_types)?;
    // The 16-subplan ceiling bounds the WHOLE tree once nesting (#73 §3) can
    // grow it past one level — matching the recursive decoder's DoS budget, so a
    // plan `prepare` accepts is a plan `decode` accepts.
    // A materialized derived table's BODY owns its lifts (they never join the
    // statement-level list), so the DoS ceiling — and the footprint below —
    // have to count them here or a body could smuggle an unbounded tree past
    // both. Same 16, matching the recursive decoder's budget.
    // Every OWNED list counts too: a derived body's (format 52) and every
    // compound arm's (format 56). They never join the statement-level list, so
    // a component could otherwise smuggle an unbounded tree past both the DoS
    // ceiling and the footprint. Same 16, matching the recursive decoder's
    // budget.
    if owned_subplan_total(&plan_stmt) + total_subplans(&subplans) > 16 {
        return Err(bind_err(
            "too many subqueries in one statement (max 16, including nested)",
        ));
    }
    let footprint = compute_footprint(&plan_stmt, &subplans, schema)?;
    // A context slot whose type could not be inferred cannot be type-checked
    // against the session value at execute time — reject it at prepare with a
    // clear message rather than failing opaquely later (fail closed).
    let n_user = param_types.len() - context_keys.len();
    for (p, key) in context_keys.iter().enumerate() {
        // A list slot (§2.6) has no scalar type by construction — `IN` checks
        // membership, it does not unify with a column type — so the
        // type-inference requirement does not apply to it. Its wrong-type case
        // is caught instead when `in_list_3vl` refuses a non-list value.
        if list_keys.contains(key) {
            continue;
        }
        if param_types[n_user + p].is_none() {
            return Err(bind_err(format!(
                "cannot infer the type of current_setting('{key}'); \
                 use it in a typed comparison (e.g. `col = current_setting('{key}')`)"
            )));
        }
    }
    // Record the target table's RLS epoch + content hash so a cached plan can
    // be detected as stale after a policy edit (Phase-5 leak-proofing, §4).
    // Recorded for EVERY plan (even non-RLS), so that later ENABLING RLS on the
    // table invalidates plans compiled before it.
    // One stamp per table whose policy this plan baked in. For a join that is
    // BOTH sides, and for a compound EVERY arm's tables: stamping less would
    // let a cached plan keep serving some table's rows under a policy that has
    // since been tightened, which is the leak §4 exists to close.
    let select_tables = |sp: &SelectPlan, out: &mut Vec<u32>| {
        out.push(sp.table);
        for j in &sp.joins {
            out.push(j.table);
        }
    };
    // A subplan's tables are the statement's tables — stamp them too, and
    // recursively for nested lifts (#73 §3), so a policy edit on ANY table read
    // at ANY depth invalidates the cached plan. Missing a nested table's stamp
    // would let it keep serving rows under a since-tightened policy (§4 leak).
    fn stamp_subplan_tables(
        s: &SubPlan,
        select_tables: &impl Fn(&SelectPlan, &mut Vec<u32>),
        out: &mut Vec<u32>,
    ) {
        stamp_body_tables(&s.body, select_tables, out);
        for c in &s.subplans {
            stamp_subplan_tables(c, select_tables, out);
        }
    }
    fn stamp_body_tables(
        b: &SubBody,
        select_tables: &impl Fn(&SelectPlan, &mut Vec<u32>),
        out: &mut Vec<u32>,
    ) {
        match b {
            SubBody::Select(sp) => select_tables(sp, out),
            SubBody::Compound(c) => stamp_compound_tables(c, select_tables, out),
            SubBody::Derived(dp) => stamp_derived_tables(dp, select_tables, out),
        }
    }
    // Body ‖ outer ‖ the lifts the body owns (format 52) — missing any of them
    // would let the plan keep serving a since-tightened table.
    fn stamp_derived_tables(
        dp: &DerivedPlan,
        select_tables: &impl Fn(&SelectPlan, &mut Vec<u32>),
        out: &mut Vec<u32>,
    ) {
        stamp_body_tables(&dp.body, select_tables, out);
        select_tables(&dp.outer, out);
        for s in &dp.body_subplans {
            stamp_subplan_tables(s, select_tables, out);
        }
    }
    // A compound's arms AND the lifts those arms own (format 56).
    fn stamp_compound_tables(
        c: &CompoundPlan,
        select_tables: &impl Fn(&SelectPlan, &mut Vec<u32>),
        out: &mut Vec<u32>,
    ) {
        for arm in &c.arms {
            match arm {
                crate::plan::CompoundArm::Select(sp) => select_tables(sp, out),
                crate::plan::CompoundArm::Derived(dp) => {
                    stamp_derived_tables(dp, select_tables, out)
                }
            }
        }
        for arm in &c.arm_subplans {
            for s in arm {
                stamp_subplan_tables(s, select_tables, out);
            }
        }
    }
    let mut stamped: Vec<u32> = Vec::new();
    for s in &subplans {
        stamp_subplan_tables(s, &select_tables, &mut stamped);
    }
    match &plan_stmt {
        PlanStmt::Select(sp) => select_tables(sp, &mut stamped),
        PlanStmt::Compound(c) => {
            stamp_compound_tables(c, &select_tables, &mut stamped);
            // Arms often read the same table; one stamp per table suffices.
            stamped.sort_unstable();
            stamped.dedup();
        }
        // A recursive CTE reads the base tables of all three components; stamp
        // each (the CTE working table itself is filtered out below).
        PlanStmt::RecursiveCte(rc) => {
            select_tables(&rc.anchor, &mut stamped);
            select_tables(&rc.recursive, &mut stamped);
            select_tables(&rc.outer, &mut stamped);
        }
        // A materialized derived table reads its body's tables (every arm of a
        // compound body) plus the outer's; stamp each (the working table itself
        // is filtered out below).
        PlanStmt::Derived(dp) => stamp_derived_tables(dp, &select_tables, &mut stamped),
        PlanStmt::Insert { table, .. }
        | PlanStmt::Update { table, .. }
        | PlanStmt::Delete { table, .. } => stamped.push(*table),
        PlanStmt::Begin
        | PlanStmt::Commit
        | PlanStmt::Rollback
        | PlanStmt::Savepoint(_)
        | PlanStmt::Release(_)
        | PlanStmt::RollbackTo(_) => {}
    }
    // The DUAL and recursive-CTE working-table sentinels are not catalog tables —
    // they carry no policy, so never stamp them (and `catalog.get` would treat a
    // u32::MAX-ish id as an ordinary miss, wasting a stamp slot).
    stamped.retain(|&t| t != crate::plan::DUAL_TABLE && t != CTE_TABLE);
    // One stamp per table is enough however many places read it.
    stamped.sort_unstable();
    stamped.dedup();

    let policies: Vec<PolicyStamp> = stamped
        .into_iter()
        .map(|t| {
            let tp = catalog.get(t);
            PolicyStamp {
                table: t,
                epoch: tp.map_or(0, |tp| tp.epoch),
                hash: crate::policy::table_policy_hash(tp),
            }
        })
        .collect();

    // `n_params` now counts user params PLUS the reserved context slots that
    // `current_setting()` appended, so the executor's param array is sized for
    // both. n_user_params = n_params - context_keys.len().
    Ok(CompiledPlan {
        stmt: plan_stmt,
        schema_hash: schema.hash(),
        n_params: param_types.len() as u16,
        param_types,
        context_keys,
        policies,
        subplans,
        consts,
        footprint,
    })
}

fn resolve_table<'s>(schema: &'s Schema, name: &str) -> Result<(u32, &'s TableDef)> {
    let id = schema
        .table_id(name)
        .ok_or_else(|| bind_err(format!("unknown table `{name}`")))?;
    Ok((id, schema.table(id).expect("id from table_id")))
}

/// Like [`resolve_table`], but a name matching the in-scope recursive CTE (if
/// any) resolves to its working table (id [`CTE_TABLE`], `def` from the
/// [`CteRef`]) instead of the schema. Identifiers are case-sensitive (as
/// everywhere in mpedb), matching `resolve_table`'s exact name lookup.
fn resolve_table_cte<'s>(
    schema: &'s Schema,
    cte: Option<CteRef<'s>>,
    name: &str,
) -> Result<(u32, &'s TableDef)> {
    if let Some(c) = cte {
        if name == c.name {
            return Ok((CTE_TABLE, c.def));
        }
    }
    resolve_table(schema, name)
}


// ---- the MPEE A/B switch, for callers that have no environment -------------

/// Tri-state override of the `MPEDB_NO_MPEE` environment switch:
/// 0 = unset (consult the environment), 1 = force the solver ON,
/// 2 = force it OFF.
static MPEE_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// `Some(true)` = solver forced off, `Some(false)` = forced on, `None` = defer
/// to `MPEDB_NO_MPEE`.
pub(crate) fn mpee_override() -> Option<bool> {
    match MPEE_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

/// Force the MPEE join solver on or off for subsequent compilations, or hand
/// control back to `MPEDB_NO_MPEE` with `None`.
///
/// The environment variable exists so a claim about what the solver buys is a
/// paired measurement of ONE binary (see `mpee::disabled`). That argument is
/// unchanged here — this is the same switch, reachable from a process that has
/// no environment to set. The browser playground is the motivating case: it
/// renders the user's textual join order beside the solver's chosen one, which
/// means compiling the same SQL both ways in one address space.
///
/// **Process-global and not synchronized with in-flight compilation.** It is
/// meant for a single-threaded A/B (compile, read, compile, read), not for
/// per-statement control in a threaded program.
pub fn set_mpee_enabled(on: Option<bool>) {
    let v = match on {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    };
    MPEE_OVERRIDE.store(v, std::sync::atomic::Ordering::Relaxed);
}
