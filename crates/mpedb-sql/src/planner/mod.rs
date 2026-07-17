//! Physical planning: decompose WHERE into AND-conjuncts, extract the access
//! path (PK point > PK range > secondary unique point > full scan), compute
//! the residual filter, elide provably redundant ORDER BY, and compute the
//! precomputed footprint (DESIGN.md §7.3).

use crate::ast::{self, BinOp};
use std::collections::BTreeSet;

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
use crate::binder::{compile_program, BExpr, Binder, Scope, Ty};
use crate::plan::{
    render_program, AccessPath, AggCall, Aggregation, CompiledPlan, ConflictProbe, InsertSource,
    CompoundPlan, GroupKey, Join, JoinKind, OrderOver, PlanOnConflict, PlanStmt, PolicyStamp,
    Projection, SelectPlan, SubPlan, SubPlanKind,
};
use crate::policy::{PolicyCatalog, TablePolicies};
use mpedb_types::{ExprProgram, ColumnType, Error, Footprint, KeyAccess, KeyBound, KeyPart, PolicyCmd, Result, Schema,
    TableDef, Value,};

mod access;
mod aggregate;
mod footprint;
mod join;
mod select;
mod subquery;

#[cfg(test)]
pub(crate) mod tests;

pub(crate) use footprint::compute_footprint;
use access::extract_access;
use aggregate::{contains_agg, plan_aggregate_select};
use join::plan_join_select;
use select::plan_select;

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
/// (DESIGN-MULTIDB.md §2.2/§3.2). Policies may not use `$`/`?` params.
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

/// Canonical secondary-index numbering helper (DESIGN.md §4.4): index 0 is
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
    // The engine's secondary index probe takes ONE value (`get_by_index`), so a
    // multi-column target has no index to use even if each column is unique
    // alone — and "unique together" is not something the schema can declare.
    let [col] = target else { return None };
    // An `indexed` (non-unique) column cannot witness a conflict: nothing
    // stops several rows from sharing the value, so there is no single row
    // to have conflicted with. PG rejects the same shape at prepare.
    if !table.columns[*col as usize].unique {
        return None;
    }
    let ino = secondary_indexes(table)
        .iter()
        .position(|c| *c == Some(*col))?;
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

