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
type PlannedStmt = (PlanStmt, Vec<Option<ColumnType>>, Vec<String>, BTreeSet<String>);
use crate::binder::{compile_program, BExpr, Binder, Scope};
use crate::plan::{
    render_program, AccessPath, AggCall, Aggregation, CompiledPlan, ConflictProbe, InsertSource,
    Join, OrderOver, PlanOnConflict, PlanStmt, PolicyStamp, Projection,
};
use crate::policy::{PolicyCatalog, TablePolicies};
use mpedb_types::{ExprProgram, ColumnType, Error, Footprint, KeyAccess, KeyBound, KeyPart, PolicyCmd, Result, Schema,
    TableDef, Value,};

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
/// unique index 1, 2, ... — columns with `unique = true` in declaration
/// order, skipping a column that is by itself the entire primary key.
pub fn secondary_indexes(table: &TableDef) -> Vec<u16> {
    table
        .columns
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            c.unique && !(table.primary_key.len() == 1 && table.primary_key[0] == *i as u16)
        })
        .map(|(i, _)| i as u16)
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
    let ino = secondary_indexes(table).iter().position(|c| c == col)?;
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
    let (plan_stmt, param_types, context_keys, list_keys) = match stmt {
        ast::Stmt::Begin => (PlanStmt::Begin, vec![None; n_params as usize], Vec::new(), BTreeSet::new()),
        ast::Stmt::Commit => (PlanStmt::Commit, vec![None; n_params as usize], Vec::new(), BTreeSet::new()),
        ast::Stmt::Rollback => (PlanStmt::Rollback, vec![None; n_params as usize], Vec::new(), BTreeSet::new()),
        ast::Stmt::Select(s) => plan_select(s, schema, n_params, catalog, &mut consts)?,
        ast::Stmt::Insert(s) => plan_insert(s, schema, n_params, catalog, &mut consts)?,
        ast::Stmt::Update(s) => plan_update(s, schema, n_params, catalog, &mut consts)?,
        ast::Stmt::Delete(s) => plan_delete(s, schema, n_params, catalog, &mut consts)?,
    };
    let footprint = compute_footprint(&plan_stmt, schema)?;
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
    let target = match &plan_stmt {
        PlanStmt::Select { table, .. }
        | PlanStmt::Insert { table, .. }
        | PlanStmt::Update { table, .. }
        | PlanStmt::Delete { table, .. } => Some(*table),
        PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => None,
    };
    // One stamp per table whose policy this plan baked in. For a join that is
    // BOTH: stamping only the outer would let a cached plan keep serving the
    // inner table's rows under a policy that has since been tightened, which is
    // the leak §4 exists to close.
    let mut stamped: Vec<u32> = target.into_iter().collect();
    if let PlanStmt::Select {
        join: Some(j), ..
    } = &plan_stmt
    {
        stamped.push(j.table);
    }

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

/// Does this expression contain an aggregate anywhere?
fn contains_agg(e: &ast::Expr) -> bool {
    use ast::Expr as E;
    match e {
        E::Agg(..) => true,
        E::Unary(_, a) | E::IsNull(a, _) => contains_agg(a),
        E::Binary(_, a, b) | E::Like(a, b) => contains_agg(a) || contains_agg(b),
        E::InContext(a, _, _) => contains_agg(a),
        E::InList(a, xs, _) => contains_agg(a) || xs.iter().any(contains_agg),
        E::Coalesce(xs) | E::Func(_, xs) => xs.iter().any(contains_agg),
        E::Case(arms, els) => {
            arms.iter().any(|(c, r)| contains_agg(c) || contains_agg(r))
                || els.as_deref().is_some_and(contains_agg)
        }
        E::Lit(_) | E::Param(_) | E::Col(_) | E::ContextRef(_) | E::Excluded(_)
        | E::Qualified(..) => false,
    }
}

/// Lift every aggregate out of `e`, replacing it with a reference to its slot in
/// the GROUPED tuple. Returns the rewritten expression.
///
/// The two tuples are the crux. An aggregate's ARGUMENT is evaluated over the
/// base row (`sum(qty)` needs each row's qty); the aggregate's RESULT lives in
/// the grouped tuple `[keys ‖ aggs]`, and so does everything the projection and
/// HAVING say about it. Mixing them up is how `sum(x) + 1` ends up reading
/// column 1 of the base row.
fn lift_aggs(
    e: &ast::Expr,
    group_by: &[u16],
    // Name -> slot in the row being aggregated. A `Scope`, so this works over a
    // join's `[outer ‖ inner]` as well as one table — the rule it enforces ("a
    // bare column must be a GROUP BY key") is about the ROW, and does not care
    // how many tables built it.
    scope: &Scope<'_>,
    aggs: &mut Vec<(mpedb_types::AggFn, Option<ast::Expr>, bool)>,
) -> Result<ast::Expr> {
    use ast::Expr as E;
    let rec = |x: &ast::Expr, aggs: &mut Vec<_>| lift_aggs(x, group_by, scope, aggs);
    Ok(match e {
        E::Agg(f, arg, distinct) => {
            let spec = (*f, arg.as_deref().cloned(), *distinct);
            // Reuse an identical aggregate rather than adding a slot: `SELECT
            // count(*) ... ORDER BY count(*)` is one aggregate named twice, and
            // lifting it twice would accumulate it twice.
            let slot = group_by.len()
                + match aggs.iter().position(|a| *a == spec) {
                    Some(i) => i,
                    None => {
                        aggs.push(spec);
                        aggs.len() - 1
                    }
                };
            // The grouped tuple has no table, so name the slot positionally;
            // `synthetic_grouped_table` below gives those names meaning.
            E::Col(format!("__g{slot}"))
        }
        // A bare column in an aggregate query must be a GROUP BY key — SQL's
        // rule, and not pedantry: `SELECT name, count(*) FROM t` with no GROUP
        // BY has no answer for which `name` to show. sqlite invents one; the
        // rigid engine says so instead.
        E::Col(_) | E::Qualified(..) => {
            let (idx, _) = match e {
                E::Col(n) => scope.resolve(n)?,
                E::Qualified(q, n) => scope.resolve_qualified(q, n)?,
                _ => unreachable!("matched above"),
            };
            let pos = group_by.iter().position(|g| *g == idx).ok_or_else(|| {
                bind_err(format!(
                    "column `{}` must appear in GROUP BY or be inside an aggregate — \
                     otherwise there is no single value for it in the group",
                    scope.slot_name(idx)
                ))
            })?;
            E::Col(format!("__g{pos}"))
        }
        E::Unary(op, a) => E::Unary(*op, Box::new(rec(a, aggs)?)),
        E::IsNull(a, n) => E::IsNull(Box::new(rec(a, aggs)?), *n),
        E::Binary(op, a, b) => {
            E::Binary(*op, Box::new(rec(a, aggs)?), Box::new(rec(b, aggs)?))
        }
        E::Like(a, b) => E::Like(Box::new(rec(a, aggs)?), Box::new(rec(b, aggs)?)),
        E::InList(a, xs, n) => E::InList(
            Box::new(rec(a, aggs)?),
            xs.iter().map(|x| rec(x, aggs)).collect::<Result<_>>()?,
            *n,
        ),
        E::InContext(a, k, n) => E::InContext(Box::new(rec(a, aggs)?), k.clone(), *n),
        E::Coalesce(xs) => E::Coalesce(xs.iter().map(|x| rec(x, aggs)).collect::<Result<_>>()?),
        E::Func(f, xs) => E::Func(
            f.clone(),
            xs.iter().map(|x| rec(x, aggs)).collect::<Result<_>>()?,
        ),
        E::Case(arms, els) => E::Case(
            arms.iter()
                .map(|(c, r)| Ok((rec(c, aggs)?, rec(r, aggs)?)))
                .collect::<Result<_>>()?,
            match els {
                Some(x) => Some(Box::new(rec(x, aggs)?)),
                None => None,
            },
        ),
        other @ (E::Lit(_) | E::Param(_) | E::ContextRef(_) | E::Excluded(_)) => other.clone(),
    })
}

