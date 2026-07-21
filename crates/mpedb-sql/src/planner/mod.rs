//! Physical planning: decompose WHERE into AND-conjuncts, extract the access
//! path (PK point > PK range > secondary unique point > full scan), compute
//! the residual filter, elide provably redundant ORDER BY, and compute the
//! precomputed footprint (design/DESIGN.md §7.3).

use crate::ast::{self, BinOp};
use std::collections::{BTreeMap, BTreeSet};

/// The catalog's transactionally-exact per-table row count, as the planner
/// sees it: a closure, so the SQL crate keeps depending only on `mpedb-types`
/// and a caller with no database (`mpedb_sql::prepare`) passes a zero source.
///
/// This is the ONLY statistic the planner reads, it reaches only the MPEE join
/// solver ([`mpee`]), and it is consumed only through a magnitude bucket — see
/// design/DESIGN-MPEE-SOLVER.md §2.1/§6 for why that quantization is what keeps
/// content-hashed plan identity stable across commits.
pub type RowCountFn<'a> = &'a dyn Fn(u32) -> u64;

/// A row-count source for callers that have no catalog: every table unknown.
/// The solver then still runs — its decisive term (cartesian-step count) is
/// purely structural — but cannot rank tables by size.
pub const NO_ROW_COUNTS: RowCountFn<'static> = &|_| 0;

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
    FrameBound, FrameMode, InsertSource, CompoundPlan, GroupKey, Join, JoinKind, OrderOver,
    PlanOnConflict, PlanStmt, PolicyStamp, Projection, RecursiveCtePlan, SelectPlan, SubBody,
    SubPlan, SubPlanKind, WindowSpec, CTE_TABLE, MAX_PLAN_SUBPLANS,
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
mod prune;
mod recursive;
mod select;
mod subquery;
mod window;

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

fn and(a: BExpr, b: BExpr) -> BExpr {
    BExpr::Binary(BinOp::And, Box::new(a), Box::new(b))
}
fn or(a: BExpr, b: BExpr) -> BExpr {
    BExpr::Binary(BinOp::Or, Box::new(a), Box::new(b))
}

