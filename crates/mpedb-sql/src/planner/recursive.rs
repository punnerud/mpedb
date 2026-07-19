//! Planning for `WITH RECURSIVE` (recursive CTEs, design/DESIGN-CTE-RECURSIVE.md
//! stage 1). Unlike a non-recursive CTE — flattened onto its base table at bind
//! time (design/DESIGN-CTE.md) — this compiles to a genuine fixpoint node,
//! [`PlanStmt::RecursiveCte`], that the executor iterates.
//!
//! The three components (anchor / recursive term / outer statement) are ordinary
//! SELECTs, planned by the shared [`plan_select`]. The recursive term and the
//! outer statement see the CTE's WORKING TABLE — a synthetic, non-schema table
//! ([`CTE_TABLE`], FullScan-only) threaded through name resolution as a
//! [`CteRef`]; the anchor never does.

use super::*;

/// Plan `WITH RECURSIVE t(cols) AS (<anchor> UNION[ ALL] <recursive>) <outer>`.
///
/// Stage 1: a single recursive CTE, a single anchor and a single recursive term.
/// Parameters are confined to the outer statement (the parser makes the body
/// parameter-free); subqueries and `current_setting()` are refused in every
/// component, keeping the parameter layout `[user]` only.
pub(super) fn plan_recursive_cte(
    rc: &ast::RecursiveCteStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let name = rc.name.as_str();

    // 1. Anchor — planned against the REAL schema, so it cannot reference the
    //    working table (a `FROM <name>` there resolves to a base table or errors
    //    as unknown). Its projection fixes the CTE's arity and column types.
    let (a_stmt, a_ptypes, a_ctx, _a_list, a_out, a_subs) =
        plan_select(&rc.anchor, schema, n_params, catalog, mode, host_udfs, consts, None)?;
    reject_unsupported_component(name, "anchor", &a_ctx, &a_subs)?;
    let anchor = into_select(a_stmt);
    if a_out.len() != rc.columns.len() {
        return Err(bind_err(format!(
            "recursive CTE \"{name}\" declares {} column(s) but its anchor SELECT returns {}",
            rc.columns.len(),
            a_out.len()
        )));
    }
    let mut col_types = Vec::with_capacity(a_out.len());
    for (i, ty) in a_out.iter().enumerate() {
        match ty {
            Some(t) => col_types.push(*t),
            None => {
                return Err(bind_err(format!(
                    "recursive CTE \"{name}\" column \"{}\" has no type in the anchor \
                     (a bare NULL / untyped expression) — give the anchor a typed value",
                    rc.columns[i]
                )))
            }
        }
    }

    // 2. Recursive term — planned WITH the working table in scope. Must reference
    //    it exactly once, in a FROM/JOIN operand (§3), and agree on arity/types.
    let cte_def = crate::plan::cte_working_table_def(name, &rc.columns, &col_types);
    let cte = CteRef { name, def: &cte_def };
    let (r_stmt, r_ptypes, r_ctx, _r_list, r_out, r_subs) =
        plan_select(&rc.recursive, schema, n_params, catalog, mode, host_udfs, consts, Some(cte))?;
    reject_unsupported_component(name, "recursive term", &r_ctx, &r_subs)?;
    let recursive = into_select(r_stmt);
    check_recursive_term(&recursive, name)?;
    if r_out.len() != col_types.len() {
        return Err(bind_err(format!(
            "recursive CTE \"{name}\": the recursive term returns {} column(s) but the \
             anchor returns {}",
            r_out.len(),
            col_types.len()
        )));
    }
    for (i, ty) in r_out.iter().enumerate() {
        // A bare NULL (`None`) is assignable to the nullable CTE column; a typed
        // value must equal the anchor's type — a rigid engine never coerces
        // across the UNION (that is exactly where sqlite would silently do so).
        if let Some(t) = ty {
            if *t != col_types[i] {
                return Err(bind_err(format!(
                    "recursive CTE \"{name}\" column \"{}\": the anchor is {:?} but the \
                     recursive term is {:?} (mpedb does not coerce across the UNION)",
                    rc.columns[i], col_types[i], t
                )));
            }
        }
    }

    // 3. Outer statement — also sees the working table. Stage 1: a plain SELECT.
    let outer_ast = match rc.outer.as_ref() {
        ast::Stmt::Select(s) => s,
        _ => {
            return Err(bind_err(
                "the statement after a recursive CTE must be a SELECT (stage 1)",
            ))
        }
    };
    let (o_stmt, o_ptypes, o_ctx, _o_list, o_out, o_subs) =
        plan_select(outer_ast, schema, n_params, catalog, mode, host_udfs, consts, Some(cte))?;
    reject_unsupported_component(name, "outer statement", &o_ctx, &o_subs)?;
    let outer = into_select(o_stmt);

    // 4. Unify the parameter space. The body is parameter-free (the parser
    //    enforces it), so in practice only the outer statement constrains params.
    let param_types = unify_param_types(n_params, &[&a_ptypes, &r_ptypes, &o_ptypes])?;

    let plan = PlanStmt::RecursiveCte(RecursiveCtePlan {
        name: rc.name.clone(),
        columns: rc.columns.clone(),
        col_types,
        union_all: rc.union_all,
        anchor,
        recursive,
        outer,
    });
    // No lifted subplans / context in a stage-1 recursive CTE. `out_types` is the
    // outer statement's, but only compound planning consumes it and a compound
    // never wraps a recursive CTE, so it is effectively unused.
    Ok((plan, param_types, Vec::new(), BTreeSet::new(), o_out, Vec::new()))
}

