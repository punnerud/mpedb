//! Compound SELECT (UNION/EXCEPT/INTERSECT) planning (moved verbatim from
//! `planner/mod.rs`).

use super::*;

/// Bind and plan a compound SELECT: plan each arm as an ordinary select, then
/// check the arms AGREE — same arity, same output types (rigid engine: no
/// sqlite-style cross-arm coercion; `CAST` one side instead), one shared
/// parameter table — and resolve the compound-level ORDER BY against the
/// first arm's output.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_compound(
    c: &ast::CompoundStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    let mut arms: Vec<crate::plan::CompoundArm> = Vec::with_capacity(c.arms.len());
    let mut param_types: Vec<Option<ColumnType>> = Vec::new();
    let mut context_keys: Vec<String> = Vec::new();
    let mut list_keys: BTreeSet<String> = BTreeSet::new();
    let mut out_types: Vec<Option<ColumnType>> = Vec::new();
    // Each arm OWNS its lifts (format 56). See `CompoundPlan::arm_subplans`.
    // A Derived arm owns lifts inside the DerivedPlan; its list entry is empty.
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
        // Nested `SELECT … FROM (<non-flat body>)` as a compound arm: materialise
        // via DerivedPlan (format 58). The passthrough/splice rewrite already
        // removed identity wrappers; what remains here cannot be flattened.
        let (arm, ptypes, ckeys, lkeys, otypes, arm_subs, extra_slots) =
            if arm_ast.from_derived.is_some() {
                let (stmt, ptypes, ckeys, lkeys, otypes, _subs) = derived::plan_derived_select(
                    arm_ast, schema, arm_base, catalog, mode, host_udfs, row_count, consts,
                )?;
                let crate::plan::PlanStmt::Derived(dp) = stmt else {
                    return Err(Error::Internal(
                        "plan_derived_select produced a non-derived".into(),
                    ));
                };
                // Compound arms may not carry their own ORDER BY/LIMIT — the
                // compound owns those. A derived outer that still has them is a
                // non-passthrough wrapper we correctly refuse to splice.
                if !dp.outer.order_by.is_empty()
                    || dp.outer.order_junk != 0
                    || dp.outer.limit.is_some()
                    || dp.outer.offset.is_some()
                {
                    return Err(bind_err(
                        "a derived table with ORDER BY/LIMIT as a compound arm is \
                         not supported — the compound's own ORDER BY/LIMIT binds \
                         the whole chain",
                    ));
                }
                let body_slots = dp.reserved_slots();
                (
                    crate::plan::CompoundArm::Derived(dp),
                    ptypes,
                    ckeys,
                    lkeys,
                    otypes,
                    Vec::new(),
                    body_slots,
                )
            } else {
                let (stmt, ptypes, ckeys, lkeys, otypes, arm_subs) = plan_select(
                    arm_ast, schema, arm_base, catalog, mode, host_udfs, row_count, consts, None,
                    &[],
                )?;
                let PlanStmt::Select(sp) = stmt else {
                    return Err(Error::Internal("plan_select produced a non-select".into()));
                };
                let n = arm_subs.len() as u16;
                (
                    crate::plan::CompoundArm::Select(sp),
                    ptypes,
                    ckeys,
                    lkeys,
                    otypes,
                    arm_subs,
                    n,
                )
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
            .checked_add(extra_slots)
            .ok_or_else(|| bind_err("too many subqueries in one compound SELECT"))?;
        if n_slots as usize > MAX_PLAN_SUBPLANS {
            return Err(bind_err(
                "too many subqueries in one compound SELECT (max 16 across all arms)",
            ));
        }
        arm_subplans.push(arm_subs);
        arms.push(arm);
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
    // A DERIVED arm's slots are the BODY's columns, which no schema table
    // holds — `DerivedPlan::columns` is where their names live. Reaching for
    // `schema.table(sp.table)` there resolved nothing and reported the name as
    // absent, so `… UNION … ORDER BY id` refused over an arm whose FROM is a
    // materialized derived table (SQLAlchemy writes exactly that for a UNION of
    // two LIMITed selectables). An ordinal or an explicit alias worked, which is
    // what made it look like a naming rule rather than a missing case.
    let arm_name = |arm: &CompoundArm, j: usize| -> Option<String> {
        if let CompoundArm::Derived(dp) = arm {
            if let Some(Projection::Column(i)) = dp.outer.projection.get(j) {
                return dp.columns.get(*i as usize).cloned();
            }
        }
        out_name(arm.output_select(), j)
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
        let ast::Expr::Col(n, _) = e else {
            return Err(bind_err(
                "ORDER BY over a compound must name an output column or ordinal",
            ));
        };
        let pos = (0..arity).find(|&j| {
            arm_name(&arms[0], j).is_some_and(|nm| nm.eq_ignore_ascii_case(n))
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