/// A synthetic `TableDef` describing the GROUPED tuple `[keys ‖ aggs]`, so the
/// projection and HAVING can be bound by the ordinary binder against it.
///
/// The grouped tuple is not a table, but it IS a tuple with typed slots — which
/// is exactly what the binder needs. Reusing the binder here rather than writing
/// a second resolution path means the type rules, 3VL and constant folding are
/// the same ones as everywhere else, instead of a parallel set that drifts.
fn synthetic_grouped_table(
    // The columns of the row being aggregated. A slice rather than a
    // `TableDef`, because for a join that row is `[outer ‖ inner]` and no table
    // describes it.
    columns: &[mpedb_types::ColumnDef],
    group_by: &[u16],
    aggs: &[(mpedb_types::AggFn, Option<ast::Expr>, bool)],
    agg_types: &[Option<ColumnType>],
) -> TableDef {
    let mut out: Vec<mpedb_types::ColumnDef> = Vec::with_capacity(group_by.len() + aggs.len());
    for (k, g) in group_by.iter().enumerate() {
        let src = &columns[*g as usize];
        out.push(mpedb_types::ColumnDef {
            name: format!("__g{k}"),
            ty: src.ty,
            nullable: true, // a group key can be NULL; NULLs group together
            unique: false,
            default: None,
            check: None,
        });
    }
    for (i, (f, _, _)) in aggs.iter().enumerate() {
        let ty = match f {
            mpedb_types::AggFn::Count => ColumnType::Int64,
            mpedb_types::AggFn::Avg => ColumnType::Float64,
            // SUM/MIN/MAX keep the argument's type.
            _ => agg_types[i].unwrap_or(ColumnType::Int64),
        };
        out.push(mpedb_types::ColumnDef {
            name: format!("__g{}", group_by.len() + i),
            ty,
            // Every aggregate except COUNT is NULL over an empty group.
            nullable: !matches!(f, mpedb_types::AggFn::Count),
            unique: false,
            default: None,
            check: None,
        });
    }
    TableDef {
        // Not a table anyone named, and nothing resolves a qualifier against
        // it: `lift_aggs` has already rewritten every column reference to the
        // positional `__gN`.
        name: String::new(),
        columns: out,
        primary_key: vec![0],
    }
}