/// A `plan_select` result is always a `PlanStmt::Select`.
pub(super) fn into_select(stmt: PlanStmt) -> SelectPlan {
    match stmt {
        PlanStmt::Select(sp) => sp,
        _ => unreachable!("plan_select always yields PlanStmt::Select"),
    }
}

/// Stage 1 keeps the parameter layout trivial (`[user]` only): a recursive CTE
/// component may not lift subqueries or reference session context.
fn reject_unsupported_component(
    name: &str,
    which: &str,
    ctx: &[String],
    subs: &[SubPlan],
) -> Result<()> {
    if !subs.is_empty() {
        return Err(bind_err(format!(
            "recursive CTE \"{name}\": subqueries in the {which} are not supported yet"
        )));
    }
    if !ctx.is_empty() {
        return Err(bind_err(format!(
            "recursive CTE \"{name}\": current_setting() in the {which} is not supported yet"
        )));
    }
    Ok(())
}

/// Enforce the §3 restrictions on the recursive term. `validate` re-checks all of
/// these on the decoded plan, so a hand-crafted blob cannot smuggle one past.
fn check_recursive_term(sp: &SelectPlan, name: &str) -> Result<()> {
    let refs = (sp.table == CTE_TABLE) as usize
        + sp.joins.iter().filter(|j| j.table == CTE_TABLE).count();
    match refs {
        0 => {
            return Err(bind_err(format!(
                "the recursive term of \"{name}\" must reference \"{name}\""
            )))
        }
        1 => {}
        _ => {
            return Err(bind_err(format!(
                "the recursive term of \"{name}\" may reference \"{name}\" only once \
                 (no self-join or multiple references of the CTE)"
            )))
        }
    }
    for j in &sp.joins {
        if j.table == CTE_TABLE && j.kind != JoinKind::Inner {
            return Err(bind_err(format!(
                "\"{name}\" may not appear on the null-extended side of an outer join \
                 in its recursive term"
            )));
        }
    }
    if sp.aggregate.is_some() {
        return Err(bind_err(format!(
            "the recursive term of \"{name}\" may not use aggregates or GROUP BY"
        )));
    }
    if sp.distinct {
        return Err(bind_err(format!(
            "the recursive term of \"{name}\" may not use DISTINCT"
        )));
    }
    if !sp.windows.is_empty() {
        return Err(bind_err(format!(
            "the recursive term of \"{name}\" may not use window functions"
        )));
    }
    Ok(())
}

/// Element-wise unify the per-component parameter-type vectors (all sized
/// `n_params`). A slot two components constrain to different types is an error;
/// in stage 1 only the outer statement constrains anything (the body is
/// parameter-free).
pub(super) fn unify_param_types(
    n_params: u16,
    sources: &[&[Option<ColumnType>]],
) -> Result<Vec<Option<ColumnType>>> {
    let mut out = vec![None; n_params as usize];
    for src in sources {
        for (i, t) in src.iter().enumerate() {
            if let Some(t) = t {
                match &out[i] {
                    None => out[i] = Some(*t),
                    Some(existing) if existing == t => {}
                    Some(existing) => {
                        return Err(bind_err(format!(
                            "parameter ${} is used with conflicting types {existing:?} and {t:?}",
                            i + 1
                        )))
                    }
                }
            }
        }
    }
    Ok(out)
}
