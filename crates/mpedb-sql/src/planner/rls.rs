//! Row-level-security predicate injection (moved verbatim from `planner/mod.rs`).

use super::*;

pub(super) fn and(a: BExpr, b: BExpr) -> BExpr {
    BExpr::Binary(BinOp::And, Box::new(a), Box::new(b))
}
fn or(a: BExpr, b: BExpr) -> BExpr {
    BExpr::Binary(BinOp::Or, Box::new(a), Box::new(b))
}

/// AND-combine an optional user predicate with an optional injected policy.
pub(super) fn merge_and(user: Option<BExpr>, policy: Option<BExpr>) -> Option<BExpr> {
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
pub(super) fn read_policy(
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
pub(super) fn write_check(
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