/// Plan a `GROUP BY` / aggregate SELECT.
///
/// `access` and `filter` are already built and are NOT touched here — that is
/// the point. They carry the merged `(WHERE ∧ effective-policy)` predicate, and
/// DESIGN-MULTIDB §4 requires aggregation to consume rows only after it. The
/// executor honours that by aggregating `gather_rows`' output; this function
/// simply never gets the chance to reorder them.
#[allow(clippy::too_many_arguments)]
/// Resolve each `ORDER BY` key to its position in the SELECT list — the
/// `OrderOver::Projection` form.
///
/// Two things only this form can express, both of which sqlite and PG have:
///
///   `ORDER BY 1` — a bare integer literal is an ORDINAL, SQL's oldest wart.
///     It names the first output column, not the number 1. (`ORDER BY 1 + 1`
///     is not an ordinal in PG either: only a literal counts, and the AST is
///     checked before folding so a folded `2` cannot sneak in as one.)
///   `ORDER BY amt * 2` — a computed key, which no base-column index names.
fn distinct_order_by(
    s: &ast::SelectStmt,
    // The row being selected FROM: one table, or a join's `[outer ‖ inner]`.
    scope: &Scope<'_>,
    // When present, a key that is NOT in the SELECT list may be APPENDED to the
    // projection as a sort-only column rather than refused. `None` for the
    // callers that must refuse (DISTINCT, and the grouped path, where the sort
    // key lives in the grouped tuple instead).
    mut junk: Option<(&mut Vec<Projection>, &mut Binder<'_>)>,
) -> Result<(Vec<(u16, bool)>, u16)> {
    let Some(items) = s.items.as_ref() else {
        // `SELECT *`: the projection is the base row, column for column, so a
        // base-column index IS the output position and an ordinal counts over
        // the same list.
        let mut out = Vec::with_capacity(s.order_by.len());
        let mut n_junk = 0u16;
        for (i, (e, desc)) in s.order_by.iter().enumerate() {
            if let Some(pos) = ordinal(e, scope.width())? {
                out.push((pos, *desc));
                continue;
            }
            // The projection IS the base row here, so a column's slot is its
            // output position.
            match col_slot(e, scope) {
                Some(slot) => out.push((slot, *desc)),
                // `SELECT * FROM t ORDER BY a + 1`: every column is already in
                // the output, but a computed key still is not.
                None => {
                    let (pos, added) = push_junk(&mut junk, e, scope, i)?;
                    out.push((pos, *desc));
                    n_junk += added;
                }
            }
        }
        return Ok((out, n_junk));
    };
    // Which selected items are plain column references, and to which slot. A
    // key matches an item when they name the SAME COLUMN, which is a question
    // about slots and not about spelling: `t.a` and `a` are one column, and —
    // the case that made the old spelling-based match unfixable — `emp.did`
    // and `dept.did` are two, but strip the qualifier and they are both `did`.
    let item_slots: Vec<Option<u16>> = items.iter().map(|it| col_slot(it, scope)).collect();
    let mut out = Vec::with_capacity(s.order_by.len());
    let mut n_junk = 0u16;
    for (i, (key, desc)) in s.order_by.iter().enumerate() {
        if let Some(pos) = ordinal(key, items.len())? {
            out.push((pos, *desc));
            continue;
        }
        let pos = match col_slot(key, scope) {
            Some(slot) => item_slots.iter().position(|s| *s == Some(slot)),
            // Not a column: fall back to comparing the expressions, which is
            // what makes `SELECT amt * 2 … ORDER BY amt * 2` match.
            None => items.iter().position(|it| it == key),
        };
        match pos {
            Some(pos) => out.push((pos as u16, *desc)),
            None => {
                let (pos, added) = push_junk(&mut junk, key, scope, i)?;
                out.push((pos, *desc));
                n_junk += added;
            }
        }
    }
    Ok((out, n_junk))
}

/// The slot this expression names, if it is a plain column reference.
///
/// `None` for anything else, INCLUDING a name that does not resolve or is
/// ambiguous: those are real errors, but they are reported by the bind that
/// follows, with a message about the actual problem rather than about the sort.
fn col_slot(e: &ast::Expr, scope: &Scope<'_>) -> Option<u16> {
    match e {
        ast::Expr::Col(n) => scope.resolve(n).ok().map(|(i, _)| i),
        ast::Expr::Qualified(q, n) => scope.resolve_qualified(q, n).ok().map(|(i, _)| i),
        _ => None,
    }
}

/// Append one sort-only ("junk") column to the projection and return its output
/// position, or report that the key has nowhere to live.
fn push_junk(
    junk: &mut Option<(&mut Vec<Projection>, &mut Binder<'_>)>,
    key: &ast::Expr,
    scope: &Scope<'_>,
    i: usize,
) -> Result<(u16, u16)> {
    let Some((projection, binder)) = junk else {
        return Err(bind_err(format!(
            "{} must be in the SELECT list when SELECT DISTINCT is used — otherwise \
             which duplicate row survives is what decides the order, and the query \
             does not say",
            describe_key(key, i)
        )));
    };
    let (b, _) = binder.bind_expr(key)?;
    // A CONSTANT sort key names no column, so it cannot order anything: every
    // row gets the same key and the sort is a no-op. sqlite accepts it and
    // hands back scan order — an arbitrary answer to a query that asked for an
    // order. PostgreSQL refuses, and it is right to, because the reason people
    // write this is that they meant an ordinal: `ORDER BY 2` IS an output
    // position, and `1 + 1` looks like one until it silently is not.
    if matches!(b, BExpr::Const(_)) {
        return Err(bind_err(format!(
            "{} is a constant — it names no column, so it orders nothing. A bare integer \
             like `ORDER BY 2` is an output position; an expression is not.",
            describe_key(key, i)
        )));
    }
    let program = compile_program(&b)?;
    let name = render_program(&program, &|c| scope.slot_name(c));
    projection.push(Projection::Expr { program, name });
    if projection.len() > crate::parser::MAX_SELECT_ITEMS {
        return Err(bind_err("too many ORDER BY keys to sort by".to_string()));
    }
    Ok(((projection.len() - 1) as u16, 1))
}

/// Name one `ORDER BY` key for an error message. A column has a name worth
/// quoting; an expression does not — `agg_item_name` renders it `?column?`,
/// which tells the reader nothing about WHICH key is wrong. Fall back to the
/// 1-based position, the way sqlite says "1st ORDER BY term".
fn describe_key(e: &ast::Expr, pos: usize) -> String {
    match e {
        ast::Expr::Col(n) => format!("ORDER BY `{n}`"),
        ast::Expr::Qualified(q, n) => format!("ORDER BY `{q}.{n}`"),
        _ => format!(
            "the {}{} ORDER BY key",
            pos + 1,
            match pos + 1 {
                1 => "st",
                2 => "nd",
                3 => "rd",
                _ => "th",
            }
        ),
    }
}

/// `ORDER BY <integer literal>` — a 1-based ordinal into the SELECT list.
/// `None` if the key is not a bare integer literal.
fn ordinal(key: &ast::Expr, n_items: usize) -> Result<Option<u16>> {
    let ast::Expr::Lit(Value::Int(n)) = key else {
        return Ok(None);
    };
    if *n < 1 || *n as usize > n_items {
        return Err(bind_err(format!(
            "ORDER BY {n} is out of range — there {} {n_items} output column{}",
            if n_items == 1 { "is" } else { "are" },
            if n_items == 1 { "" } else { "s" }
        )));
    }
    Ok(Some((*n - 1) as u16))
}

/// With `SELECT DISTINCT`, every `ORDER BY` key must be one of the selected
/// items — PostgreSQL's rule, and it is not pedantry.
///
/// DISTINCT collapses duplicate OUTPUT rows, so of each duplicate group exactly
/// one survives. If the sort key is not in the output, then *which* duplicate
/// survives is what decides where the row sorts — and that choice is the
/// engine's, not the query's. `SELECT DISTINCT dept FROM t ORDER BY amt` has no
/// answer: the dept 'eng' has many amts and no reason to prefer any of them.
/// sqlite picks one and reports a stable-looking answer; the rigid engine says
/// there is no answer instead.
fn check_distinct_order_by(s: &ast::SelectStmt, table: &TableDef) -> Result<()> {
    if !s.distinct || s.order_by.is_empty() {
        return Ok(());
    }
    // `SELECT DISTINCT *` outputs every column, so no key can be missing.
    let Some(items) = s.items.as_ref() else {
        return Ok(());
    };
    // `ORDER BY t.a` and a selected `a` are the same column; compare with the
    // qualifier stripped so the rule is about meaning, not spelling.
    let strip = |e: &ast::Expr| -> ast::Expr {
        match e {
            ast::Expr::Qualified(q, n) if q.eq_ignore_ascii_case(&table.name) => {
                ast::Expr::Col(n.clone())
            }
            other => other.clone(),
        }
    };
    for (i, (key, _)) in s.order_by.iter().enumerate() {
        // An ordinal already names an output position, so it cannot be outside
        // the SELECT list; `ordinal` range-checks it later.
        if matches!(key, ast::Expr::Lit(Value::Int(_))) {
            continue;
        }
        let stripped = strip(key);
        if !items.iter().any(|it| strip(it) == stripped) {
            return Err(bind_err(format!(
                "{} must be in the SELECT list when SELECT DISTINCT is used — otherwise \
                 which duplicate row survives is what decides the order, and the query \
                 does not say",
                describe_key(key, i)
            )));
        }
    }
    Ok(())
}

/// `SELECT … FROM a INNER JOIN b ON <cond> [WHERE …]`, as a nested loop.
///
/// The evaluation order is the security contract, and it is why the pieces are
/// separate fields rather than one AND-ed predicate:
///
/// ```text
/// for each outer row matching `access`:
///     if not `filter`(outer):            continue   <- a's RLS USING
///     for each inner row matching `join.access`:
///         if not `join.policy`(inner):   continue   <- b's RLS USING
///         if not `join.on`(outer ‖ inner):   continue
///         if not `joined_filter`(outer ‖ inner): continue
///         emit
/// ```
///
/// Both policies run over ONE row, and before anything that can raise. mpedb's
/// expressions raise on division by zero and on overflow, and a raise is
/// observable — so `ON a.x / b.secret > 1` evaluated before b's policy would
/// report the existence of a row the policy hides, without ever returning it.
/// AND-ing everything into one predicate would leave that ordering to whatever
/// the compiler emitted.
///
/// What this deliberately does NOT do yet: push the user's WHERE into either
/// side. Every conjunct waits for the joined row, so the outer is a full scan
/// unless its POLICY pins a key, and the inner is re-scanned per outer row —
/// O(n·m). Correct, and slow enough that EXPLAIN says so.
#[allow(clippy::too_many_arguments)]
fn plan_join_select(
    s: &ast::SelectStmt,
    jc: &ast::JoinClause,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (outer_id, outer) = resolve_table(schema, &s.table)?;
    let (inner_id, inner) = resolve_table(schema, &jc.table)?;

    // Each table's policy binds over ITS OWN row: a policy is a single-row
    // scalar predicate (DESIGN-MULTIDB §253), so it must be bound in a
    // single-table scope or its column slots would be join slots.
    let mut ob = Binder::new(outer, n_params, true);
    let outer_policy = read_policy(&mut ob, catalog, outer_id, &outer.name, PolicyCmd::Select)?;
    let (access, outer_residual) = extract_access(outer_policy, outer, consts)?;
    let filter = outer_residual.map(|e| compile_program(&e)).transpose()?;

    let mut ib = ob.rescope(Scope::single(inner));
    let inner_policy = read_policy(&mut ib, catalog, inner_id, &inner.name, PolicyCmd::Select)?;
    let inner_policy = inner_policy.map(|e| compile_program(&e)).transpose()?;

    // Everything else sees the joined tuple.
    let mut binder = ib.rescope(Scope::joined(vec![outer, inner])?);
    let on = compile_program(&binder.bind_predicate(&jc.on)?)?;
    let joined_filter = s
        .where_clause
        .as_ref()
        .map(|e| binder.bind_predicate(e))
        .transpose()?
        .map(|e| compile_program(&e))
        .transpose()?;

    // An aggregate over a join groups the JOINED row. Nothing in the grouping
    // step is about tables — it is about the row it is handed — so the same
    // planner runs, given the joined row's columns and scope.
    let has_agg = s
        .items
        .as_ref()
        .is_some_and(|i| i.iter().any(contains_agg))
        || s.having.as_ref().is_some_and(contains_agg)
        || s.order_by.iter().any(|(e, _)| contains_agg(e))
        || !s.group_by.is_empty();
    if has_agg {
        let mut joined_columns = outer.columns.clone();
        joined_columns.extend(inner.columns.iter().cloned());
        return plan_aggregate_select(
            s,
            &joined_columns,
            &Scope::joined(vec![outer, inner])?,
            outer_id,
            access,
            filter,
            Some(Join {
                table: inner_id,
                access: AccessPath::FullScan,
                on,
                policy: inner_policy,
            }),
            joined_filter,
            binder,
            consts,
        );
    }

    // Projection over the joined tuple. `SELECT *` is every column of both
    // sides, outer first — the same order the tuple is built in.
    let mut projection: Vec<Projection> = match &s.items {
        None => (0..binder.scope_width() as u16).map(Projection::Column).collect(),
        Some(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let (b, _) = binder.bind_expr(item)?;
                out.push(match b {
                    BExpr::Col(i) => Projection::Column(i),
                    other => {
                        let program = compile_program(&other)?;
                        let name = render_program(&program, &joined_namer(outer, inner));
                        Projection::Expr { program, name }
                    }
                });
            }
            out
        }
    };

    // ORDER BY over the joined row. A bare column is a slot of it; anything
    // else takes the sort-only-column route, exactly as the single-table path
    // does.
    let mut order_by = Vec::with_capacity(s.order_by.len());
    let mut order_over = OrderOver::BaseRow;
    let mut order_junk = 0u16;
    if !s.order_by.is_empty() {
        // The "base row" of a join IS the joined row, and it is built in full
        // before the sort — so sorting it is the same operation, just wider.
        let mut keys = Vec::with_capacity(s.order_by.len());
        let mut all_cols = true;
        for (e, desc) in &s.order_by {
            match binder.bind_expr(e) {
                Ok((BExpr::Col(i), _)) => keys.push((i, *desc)),
                _ => {
                    all_cols = false;
                    break;
                }
            }
        }
        if all_cols {
            order_by = keys;
        } else {
            let joined = Scope::joined(vec![outer, inner])?;
            let (keys, n_junk) = join_order_by(s, &joined, &mut projection, &mut binder, outer, inner)?;
            order_by = keys;
            order_over = OrderOver::Projection;
            order_junk = n_junk;
        }
    }

    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Select {
            table: outer_id,
            access,
            join: Some(Join {
                table: inner_id,
                // The inner side is re-read per outer row; no pushdown yet.
                access: AccessPath::FullScan,
                on,
                policy: inner_policy,
            }),
            joined_filter,
            filter,
            projection,
            order_by,
            order_over,
            order_junk,
            limit: s.limit,
            offset: s.offset,
            distinct: s.distinct,
            aggregate: None,
        },
        param_types,
        context_keys,
        list_keys,
    ))
}

/// Name a joined-tuple slot for EXPLAIN and output columns: `<table>.<column>`,
/// because `id` alone would be a lie about which side it came from.
fn joined_namer<'a>(
    outer: &'a TableDef,
    inner: &'a TableDef,
) -> impl Fn(u16) -> String + use<'a> {
    move |c: u16| {
        let n = outer.columns.len();
        if (c as usize) < n {
            format!("{}.{}", outer.name, outer.columns[c as usize].name)
        } else if (c as usize) < n + inner.columns.len() {
            format!("{}.{}", inner.name, inner.columns[c as usize - n].name)
        } else {
            format!("col#{c}")
        }
    }
}