/// Bind and plan one parsed statement into a [`CompiledPlan`].
pub(crate) fn plan_statement(
    stmt: &ast::Stmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
) -> Result<CompiledPlan> {
    let mut consts: Vec<Value> = Vec::new();
    let txn = |p: PlanStmt| {
        (p, vec![None; n_params as usize], Vec::new(), BTreeSet::new(), Vec::new(), Vec::new())
    };
    let (plan_stmt, param_types, context_keys, list_keys, _out_types, subplans) = match stmt {
        ast::Stmt::Begin => txn(PlanStmt::Begin),
        ast::Stmt::Commit => txn(PlanStmt::Commit),
        ast::Stmt::Rollback => txn(PlanStmt::Rollback),
        ast::Stmt::Select(s) => plan_select(s, schema, n_params, catalog, &mut consts)?,
        ast::Stmt::Compound(c) => plan_compound(c, schema, n_params, catalog, &mut consts)?,
        ast::Stmt::Insert(s) => plan_insert(s, schema, n_params, catalog, &mut consts)?,
        ast::Stmt::Update(s) => plan_update(s, schema, n_params, catalog, &mut consts)?,
        ast::Stmt::Delete(s) => plan_delete(s, schema, n_params, catalog, &mut consts)?,
    };
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
    let mut stamped: Vec<u32> = Vec::new();
    for s in &subplans {
        select_tables(&s.plan, &mut stamped);
    }
    match &plan_stmt {
        PlanStmt::Select(sp) => select_tables(sp, &mut stamped),
        PlanStmt::Compound(c) => {
            for arm in &c.arms {
                select_tables(arm, &mut stamped);
            }
            // Arms often read the same table; one stamp per table suffices.
            stamped.sort_unstable();
            stamped.dedup();
        }
        PlanStmt::Insert { table, .. }
        | PlanStmt::Update { table, .. }
        | PlanStmt::Delete { table, .. } => stamped.push(*table),
        PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => {}
    }
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
        for c in secondary_indexes(table).into_iter().flatten() {
            // Only UNIQUE columns can witness a conflict; an `indexed`
            // (non-unique) column is a secondary index but never usable here.
            if table.columns[c as usize].unique {
                usable.push(format!("({})", table.columns[c as usize].name));
            }
        }
        return Err(bind_err(format!(
            "ON CONFLICT ({}) is not supported: the target must be a key this can probe to \
             find the row you conflicted with — the primary key, or one UNIQUE column. \
             Usable here: {}.",
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
        let (b, ty) = binder.bind_expr(e)?;
        if let Some(t) = ty {
            // Same rule as `bind_assign`: `any` accepts every typed value.
            if t != table.columns[i].ty && table.columns[i].ty != ColumnType::Any {
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
        // RETURNING *
        return Ok(Some(
            (0..table.columns.len() as u16).map(Projection::Column).collect(),
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
fn plan_compound(
    c: &ast::CompoundStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let mut arms: Vec<SelectPlan> = Vec::with_capacity(c.arms.len());
    let mut param_types: Vec<Option<ColumnType>> = Vec::new();
    let mut context_keys: Vec<String> = Vec::new();
    let mut list_keys: BTreeSet<String> = BTreeSet::new();
    let mut out_types: Vec<Option<ColumnType>> = Vec::new();

    for (k, arm_ast) in c.arms.iter().enumerate() {
        let (stmt, ptypes, ckeys, lkeys, otypes, arm_subs) =
            plan_select(arm_ast, schema, n_params, catalog, consts)?;
        let PlanStmt::Select(sp) = stmt else {
            return Err(Error::Internal("plan_select produced a non-select".into()));
        };
        // Arm-local subplans would need slot allocation coordinated across
        // arms (each arm's binder numbers its own slots after the user
        // params) — refuse until that is built.
        if !arm_subs.is_empty() {
            return Err(bind_err(
                "a subquery inside a compound SELECT is not supported yet",
            ));
        }
        // Context slots are appended AFTER the user params, so two arms
        // binding different key sets would give the same slot index two
        // meanings. Identical key lists (the common case: same policy on the
        // same table) line up by construction; anything else is refused
        // rather than silently misread.
        if k == 0 {
            context_keys = ckeys;
            param_types = ptypes;
        } else {
            if ckeys != context_keys {
                return Err(bind_err(
                    "compound arms bind different session-context keys — not supported yet",
                ));
            }
            // One statement, one parameter table: unify element-wise.
            for (i, (have, arm)) in param_types.iter_mut().zip(&ptypes).enumerate() {
                match (&have, arm) {
                    (None, Some(t)) => *have = Some(*t),
                    (Some(a), Some(b)) if a != b => {
                        return Err(bind_err(format!(
                            "parameter ${} is used as {a} in one compound arm and {b} in another",
                            i + 1
                        )));
                    }
                    _ => {}
                }
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
    let mut order_by: Vec<(u16, bool)> = Vec::with_capacity(c.order_by.len());
    for (e, desc) in &c.order_by {
        if let Some(pos) = select::ordinal(e, arity)? {
            order_by.push((pos, *desc));
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
            Some(j) => order_by.push((j as u16, *desc)),
            None => {
                return Err(bind_err(format!(
                    "ORDER BY `{n}` does not name an output column of the compound's \
                     first SELECT"
                )))
            }
        }
    }

    let ops = c.ops.clone();
    Ok((
        PlanStmt::Compound(CompoundPlan {
            arms,
            ops,
            order_by,
            limit: c.limit,
            offset: c.offset,
        }),
        param_types,
        context_keys,
        list_keys,
        out_types,
        Vec::new(),
    ))
}

fn plan_insert(
    s: &ast::InsertStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
    let mut binder = Binder::new(table, n_params, true);

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
                cols.push(idx);
            }
            cols
        }
        None => (0..table.columns.len() as u16).collect(),
    };
    let mut slot_of_col: Vec<Option<usize>> = vec![None; table.columns.len()];
    for (slot, &col) in listed.iter().enumerate() {
        slot_of_col[col as usize] = Some(slot);
    }
    // Columns omitted from the list must be defaultable.
    for (ci, col) in table.columns.iter().enumerate() {
        if slot_of_col[ci].is_none() && !col.nullable && col.default.is_none() {
            return Err(bind_err(format!(
                "column `{}` is NOT NULL without a default and must be inserted",
                col.name
            )));
        }
    }

    let mut rows = Vec::with_capacity(s.rows.len());
    for row in &s.rows {
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
                        BExpr::Const(v) => {
                            let v = coerce_const(v, col.ty);
                            if v.is_null() && !col.nullable {
                                return Err(bind_err(format!(
                                    "cannot insert NULL into NOT NULL column `{}`",
                                    col.name
                                )));
                            }
                            if !v.fits(col.ty) {
                                return Err(bind_err(format!(
                                    "value of type {} cannot be inserted into column `{}` of type {}",
                                    v.type_name(),
                                    col.name,
                                    col.ty
                                )));
                            }
                            InsertSource::Const(push_plan_const(consts, v)?)
                        }
                        BExpr::Param(i) => {
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
                            InsertSource::Param(i)
                        }
                        _ => {
                            return Err(bind_err(
                                "INSERT values must be literals or parameters",
                            ))
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

    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Insert {
            table: table_id,
            rows,
            with_check,
            on_conflict,
            returning,
        },
        param_types,
        context_keys,
        list_keys,
        Vec::new(),
        Vec::new(),
    ))
}

fn plan_update(
    s: &ast::UpdateStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
    let mut binder = Binder::new(table, n_params, true);

    let mut set = Vec::with_capacity(s.set.len());
    let mut seen = vec![false; table.columns.len()];
    for (name, expr) in &s.set {
        let idx = table.column_index(name).ok_or_else(|| {
            bind_err(format!("unknown column `{name}` in table `{}`", table.name))
        })?;
        if table.is_pk_column(idx) {
            return Err(bind_err(format!(
                "cannot update primary key column `{name}`"
            )));
        }
        if seen[idx as usize] {
            return Err(bind_err(format!("column `{name}` set more than once")));
        }
        seen[idx as usize] = true;
        let col = &table.columns[idx as usize];
        let b = binder.bind_assign(expr, col)?;
        set.push((idx, compile_program(&b)?));
    }

    let bound_where = s
        .where_clause
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
        Vec::new(),
    ))
}

fn plan_delete(
    s: &ast::DeleteStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
    let mut binder = Binder::new(table, n_params, true);
    let bound_where = s
        .where_clause
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
        Vec::new(),
    ))
}

/// Fold an Int constant into a Float column context (the single legal
/// implicit coercion).
fn coerce_const(v: Value, ty: ColumnType) -> Value {
    match (&v, ty) {
        (Value::Int(i), ColumnType::Float64) => Value::Float(*i as f64),
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
fn as_col_cmp(e: &BExpr) -> Option<(u16, BinOp, Atom)> {
    let BExpr::Binary(op, l, r) = e else { return None };
    let flipped = |op: BinOp| match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    };
    match (l.as_ref(), r.as_ref()) {
        (BExpr::Col(c), rhs) => as_atom(rhs).map(|a| (*c, *op, a)),
        (lhs, BExpr::Col(c)) => as_atom(lhs).map(|a| (*c, flipped(*op), a)),
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
            BExpr::Unary(_, a) | BExpr::Like(a, _) | BExpr::Cast(a, _) | BExpr::InParam(a, _) => {
                stack.push(a)
            }
            BExpr::Binary(_, a, b) => {
                stack.push(a);
                stack.push(b);
            }
            BExpr::InList(a, list) => {
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
            BExpr::Coalesce(args) | BExpr::Call(_, args) => stack.extend(args.iter()),
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
