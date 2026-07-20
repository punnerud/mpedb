//! Planning for MATERIALIZED derived tables (design/DESIGN-DERIVED-TABLES.md
//! §5, Stage A). A `FROM (<body>) [AS] alias` whose body the Stage-B flattener
//! could not splice (aggregate / GROUP BY / HAVING / DISTINCT / join / ORDER
//! BY+LIMIT / window / compound bodies) compiles to [`PlanStmt::Derived`]: the
//! body planned as an ordinary SELECT/compound, and the outer statement planned
//! with the derived alias resolving to the [`CTE_TABLE`] working-table sentinel
//! — the exact name-resolution mechanism the recursive CTE uses ([`CteRef`]).
//! The executor runs the body EXACTLY ONCE into an in-memory row set and scans
//! it from the outer.

use super::recursive::{into_select, unify_param_types};
use super::*;

/// Plan `SELECT … FROM (<body>) [AS] alias …` by materialization.
///
/// Stage 1 mirrors the recursive CTE's parameter discipline: lifted subqueries
/// and `current_setting()` are refused in both components, keeping the layout
/// `[user]` only. The body is planned with the alias NOT in scope, so a body
/// that references an outer table (LATERAL) fails as an unknown table/column —
/// the same error sqlite gives (sqlite has no LATERAL either).
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_derived_select(
    s: &ast::SelectStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let body_ast = s.from_derived.as_deref().expect("caller checked from_derived");
    // The alias is how the outer addresses the body's columns. An alias-less
    // derived table (sqlite allows `FROM (SELECT …)`) gets a synthetic name no
    // identifier can spell, so it can never collide with a real table name nor
    // be referenced by one.
    let name = s.alias.clone().unwrap_or_else(|| "(derived table)".to_string());

    // 1. The body, planned as a standalone statement.
    let (b_stmt, b_ptypes, b_ctx, _b_list, b_out, b_subs) = match body_ast {
        ast::SubqueryBody::Select(bs) => {
            plan_select(bs, schema, n_params, catalog, mode, host_udfs, row_count, consts, None)?
        }
        ast::SubqueryBody::Compound(bc) => {
            plan_compound(bc, schema, n_params, catalog, mode, host_udfs, row_count, consts)?
        }
    };
    reject_context(&name, "derived-table body", &b_ctx)?;
    let body = match b_stmt {
        PlanStmt::Select(sp) => SubBody::Select(sp),
        PlanStmt::Compound(c) => SubBody::Compound(c),
        _ => return Err(Error::Internal("body planning produced a non-select".into())),
    };
    // The body OWNS its lifts (design/DESIGN-DERIVED-TABLES.md §5.5): they
    // correlate to the BODY's row, are filled while the body materialises, and
    // the outer never sees them. A COMPOUND body owns them one level further
    // down — per ARM (§5.6/format 56) — so `plan_compound` hands back an empty
    // list there and the arms carry the lifts themselves. Either way the
    // reserved region starts at `n_params`.
    debug_assert!(b_subs.is_empty() || body.as_select().is_some());
    let body_sub_base = n_params;
    // Reserved slots the BODY owns: its own lifts, or — for a compound body —
    // its arms'.
    let n_body_slots = b_subs.len() as u16
        + match &body {
            SubBody::Compound(c) => c.n_arm_slots(),
            SubBody::Select(_) => 0,
        };

    // 2. The synthetic working-table def: the body's output columns. Types come
    //    from the body's inferred output types; an output the body leaves
    //    untyped (a bare NULL) is `any`, decided per value at runtime — never a
    //    coercion, never a wrong answer.
    let col_types: Vec<ColumnType> = b_out.iter().map(|t| t.unwrap_or(ColumnType::Any)).collect();
    let columns = body_output_names(&body, schema);
    if columns.len() != col_types.len() || columns.is_empty() {
        return Err(Error::Internal(
            "derived-table body output arity disagrees with its types".into(),
        ));
    }

    // 3. The outer statement, planned with the alias resolving to the working
    //    table (FullScan-only; `plan_select` guards its access extraction).
    let def = crate::plan::cte_working_table_def(&name, &columns, &col_types);
    let cte = CteRef { name: &name, def: &def };
    let outer_ast = ast::SelectStmt {
        table: Some(name.clone()),
        from_derived: None,
        alias: None,
        joins: s.joins.clone(),
        distinct: s.distinct,
        items: s.items.clone(),
        where_clause: s.where_clause.clone(),
        group_by: s.group_by.clone(),
        having: s.having.clone(),
        order_by: s.order_by.clone(),
        limit: s.limit,
        offset: s.offset,
    };
    // Refuse an outer-statement subquery BEFORE planning: the lift builds its
    // correlation scope from the schema alone (it cannot see the working
    // table), so letting it run would misreport the stage-1 gap as
    // "unknown table `<alias>`".
    if subquery::has_subquery(&outer_ast) {
        return Err(bind_err(format!(
            "derived table \"{name}\": a subquery in the outer statement is not supported yet"
        )));
    }
    let (o_stmt, o_ptypes, o_ctx, _o_list, o_out, o_subs) =
        plan_select(&outer_ast, schema, n_params, catalog, mode, host_udfs, row_count, consts, Some(cte))?;
    reject_context(&name, "outer statement", &o_ctx)?;
    // Unreachable — `has_subquery` refused above — but a silent slot collision
    // would be worse than a redundant check: the outer numbers ITS lifts from
    // `n_params` too, which is exactly where the body's live.
    if !o_subs.is_empty() {
        return Err(bind_err(format!(
            "derived table \"{name}\": a subquery in the outer statement is not supported yet"
        )));
    }
    let outer = into_select(o_stmt);
    // The materialized rows are read exactly once: the FROM position that
    // defines the alias (which the RIGHT-join rewrite may have moved into a
    // LEFT-join operand). A SECOND reference — the user naming the alias again
    // as a join operand — is refused the way sqlite refuses it.
    let refs = (outer.table == CTE_TABLE) as usize
        + outer.joins.iter().filter(|j| j.table == CTE_TABLE).count();
    if refs != 1 {
        return Err(bind_err(format!(
            "no such table: {name} — a derived table's alias names its rows only in \
             the FROM position that defines it"
        )));
    }

    // 4. One statement, one parameter table. The layout is
    //    `[user ‖ body subplans]`: `b_ptypes` already spans it (the body was
    //    planned with `n_params` user slots and grew by its own lifts), the
    //    outer's spans only the user prefix, and unifying pins the reserved
    //    slot types the body inferred.
    let n_total = n_params
        .checked_add(n_body_slots)
        .ok_or_else(|| bind_err("too many parameters (including reserved slots)"))?;
    let param_types = unify_param_types(n_total, &[&b_ptypes, &o_ptypes])?;
    let plan = PlanStmt::Derived(crate::plan::DerivedPlan {
        name,
        columns,
        col_types,
        body,
        body_subplans: b_subs,
        body_sub_base,
        outer,
    });
    Ok((plan, param_types, Vec::new(), BTreeSet::new(), o_out, Vec::new()))
}