/// `ORDER BY` for a join that needs sort-only columns.
fn join_order_by(
    s: &ast::SelectStmt,
    _joined: &Scope<'_>,
    projection: &mut Vec<Projection>,
    binder: &mut Binder<'_>,
    outer: &TableDef,
    inner: &TableDef,
) -> Result<(Vec<(u16, bool)>, u16)> {
    let items = s.items.as_ref();
    let mut keys = Vec::with_capacity(s.order_by.len());
    let mut n_junk = 0u16;
    for (i, (e, desc)) in s.order_by.iter().enumerate() {
        if let Some(items) = items {
            if let Some(pos) = ordinal(e, items.len())? {
                keys.push((pos, *desc));
                continue;
            }
            if let Some(pos) = items.iter().position(|it| it == e) {
                keys.push((pos as u16, *desc));
                continue;
            }
        }
        let (b, _) = binder.bind_expr(e)?;
        if matches!(b, BExpr::Const(_)) {
            return Err(bind_err(format!(
                "{} is a constant — it names no column, so it orders nothing.",
                describe_key(e, i)
            )));
        }
        let program = compile_program(&b)?;
        let name = render_program(&program, &joined_namer(outer, inner));
        projection.push(Projection::Expr { program, name });
        keys.push(((projection.len() - 1) as u16, *desc));
        n_junk += 1;
    }
    Ok((keys, n_junk))
}

/// Plan an aggregate SELECT over `base` — one table, or a join's
/// `[outer ‖ inner]`. Everything here is about the ROW being aggregated, so all
/// the join changes is how wide that row is and how names resolve into it.
#[allow(clippy::too_many_arguments)]
fn plan_aggregate_select(
    s: &ast::SelectStmt,
    // The row being aggregated: its columns (for the grouped tuple's types) and
    // its scope (for name resolution and messages).
    base_columns: &[mpedb_types::ColumnDef],
    base_scope: &Scope<'_>,
    table_id: u32,
    access: AccessPath,
    filter: Option<ExprProgram>,
    join: Option<Join>,
    joined_filter: Option<ExprProgram>,
    mut binder: Binder<'_>,
    _consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    // 1. GROUP BY columns -> base-row slots.
    let mut group_by = Vec::with_capacity(s.group_by.len());
    for g in &s.group_by {
        let (i, _) = match g {
            ast::Expr::Col(n) => base_scope.resolve(n)?,
            ast::Expr::Qualified(q, n) => base_scope.resolve_qualified(q, n)?,
            // `GROUP BY a + 1` groups by a computed key, which the grouped
            // tuple's positional keys cannot name. sqlite and PG allow it.
            _ => {
                return Err(bind_err(
                    "GROUP BY must name a column — grouping by an expression is not \
                     supported (sqlite and PostgreSQL do allow it)"
                        .to_string(),
                ))
            }
        };
        if group_by.contains(&i) {
            return Err(bind_err(format!(
                "column `{}` repeated in GROUP BY",
                base_scope.slot_name(i)
            )));
        }
        group_by.push(i);
    }

    // 2. Lift the aggregates out of the SELECT list and HAVING.
    let items = s.items.as_ref().ok_or_else(|| {
        bind_err("SELECT * with GROUP BY has no meaning — list the group keys and aggregates")
    })?;
    let mut agg_specs: Vec<(mpedb_types::AggFn, Option<ast::Expr>, bool)> = Vec::new();
    let mut rewritten = Vec::with_capacity(items.len());
    for item in items {
        rewritten.push(lift_aggs(item, &group_by, base_scope, &mut agg_specs)?);
    }
    let rewritten_having = match &s.having {
        Some(h) => Some(lift_aggs(h, &group_by, base_scope, &mut agg_specs)?),
        None => None,
    };
    // ORDER BY is lifted HERE, with the others, because `ORDER BY count(*)` may
    // name an aggregate that is NOT in the SELECT list — `SELECT dept FROM t
    // GROUP BY dept ORDER BY count(*)` is legal in sqlite and PG. Lifting it
    // late, after the grouped tuple was built, would leave that aggregate with
    // nowhere to live. `lift_aggs` reuses an identical existing slot, so
    // ordering by an aggregate that IS selected does not compute it twice.
    let mut rewritten_order = Vec::with_capacity(s.order_by.len());
    for (e, desc) in &s.order_by {
        rewritten_order.push((lift_aggs(e, &group_by, base_scope, &mut agg_specs)?, *desc));
    }

    // 3. Bind each aggregate ARGUMENT over the BASE row.
    let mut aggs = Vec::with_capacity(agg_specs.len());
    let mut agg_types = Vec::with_capacity(agg_specs.len());
    for (f, arg, distinct) in &agg_specs {
        match arg {
            None => {
                aggs.push(AggCall {
                    func: *f,
                    arg: None,
                    distinct: false,
                });
                agg_types.push(Some(ColumnType::Int64));
            }
            Some(a) => {
                let (b, ty) = binder.bind_expr(a)?;
                agg_types.push(ty);
                aggs.push(AggCall {
                    func: *f,
                    arg: Some(compile_program(&b)?),
                    distinct: *distinct,
                });
            }
        }
    }

    // 4. Bind the rewritten projection/HAVING over the GROUPED tuple — a
    //    different tuple from the base row, carrying the same parameter table.
    let grouped = synthetic_grouped_table(base_columns, &group_by, &agg_specs, &agg_types);
    let mut binder = binder.rescope(Scope::single(&grouped));

    let mut projection: Vec<Projection> = Vec::with_capacity(rewritten.len());
    for (item, orig) in rewritten.iter().zip(items) {
        let (b, _) = binder.bind_expr(item)?;
        projection.push(match b {
            BExpr::Col(i) => Projection::Expr {
                program: compile_program(&BExpr::Col(i))?,
                name: agg_item_name(orig),
            },
            other => Projection::Expr {
                program: compile_program(&other)?,
                name: agg_item_name(orig),
            },
        });
    }
    let having = match &rewritten_having {
        Some(h) => {
            let b = binder.bind_predicate(h)?;
            Some(compile_program(&b)?)
        }
        None => None,
    };

    // 5. ORDER BY. Preferred form: every key is a bare column of the GROUPED
    //    tuple — a group key or an aggregate slot — so the sort runs there and
    //    `ORDER BY count(*)` works even unselected. A key computed FROM those
    //    (`ORDER BY count(*) + 1`) is not a column of any tuple that exists
    //    yet, so it gets a sort-only column appended to the projection, exactly
    //    as the plain path does for `ORDER BY amt + 1`.
    if s.distinct {
        let (order_by, _) = distinct_order_by(s, base_scope, None)?;
        let (param_types, context_keys, list_keys) = binder.into_parts();
        return Ok((
            PlanStmt::Select {
                table: table_id,
                access,
                join,
                joined_filter,
                filter,
                projection,
                order_by,
                order_over: OrderOver::Projection,
                order_junk: 0,
                limit: s.limit,
                offset: s.offset,
                distinct: true,
                aggregate: Some(Aggregation {
                    group_by,
                    aggs,
                    having,
                }),
            },
            param_types,
            context_keys,
            list_keys,
        ));
    }
    let mut grouped_keys = Vec::with_capacity(rewritten_order.len());
    for (e, desc) in &rewritten_order {
        match binder.bind_expr(e)? {
            (BExpr::Col(i), _) => grouped_keys.push((i, *desc)),
            // Not a bare column of the grouped tuple. Stop: the keys must all
            // index the SAME tuple, so one computed key moves every key to the
            // projection.
            _ => break,
        }
    }
    let (order_by, order_over, order_junk) = if grouped_keys.len() == rewritten_order.len() {
        (grouped_keys, OrderOver::Grouped, 0)
    } else {
        let mut keys = Vec::with_capacity(rewritten_order.len());
        let mut n_junk = 0u16;
        for (i, ((e, desc), (orig, _))) in rewritten_order.iter().zip(&s.order_by).enumerate() {
            // An ordinal or a repeat of a selected item needs no new column.
            if let Some(pos) = ordinal(orig, items.len())? {
                keys.push((pos, *desc));
                continue;
            }
            match rewritten.iter().position(|it| it == e) {
                Some(pos) => keys.push((pos as u16, *desc)),
                None => {
                    let mut junk = Some((&mut projection, &mut binder));
                    let (pos, added) = push_junk(&mut junk, e, &Scope::single(&grouped), i)?;
                    keys.push((pos, *desc));
                    n_junk += added;
                }
            }
        }
        (keys, OrderOver::Projection, n_junk)
    };

    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Select {
            table: table_id,
            access,
            join,
            joined_filter,
            filter,
            projection,
            order_by,
            order_over,
            order_junk,
            limit: s.limit,
            offset: s.offset,
            distinct: s.distinct,
            aggregate: Some(Aggregation {
                group_by,
                aggs,
                having,
            }),
        },
        param_types,
        context_keys,
        list_keys,
    ))
}

