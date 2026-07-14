//! Physical planning: decompose WHERE into AND-conjuncts, extract the access
//! path (PK point > PK range > secondary unique point > full scan), compute
//! the residual filter, elide provably redundant ORDER BY, and compute the
//! precomputed footprint (DESIGN.md §7.3).

use crate::ast::{self, BinOp};
use crate::binder::{compile_program, BExpr, Binder};
use crate::plan::{render_program, AccessPath, CompiledPlan, InsertSource, PlanStmt, Projection};
use crate::policy::{PolicyCatalog, TablePolicies};
use mpedb_types::{
    ColumnType, Error, Footprint, KeyAccess, KeyBound, KeyPart, PolicyCmd, Result, Schema,
    TableDef, Value,
};

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
    let (plan_stmt, param_types, context_keys) = match stmt {
        ast::Stmt::Begin => (PlanStmt::Begin, vec![None; n_params as usize], Vec::new()),
        ast::Stmt::Commit => (PlanStmt::Commit, vec![None; n_params as usize], Vec::new()),
        ast::Stmt::Rollback => (PlanStmt::Rollback, vec![None; n_params as usize], Vec::new()),
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
    let tp = target.and_then(|t| catalog.get(t));
    let policy_epoch = tp.map_or(0, |tp| tp.epoch);
    let policy_hash = crate::policy::table_policy_hash(tp);

    // `n_params` now counts user params PLUS the reserved context slots that
    // `current_setting()` appended, so the executor's param array is sized for
    // both. n_user_params = n_params - context_keys.len().
    Ok(CompiledPlan {
        stmt: plan_stmt,
        schema_hash: schema.hash(),
        n_params: param_types.len() as u16,
        param_types,
        context_keys,
        policy_epoch,
        policy_hash,
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

fn plan_select(
    s: &ast::SelectStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<(PlanStmt, Vec<Option<ColumnType>>, Vec<String>)> {
    let (table_id, table) = resolve_table(schema, &s.table)?;
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

    let projection = match &s.items {
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

    let mut order_by = Vec::with_capacity(s.order_by.len());
    for (name, desc) in &s.order_by {
        let idx = table.column_index(name).ok_or_else(|| {
            bind_err(format!("unknown column `{name}` in ORDER BY"))
        })?;
        order_by.push((idx, *desc));
    }
    // A PK-prefix ORDER BY, all ascending, over a PK-ordered access path is
    // already satisfied by scan order: drop the sort.
    let pk_ordered_access = !matches!(access, AccessPath::IndexPoint { .. });
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

    let (param_types, context_keys) = binder.into_parts();
    Ok((
        PlanStmt::Select {
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
    ))
}

fn plan_insert(
    s: &ast::InsertStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<(PlanStmt, Vec<Option<ColumnType>>, Vec<String>)> {
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
    let (param_types, context_keys) = binder.into_parts();
    Ok((
        PlanStmt::Insert {
            table: table_id,
            rows,
            with_check,
        },
        param_types,
        context_keys,
    ))
}

fn plan_update(
    s: &ast::UpdateStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<(PlanStmt, Vec<Option<ColumnType>>, Vec<String>)> {
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
    let (param_types, context_keys) = binder.into_parts();
    Ok((
        PlanStmt::Update {
            table: table_id,
            access,
            filter,
            set,
            with_check,
        },
        param_types,
        context_keys,
    ))
}

fn plan_delete(
    s: &ast::DeleteStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<(PlanStmt, Vec<Option<ColumnType>>, Vec<String>)> {
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
    let (param_types, context_keys) = binder.into_parts();
    Ok((
        PlanStmt::Delete {
            table: table_id,
            access,
            filter,
        },
        param_types,
        context_keys,
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
        PlanStmt::Select { table, access, .. } => {
            let (key_access, indexes_used) = access_key_and_indexes(access);
            Footprint {
                tables_read: table_bit(*table)?,
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