/// Neither component may reference session context: `current_setting()` takes a
/// reserved slot that would have to be reconciled ACROSS the two components,
/// which stage 1 does not do (the recursive-CTE rule, unchanged). A literal
/// `'now'` rides the same slot mechanism and is refused here for the same
/// reason.
///
/// Lifted subqueries are NO LONGER refused in the body — the body OWNS them
/// (see [`crate::plan::DerivedPlan`]); the outer's are refused at its own call
/// site, where the reason (slot collision with the body's) is local.
fn reject_context(name: &str, which: &str, ctx: &[String]) -> Result<()> {
    if !ctx.is_empty() {
        let what = if ctx.iter().any(|k| k == crate::STATEMENT_INSTANT_KEY) {
            "'now'"
        } else {
            "current_setting()"
        };
        return Err(bind_err(format!(
            "derived table \"{name}\": {what} in the {which} is not supported yet"
        )));
    }
    Ok(())
}

/// The body's output column NAMES, in projection order — sqlite's rule: the
/// item's alias, else a bare column's own (SHORT) name, else the rendered
/// expression. `Projection::Column` entries resolve through the body's
/// `[table ‖ joins]` defs to the short name, so a joined body's `SELECT *`
/// exposes `id, name, id, …` (not qualified spellings) and outer references
/// resolve the way they do in sqlite. A compound body's names come from its
/// first arm (sqlite's and PG's rule); a select body's ORDER-BY junk columns
/// are not output and are excluded.
fn body_output_names(body: &SubBody, schema: &Schema) -> Vec<String> {
    let (arm, junk) = match body {
        SubBody::Select(sp) => (sp, sp.order_junk as usize),
        // Compound arms carry no junk (validate enforces it); arm 0 names the
        // output.
        SubBody::Compound(c) => (&c.arms[0], 0),
    };
    let name_slot = |slot: usize| -> String {
        let mut i = slot;
        for id in std::iter::once(arm.table).chain(arm.joins.iter().map(|j| j.table)) {
            let Some(t) = schema.table(id) else { break };
            if i < t.columns.len() {
                return t.columns[i].name.clone();
            }
            i -= t.columns.len();
        }
        // Unreachable for a well-formed body (a DUAL/FROM-less projection is
        // always Expr-shaped); a stable fallback beats a panic.
        format!("col{slot}")
    };
    arm.projection
        .iter()
        .take(arm.projection.len() - junk)
        .map(|p| match p {
            Projection::Column(i) => name_slot(*i as usize),
            Projection::Expr { name, .. } => name.clone(),
        })
        .collect()
}