/// The output column name for one item of an aggregate SELECT list.
fn agg_item_name(e: &ast::Expr) -> String {
    match e {
        ast::Expr::Col(c) => c.clone(),
        ast::Expr::Qualified(_, c) => c.clone(),
        ast::Expr::Agg(f, None, _) => format!("{}(*)", f.name()),
        ast::Expr::Agg(f, Some(a), distinct) => format!(
            "{}({}{})",
            f.name(),
            if *distinct { "DISTINCT " } else { "" },
            agg_item_name(a)
        ),
        _ => "?column?".to_string(),
    }
}

fn plan_select(
    s: &ast::SelectStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
    if let Some(jc) = &s.join {
        return plan_join_select(s, jc, schema, n_params, catalog, consts);
    }
    let mut binder = Binder::new(table, n_params, true);
    let bound_where = s
        .where_clause
        .as_ref()
        .map(|e| binder.bind_predicate(e))
        .transpose()?;
    // Inject the SELECT visibility policy AND-ed with the user WHERE, *before*
    // access extraction, so a policy conjunct that pins the PK/unique column
    // still becomes a Point/Range access and footprints only narrow (§3.3).
    let policy = read_policy(&mut binder, catalog, table_id, &table.name, PolicyCmd::Select)?;
    let (access, residual) = extract_access(merge_and(bound_where, policy), table, consts)?;
    let filter = residual.map(|e| compile_program(&e)).transpose()?;
    let (join, joined_filter) = (None, None);

    check_distinct_order_by(s, table)?;

    // Is this an aggregate query? Either an aggregate appears, or GROUP BY does.
    let has_agg = s
        .items
        .as_ref()
        .is_some_and(|items| items.iter().any(contains_agg))
        || s.having.as_ref().is_some_and(contains_agg)
        // ORDER BY too: `SELECT dept FROM t ORDER BY count(*)` is an aggregate
        // query even though no aggregate appears in the SELECT list, and
        // routing it to the plain planner would report the wrong problem.
        || s.order_by.iter().any(|(e, _)| contains_agg(e))
        || !s.group_by.is_empty();
    if has_agg {
        return plan_aggregate_select(
            s,
            &table.columns,
            &Scope::single(table),
            table_id,
            access,
            filter,
            None,
            None,
            binder,
            consts,
        );
    }

    let mut projection: Vec<Projection> = match &s.items {
        None => (0..table.columns.len() as u16).map(Projection::Column).collect(),
        Some(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let (b, _) = binder.bind_expr(item)?;
                out.push(match b {
                    BExpr::Col(i) => Projection::Column(i),
                    other => {
                        let program = compile_program(&other)?;
                        let name = render_program(&program, &|c| {
                            table
                                .columns
                                .get(c as usize)
                                .map(|c| c.name.clone())
                                .unwrap_or_else(|| format!("col#{c}"))
                        });
                        Projection::Expr { program, name }
                    }
                });
            }
            out
        }
    };

    // Prefer sorting the BASE row: it is the only form that keeps the PK-prefix
    // elision and the streaming top-K path, both of which are about scan order.
    // That needs every key to be a plain column of this table AND no dedup in
    // between — under DISTINCT the sort must follow the dedup, so the base row
    // is not the tuple being ordered at all.
    let mut base_keys = Vec::with_capacity(s.order_by.len());
    for (e, desc) in &s.order_by {
        if s.distinct {
            break;
        }
        // A NAMED key must name a real column, and saying so beats any later
        // complaint: `ORDER BY nope` is a typo, and reporting it as "not in the
        // SELECT list" would send the reader looking in the wrong place.
        let name = match e {
            ast::Expr::Col(n) => n,
            ast::Expr::Qualified(q, n) if q.eq_ignore_ascii_case(&table.name) => n,
            // Not a name at all (an ordinal, an expression): the base row
            // cannot be the tuple being sorted.
            _ => break,
        };
        let idx = table
            .column_index(name)
            .ok_or_else(|| bind_err(format!("unknown column `{name}` in ORDER BY")))?;
        base_keys.push((idx, *desc));
    }
    let (mut order_by, order_over, order_junk) = if base_keys.len() == s.order_by.len()
        && !s.distinct
    {
        (base_keys, OrderOver::BaseRow, 0)
    } else {
        // A computed key, an ordinal, or DISTINCT: sort the output instead. A
        // key that is not in the output at all gets a sort-only column appended
        // — unless DISTINCT, where `None` here makes that an error rather than
        // a dedup on an invisible value.
        let junk = if s.distinct {
            None
        } else {
            Some((&mut projection, &mut binder))
        };
        let (keys, n_junk) = distinct_order_by(s, &Scope::single(table), junk)?;
        (keys, OrderOver::Projection, n_junk)
    };
    // A PK-prefix ORDER BY, all ascending, over a PK-ordered access path is
    // already satisfied by scan order: drop the sort. Not under DISTINCT — the
    // indices are output positions there, and the dedup between the scan and
    // the sort means scan order does not survive to the output anyway.
    let pk_ordered_access =
        !matches!(access, AccessPath::IndexPoint { .. }) && order_over == OrderOver::BaseRow;
    if pk_ordered_access
        && !order_by.is_empty()
        && order_by.len() <= table.primary_key.len()
        && order_by
            .iter()
            .enumerate()
            .all(|(k, (c, desc))| !desc && table.primary_key[k] == *c)
    {
        order_by.clear();
    }

    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Select {
            aggregate: None,
            join,
            joined_filter,
            distinct: s.distinct,
            order_over,
            order_junk,
            table: table_id,
            access,
            filter,
            projection,
            order_by,
            limit: s.limit,
            offset: s.offset,
        },
        param_types,
        context_keys,
        list_keys,
    ))
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
        for c in secondary_indexes(table) {
            usable.push(format!("({})", table.columns[c as usize].name));
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
            if t != table.columns[i].ty {
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

fn split_and(e: BExpr, out: &mut Vec<BExpr>) {
    match e {
        BExpr::Binary(BinOp::And, l, r) => {
            split_and(*l, out);
            split_and(*r, out);
        }
        other => out.push(other),
    }
}

/// Decompose the (already bound and folded) WHERE expression into an access
/// path plus a residual predicate. Consumed conjuncts move into the access
/// path; literals become plan-level constants.
fn extract_access(
    bound_where: Option<BExpr>,
    table: &TableDef,
    consts: &mut Vec<Value>,
) -> Result<(AccessPath, Option<BExpr>)> {
    let Some(w) = bound_where else {
        return Ok((AccessPath::FullScan, None));
    };
    let mut conjuncts = Vec::new();
    split_and(w, &mut conjuncts);
    let cmps: Vec<Option<(u16, BinOp, Atom)>> = conjuncts.iter().map(as_col_cmp).collect();
    let mut consumed = vec![false; conjuncts.len()];

    // Find the first unconsumed conjunct `col <op-in-set> atom` on `col`.
    let find = |consumed: &[bool], col: u16, ops: &[BinOp]| -> Option<(usize, BinOp, Atom)> {
        cmps.iter().enumerate().find_map(|(i, c)| match c {
            Some((cc, op, atom)) if !consumed[i] && *cc == col && ops.contains(op) => {
                Some((i, *op, atom.clone()))
            }
            _ => None,
        })
    };

    // 1. Every PK column pinned by equality -> PkPoint.
    let pins: Vec<Option<(usize, BinOp, Atom)>> = table
        .primary_key
        .iter()
        .map(|&pk| find(&consumed, pk, &[BinOp::Eq]))
        .collect();
    if pins.iter().all(Option::is_some) {
        let mut parts = Vec::with_capacity(pins.len());
        for pin in pins.into_iter().flatten() {
            let (i, _, atom) = pin;
            consumed[i] = true;
            parts.push(atom.to_key_part(consts)?);
        }
        let residual = rebuild_residual(conjuncts, &consumed);
        return Ok((AccessPath::PkPoint(parts), residual));
    }

    // 2. Point probe of a secondary unique index — BEFORE any PK range:
    // a unique probe returns at most one row, so it strictly dominates a
    // range scan (`WHERE pk >= $1 AND unique_col = $2` must not scan an
    // unbounded range; the range conjuncts stay behind as the residual
    // filter). First matching conjunct in statement order wins; indexes
    // beyond the 64-bit footprint bitmap are never chosen.
    let sec = secondary_indexes(table);
    let probe = cmps.iter().enumerate().find_map(|(i, c)| match c {
        Some((col, BinOp::Eq, atom)) => sec
            .iter()
            .position(|sc| sc == col)
            .filter(|pos| *pos < 63)
            .map(|pos| (i, (pos + 1) as u32, atom.clone())),
        _ => None,
    });
    if let Some((i, index_no, atom)) = probe {
        consumed[i] = true;
        let part = atom.to_key_part(consts)?;
        let residual = rebuild_residual(conjuncts, &consumed);
        return Ok((AccessPath::IndexPoint { index_no, part }, residual));
    }

    // 3. Range over the first PK column.
    let first_pk = table.primary_key[0];
    let mut lo = None;
    let mut hi = None;
    if table.primary_key.len() > 1 {
        // Equality on the first PK column of a multi-column PK when full
        // pinning failed: inclusive point-range lo = hi.
        if let Some((i, _, atom)) = find(&consumed, first_pk, &[BinOp::Eq]) {
            consumed[i] = true;
            let part = atom.to_key_part(consts)?;
            let bound = KeyBound {
                parts: vec![part],
                inclusive: true,
            };
            lo = Some(bound.clone());
            hi = Some(bound);
        }
    }
    if lo.is_none() && hi.is_none() {
        if let Some((i, op, atom)) = find(&consumed, first_pk, &[BinOp::Gt, BinOp::Ge]) {
            consumed[i] = true;
            lo = Some(KeyBound {
                parts: vec![atom.to_key_part(consts)?],
                inclusive: op == BinOp::Ge,
            });
        }
        if let Some((i, op, atom)) = find(&consumed, first_pk, &[BinOp::Lt, BinOp::Le]) {
            consumed[i] = true;
            hi = Some(KeyBound {
                parts: vec![atom.to_key_part(consts)?],
                inclusive: op == BinOp::Le,
            });
        }
    }
    if lo.is_some() || hi.is_some() {
        let residual = rebuild_residual(conjuncts, &consumed);
        return Ok((AccessPath::PkRange { lo, hi }, residual));
    }

    // 4. Full scan; the whole predicate stays as the filter.
    let residual = rebuild_residual(conjuncts, &consumed);
    Ok((AccessPath::FullScan, residual))
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

// ---- footprint ---------------------------------------------------------------

fn access_key_and_indexes(a: &AccessPath) -> (KeyAccess, u64) {
    match a {
        AccessPath::PkPoint(parts) => (KeyAccess::Point(parts.clone()), 1),
        AccessPath::PkRange { lo, hi } => (
            KeyAccess::Range {
                lo: lo.clone(),
                hi: hi.clone(),
            },
            1,
        ),
        // The secondary probe also fetches the row through the PK tree, so
        // both index bits are set. Key access degrades honestly to Full.
        AccessPath::IndexPoint { index_no, .. } => {
            (KeyAccess::Full, 1 | (1u64 << (*index_no).min(63)))
        }
        AccessPath::FullScan => (KeyAccess::Full, 1),
    }
}

/// Compute the footprint a statement must carry. Also used by
/// [`CompiledPlan::decode`] to verify that a stored footprint was not forged.
pub(crate) fn compute_footprint(stmt: &PlanStmt, schema: &Schema) -> Result<Footprint> {
    let table_bit = |id: u32| -> Result<u64> {
        if schema.table(id).is_none() || id >= 64 {
            return Err(Error::Corrupt(format!("table id {id} out of range")));
        }
        Ok(1u64 << id)
    };
    let all_secondary_bits = |t: &TableDef| -> Result<u64> {
        let n = secondary_indexes(t).len();
        if n > 63 {
            return Err(Error::Unsupported(
                "more than 63 secondary indexes on one table".into(),
            ));
        }
        let mut bits = 1u64; // PK tree
        for k in 0..n {
            bits |= 1u64 << (k + 1);
        }
        Ok(bits)
    };
    Ok(match stmt {
        PlanStmt::Select {
            table,
            access,
            join,
            ..
        } => {
            let (key_access, mut indexes_used) = access_key_and_indexes(access);
            // ONE BIT PER TABLE READ. A join that claimed only the outer would
            // under-claim `tables_read`, and `conflicts_with` is a bitmap AND —
            // so a writer to the inner table would not be seen to conflict with
            // this reader, and the commit path would group them as independent.
            let mut tables_read = table_bit(*table)?;
            let mut key_access = key_access;
            if let Some(j) = join {
                tables_read |= table_bit(j.table)?;
                let (jkey, jidx) = access_key_and_indexes(&j.access);
                indexes_used |= jidx;
                let _ = jkey;
                // `key_access` is per-STATEMENT, and it names ONE key space. A
                // Point on the outer stops describing what this reads the
                // moment a second table joins in, and a claim narrower than the
                // truth is a claim that rows this statement does read are rows
                // it does not. Full is the only honest answer the type can
                // express — it costs conflict precision, never correctness.
                key_access = KeyAccess::Full;
            }
            Footprint {
                tables_read,
                tables_written: 0,
                indexes_used,
                key_access,
                read_only: true,
            }
        }
        PlanStmt::Insert { table, rows, .. } => {
            let t = schema
                .table(*table)
                .ok_or_else(|| Error::Corrupt("table id out of range".into()))?;
            // Single-row insert with every PK column from Param/Const gives an
            // exact point write set; multi-row or defaulted PK degrades to Full.
            let key_access = if rows.len() == 1 {
                let parts: Option<Vec<KeyPart>> = t
                    .primary_key
                    .iter()
                    .map(|&c| match rows[0].get(c as usize) {
                        Some(InsertSource::Param(i)) => Some(KeyPart::Param(*i)),
                        Some(InsertSource::Const(i)) => Some(KeyPart::Const(*i)),
                        _ => None,
                    })
                    .collect();
                parts.map_or(KeyAccess::Full, KeyAccess::Point)
            } else {
                KeyAccess::Full
            };
            Footprint {
                tables_read: 0,
                tables_written: table_bit(*table)?,
                // All unique indexes are maintained by an insert.
                indexes_used: all_secondary_bits(t)?,
                key_access,
                read_only: false,
            }
        }
        PlanStmt::Update {
            table, access, set, ..
        } => {
            let t = schema
                .table(*table)
                .ok_or_else(|| Error::Corrupt("table id out of range".into()))?;
            let (key_access, mut indexes_used) = access_key_and_indexes(access);
            let sec = secondary_indexes(t);
            for (col, _) in set {
                if let Some(pos) = sec.iter().position(|c| c == col) {
                    if pos + 1 > 63 {
                        return Err(Error::Unsupported(
                            "more than 63 secondary indexes on one table".into(),
                        ));
                    }
                    indexes_used |= 1u64 << (pos + 1);
                }
            }
            let bit = table_bit(*table)?;
            Footprint {
                tables_read: bit,
                tables_written: bit,
                indexes_used,
                key_access,
                read_only: false,
            }
        }
        PlanStmt::Delete { table, access, .. } => {
            let t = schema
                .table(*table)
                .ok_or_else(|| Error::Corrupt("table id out of range".into()))?;
            let (key_access, indexes_used) = access_key_and_indexes(access);
            let bit = table_bit(*table)?;
            Footprint {
                tables_read: bit,
                tables_written: bit,
                // A delete unlinks the row from every index.
                indexes_used: indexes_used | all_secondary_bits(t)?,
                key_access,
                read_only: false,
            }
        }
        // Transaction control touches no tables. KeyAccess::Full is the
        // honest "no key claim" value; read_only routes them past nothing —
        // the engine special-cases them anyway.
        PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => Footprint {
            tables_read: 0,
            tables_written: 0,
            indexes_used: 0,
            key_access: KeyAccess::Full,
            read_only: true,
        },
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::prepare;
    use mpedb_types::{ColumnDef, DefaultExpr};

    fn col(name: &str, ty: ColumnType) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            ty,
            nullable: true,
            unique: false,
            default: None,
            check: None,
        }
    }

    /// Tables sort by name: events = 0, orders = 1, users = 2.
    pub(crate) fn test_schema() -> Schema {
        let users = TableDef {
            name: "users".into(),
            columns: vec![
                ColumnDef { nullable: false, ..col("id", ColumnType::Int64) },
                ColumnDef {
                    nullable: false,
                    unique: true,
                    ..col("email", ColumnType::Text)
                },
                col("age", ColumnType::Int64),
                col("score", ColumnType::Float64),
                col("active", ColumnType::Bool),
                ColumnDef {
                    default: Some(DefaultExpr::Now),
                    ..col("created", ColumnType::Timestamp)
                },
            ],
            primary_key: vec![0],
        };
        let orders = TableDef {
            name: "orders".into(),
            columns: vec![
                ColumnDef { nullable: false, ..col("user_id", ColumnType::Int64) },
                ColumnDef { nullable: false, ..col("item_no", ColumnType::Int64) },
                ColumnDef { unique: true, ..col("sku", ColumnType::Text) },
                col("note", ColumnType::Text),
            ],
            primary_key: vec![0, 1],
        };
        let events = TableDef {
            name: "events".into(),
            columns: vec![
                ColumnDef {
                    nullable: false,
                    default: Some(DefaultExpr::Now),
                    ..col("ts", ColumnType::Timestamp)
                },
                col("msg", ColumnType::Text),
            ],
            primary_key: vec![0],
        };
        Schema::new(vec![users, orders, events]).unwrap()
    }

    fn access_of(plan: &CompiledPlan) -> &AccessPath {
        match &plan.stmt {
            PlanStmt::Select { access, .. }
            | PlanStmt::Update { access, .. }
            | PlanStmt::Delete { access, .. } => access,
            other => panic!("no access path in {other:?}"),
        }
    }

    fn filter_of(plan: &CompiledPlan) -> Option<&mpedb_types::ExprProgram> {
        match &plan.stmt {
            PlanStmt::Select { filter, .. }
            | PlanStmt::Update { filter, .. }
            | PlanStmt::Delete { filter, .. } => filter.as_ref(),
            other => panic!("no filter in {other:?}"),
        }
    }

    #[test]
    fn secondary_index_numbering() {
        let s = test_schema();
        // users: id is by itself the whole PK -> skipped even though the PK
        // tree covers it; email (declared unique) is index 1.
        assert_eq!(secondary_indexes(s.table(2).unwrap()), vec![1]);
        // orders: sku is index 1.
        assert_eq!(secondary_indexes(s.table(1).unwrap()), vec![2]);
        // A unique column that is part of a multi-column PK is NOT skipped.
        let t = TableDef {
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    nullable: false,
                    unique: true,
                    ..col("a", ColumnType::Int64)
                },
                ColumnDef { nullable: false, ..col("b", ColumnType::Int64) },
            ],
            primary_key: vec![0, 1],
        };
        assert_eq!(secondary_indexes(&t), vec![0]);
    }

    #[test]
    fn pk_point_on_single_column_pk() {
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE id = $1", &s).unwrap();
        assert_eq!(access_of(&p), &AccessPath::PkPoint(vec![KeyPart::Param(0)]));
        assert!(filter_of(&p).is_none());
        assert_eq!(p.param_types, vec![Some(ColumnType::Int64)]);
        // Reversed operand order works too, with a literal into the pool.
        let p = prepare("SELECT * FROM users WHERE 5 = id", &s).unwrap();
        assert_eq!(access_of(&p), &AccessPath::PkPoint(vec![KeyPart::Const(0)]));
        assert_eq!(p.consts, vec![Value::Int(5)]);
    }

    #[test]
    fn pk_point_consumes_only_key_conjuncts() {
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE id = 1 AND age > 2", &s).unwrap();
        assert_eq!(access_of(&p), &AccessPath::PkPoint(vec![KeyPart::Const(0)]));
        let f = filter_of(&p).expect("residual filter");
        // Residual is `age > 2`.
        let name = crate::plan::render_program(f, &|c| format!("c{c}"));
        assert_eq!(name, "c2 > 2");
    }

    #[test]
    fn multi_column_pk_point_and_point_range() {
        let s = test_schema();
        let p = prepare(
            "SELECT * FROM orders WHERE user_id = 1 AND item_no = $1",
            &s,
        )
        .unwrap();
        assert_eq!(
            access_of(&p),
            &AccessPath::PkPoint(vec![KeyPart::Const(0), KeyPart::Param(0)])
        );
        // Only the first PK column pinned: inclusive point-range.
        let p = prepare("SELECT * FROM orders WHERE user_id = 7", &s).unwrap();
        let b = KeyBound {
            parts: vec![KeyPart::Const(0)],
            inclusive: true,
        };
        assert_eq!(
            access_of(&p),
            &AccessPath::PkRange {
                lo: Some(b.clone()),
                hi: Some(b)
            }
        );
        assert!(filter_of(&p).is_none());
        // Second PK column alone cannot be used: full scan + residual.
        let p = prepare("SELECT * FROM orders WHERE item_no = 7", &s).unwrap();
        assert_eq!(access_of(&p), &AccessPath::FullScan);
        assert!(filter_of(&p).is_some());
    }

    #[test]
    fn pk_range_extraction() {
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE id > 1 AND id <= $1", &s).unwrap();
        assert_eq!(
            access_of(&p),
            &AccessPath::PkRange {
                lo: Some(KeyBound {
                    parts: vec![KeyPart::Const(0)],
                    inclusive: false
                }),
                hi: Some(KeyBound {
                    parts: vec![KeyPart::Param(0)],
                    inclusive: true
                }),
            }
        );
        assert!(filter_of(&p).is_none());
        // One-sided range.
        let p = prepare("SELECT * FROM users WHERE id >= 10", &s).unwrap();
        assert_eq!(
            access_of(&p),
            &AccessPath::PkRange {
                lo: Some(KeyBound {
                    parts: vec![KeyPart::Const(0)],
                    inclusive: true
                }),
                hi: None,
            }
        );
        // Extra bounds on the same column stay in the residual.
        let p = prepare("SELECT * FROM users WHERE id > 1 AND id > 2", &s).unwrap();
        assert!(matches!(access_of(&p), AccessPath::PkRange { lo: Some(_), hi: None }));
        assert!(filter_of(&p).is_some());
    }

    /// The whole reason BETWEEN is desugared in the parser instead of carried
    /// as its own node: `x >= lo AND x <= hi` is the shape extract_access
    /// already recognises, so BETWEEN becomes a range SCAN with no residual
    /// filter and no second spelling for the planner to learn.
    #[test]
    fn between_plans_as_a_range_scan_not_a_full_scan() {
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE id BETWEEN 1 AND $1", &s).unwrap();
        assert_eq!(
            access_of(&p),
            &AccessPath::PkRange {
                lo: Some(KeyBound {
                    parts: vec![KeyPart::Const(0)],
                    inclusive: true
                }),
                hi: Some(KeyBound {
                    parts: vec![KeyPart::Param(0)],
                    inclusive: true
                }),
            }
        );
        assert!(filter_of(&p).is_none(), "BETWEEN must leave no residual filter");

        // NOT BETWEEN cannot be a range (it is the complement of one), so it
        // must fall back honestly rather than plan a wrong range.
        let p = prepare("SELECT * FROM users WHERE id NOT BETWEEN 1 AND 5", &s).unwrap();
        assert_eq!(access_of(&p), &AccessPath::FullScan);
        assert!(filter_of(&p).is_some());
    }

    #[test]
    fn in_list_is_a_full_scan_with_a_residual_for_now() {
        // A PK IN-list could become n point lookups; it does not yet, and the
        // honest plan is a scan plus the filter -- correct, just not clever.
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE id IN (1, 2)", &s).unwrap();
        assert_eq!(access_of(&p), &AccessPath::FullScan);
        assert!(filter_of(&p).is_some());
    }

    #[test]
    fn unique_probe_beats_pk_range() {
        // `WHERE id >= $1 AND email = $2` must be a unique probe with the
        // range as residual — not an unbounded PK range scan (workbench
        // finding, 2026-07-13).
        let schema = test_schema();
        let p = prepare("SELECT id FROM users WHERE id >= $1 AND email = $2", &schema).unwrap();
        match &p.stmt {
            PlanStmt::Select { access, filter, .. } => {
                assert!(
                    matches!(access, AccessPath::IndexPoint { .. }),
                    "expected IndexPoint, got {access:?}"
                );
                assert!(filter.is_some(), "range conjunct must remain as residual");
            }
            other => panic!("unexpected stmt {other:?}"),
        }
    }

    #[test]
    fn index_point_on_unique_column() {
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE email = $1 AND age = 3", &s).unwrap();
        assert_eq!(
            access_of(&p),
            &AccessPath::IndexPoint {
                index_no: 1,
                part: KeyPart::Param(0)
            }
        );
        assert!(filter_of(&p).is_some());
        assert_eq!(p.footprint.indexes_used, 0b11); // PK fetch + index 1
        assert_eq!(p.footprint.key_access, KeyAccess::Full);
        // PK access beats index access.
        let p = prepare("SELECT * FROM users WHERE email = 'a' AND id = 1", &s).unwrap();
        assert!(matches!(access_of(&p), AccessPath::PkPoint(_)));
    }

    #[test]
    fn null_literal_is_never_a_key() {
        let s = test_schema();
        let p = prepare("SELECT * FROM users WHERE id = NULL", &s).unwrap();
        // `id = NULL` folds to NULL, which is not extractable: full scan.
        assert_eq!(access_of(&p), &AccessPath::FullScan);
        assert!(filter_of(&p).is_some());
    }

    #[test]
    fn order_by_pk_prefix_elision() {
        let s = test_schema();
        let order = |sql: &str| match prepare(sql, &s).unwrap().stmt {
            PlanStmt::Select { order_by, .. } => order_by,
            other => panic!("{other:?}"),
        };
        assert_eq!(order("SELECT * FROM users ORDER BY id"), vec![]);
        assert_eq!(order("SELECT * FROM users ORDER BY id ASC"), vec![]);
        assert_eq!(order("SELECT * FROM users ORDER BY id DESC"), vec![(0, true)]);
        assert_eq!(order("SELECT * FROM users ORDER BY email"), vec![(1, false)]);
        assert_eq!(order("SELECT * FROM orders ORDER BY user_id, item_no"), vec![]);
        assert_eq!(order("SELECT * FROM orders ORDER BY user_id"), vec![]);
        assert_eq!(
            order("SELECT * FROM orders ORDER BY item_no, user_id"),
            vec![(1, false), (0, false)]
        );
        // Not elided over an index probe (index order != PK order).
        assert_eq!(
            order("SELECT * FROM users WHERE email = 'x' ORDER BY id"),
            vec![(0, false)]
        );
        // Unknown ORDER BY column is a bind error.
        assert!(matches!(
            prepare("SELECT * FROM users ORDER BY nope", &s),
            Err(Error::Bind(_))
        ));
    }

    #[test]
    fn select_star_projects_all_columns_in_order() {
        let s = test_schema();
        match prepare("SELECT * FROM users", &s).unwrap().stmt {
            PlanStmt::Select { projection, .. } => {
                assert_eq!(
                    projection,
                    (0..6u16).map(Projection::Column).collect::<Vec<_>>()
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn insert_footprint_point_extraction() {
        let s = test_schema();
        // Single row, PK from a literal: exact point write set.
        let p = prepare("INSERT INTO users (id, email) VALUES (1, 'a')", &s).unwrap();
        assert_eq!(p.footprint.key_access, KeyAccess::Point(vec![KeyPart::Const(0)]));
        assert_eq!(p.footprint.tables_written, 1 << 2);
        assert_eq!(p.footprint.tables_read, 0);
        assert_eq!(p.footprint.indexes_used, 0b11); // PK + email index
        assert!(!p.footprint.read_only);
        // Multi-row: Full.
        let p = prepare("INSERT INTO users (id, email) VALUES (1, 'a'), (2, 'b')", &s).unwrap();
        assert_eq!(p.footprint.key_access, KeyAccess::Full);
        // Defaulted PK: Full.
        let p = prepare("INSERT INTO events (msg) VALUES ('x')", &s).unwrap();
        assert_eq!(p.footprint.key_access, KeyAccess::Full);
        match &p.stmt {
            PlanStmt::Insert { rows, .. } => {
                assert_eq!(rows[0][0], InsertSource::Default);
                assert!(matches!(rows[0][1], InsertSource::Const(_)));
            }
            other => panic!("{other:?}"),
        }
        // Multi-column PK point.
        let p = prepare("INSERT INTO orders (user_id, item_no) VALUES ($1, $2)", &s).unwrap();
        assert_eq!(
            p.footprint.key_access,
            KeyAccess::Point(vec![KeyPart::Param(0), KeyPart::Param(1)])
        );
    }

    #[test]
    fn update_delete_footprints() {
        let s = test_schema();
        let p = prepare("UPDATE users SET age = age + 1 WHERE id = $1", &s).unwrap();
        assert_eq!(p.footprint.tables_read, 1 << 2);
        assert_eq!(p.footprint.tables_written, 1 << 2);
        assert_eq!(p.footprint.indexes_used, 0b01); // age has no index
        assert!(matches!(p.footprint.key_access, KeyAccess::Point(_)));
        assert!(!p.footprint.read_only);
        // Updating an indexed column adds its bit.
        let p = prepare("UPDATE users SET email = $1 WHERE id = $2", &s).unwrap();
        assert_eq!(p.footprint.indexes_used, 0b11);
        // Delete maintains every index.
        let p = prepare("DELETE FROM users WHERE id = 1", &s).unwrap();
        assert_eq!(p.footprint.indexes_used, 0b11);
        assert!(matches!(p.footprint.key_access, KeyAccess::Point(_)));
        let p = prepare("DELETE FROM orders", &s).unwrap();
        assert_eq!(p.footprint.key_access, KeyAccess::Full);
        assert_eq!(p.footprint.indexes_used, 0b11);
    }

    #[test]
    fn txn_control_footprints() {
        let s = test_schema();
        for sql in ["BEGIN", "COMMIT", "ROLLBACK"] {
            let p = prepare(sql, &s).unwrap();
            assert_eq!(p.footprint.tables_read, 0);
            assert_eq!(p.footprint.tables_written, 0);
            assert_eq!(p.footprint.indexes_used, 0);
            assert_eq!(p.footprint.key_access, KeyAccess::Full);
            assert!(p.footprint.read_only);
            assert_eq!(p.n_params, 0);
        }
    }

    #[test]
    fn update_rejects_pk_and_bad_types() {
        let s = test_schema();
        match prepare("UPDATE users SET id = 2 WHERE id = 1", &s) {
            Err(Error::Bind(m)) => assert!(m.contains("primary key")),
            other => panic!("expected bind error, got {other:?}"),
        }
        assert!(matches!(
            prepare("UPDATE orders SET item_no = 1", &s),
            Err(Error::Bind(_))
        ));
        assert!(matches!(
            prepare("UPDATE users SET age = 'x'", &s),
            Err(Error::Bind(_))
        ));
        assert!(matches!(
            prepare("UPDATE users SET email = NULL", &s),
            Err(Error::Bind(_))
        ));
        assert!(matches!(
            prepare("UPDATE users SET age = 1, age = 2", &s),
            Err(Error::Bind(_))
        ));
        // Int expression into float column is coerced.
        let p = prepare("UPDATE users SET score = age + 1 WHERE id = 1", &s).unwrap();
        match &p.stmt {
            PlanStmt::Update { set, .. } => {
                let rendered = crate::plan::render_program(&set[0].1, &|c| format!("c{c}"));
                assert_eq!(rendered, "float(c2 + 1)");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn insert_binding_rules() {
        let s = test_schema();
        // Omitting a NOT NULL column without default is a bind error.
        match prepare("INSERT INTO users (id) VALUES (1)", &s) {
            Err(Error::Bind(m)) => assert!(m.contains("email")),
            other => panic!("expected bind error, got {other:?}"),
        }
        // Explicit NULL into NOT NULL column.
        assert!(matches!(
            prepare("INSERT INTO users (id, email) VALUES (1, NULL)", &s),
            Err(Error::Bind(_))
        ));
        // Type mismatch.
        assert!(matches!(
            prepare("INSERT INTO users (id, email) VALUES ('x', 'a')", &s),
            Err(Error::Bind(_))
        ));
        // Non-literal expressions are rejected.
        assert!(matches!(
            prepare("INSERT INTO users (id, email) VALUES (1 + id, 'a')", &s),
            Err(Error::Bind(_))
        ));
        // ...but constant-foldable expressions are fine.
        let p = prepare("INSERT INTO users (id, email) VALUES (-1, 'a')", &s).unwrap();
        assert_eq!(p.consts[0], Value::Int(-1));
        // Int literal into float column is folded to a float const.
        let p = prepare("INSERT INTO users (id, email, score) VALUES (1, 'a', 5)", &s).unwrap();
        assert_eq!(p.consts[2], Value::Float(5.0));
        // Wrong tuple width.
        assert!(matches!(
            prepare("INSERT INTO users (id, email) VALUES (1)", &s),
            Err(Error::Bind(_))
        ));
        // Duplicate column.
        assert!(matches!(
            prepare("INSERT INTO users (id, id) VALUES (1, 2)", &s),
            Err(Error::Bind(_))
        ));
        // Param types unify to column types.
        let p = prepare("INSERT INTO users (id, email, score) VALUES ($1, $2, $3)", &s).unwrap();
        assert_eq!(
            p.param_types,
            vec![
                Some(ColumnType::Int64),
                Some(ColumnType::Text),
                Some(ColumnType::Float64)
            ]
        );
        // Conflicting param inference across columns.
        assert!(matches!(
            prepare("INSERT INTO users (id, email) VALUES ($1, $1)", &s),
            Err(Error::Bind(_))
        ));
    }

    #[test]
    fn unknown_table_is_bind_error() {
        let s = test_schema();
        match prepare("SELECT * FROM nope", &s) {
            Err(Error::Bind(m)) => assert!(m.contains("nope")),
            other => panic!("expected bind error, got {other:?}"),
        }
    }
}