/// AND-combine an optional user predicate with an optional injected policy.
fn merge_and(user: Option<BExpr>, policy: Option<BExpr>) -> Option<BExpr> {
    match (user, policy) {
        (Some(u), Some(p)) => Some(and(u, p)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Parse + bind one policy predicate SOURCE against the shared `binder`, so its
/// `current_setting()` refs share the statement's reserved-parameter space
/// (design/DESIGN-MULTIDB.md §2.2/§3.2). Policies may not use `$`/`?` params.
fn bind_policy_src(binder: &mut Binder, src: &str) -> Result<BExpr> {
    let (expr, n_params) = crate::parser::parse_expr_only(src)?;
    if n_params > 0 {
        return Err(bind_err("RLS policy predicate must not use `$`/`?` parameters"));
    }
    binder.bind_predicate(&expr)
}

/// `(perm1 ∨ … ∨ permN) ∧ restr1 ∧ … ∧ restrM` over the `USING` predicates that
/// govern `cmd`. **Empty permissive set ⇒ literal `FALSE` (default-deny, §3.5)** —
/// this is emitted as `Const(false)`, never omitted, so a merged `where ∧ FALSE`
/// hides every row instead of accidentally exposing the table.
fn using_group(binder: &mut Binder, tp: &TablePolicies, cmd: PolicyCmd) -> Result<BExpr> {
    let mut perms: Vec<BExpr> = Vec::new();
    let mut restrs: Vec<BExpr> = Vec::new();
    for p in &tp.policies {
        if !p.command.governs(cmd) {
            continue;
        }
        if let Some(src) = &p.using_src {
            let b = bind_policy_src(binder, src)?;
            if p.permissive {
                perms.push(b);
            } else {
                restrs.push(b);
            }
        }
    }
    let mut eff = perms
        .into_iter()
        .reduce(or)
        .unwrap_or(BExpr::Const(Value::Bool(false)));
    for r in restrs {
        eff = and(eff, r);
    }
    Ok(eff)
}

/// **Fail-closed assertion (DESIGN-MULTIDB §6.3).** A table declared
/// `require_policy = true` must actually be protected for the command being
/// compiled — otherwise `prepare` errors instead of quietly compiling a plan
/// that returns every row to every caller.
///
/// This exists because the failure it guards is silent and asymmetric: forget
/// one `ENABLE ROW LEVEL SECURITY` and nothing complains, no context value trips
/// it, and the table reads exactly like a working one. The assertion converts
/// that into a loud error at prepare, in the process that declared the intent.
///
/// "Protected" means BOTH: RLS enabled, and at least one policy governing `cmd`.
/// The second half matters even though our empty-permissive-set rule already
/// default-denies (a literal FALSE, §3.5) — denying every row is safe but is
/// almost never what someone who wrote `require_policy = true` meant, and
/// discovering it as "the table is mysteriously empty" is worse than an error.
/// A deliberate deny-all is still expressible: write it (`FOR DELETE USING
/// (false)`), do not leave it implied.
fn assert_policy_required(
    catalog: &PolicyCatalog,
    table_id: u32,
    table_name: &str,
    cmd: PolicyCmd,
) -> Result<()> {
    if !catalog.requires_policy(table_id) {
        return Ok(());
    }
    let tp = catalog.get(table_id);
    if !tp.is_some_and(|t| t.rls_enabled) {
        return Err(Error::Config(format!(
            "table `{table_name}` is declared require_policy = true but row-level security \
             is not enabled on it — refusing to compile a plan that would expose every row \
             (run `ALTER TABLE {table_name} ENABLE ROW LEVEL SECURITY`)"
        )));
    }
    let governed = tp
        .map(|t| t.policies.iter().any(|p| p.command.governs(cmd)))
        .unwrap_or(false);
    if !governed {
        return Err(Error::Config(format!(
            "table `{table_name}` is declared require_policy = true but no policy governs \
             {cmd:?} — every {cmd:?} would be denied by default-deny; declare the intent \
             explicitly with a policy for this command"
        )));
    }
    Ok(())
}

/// The effective READ predicate injected for `cmd`, or `None` when RLS is not
/// enabled on the table. For UPDATE/DELETE — which always read the old row —
/// the SELECT visibility group is AND-ed in too (PG read-via-write, §3.6), so a
/// caller can never mutate (or infer the existence of) a row it cannot SELECT.
fn read_policy(
    binder: &mut Binder,
    catalog: &PolicyCatalog,
    table_id: u32,
    table_name: &str,
    cmd: PolicyCmd,
) -> Result<Option<BExpr>> {
    assert_policy_required(catalog, table_id, table_name, cmd)?;
    let tp = match catalog.get(table_id) {
        Some(tp) if tp.rls_enabled => tp,
        _ => return Ok(None),
    };
    let mut eff = using_group(binder, tp, cmd)?;
    if matches!(cmd, PolicyCmd::Update | PolicyCmd::Delete) {
        let sel = using_group(binder, tp, PolicyCmd::Select)?;
        eff = and(eff, sel);
    }
    Ok(Some(eff))
}

/// The effective `WITH CHECK` predicate for a write `cmd` (Insert/Update),
/// evaluated against the NEW row. A policy's `WITH CHECK` source falls back to
/// its `USING` when absent (PG rule). Empty permissive set with RLS enabled ⇒
/// literal `FALSE` (reject every write — default-deny). `None` when RLS is off.
fn write_check(
    binder: &mut Binder,
    catalog: &PolicyCatalog,
    table_id: u32,
    table_name: &str,
    cmd: PolicyCmd,
) -> Result<Option<BExpr>> {
    assert_policy_required(catalog, table_id, table_name, cmd)?;
    let tp = match catalog.get(table_id) {
        Some(tp) if tp.rls_enabled => tp,
        _ => return Ok(None),
    };
    let mut perms: Vec<BExpr> = Vec::new();
    let mut restrs: Vec<BExpr> = Vec::new();
    for p in &tp.policies {
        if !p.command.governs(cmd) {
            continue;
        }
        if let Some(src) = p.check_src.as_ref().or(p.using_src.as_ref()) {
            let b = bind_policy_src(binder, src)?;
            if p.permissive {
                perms.push(b);
            } else {
                restrs.push(b);
            }
        }
    }
    let mut eff = perms
        .into_iter()
        .reduce(or)
        .unwrap_or(BExpr::Const(Value::Bool(false)));
    for r in restrs {
        eff = and(eff, r);
    }
    Ok(Some(eff))
}

/// Canonical secondary-index numbering helper (design/DESIGN.md §4.4): index 0 is
/// the PK tree; the returned vector lists the column index of secondary
/// index 1, 2, ... — columns with `unique = true` OR `indexed = true`, in
/// declaration order, skipping a column that is by itself the entire primary
/// key. UNIQUE index trees are keyed `value → pk`; non-unique ones use the
/// composite key `(value ‖ pk) → pk` (unique by construction).
pub fn secondary_indexes(table: &TableDef) -> Vec<Option<u16>> {
    // `TableDef.indexes` is the single source of index numbering
    // (DESIGN-SCHEMA-V2): index_no = position + 1, 0 = the PK tree. Each
    // entry is `Some(column)` for a single-column index — the only shape the
    // planner exploits until #55 — or `None` for a composite entry, which
    // KEEPS its index_no (numbering must stay aligned with the engine's
    // trees) but is never offered as an access path.
    table
        .indexes
        .iter()
        .map(|ix| match ix.columns[..] {
            [c] => Some(c),
            _ => None,
        })
        .collect()
}

/// How `ON CONFLICT (<target>) DO UPDATE` must find the conflicting row.
///
/// The single source of truth for both the planner (which records it) and
/// `CompiledPlan::validate` (which recomputes it and demands a match). A blob
/// claiming "target (email), probe pk" would find a row by PK and report it as
/// the email conflict — the wrong row, silently.
///
/// `None` = the target is neither the PK nor a single secondary UNIQUE column,
/// so there is no key to probe by.
pub(crate) fn conflict_probe_opt(table: &TableDef, target: &[u16]) -> Option<ConflictProbe> {
    if target == table.primary_key {
        return Some(ConflictProbe::Pk);
    }
    // A UNIQUE index whose column SET equals the target set can witness the
    // conflict (#55: composite targets included — order-insensitive, as in
    // PostgreSQL, which matches targets against unique indexes by column
    // set). A non-unique index cannot: nothing stops several rows from
    // sharing the values, so there is no single row to have conflicted
    // with — PG rejects the same shape at prepare.
    let mut want: Vec<u16> = target.to_vec();
    want.sort_unstable();
    let ino = table.indexes.iter().position(|ix| {
        if !ix.unique || ix.columns.len() != want.len() || ix.predicate.is_some() {
            return false;
        }
        let mut cols = ix.columns.clone();
        cols.sort_unstable();
        cols == want
    })?;
    Some(ConflictProbe::Index(ino as u32 + 1))
}

/// The validate-side view: a target that resolves to nothing is corrupt, and
/// `Pk` is the safe thing to compare an unresolvable one against (it will not
/// match a real `Index` plan).
pub(crate) fn conflict_probe(table: &TableDef, target: &[u16]) -> ConflictProbe {
    conflict_probe_opt(table, target).unwrap_or(ConflictProbe::Pk)
}

fn bind_err(msg: impl Into<String>) -> Error {
    Error::Bind(msg.into())
}

/// Count every subplan in the tree — the top-level lifts plus, recursively,
/// each subplan's own nested lifts (#73 §3).
fn total_subplans(subs: &[SubPlan]) -> usize {
    subs.iter()
        .map(|s| {
            1 + total_subplans(&s.subplans)
                + match &s.body {
                    // A compound body's arms own their lifts (format 56); they
                    // are part of THIS plan's tree and count against the same
                    // ceiling the decoder enforces.
                    SubBody::Compound(c) => compound_subplan_total(c),
                    SubBody::Select(_) => 0,
                }
        })
        .sum()
}

/// Every lift a compound's arms own, transitively.
fn compound_subplan_total(c: &CompoundPlan) -> usize {
    c.arm_subplans.iter().map(|a| total_subplans(a)).sum()
}

/// Lifts a statement's COMPONENTS own (never on the statement-level list): a
/// materialized derived table's body (format 52) and a compound's arms
/// (format 56), at every depth.
fn owned_subplan_total(stmt: &PlanStmt) -> usize {
    match stmt {
        PlanStmt::Compound(c) => compound_subplan_total(c),
        PlanStmt::Derived(dp) => {
            total_subplans(&dp.body_subplans)
                + match &dp.body {
                    SubBody::Compound(c) => compound_subplan_total(c),
                    SubBody::Select(_) => 0,
                }
        }
        _ => 0,
    }
}

/// Hook for aggregate + correlated-slot discipline. All positions are legal
/// now: per-row (WHERE → `post_filter`, GROUP BY key, aggregate arg, FILTER)
/// fill via `row_params`; per-group (HAVING, non-key SELECT-list) use the
/// group's first base-row param scratch (sqlite bare-column convention; Django
/// OuterRef on a group key is constant within the group). Kept so a future
/// tightening has a single call site in select/join planners.
fn reject_correlated_in_aggregate(
    sp: &SelectPlan,
    _sub_base: u16,
    _correlated: &[bool],
) -> Result<()> {
    let _ = sp;
    Ok(())
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
            plan_select(s, schema, n_params, catalog, mode, host_udfs, row_count, &mut consts, None)?
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
        match &s.body {
            SubBody::Select(sp) => select_tables(sp, out),
            SubBody::Compound(c) => stamp_compound_tables(c, select_tables, out),
        }
        for c in &s.subplans {
            stamp_subplan_tables(c, select_tables, out);
        }
    }
    // A compound's arms AND the lifts those arms own (format 56).
    fn stamp_compound_tables(
        c: &CompoundPlan,
        select_tables: &impl Fn(&SelectPlan, &mut Vec<u32>),
        out: &mut Vec<u32>,
    ) {
        for arm in &c.arms {
            select_tables(arm, out);
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
        PlanStmt::Derived(dp) => {
            match &dp.body {
                SubBody::Select(sp) => select_tables(sp, &mut stamped),
                SubBody::Compound(c) => stamp_compound_tables(c, &select_tables, &mut stamped),
            }
            // The BODY's own lifts read tables too (format 52) — missing their
            // stamp would let the plan keep serving a since-tightened table.
            for s in &dp.body_subplans {
                stamp_subplan_tables(s, &select_tables, &mut stamped);
            }
            select_tables(&dp.outer, &mut stamped);
        }
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


/// Compile an `ON CONFLICT` action.
///
/// The target must be a key the executor can PROBE: the primary key, or one
/// secondary UNIQUE column. That is the real constraint, and it is not
/// stylistic — the executor has to find the row you conflicted with, and
/// guessing ("you said (email), I will upsert on the PK anyway") updates the
/// wrong row silently.
///
/// A multi-column non-PK target has no probe even when each column is unique
/// on its own: `get_by_index` takes one value, and "unique together" is not
/// something the schema can declare.
fn plan_on_conflict(
    oc: &ast::OnConflict,
    binder: &mut Binder,
    table: &mpedb_types::TableDef,
    _table_id: u32,
    _consts: &mut Vec<Value>,
) -> Result<PlanOnConflict> {
    let (target, set, where_clause) = match oc {
        ast::OnConflict::Error => return Ok(PlanOnConflict::Error),
        ast::OnConflict::DoNothing => return Ok(PlanOnConflict::DoNothing),
        // `INSERT OR REPLACE` is a first-class executor variant: it deletes
        // every existing row the proposed row would conflict with (on the PK OR
        // any secondary UNIQUE index) then inserts — sqlite's real
        // delete-on-any-unique semantics, which a single PK-keyed upsert cannot
        // express (it only covers PK conflicts and updates one row).
        ast::OnConflict::Replace => return Ok(PlanOnConflict::Replace),
        ast::OnConflict::DoUpdate {
            target,
            set,
            where_clause,
        } => (target, set, where_clause),
    };
    let mut tcols = Vec::with_capacity(target.len());
    for name in target {
        let i = table
            .columns
            .iter()
            .position(|c| c.name == *name)
            .ok_or_else(|| bind_err(format!("unknown conflict-target column `{name}`")))?;
        tcols.push(i as u16);
    }
    let Some(probe) = conflict_probe_opt(table, &tcols) else {
        let pk_names: Vec<&str> = table
            .primary_key
            .iter()
            .map(|i| table.columns[*i as usize].name.as_str())
            .collect();
        let mut usable = vec![format!("({})", pk_names.join(", "))];
        // Only UNIQUE indexes can witness a conflict; a non-unique index
        // never can (several rows may share the values).
        for ix in table.indexes.iter().filter(|ix| ix.unique) {
            let names: Vec<&str> = ix
                .columns
                .iter()
                .map(|&c| table.columns[c as usize].name.as_str())
                .collect();
            usable.push(format!("({})", names.join(", ")));
        }
        return Err(bind_err(format!(
            "ON CONFLICT ({}) is not supported: the target must be a key this can probe to \
             find the row you conflicted with — the primary key, or a UNIQUE index's \
             column set. Usable here: {}.",
            target.join(", "),
            usable.join(", ")
        )));
    };
    // `excluded.<c>` is in scope only here, and binds to Col(n + i): the
    // executor runs these over [existing ‖ proposed].
    binder.set_allow_excluded(true);
    let mut bset = Vec::with_capacity(set.len());
    for (name, e) in set {
        let i = table
            .columns
            .iter()
            .position(|c| c.name == *name)
            .ok_or_else(|| bind_err(format!("unknown column `{name}` in DO UPDATE SET")))?;
        if table.columns[i].generated.is_some() {
            binder.set_allow_excluded(false);
            return Err(bind_err(format!(
                "cannot UPDATE generated column `{name}`"
            )));
        }
        let (b, ty) = binder.bind_expr(e)?;
        if let Some(t) = ty {
            // Same rule as `bind_assign`: `any` accepts every typed value, and
            // so does a column that converts on store (#113) — the conversion
            // runs at write time and the engine validates its result.
            if t != table.columns[i].ty
                && table.columns[i].ty != ColumnType::Any
                && !table.columns[i].converts_on_store()
            {
                binder.set_allow_excluded(false);
                return Err(bind_err(format!(
                    "cannot assign {t} to column `{name}` of type {}",
                    table.columns[i].ty
                )));
            }
        }
        bset.push((i as u16, compile_program(&b)?));
    }
    let filter = match where_clause {
        Some(w) => {
            let (b, ty) = binder.bind_expr(w)?;
            // A boolean context like any other: a non-bool is truthy-tested the
            // way sqlite does (`Binder::coerce_bool_ctx`).
            let (b, ty) = match binder.coerce_bool_ctx(b, ty) {
                Ok(v) => v,
                Err(e) => {
                    binder.set_allow_excluded(false);
                    return Err(e);
                }
            };
            if !matches!(ty, Some(ColumnType::Bool) | None) {
                binder.set_allow_excluded(false);
                return Err(bind_err("ON CONFLICT ... WHERE must be a bool condition"));
            }
            Some(compile_program(&b)?)
        }
        None => None,
    };
    binder.set_allow_excluded(false);
    Ok(PlanOnConflict::DoUpdate {
        target: tcols,
        probe,
        set: bset,
        filter,
    })
}

/// Compile a `RETURNING` clause into a projection over the written row.
fn plan_returning(
    r: Option<&Option<Vec<ast::Expr>>>,
    binder: &mut Binder,
    table: &mpedb_types::TableDef,
) -> Result<Option<Vec<Projection>>> {
    let Some(items) = r else { return Ok(None) };
    let Some(items) = items else {
        // RETURNING * — the VISIBLE columns only; the hidden implicit rowid is
        // never surfaced by a star (#94), exactly as `SELECT *`.
        return Ok(Some(
            (0..table.visible_column_count() as u16).map(Projection::Column).collect(),
        ));
    };
    let mut proj = Vec::with_capacity(items.len());
    for e in items {
        match e {
            ast::Expr::Col(name) => {
                let i = table
                    .columns
                    .iter()
                    .position(|c| c.name == *name)
                    .ok_or_else(|| bind_err(format!("unknown column `{name}` in RETURNING")))?;
                proj.push(Projection::Column(i as u16));
            }
            other => {
                let (b, _) = binder.bind_expr(other)?;
                proj.push(Projection::Expr {
                    program: compile_program(&b)?,
                    name: render_expr_name(other),
                });
            }
        }
    }
    Ok(Some(proj))
}

/// A display name for a RETURNING expression item.
fn render_expr_name(e: &ast::Expr) -> String {
    match e {
        ast::Expr::Col(c) => c.clone(),
        _ => "?column?".to_string(),
    }
}


/// Bind and plan a compound SELECT: plan each arm as an ordinary select, then
/// check the arms AGREE — same arity, same output types (rigid engine: no
/// sqlite-style cross-arm coercion; `CAST` one side instead), one shared
/// parameter table — and resolve the compound-level ORDER BY against the
/// first arm's output.
#[allow(clippy::too_many_arguments)]
fn plan_compound(
    c: &ast::CompoundStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let mut arms: Vec<SelectPlan> = Vec::with_capacity(c.arms.len());
    let mut param_types: Vec<Option<ColumnType>> = Vec::new();
    let mut context_keys: Vec<String> = Vec::new();
    let mut list_keys: BTreeSet<String> = BTreeSet::new();
    let mut out_types: Vec<Option<ColumnType>> = Vec::new();
    // Each arm OWNS its lifts (format 56). See `CompoundPlan::arm_subplans`.
    let mut arm_subplans: Vec<Vec<SubPlan>> = Vec::with_capacity(c.arms.len());
    let mut n_slots: u16 = 0;

    for (k, arm_ast) in c.arms.iter().enumerate() {
        // Arm-local subplans take the reserved slots AFTER those of the arms
        // before them: planning arm `k` with the accumulated count as its
        // parameter base numbers its `Param` references against the FINAL
        // statement layout `[level params ‖ arm0 subs ‖ arm1 subs ‖ …]` by
        // construction — the cross-arm slot coordination the old refusal
        // asked for, with no post-hoc remap to get wrong.
        let arm_base = n_params + n_slots;
        let (stmt, ptypes, ckeys, lkeys, otypes, arm_subs) =
            plan_select(arm_ast, schema, arm_base, catalog, mode, host_udfs, row_count, consts, None)?;
        let PlanStmt::Select(sp) = stmt else {
            return Err(Error::Internal("plan_select produced a non-select".into()));
        };
        // A CORRELATED arm subplan used to be refused here — the arm executor
        // (`exec_select`) has no per-row fill phase, so its slot would have been
        // an unfilled hole. It is no longer HOISTED to the statement: the arm
        // OWNS it, and `exec_compound` runs the arm through
        // `exec_select_leveled` — the identical discipline `exec_derived` runs
        // for a body-owned lift, and the top level for its own. So a correlated
        // lift is filled per ARM row, which is the only row it can name, and
        // the arm may carry the matching `post_filter`.
        n_slots = n_slots
            .checked_add(arm_subs.len() as u16)
            .ok_or_else(|| bind_err("too many subqueries in one compound SELECT"))?;
        if n_slots as usize > MAX_PLAN_SUBPLANS {
            return Err(bind_err(
                "too many subqueries in one compound SELECT (max 16 across all arms)",
            ));
        }
        arm_subplans.push(arm_subs);
        // Context slots are appended AFTER the user params, so two arms
        // binding different key sets would give the same slot index two
        // meanings. Identical key lists (the common case: same policy on the
        // same table) line up by construction; anything else is refused
        // rather than silently misread.
        if k == 0 {
            context_keys = ckeys;
        } else if ckeys != context_keys {
            return Err(bind_err(
                "compound arms bind different session-context keys — not supported yet",
            ));
        }
        // One statement, one parameter table: unify element-wise. Arms may
        // return tables of different lengths now (each covers its own
        // reserved slots); a slot outside an arm's table is simply
        // unconstrained by that arm.
        for (i, t) in ptypes.iter().enumerate() {
            if param_types.len() <= i {
                param_types.push(None);
            }
            match (&param_types[i], t) {
                (None, Some(t)) => param_types[i] = Some(*t),
                (Some(a), Some(b)) if a != b => {
                    return Err(bind_err(format!(
                        "parameter ${} is used as {a} in one compound arm and {b} in another",
                        i + 1
                    )));
                }
                _ => {}
            }
        }
        list_keys.extend(lkeys);

        // Arms must agree on the output shape. `None` (a bare NULL item) is
        // compatible with anything — it stays NULL whatever the column is.
        if k == 0 {
            out_types = otypes;
        } else {
            if otypes.len() != out_types.len() {
                return Err(bind_err(format!(
                    "compound arms must select the same number of columns \
                     (first arm has {}, arm {} has {})",
                    out_types.len(),
                    k + 1,
                    otypes.len()
                )));
            }
            for (j, (have, arm)) in out_types.iter_mut().zip(&otypes).enumerate() {
                match (&have, arm) {
                    (None, Some(t)) => *have = Some(*t),
                    // A DYNAMICALLY typed arm (`any` — a typeless column, a
                    // host UDF, a per-row CASE) unifies with any concrete type
                    // and the column stays `any`, exactly as an `any` operand
                    // does in a comparison or a CASE arm. sqlite has no static
                    // column type for a compound at all: every row keeps the
                    // storage class its own arm produced, which is what `any`
                    // says. Two DIFFERENT concrete types still refuse — there
                    // the arms really do disagree about the value's type.
                    (Some(ColumnType::Any), Some(_)) => {}
                    (Some(_), Some(ColumnType::Any)) => *have = Some(ColumnType::Any),
                    (Some(a), Some(b)) if a != b => {
                        return Err(bind_err(format!(
                            "column {} of the compound is {a} in one arm and {b} in \
                             another — CAST one side to make them agree",
                            j + 1
                        )));
                    }
                    _ => {}
                }
            }
        }
        arms.push(sp);
    }

    // The compound-level ORDER BY names the OUTPUT: an ordinal, a first-arm
    // output name (a select-item alias or a plain column's name), nothing
    // else — no tuple upstream of the set op survives to be sorted.
    let arity = out_types.len();
    let out_name = |sp: &SelectPlan, j: usize| -> Option<String> {
        match sp.projection.get(j)? {
            Projection::Expr { name, .. } => Some(name.clone()),
            Projection::Column(i) => {
                let t = schema.table(sp.table)?;
                // Only a single-table arm has unambiguous bare names; a
                // joined arm's slot names are qualified and never match a
                // bare ORDER BY identifier.
                if sp.joins.is_empty() {
                    t.columns.get(*i as usize).map(|col| col.name.clone())
                } else {
                    None
                }
            }
        }
    };
    let mut order_by: OrderKeys = Vec::with_capacity(c.order_by.len());
    for (e, dir) in &c.order_by {
        // Peel an explicit `COLLATE` off the term; the inner expression resolves
        // to an output column/ordinal as before, and the collation rides the sort.
        let (e, coll) = peel_order_collate(e, host_udfs.colls())?;
        let coll = coll.unwrap_or_default();
        if let Some(pos) = select::ordinal(e, arity)? {
            order_by.push((pos, *dir, coll));
            continue;
        }
        let ast::Expr::Col(n) = e else {
            return Err(bind_err(
                "ORDER BY over a compound must name an output column or ordinal",
            ));
        };
        let pos = (0..arity).find(|&j| {
            out_name(&arms[0], j).is_some_and(|nm| nm.eq_ignore_ascii_case(n))
        });
        match pos {
            Some(j) => order_by.push((j as u16, *dir, coll)),
            None => {
                return Err(bind_err(format!(
                    "ORDER BY `{n}` does not name an output column of the compound's \
                     first SELECT"
                )))
            }
        }
    }

    // Context slots sit LAST in the layout `[user ‖ subplan results ‖ context]`,
    // but each arm numbered its own context slots right after ITS reserved
    // region — with per-arm subplan offsets in play the positions no longer
    // agree across arms. Refuse the combination rather than misnumber a slot.
    if n_slots != 0 && !context_keys.is_empty() {
        return Err(bind_err(
            "current_setting() and a subquery together in a compound SELECT are \
             not supported yet",
        ));
    }
    // Canonical shape: a lift-free compound carries NO per-arm lists at all, so
    // its bytes (and therefore its plan hash) are what they were before an arm
    // could own anything.
    if n_slots == 0 {
        arm_subplans.clear();
    }
    // The parameter table must SPAN the whole reserved region even when the last
    // arm lifted nothing (its `ptypes` then stop short of the layout's end):
    // `n_params` is taken from this vector's length one level up.
    if param_types.len() < (n_params + n_slots) as usize {
        param_types.resize((n_params + n_slots) as usize, None);
    }
    let ops = c.ops.clone();
    Ok((
        PlanStmt::Compound(CompoundPlan {
            arms,
            ops,
            order_by,
            limit: c.limit,
            offset: c.offset,
            arm_subplans,
            arm_sub_base: n_params,
        }),
        param_types,
        context_keys,
        list_keys,
        out_types,
        // The arms OWN their lifts — the statement-level list stays EMPTY
        // (validate-enforced), exactly as it does for a derived table.
        Vec::new(),
    ))
}

/// `INSERT … VALUES (<expression>)` → `INSERT … SELECT <expression>`, or
/// `None` to leave the statement alone.
///
/// Only a SINGLE row is rewritten — a multi-row VALUES would need
/// `UNION ALL`, which the INSERT … SELECT path refuses by name — and only when
/// the row holds something the VALUES path cannot carry. That is decided by
/// BINDING it: a bare parameter and anything that const-folds (`1 + 1`,
/// `-24`) stay on the VALUES path with their existing constant coercion and
/// NOT-NULL checks, so no statement that compiles today changes shape. A bind
/// error here yields `None` too — the real path reports it, with its own
/// message.
fn values_as_select(
    s: &ast::InsertStmt,
    table: &mpedb_types::TableDef,
    n_params: u16,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
) -> Option<ast::InsertStmt> {
    if s.select.is_some() || s.rows.len() != 1 {
        return None;
    }
    let row = &s.rows[0];
    let mut probe = Binder::new(table, n_params, true);
    probe.set_dialect(mode);
    probe.set_host_udfs(host_udfs);
    let needs_select = row.iter().any(|e| match probe.bind_expr(e) {
        Ok((BExpr::Const(_), _)) | Ok((BExpr::Param(_), _)) => false,
        Ok(_) => true,
        // A subquery has no bound form OUTSIDE a SELECT — the binder refuses it
        // by name — so a refusal is the signal too, for everything that is not
        // already a literal or a bare parameter (which cannot fail to bind).
        // The SELECT path then reports whatever the real error is.
        Err(_) => !matches!(e, ast::Expr::Lit(_) | ast::Expr::Param(_)),
    });
    if !needs_select {
        return None;
    }
    Some(ast::InsertStmt {
        table: s.table.clone(),
        columns: s.columns.clone(),
        rows: Vec::new(),
        select: Some(Box::new(ast::SelectStmt {
            table: None,
            from_derived: None,
            alias: None,
            joins: Vec::new(),
            distinct: false,
            items: Some(row.iter().map(|e| (e.clone(), None)).collect()),
            where_clause: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        drop_trailing: 0,
        })),
        on_conflict: s.on_conflict.clone(),
        returning: s.returning.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
fn plan_insert(
    s: &ast::InsertStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;

    // `VALUES (<expression>)` — a function call, a scalar subquery, arithmetic
    // over one — is the same statement as `SELECT <expression>` with no FROM:
    // sqlite evaluates a VALUES row exactly once, over no row, which is what a
    // FROM-less SELECT already is here. Django writes it for every
    // `RETURNING` insert of a database function (`STRFTIME(…)`, `LOWER(?)`).
    // Rewritten rather than given its own `InsertSource` variant: the
    // INSERT … SELECT path already evaluates, projects and RETURNs, and this
    // needs no plan-format change. The trigger is a row that the VALUES path
    // would REFUSE (nothing that folds to a constant or is a bare parameter
    // moves), so every statement that compiles today keeps its exact plan.
    if let Some(rewritten) = values_as_select(s, table, n_params, mode, host_udfs) {
        return plan_insert(
            &rewritten, schema, n_params, catalog, mode, host_udfs, row_count, consts,
        );
    }

    let mut binder = Binder::new(table, n_params, true);
    binder.set_dialect(mode);
    binder.set_host_udfs(host_udfs);

    // Map each table column to its position in the VALUES tuples (or None).
    let listed: Vec<u16> = match &s.columns {
        Some(names) => {
            let mut cols = Vec::with_capacity(names.len());
            for name in names {
                let idx = table.column_index(name).ok_or_else(|| {
                    bind_err(format!("unknown column `{name}` in table `{}`", table.name))
                })?;
                if cols.contains(&idx) {
                    return Err(bind_err(format!("duplicate column `{name}` in INSERT")));
                }
                // A generated column's value is the expression's, always.
                // sqlite: "cannot INSERT into generated column".
                if table.columns[idx as usize].generated.is_some() {
                    return Err(bind_err(format!(
                        "cannot INSERT into generated column `{name}`"
                    )));
                }
                cols.push(idx);
            }
            cols
        }
        // No column list: the VISIBLE, non-GENERATED columns, in order. A hidden
        // implicit rowid (#94) is NOT listed — it falls through to
        // `InsertSource::Default` below and the rowid-alias auto-assign (#85)
        // fills it with `max(rowid)+1`. A generated column is not listed either:
        // sqlite's `INSERT INTO t VALUES (…)` counts only the non-generated
        // columns, and the value is computed at write time.
        None => (0..table.visible_column_count() as u16)
            .filter(|&i| table.columns[i as usize].generated.is_none())
            .collect(),
    };
    let mut slot_of_col: Vec<Option<usize>> = vec![None; table.columns.len()];
    for (slot, &col) in listed.iter().enumerate() {
        slot_of_col[col as usize] = Some(slot);
    }
    // A single-column INTEGER PRIMARY KEY is a rowid alias (sqlite): an omitted
    // or NULL value auto-assigns at execution time, so it is exempt from both
    // the "NOT NULL must be inserted" rule below and the NULL-const rejection.
    // The auto-assign is carried as `InsertSource::Default` on that column —
    // unambiguous, since a NOT-NULL no-default PK column could never take a
    // Default before this feature (so no plan format change is needed).
    let rowid_col = table.rowid_alias_col();
    // Columns omitted from the list must be defaultable (the rowid alias is not).
    for (ci, col) in table.columns.iter().enumerate() {
        if slot_of_col[ci].is_none()
            && !col.nullable
            && col.default.is_none()
            && col.generated.is_none()
            && Some(ci as u16) != rowid_col
        {
            return Err(bind_err(format!(
                "column `{}` is NOT NULL without a default and must be inserted",
                col.name
            )));
        }
    }

    // INSERT … SELECT: plan the source query and map its output tuple to the
    // target columns. Its params/context/list keys and subplans merge into
    // this statement's below.
    let mut from_select = None;
    let mut sel_ptypes: Vec<Option<ColumnType>> = Vec::new();
    let mut sel_ctx: Vec<String> = Vec::new();
    let mut sel_list: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut sel_subplans: Vec<SubPlan> = Vec::new();
    if let Some(sel_stmt) = &s.select {
        let (sp_stmt, sp_pt, sp_ctx, sp_list, _sp_agg, sp_sub) =
            plan_select(sel_stmt, schema, n_params, catalog, mode, host_udfs, row_count, consts, None)?;
        let PlanStmt::Select(sp) = sp_stmt else {
            return Err(bind_err(
                "INSERT … SELECT: a compound (UNION/EXCEPT/INTERSECT) source is not supported",
            ));
        };
        if sp.projection.len() != listed.len() {
            return Err(bind_err(format!(
                "INSERT … SELECT: the source has {} column(s), but {} are expected",
                sp.projection.len(),
                listed.len()
            )));
        }
        let col_map: Vec<Option<u16>> =
            slot_of_col.iter().map(|s| s.map(|x| x as u16)).collect();
        from_select = Some(crate::plan::InsertSelect { plan: Box::new(sp), col_map });
        sel_ptypes = sp_pt;
        sel_ctx = sp_ctx;
        sel_list = sp_list;
        sel_subplans = sp_sub;
    }

    let mut rows = Vec::with_capacity(s.rows.len());
    for row in &s.rows {
        if from_select.is_some() {
            return Err(bind_err("INSERT cannot have both VALUES and a SELECT source"));
        }
        if row.len() != listed.len() {
            return Err(bind_err(format!(
                "INSERT row has {} values, expected {}",
                row.len(),
                listed.len()
            )));
        }
        let mut sources = Vec::with_capacity(table.columns.len());
        for (ci, col) in table.columns.iter().enumerate() {
            let src = match slot_of_col[ci] {
                None => InsertSource::Default,
                Some(slot) => {
                    let (b, _) = binder.bind_expr(&row[slot])?;
                    match b {
                        // An explicit NULL on the rowid-alias PK auto-assigns,
                        // exactly like an omitted value — carried as Default and
                        // resolved to max(rowid)+1 at execution.
                        BExpr::Const(v) if v.is_null() && Some(ci as u16) == rowid_col => {
                            InsertSource::Default
                        }
                        BExpr::Const(v) => {
                            // On a column that CONVERTS on store (#113) sqlite's
                            // affinity is the WHOLE rule, and `coerce_const`
                            // must not run on top of it: its float→int step is
                            // looser than `sqlite3VdbeIntegerAffinity`, and
                            // stacking the two stored `'-9223372036854775809'`
                            // as the clamped i64 MIN where sqlite keeps the
                            // real. A boolean is folded to its integer first,
                            // because sqlite has no boolean storage class for
                            // an affinity to see.
                            let v = if col.converts_on_store() {
                                col.store(match v {
                                    Value::Bool(b) if binder.sqlite_dialect() => {
                                        Value::Int(b as i64)
                                    }
                                    other => other,
                                })
                            } else {
                                coerce_const(v, col.ty, binder.sqlite_dialect())
                            };
                            if v.is_null() && !col.nullable {
                                return Err(bind_err(format!(
                                    "cannot insert NULL into NOT NULL column `{}`",
                                    col.name
                                )));
                            }
                            if !v.fits(col.ty) {
                                // Name the reason when `coerce_const` TRIED and
                                // the value itself was the obstacle — sqlite
                                // STRICT refuses this one too ("cannot store
                                // REAL value in INT column"), so saying which
                                // real is the useful half of the message.
                                let why = match (&v, col.ty) {
                                    (Value::Float(_), ColumnType::Int64) => {
                                        " — it is not exactly an integer in the int64 range"
                                    }
                                    _ => "",
                                };
                                return Err(bind_err(format!(
                                    "value of type {} cannot be inserted into column `{}` of type {}{}",
                                    v.type_name(),
                                    col.name,
                                    col.ty,
                                    why
                                )));
                            }
                            InsertSource::Const(push_plan_const(consts, v)?)
                        }
                        BExpr::Param(i) => {
                            // A column that CONVERTS on store pins nothing: the
                            // bound value goes through sqlite's store affinity
                            // and is validated AFTER conversion, which is the
                            // whole point (`INSERT INTO t(name) VALUES (?)`
                            // with an integer bound stores `'1'`). See
                            // `ColumnDef::converts_on_store`.
                            if !col.converts_on_store() {
                                match binder.param_types[i as usize] {
                                    None => binder.param_types[i as usize] = Some(col.ty),
                                    Some(t) if t == col.ty => {}
                                    Some(t) => {
                                        return Err(bind_err(format!(
                                            "parameter ${} already inferred as {t}, but column `{}` is {}",
                                            i + 1,
                                            col.name,
                                            col.ty
                                        )))
                                    }
                                }
                            }
                            InsertSource::Param(i)
                        }
                        // Expression cell (Django bulk_create Now(), arithmetic,
                        // scalar subquery in VALUES, multi-row with mixed
                        // literals). Evaluated over the dual row at insert time.
                        other => {
                            let program = compile_program(&other)?;
                            InsertSource::Expr(program)
                        }
                    }
                }
            };
            sources.push(src);
        }
        rows.push(sources);
    }

    // RLS gate on the new row (INSERT ignores USING; WITH CHECK is the sole gate).
    let with_check = write_check(&mut binder, catalog, table_id, &table.name, PolicyCmd::Insert)?
        .map(|b| compile_program(&b))
        .transpose()?;

    // §6.5: ON CONFLICT is refused on an RLS table rather than silently
    // weakening the classification-oracle closure. `with_check.is_some()` is
    // exact — the planner emits it iff RLS is enabled on the target — and it is
    // the same signal hide_constraint_variant keys off, so the two cannot drift.
    if !matches!(s.on_conflict, ast::OnConflict::Error) && with_check.is_some() {
        return Err(bind_err(format!(
            "ON CONFLICT is not supported on `{}`, which has row-level security \
             (DESIGN-MULTIDB §6.5): a silent skip would tell the caller that a row it \
             cannot see exists, and DO UPDATE would overwrite one. Use a plain INSERT and \
             handle the rejection.",
            table.name
        )));
    }

    let on_conflict = plan_on_conflict(&s.on_conflict, &mut binder, table, table_id, consts)?;
    let returning = plan_returning(s.returning.as_ref(), &mut binder, table)?;

    let (mut param_types, mut context_keys, mut list_keys) = binder.into_parts();
    // Merge the source query's inferences into this statement's (INSERT …
    // SELECT). Param spaces are shared (both planned against the same
    // `n_params`), so unify element-wise; a genuine type conflict is an error.
    if from_select.is_some() {
        if param_types.len() < sel_ptypes.len() {
            param_types.resize(sel_ptypes.len(), None);
        }
        for (i, t) in sel_ptypes.into_iter().enumerate() {
            if let Some(t) = t {
                match param_types[i] {
                    None => param_types[i] = Some(t),
                    Some(existing) if existing == t => {}
                    Some(existing) => {
                        return Err(bind_err(format!(
                            "parameter ${} used as both {existing} and {t}",
                            i + 1
                        )))
                    }
                }
            }
        }
        context_keys.extend(sel_ctx);
        context_keys.sort();
        context_keys.dedup();
        list_keys.extend(sel_list);
        // A source item that is a BARE parameter lands in exactly one column,
        // so it takes that column's type — the same inference the VALUES path
        // makes for `VALUES (?)`. Without it a `VALUES (LOWER(?), ?)` rewritten
        // to a SELECT would leave the second parameter untyped, and a caller
        // that relies on the declared type to convert (the C-API shim turns an
        // `int` 0/1 into a `bool` when the plan says the column is one) would
        // send the wrong storage class. Only fills a slot nothing else typed.
        if let Some(sel) = &s.select {
            if let Some(items) = &sel.items {
                for (slot, (item, _)) in items.iter().enumerate() {
                    let (Some(&col), ast::Expr::Param(i)) = (listed.get(slot), item) else {
                        continue;
                    };
                    let i = *i as usize;
                    if param_types.get(i).is_some_and(|t| t.is_none()) {
                        param_types[i] = Some(table.columns[col as usize].ty);
                    }
                }
            }
        }
    }
    Ok((
        PlanStmt::Insert {
            table: table_id,
            rows,
            from_select,
            with_check,
            on_conflict,
            returning,
        },
        param_types,
        context_keys,
        list_keys,
        Vec::new(),
        sel_subplans,
    ))
}

#[allow(clippy::too_many_arguments)]
fn plan_update(
    s: &ast::UpdateStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
    // Subqueries in the WHERE lift out FIRST (#97), exactly as they do for a
    // SELECT: each becomes a `SubPlan` + reserved slot and is replaced by
    // `Param(slot)`, so everything below sees only a parameter.
    let (where_ast, subplans, slot_types) = lift_where(
        s.where_clause.as_ref(), table, &s.table, schema, n_params, catalog, mode, host_udfs,
        row_count, consts, "UPDATE",
    )?;
    let eff_params = n_params + subplans.len() as u16;
    let mut binder = Binder::new(table, eff_params, true);
    binder.set_dialect(mode);
    binder.set_host_udfs(host_udfs);
    for (i, ty) in slot_types.iter().enumerate() {
        binder.pin_param(n_params + i as u16, *ty);
    }

    // sqlite (R-34751-18293): when a column is assigned more than once, all but
    // the RIGHTMOST occurrence is ignored — not evaluated, not type-checked. So
    // resolve each name, then keep only the last expression per column (in
    // first-appearance order) and bind/compile just those. The executor
    // evaluates every SET against the OLD row, so collapsing duplicates never
    // changes a surviving assignment.
    let mut last_expr: Vec<Option<&ast::Expr>> = vec![None; table.columns.len()];
    let mut order: Vec<u16> = Vec::new();
    for (name, expr) in &s.set {
        let idx = table.column_index(name).ok_or_else(|| {
            bind_err(format!("unknown column `{name}` in table `{}`", table.name))
        })?;
        if table.is_pk_column(idx) {
            return Err(bind_err(format!(
                "cannot update primary key column `{name}`"
            )));
        }
        // sqlite: "cannot UPDATE generated column".
        if table.columns[idx as usize].generated.is_some() {
            return Err(bind_err(format!(
                "cannot UPDATE generated column `{name}`"
            )));
        }
        if last_expr[idx as usize].is_none() {
            order.push(idx);
        }
        last_expr[idx as usize] = Some(expr);
    }
    let mut set = Vec::with_capacity(order.len());
    for idx in order {
        let col = &table.columns[idx as usize];
        let expr = last_expr[idx as usize].expect("recorded in order");
        let b = binder.bind_assign(expr, col)?;
        set.push((idx, compile_program(&b)?));
    }

    let bound_where = where_ast
        .as_ref()
        .map(|e| binder.bind_predicate(e))
        .transpose()?;
    // The UPDATE policy restricts the target set, and (read-via-write) the
    // SELECT policy is folded in too — see `read_policy`.
    let policy = read_policy(&mut binder, catalog, table_id, &table.name, PolicyCmd::Update)?;
    let (access, residual) = extract_access(merge_and(bound_where, policy), table, consts)?;
    let filter = residual.map(|e| compile_program(&e)).transpose()?;

    // WITH CHECK gates the post-image (falls back to USING per PG rule).
    let with_check = write_check(&mut binder, catalog, table_id, &table.name, PolicyCmd::Update)?
        .map(|b| compile_program(&b))
        .transpose()?;
    let returning = plan_returning(s.returning.as_ref(), &mut binder, table)?;
    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Update {
            returning,
            table: table_id,
            access,
            filter,
            set,
            with_check,
        },
        param_types,
        context_keys,
        list_keys,
        Vec::new(),
        subplans,
    ))
}

/// The shared WHERE-lift both write planners run (#97). `None` / subquery-free
/// WHERE clauses take the zero-cost path and produce no subplans at all.
#[allow(clippy::too_many_arguments)]
fn lift_where(
    where_clause: Option<&ast::Expr>,
    table: &TableDef,
    table_name: &str,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
    op: &str,
) -> Result<(Option<ast::Expr>, Vec<SubPlan>, Vec<Ty>)> {
    match where_clause {
        Some(w) if subquery::expr_has_subquery(w) => {
            let (e, subs, tys) = subquery::lift_dml_where(
                w, table, table_name, schema, n_params, catalog, mode, host_udfs, row_count, consts, op,
            )?;
            Ok((Some(e), subs, tys))
        }
        other => Ok((other.cloned(), Vec::new(), Vec::new())),
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_delete(
    s: &ast::DeleteStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
    // Subqueries in the WHERE lift out FIRST (#97) — see `plan_update`.
    let (where_ast, subplans, slot_types) = lift_where(
        s.where_clause.as_ref(), table, &s.table, schema, n_params, catalog, mode, host_udfs,
        row_count, consts, "DELETE",
    )?;
    let eff_params = n_params + subplans.len() as u16;
    let mut binder = Binder::new(table, eff_params, true);
    binder.set_dialect(mode);
    binder.set_host_udfs(host_udfs);
    for (i, ty) in slot_types.iter().enumerate() {
        binder.pin_param(n_params + i as u16, *ty);
    }
    let bound_where = where_ast
        .as_ref()
        .map(|e| binder.bind_predicate(e))
        .transpose()?;
    let policy = read_policy(&mut binder, catalog, table_id, &table.name, PolicyCmd::Delete)?;
    let (access, residual) = extract_access(merge_and(bound_where, policy), table, consts)?;
    let filter = residual.map(|e| compile_program(&e)).transpose()?;
    let returning = plan_returning(s.returning.as_ref(), &mut binder, table)?;
    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Delete {
            returning,
            table: table_id,
            access,
            filter,
        },
        param_types,
        context_keys,
        list_keys,
        Vec::new(),
        subplans,
    ))
}

/// Fold a constant into its column's type where the conversion is EXACT.
///
/// Two cases: the Int -> Float widening, and (sqlite dialect only) the
/// int/bool bridge — sqlite has no boolean type, so `INSERT INTO t (flag)
/// VALUES (1)` on a `BooleanField` is the shape Django emits. Only the
/// literals 0 and 1 convert: sqlite would store `2` in its `bool` column and
/// hand `2` back, which mpedb's rigid `Bool` cannot represent, so anything
/// else falls through to the `fits` check and is refused rather than guessed.
/// A `Bool` constant landing in an int64 column goes the other way and is
/// always exact — that IS sqlite's storage (`TRUE` -> 1).
///
/// The Float -> Int direction (task #74) is sqlite's INTEGER affinity: a real
/// is stored as an integer exactly when the round trip is lossless, so
/// `INSERT INTO t (i) VALUES (8.0)` stores the integer 8 in sqlite and here.
/// `8.5` is NOT converted — it falls through to the caller's `fits` check and
/// is refused, because sqlite would keep the real in its typeless column and
/// mpedb's rigid int64 cannot. Both dialects agree on the lossless case, so
/// unlike the bool bridges this one is not dialect-gated.
fn coerce_const(v: Value, ty: ColumnType, sqlite: bool) -> Value {
    match (&v, ty) {
        (Value::Int(i), ColumnType::Float64) => Value::Float(*i as f64),
        (Value::Float(f), ColumnType::Int64) => match exact_float_as_int(*f) {
            Some(i) => Value::Int(i),
            None => v,
        },
        (Value::Int(i @ (0 | 1)), ColumnType::Bool) if sqlite => Value::Bool(*i == 1),
        (Value::Bool(b), ColumnType::Int64) if sqlite => Value::Int(*b as i64),
        _ => v,
    }
}

fn push_plan_const(consts: &mut Vec<Value>, v: Value) -> Result<u16> {
    if consts.len() >= u16::MAX as usize {
        return Err(bind_err("statement has too many constants"));
    }
    consts.push(v);
    Ok((consts.len() - 1) as u16)
}

// ---- access-path extraction -------------------------------------------------

/// A `col <op> atom` conjunct usable for key extraction.
#[derive(Clone)]
enum Atom {
    Param(u16),
    Const(Value),
}

impl Atom {
    fn to_key_part(&self, consts: &mut Vec<Value>) -> Result<KeyPart> {
        Ok(match self {
            Atom::Param(i) => KeyPart::Param(*i),
            Atom::Const(v) => KeyPart::Const(push_plan_const(consts, v.clone())?),
        })
    }
}

fn as_atom(e: &BExpr) -> Option<Atom> {
    match e {
        BExpr::Param(i) => Some(Atom::Param(*i)),
        // NULL never matches a key (PK/unique probes are on non-null values);
        // leave such conjuncts in the residual filter.
        BExpr::Const(v) if !v.is_null() => Some(Atom::Const(v.clone())),
        _ => None,
    }
}

/// `col <cmp> atom` (either operand order; op flipped when reversed).
/// Also matches [`BExpr::ClassCmp`] (inequality with free params) so a
/// `pk >= $1` bound still becomes a PkRange after the float-param compare fix.
fn as_col_cmp(e: &BExpr) -> Option<(u16, BinOp, Atom)> {
    let flipped = |op: BinOp| match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    };
    let (op, l, r) = match e {
        BExpr::Binary(op, l, r) => (*op, l.as_ref(), r.as_ref()),
        BExpr::ClassCmp(op, l, r, _, _) => (*op, l.as_ref(), r.as_ref()),
        _ => return None,
    };
    match (l, r) {
        (BExpr::Col(c), rhs) => as_atom(rhs).map(|a| (*c, op, a)),
        (lhs, BExpr::Col(c)) => as_atom(lhs).map(|a| (*c, flipped(op), a)),
        _ => None,
    }
}

/// AND a conjunct list back together, preserving order. `None` for an empty
/// list — the callers all mean "no predicate" by that.
fn and_all(conjuncts: Vec<BExpr>) -> Option<BExpr> {
    conjuncts.into_iter().reduce(and)
}

/// The highest column slot an expression reads, or `None` for a column-free
/// expression (consts/params only). What the #65 pushdown places conjuncts
/// by: left-deep prefixes share slot numbering, so a conjunct is evaluable
/// at exactly the steps whose accumulated width exceeds this.
fn max_col(e: &BExpr) -> Option<u16> {
    let mut m: Option<u16> = None;
    let mut stack = vec![e];
    while let Some(e) = stack.pop() {
        match e {
            BExpr::Col(c) => m = Some(m.map_or(*c, |p| p.max(*c))),
            BExpr::Unary(_, a)
            | BExpr::Like(a, _, _, _)
            | BExpr::Glob(a, _)
            | BExpr::Regexp(a, _)
            | BExpr::Cast(a, _)
            | BExpr::InParam(a, _) => stack.push(a),
            BExpr::Binary(_, a, b)
            | BExpr::IsDistinct(a, b, _)
            | BExpr::CollateCmp(_, a, b, _)
            | BExpr::RegexpDyn(a, b)
            | BExpr::LikeDyn(a, b, _, _)
            | BExpr::GlobDyn(a, b)
            | BExpr::ClassCmp(_, a, b, _, _) => {
                stack.push(a);
                stack.push(b);
            }
            BExpr::InList(a, list) | BExpr::InListColl(a, list, _) => {
                stack.push(a);
                stack.extend(list.iter());
            }
            BExpr::Case(arms, else_) => {
                for (c, r) in arms {
                    stack.push(c);
                    stack.push(r);
                }
                if let Some(e) = else_ {
                    stack.push(e);
                }
            }
            BExpr::Coalesce(args) | BExpr::Call(_, args) | BExpr::HostCall { args, .. } => {
                stack.extend(args.iter())
            }
            BExpr::Const(_) | BExpr::Param(_) => {}
        }
    }
    m
}

fn split_and(e: BExpr, out: &mut Vec<BExpr>) {
    match e {
        BExpr::Binary(BinOp::And, l, r) => {
            split_and(*l, out);
            split_and(*r, out);
        }
        other => out.push(other),
    }
}


/// Re-AND the unconsumed conjuncts, preserving statement order.
fn rebuild_residual(conjuncts: Vec<BExpr>, consumed: &[bool]) -> Option<BExpr> {
    let mut rest = conjuncts
        .into_iter()
        .zip(consumed)
        .filter_map(|(c, &used)| if used { None } else { Some(c) });
    let first = rest.next()?;
    Some(rest.fold(first, |acc, c| {
        BExpr::Binary(BinOp::And, Box::new(acc), Box::new(c))
    }))
}
