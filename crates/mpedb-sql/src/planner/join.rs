use super::*;
use super::select::{describe_key, distinct_order_by, ordinal};

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
/// expressions raise on arithmetic overflow, and a raise is observable — so an
/// `ON a.x * b.secret` that overflows, evaluated before b's policy, would
/// report the existence of a row the policy hides, without ever returning it.
/// (Division by zero is not such a case: like sqlite it yields NULL.) AND-ing
/// everything into one predicate would leave that ordering to whatever the
/// compiler emitted.
///
/// What this deliberately does NOT do yet: push the user's WHERE into either
/// side. Every conjunct waits for the joined row, so the outer is a full scan
/// unless its POLICY pins a key, and the inner is re-scanned per outer row —
/// O(n·m). Correct, and slow enough that EXPLAIN says so.
#[allow(clippy::too_many_arguments)]
/// `A RIGHT JOIN B ON c` is `B LEFT JOIN A ON c` — the two describe the SAME
/// row set (each B row survives; an unmatched B pairs with a NULL-extended A),
/// so the swap turns a preserved-right join into the left-deep LEFT the planner
/// and executor already speak. The catch the swap alone gets wrong is column
/// order: the OUTPUT still lists A's columns first, so a bare `SELECT *` is
/// pinned to the original order as explicit qualified items BEFORE the swap.
///
/// This generalizes to a CHAIN as long as the RIGHT join is the FIRST one:
/// `A RIGHT JOIN B ON c  [INNER|LEFT JOIN C …]` rewrites to
/// `B LEFT JOIN A ON c   [INNER|LEFT JOIN C …]`. Because `(A RIGHT JOIN B)` and
/// `(B LEFT JOIN A)` are the same row set, every INNER/LEFT join that FOLLOWS
/// applies to identical input — the whole chain stays semantically equal, only
/// the first two tables trade places (and the pinned `SELECT *` order undoes
/// that for the output). B becomes the new outer, A its first LEFT-joined inner,
/// and the trailing joins ride along unchanged (they name tables by alias, which
/// the reorder leaves resolvable).
///
/// What still can't be expressed left-deep — and is REFUSED (never answered
/// wrong), with the message pointing at the manual LEFT-JOIN rewrite:
///   - a RIGHT that is NOT first: `(… ⋈ …) RIGHT JOIN X` needs the accumulated
///     left side as a join SUBTREE on X's preserved side, which a left-deep
///     chain has no shape for;
///   - a SECOND RIGHT (it would be non-first after the swap); and
///   - a trailing FULL after this leading RIGHT: composing the RIGHT→LEFT swap
///     with FULL's both-sides-whole handling is refused out of caution, even
///     though the gather computes it correctly. A FULL in a PLAIN chain (no
///     leading RIGHT) IS supported — `plan_join_select` allows it at any
///     position and forces the held FullScan inner it needs.
pub(super) fn rewrite_right_join<'s>(
    s: &ast::SelectStmt,
    schema: &'s Schema,
    cte: Option<CteRef<'s>>,
) -> Result<Option<ast::SelectStmt>> {
    let Some(first_right) = s.joins.iter().position(|j| j.kind == ast::JoinKind::Right) else {
        return Ok(None);
    };
    // Only a LEADING RIGHT can be swapped into a left-deep LEFT chain: the swap
    // has to make the RIGHT's own table the new outer, and that is only sound
    // when nothing has accumulated to its left yet.
    if first_right != 0 {
        return Err(bind_err(
            "RIGHT JOIN in a multi-join chain is only supported as the FIRST join \
             — for a RIGHT that follows another join, swap the tables and write \
             LEFT JOIN",
        ));
    }
    // The joins that FOLLOW the leading RIGHT must be plain INNER/LEFT with an
    // explicit ON: a second RIGHT would be non-first after the swap, and FULL
    // needs both sides whole. A trailing USING/NATURAL join is refused too — the
    // swap moves the original outer from the leftmost position to second, so a
    // USING column present in BOTH swapped tables would change which side is the
    // coalesce representative (sqlite itself calls that "ambiguous"); refusing
    // sidesteps the whole class rather than risk a shifted binding.
    for j in &s.joins[1..] {
        match j.kind {
            ast::JoinKind::Inner | ast::JoinKind::Left => {}
            ast::JoinKind::Right => {
                return Err(bind_err(
                    "a second RIGHT JOIN in a multi-join chain is not supported — \
                     swap the tables and write LEFT JOIN",
                ))
            }
            // A trailing FULL after a leading RIGHT composes the RIGHT→LEFT
            // side-swap with FULL's both-sides-whole handling. The gather
            // computes it correctly (differentially verified), but the swap is
            // an extra transform on the preserved side, so this stays REFUSED
            // out of caution — a plain-chain FULL (no leading RIGHT) is the
            // supported form. Rewrite the RIGHT as a LEFT to lift the FULL into
            // a plain chain.
            ast::JoinKind::Full => {
                return Err(bind_err(
                    "FULL JOIN following a leading RIGHT JOIN is not supported — \
                     swap the RIGHT's tables and write LEFT JOIN so the FULL sits \
                     in a plain left-deep chain",
                ))
            }
        }
        if j.natural || !j.using.is_empty() {
            return Err(bind_err(
                "USING / NATURAL is not supported on a join that follows a leading \
                 RIGHT JOIN — write the ON condition explicitly",
            ));
        }
    }

    let j0 = &s.joins[0];
    // Join paths are unreachable without a FROM — the parser cannot build a
    // join clause on a FROM-less statement.
    let s_table = s.table.as_deref().expect("join on a FROM-less SELECT");
    // `SELECT *` pins the ORIGINAL table order (outer, then each join table in
    // written order) as explicit qualified items, so the swap does not surface
    // the new outer's columns first. No join here carries USING/NATURAL (the
    // leading RIGHT is parser-refused USING, the trailing joins refused above),
    // so there is nothing to coalesce — every column shows once, in order.
    let items = match &s.items {
        Some(items) => Some(items.clone()),
        None => {
            let outer_name = s.alias.clone().unwrap_or_else(|| s_table.to_string());
            let (_, ot) = resolve_table_cte(schema, cte, s_table)?;
            let mut items: Vec<(ast::Expr, Option<String>)> = Vec::new();
            // VISIBLE columns only — a hidden implicit rowid (#94) is never in a
            // `SELECT *`, on either side of the swapped chain.
            for c in ot.visible_columns() {
                items.push((ast::Expr::Qualified(outer_name.clone(), c.name.clone()), None));
            }
            for j in &s.joins {
                let (_, jt) = resolve_table_cte(schema, cte, &j.table)?;
                let jname = j.alias.clone().unwrap_or_else(|| j.table.clone());
                for c in jt.visible_columns() {
                    items.push((ast::Expr::Qualified(jname.clone(), c.name.clone()), None));
                }
            }
            Some(items)
        }
    };
    // Swapped chain: the leading RIGHT's table (`j0`) is the new outer, the
    // original outer becomes its first LEFT join, and the rest ride unchanged.
    let mut joins = Vec::with_capacity(s.joins.len());
    joins.push(ast::JoinClause {
        table: s_table.to_string(),
        alias: s.alias.clone(),
        kind: ast::JoinKind::Left,
        on: j0.on.clone(),
        // RIGHT JOIN USING / NATURAL RIGHT are refused in the parser, so
        // `j0.using` is empty here; the swapped LEFT join carries a plain ON.
        using: Vec::new(),
        natural: false,
    });
    joins.extend(s.joins[1..].iter().cloned());
    Ok(Some(ast::SelectStmt {
        table: Some(j0.table.clone()),
        from_derived: None,
        alias: j0.alias.clone(),
        joins,
        distinct: s.distinct,
        items,
        where_clause: s.where_clause.clone(),
        group_by: s.group_by.clone(),
        having: s.having.clone(),
        order_by: s.order_by.clone(),
        limit: s.limit,
        offset: s.offset,
        drop_trailing: s.drop_trailing,
    }))
}

/// MPEE (task #114, design/DESIGN-MPEE-SOLVER.md): solve this scope's join
/// order BEFORE anything else looks at the statement, and re-enter with the
/// rewritten one. Every join chain in every scope — the top SELECT, a lifted
/// subquery body, a derived body, a compound arm, a recursive-CTE component —
/// passes through here, so "run the solver after each sub-compilation, as each
/// N in the N×N" needs no extra plumbing: this IS every N.
///
/// The rewrite is answer-preserving by construction (only all-INNER chains are
/// eligible, and an INNER join chain is commutative), and it is adopted only
/// when it is STRICTLY cheaper than what the user wrote — mpedb never reorders
/// without a reason it can name in EXPLAIN.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_join_select<'s>(
    s: &ast::SelectStmt,
    schema: &'s Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
    subplans: Vec<SubPlan>,
    slot_types: Vec<Ty>,
    cte: Option<CteRef<'s>>,
) -> Result<PlannedStmt> {
    // A CORRELATED subplan is lifted BEFORE the join dispatch, and its
    // `outer_args` are slots of the joined tuple in the TEXTUAL order — v1
    // refused to reorder at all whenever one existed, because leaving them
    // pointing at the wrong columns is a silently wrong answer (measured:
    // `count(*) FILTER (WHERE EXISTS (… c.ref = t.id))` over a join returned 1
    // where sqlite returns 2). #116 makes it a CONSTRAINT instead: the solver
    // reports the permutation as a slot map and the args are remapped through
    // it (`Solved::remap`, design/DESIGN-MPEE-SOLVER.md §7).
    let solved = if super::mpee::disabled() {
        Err(super::mpee::Skip::Ineligible)
    } else {
        super::mpee::reorder(
            s, schema, n_params, catalog, mode, host_udfs, &slot_types, cte, row_count,
        )
    };
    match solved {
        Ok(sv) => {
            let subplans = sv.remap(subplans);
            plan_join_select_inner(
                &sv.stmt, schema, n_params, catalog, mode, host_udfs, row_count, consts, subplans,
                slot_types, cte,
            )
        }
        Err(_skip) => plan_join_select_inner(
            s, schema, n_params, catalog, mode, host_udfs, row_count, consts, subplans, slot_types,
            cte,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_join_select_inner<'s>(
    s: &ast::SelectStmt,
    schema: &'s Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &HostUdfSet,
    row_count: RowCountFn<'_>,
    consts: &mut Vec<Value>,
    subplans: Vec<SubPlan>,
    slot_types: Vec<Ty>,
    cte: Option<CteRef<'s>>,
) -> Result<PlannedStmt> {
    let _ = row_count; // the solver above is the only consumer at this level
    // A `NATURAL JOIN` is an implicit `USING` over the columns common to the two
    // sides. Resolve that common set from the schema FIRST — a rigid schema makes
    // it a static fact of the plan — and fill each natural join's `using`, so
    // everything below (the `SELECT *` coalesce here, the USING→ON desugar in the
    // join loop) handles a natural join exactly like an explicit USING one. A
    // natural join with NO common column keeps an empty `using` and its `ON true`,
    // i.e. a cross join — sqlite's rule.
    let natural_desugared;
    let s = if s.joins.iter().any(|j| j.natural) {
        natural_desugared = desugar_natural_joins(s, schema)?;
        &natural_desugared
    } else {
        s
    };
    // `SELECT *` over a `USING` join coalesces the join columns — each appears
    // once, from the left side. Expand the star into explicit qualified items
    // BEFORE anything below looks at it, so projection / ORDER BY / DISTINCT all
    // see a normal item list and need no special-casing (the same trick RIGHT
    // JOIN uses). The ON equalities are built separately, per join step below.
    let using_star;
    let s = if s.items.is_none() && s.joins.iter().any(|j| !j.using.is_empty()) {
        using_star = expand_using_star(s, schema)?;
        &using_star
    } else {
        s
    };
    // A plain (non-USING) join whose `SELECT *` spans any implicit-rowid table
    // (#94): expand to the VISIBLE columns as explicit qualified items so no
    // hidden rowid leaks into the output and the projection / ORDER BY / DISTINCT
    // below see a normal item list. USING stars were expanded just above (that
    // path is visible-aware too); a join with NO implicit-rowid table keeps the
    // `None` fast path unchanged.
    let implicit_star;
    let s = if s.items.is_none() {
        match expand_join_star_visible(s, schema, cte)? {
            Some(exp) => {
                implicit_star = exp;
                &implicit_star
            }
            None => s,
        }
    } else {
        s
    };
    // Subqueries were lifted by `plan_select` before the join dispatch; the
    // reserved result slots sit right after the user params.
    let eff_params = n_params + subplans.len() as u16;
    let correlated: Vec<bool> = subplans.iter().map(|p| !p.outer_args.is_empty()).collect();
    let outer_table = s.table.as_deref().expect("join on a FROM-less SELECT");
    let (outer_id, outer) = resolve_table_cte(schema, cte, outer_table)?;
    let outer_name = s.alias.clone().unwrap_or_else(|| outer.name.clone());

    // The outer's policy binds over its own row and can pin an access path. The
    // dialect set here flows to every per-table `rescope` below, so a LIKE in any
    // ON/WHERE conjunct across the join follows the database's LIKE strictness.
    let mut ob = Binder::new(outer, eff_params, true);
    ob.set_dialect(mode);
    ob.set_host_udfs(host_udfs);
    for (i, ty) in slot_types.iter().enumerate() {
        ob.pin_param(n_params + i as u16, *ty);
    }
    let outer_policy = read_policy(&mut ob, catalog, outer_id, &outer.name, PolicyCmd::Select)?;

    // Fold left over the join chain. `acc_named` grows one table per join, so
    // join `k`'s ON binds over `[outer ‖ … ‖ table_k]` — exactly the rows that
    // exist when it runs. Each table's POLICY binds over its OWN row (a policy
    // is a single-row scalar; a joined slot would be the wrong column) and runs
    // BEFORE any ON can raise on that row — the RLS-over-join ordering contract.
    let mut binder = ob;
    let mut acc_named: Vec<(String, &TableDef)> = vec![(outer_name.clone(), outer)];
    let mut joined_columns = outer.columns.clone();
    // Per-step raw material, held back so the WHERE (bound below, over the
    // full joined scope) can push conjuncts INTO each step before access
    // extraction runs — a pushed `WHERE a.x = b.y` becomes an index
    // nested-loop candidate exactly as if it had been written in the ON.
    struct StepDraft<'t> {
        jid: u32,
        jt: &'t TableDef,
        kind: JoinKind,
        policy: Option<ExprProgram>,
        bound_on: BExpr,
        /// The accumulated tuple types BEFORE this table joins in — what
        /// `extract_join_access` resolves `KeyPart::OuterCol` against.
        acc_types_before: Vec<ColumnType>,
        /// Joined-tuple width once this step's table is in.
        width_after: usize,
    }
    let mut drafts: Vec<StepDraft> = Vec::new();
    // The accumulated tuple's column types, for the equi-join pushdown: join
    // `k`'s access may reference outer slots (`KeyPart::OuterCol`), which are
    // slots of the tuple built BEFORE its own table joins in.
    let mut acc_types: Vec<ColumnType> = outer.columns.iter().map(|c| c.ty).collect();
    for jc in &s.joins {
        let (jid, jt) = resolve_table_cte(schema, cte, &jc.table)?;
        let jname = jc.alias.clone().unwrap_or_else(|| jt.name.clone());

        let mut pb = binder.rescope(Scope::single(jt));
        let policy = read_policy(&mut pb, catalog, jid, &jt.name, PolicyCmd::Select)?;
        let policy = policy.map(|e| compile_program(&e)).transpose()?;

        acc_named.push((jname, jt));
        let mut jb = pb.rescope(Scope::joined_named(acc_named.clone())?);
        // A `USING (…)` join desugars HERE to `left.ci = right.ci AND …`: only
        // now is the schema available to qualify the LEFT column, which may live
        // in any table already accumulated. A plain `ON` join binds unchanged.
        let on_desugared;
        let on_expr = if jc.using.is_empty() {
            &jc.on
        } else {
            on_desugared = using_on_expr(&jc.using, &acc_named)?;
            &on_desugared
        };
        let bound_on = jb.bind_predicate(on_expr)?;
        binder = jb;

        let kind = match jc.kind {
            ast::JoinKind::Inner => JoinKind::Inner,
            ast::JoinKind::Left => JoinKind::Left,
            // Every RIGHT was resolved by `rewrite_right_join` before planning:
            // a leading one became a swapped LEFT, a non-leading one was already
            // refused. Reaching here would be a planner bug, but keep the clean
            // refusal as defense rather than a panic.
            ast::JoinKind::Right => {
                return Err(bind_err(
                    "RIGHT JOIN in a multi-join chain is not supported — swap the \
                     tables and write LEFT JOIN",
                ))
            }
            // FULL is allowed at ANY position in the (non-leading-RIGHT) chain.
            // The gather is a strictly left-deep nested loop, so `A J1 B FULL C`
            // is `(A J1 B) FULL JOIN C` — FULL's NULL-extend-both-sides sweep
            // runs over the accumulated left relation at whatever width it has
            // reached, which composes correctly wherever FULL sits (first,
            // middle, last, several FULLs; differentially verified against
            // sqlite in `full_join_chains`). The inner side is forced to a held
            // FullScan below (line ~394) — the unmatched-inner sweep needs it
            // enumerated — and any FULL disables WHERE pushdown (below), so no
            // conjunct filters a row that has not been NULL-extended yet. A
            // leading RIGHT + FULL is the one refused case (`rewrite_right_join`
            // rejects it), keeping the RIGHT→LEFT swap and FULL decoupled.
            ast::JoinKind::Full => JoinKind::Full,
        };
        let acc_types_before = acc_types.clone();
        joined_columns.extend(jt.columns.iter().cloned());
        acc_types.extend(jt.columns.iter().map(|c| c.ty));
        drafts.push(StepDraft {
            jid,
            jt,
            kind,
            policy,
            bound_on,
            acc_types_before,
            width_after: acc_types.len(),
        });
    }

    // WHERE runs over the full joined row (`binder` is now in that scope).
    // Conjuncts reading a CORRELATED slot leave the gather for `post_filter`
    // — those slots are filled per row, after every policy has had its say.
    let bound_where = s
        .where_clause
        .as_ref()
        .map(|e| binder.bind_predicate(e))
        .transpose()?;
    let (bound_where, post_where) =
        subquery::split_correlated(bound_where, n_params, &correlated);
    let post_filter = post_where.map(|e| compile_program(&e)).transpose()?;

    // #65: push each WHERE conjunct to the EARLIEST position where every slot
    // it reads is bound — the outer's own filter, or a join step's ON. A
    // comma-join (`FROM t1, t2 WHERE t1.a = t2.b`) is `INNER ON true`, so
    // without this the gather holds the full cartesian product before the
    // WHERE sees a single row (measured: 13.5 GB on 30-60-row select4
    // tables); with it, the same equality is an ON conjunct and therefore an
    // index nested-loop candidate. Placement is by max referenced slot, and
    // NULL-extension is the boundary the rewrite must respect:
    //   - any FULL step: no pushdown at all — BOTH sides NULL-extend, so
    //     every WHERE conjunct filters rows that do not exist yet;
    //   - a conjunct whose latest table is a LEFT step's inner stays in
    //     joined_filter — it filters the NULL-EXTENDED row, and inside the
    //     ON it would decide matching instead (a different query);
    //   - outer-only (and column-free) conjuncts go to the outer filter:
    //     LEFT steps preserve every outer row, so pre-filtering the outer
    //     removes exactly the groups the WHERE would have removed whole.
    // Policies still run before any pushed conjunct's position: the outer
    // merge keeps the single-table `merge_and(user, policy)` shape, and a
    // step's ON already runs after that step's policy (#46/#49 contract).
    let outer_width = outer.columns.len();
    let any_full = drafts.iter().any(|d| d.kind == JoinKind::Full);
    let mut outer_extra: Vec<BExpr> = Vec::new();
    let mut step_extra: Vec<Vec<BExpr>> = drafts.iter().map(|_| Vec::new()).collect();
    let mut remainder: Vec<BExpr> = Vec::new();
    if let Some(w) = bound_where {
        let mut conjuncts = Vec::new();
        split_and(w, &mut conjuncts);
        for c in conjuncts {
            let dest = if any_full {
                &mut remainder
            } else {
                match max_col(&c) {
                    None => &mut outer_extra,
                    Some(m) if (m as usize) < outer_width => &mut outer_extra,
                    Some(m) => {
                        let k = drafts
                            .iter()
                            .position(|d| (m as usize) < d.width_after)
                            .expect("bound slot exceeds the joined width");
                        if drafts[k].kind == JoinKind::Inner {
                            &mut step_extra[k]
                        } else {
                            &mut remainder
                        }
                    }
                }
            };
            dest.push(c);
        }
    }

    // The recursive CTE working table is FullScan-only (no PK, no indexes): keep
    // the whole predicate as the residual rather than extracting a keyed access
    // its empty PK would vacuously satisfy.
    let (access, outer_residual) = if outer_id == CTE_TABLE {
        (AccessPath::FullScan, merge_and(and_all(outer_extra), outer_policy))
    } else {
        extract_access(
            merge_and(and_all(outer_extra), outer_policy),
            outer,
            consts,
        )?
    };
    let filter = outer_residual.map(|e| compile_program(&e)).transpose()?;

    let mut joins: Vec<Join> = Vec::new();
    for (draft, extra) in drafts.into_iter().zip(step_extra) {
        // The original ON first, pushed conjuncts after — statement order.
        let on_src = merge_and(Some(draft.bound_on), and_all(extra)).expect("ON present");
        // #49: consume pure equality conjuncts into an inner access path —
        // the index nested loop. What remains of the ON runs as the residual
        // over the joined row, preserving every raise the query could observe.
        // A FULL join never takes the index nested loop: emitting the
        // UNMATCHED inner rows requires the inner side enumerated and held,
        // so the whole ON stays residual over the held scan.
        // A FULL join, or the recursive CTE working table (FullScan-only, no key
        // tree): the whole ON stays residual over the held scan.
        let (jaccess, residual_on) = if draft.kind == JoinKind::Full || draft.jid == CTE_TABLE {
            (AccessPath::FullScan, on_src)
        } else {
            extract_join_access(on_src, &draft.acc_types_before, draft.jt, consts)?
        };
        joins.push(Join {
            table: draft.jid,
            kind: draft.kind,
            access: jaccess,
            on: compile_program(&residual_on)?,
            policy: draft.policy,
        });
    }

    let joined_filter = and_all(remainder).map(|e| compile_program(&e)).transpose()?;

    let full_scope = Scope::joined_named(acc_named.clone())?;
    let namer = joined_namer(&acc_named);

    // An aggregate over a join groups the JOINED row. Nothing in the grouping
    // step is about tables — it is about the row it is handed — so the same
    // planner runs, given the joined row's columns and scope.
    let has_agg = s
        .items
        .as_ref()
        .is_some_and(|i| i.iter().any(|(e, _)| contains_agg(e)))
        || s.having.as_ref().is_some_and(contains_agg)
        || s.order_by.iter().any(|(e, _)| contains_agg(e))
        || !s.group_by.is_empty();
    if has_agg {
        // The correlated WHERE residual rides `post_filter` into the aggregate
        // plan (#73 §1); it is filled and applied per JOINED row before
        // accumulation, so aggregation still runs over the full
        // `(WHERE ∧ every policy)` set. `post_filter` is already threaded into
        // the non-aggregate SelectPlan below.
        let planned = plan_aggregate_select(
            s,
            &full_scope,
            outer_id,
            access,
            filter,
            joins,
            joined_filter,
            post_filter,
            binder,
            mode,
            consts,
            subplans,
        )?;
        // A correlated slot may be read ONLY by the WHERE (→ post_filter); one
        // in the projection/aggregate-arg/GROUP BY/HAVING over the grouped
        // tuple is refused (validate mirrors this, but the direct query path
        // runs without a decode round-trip).
        if let PlanStmt::Select(sp) = &planned.0 {
            reject_correlated_in_aggregate(sp, n_params, &correlated)?;
        }
        return Ok(planned);
    }

    // A window over a join runs the phase over the JOINED row — free here, since
    // the window planner works off whatever base scope the binder carries.
    let has_window = s
        .items
        .as_ref()
        .is_some_and(|i| i.iter().any(|(e, _)| contains_window(e)))
        || s.order_by.iter().any(|(e, _)| contains_window(e));
    if has_window {
        if has_agg {
            return Err(bind_err(
                "window functions together with GROUP BY / aggregates in one SELECT \
                 are not supported yet (window stage 2+)",
            ));
        }
        if post_filter.is_some() || correlated.iter().any(|&c| c) {
            return Err(bind_err(
                "a window function together with a correlated subquery is not supported yet",
            ));
        }
        return plan_window_select(
            s, outer_id, access, filter, joins, joined_filter, binder, subplans,
        );
    }

    // Projection over the joined tuple. `SELECT *` is every column of every
    // side, in join order — the same order the tuple is built in.
    let mut out_types: Vec<Option<ColumnType>> = Vec::new();
    let mut projection: Vec<Projection> = match &s.items {
        None => {
            out_types = full_scope.slot_types().into_iter().map(Some).collect();
            (0..binder.scope_width() as u16).map(Projection::Column).collect()
        }
        Some(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (item, alias) in items {
                let (b, ty) = binder.bind_expr(item)?;
                out_types.push(ty);
                out.push(match (b, alias) {
                    (BExpr::Col(i), None) => Projection::Column(i),
                    // An alias survives as the output name — same one-
                    // instruction indirection as the single-table path.
                    (other, alias) => {
                        let program = compile_program(&other)?;
                        let name = alias
                            .clone()
                            .unwrap_or_else(|| render_program(&program, &namer));
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
    if s.distinct {
        // Under DISTINCT the sort AND the LIMIT/OFFSET must follow the dedup,
        // and exec only applies skip/take post-dedup on the Projection route —
        // so the joined base row is never the tuple being ordered or bounded.
        // The single-table path has enforced this all along; the join path
        // silently skipped it, and LIMIT counted pre-dedup joined duplicates
        // (adversarial review find: `SELECT DISTINCT dept.dname … LIMIT 2`
        // returned one row where sqlite3/PG return two). junk = None: a sort
        // key that is not in the SELECT list is refused, because after the
        // dedup it is *which duplicate survived* that would decide the order.
        let (keys, n_junk) = distinct_order_by(s, &full_scope, None, binder.host_colls())?;
        order_by = keys;
        order_over = OrderOver::Projection;
        order_junk = n_junk.saturating_add(s.drop_trailing);
    } else if s.drop_trailing > 0 && s.order_by.is_empty() {
        // Projection-passthrough collapse left trailing columns with no
        // ORDER BY — still need the Projection route so order_junk applies.
        order_over = OrderOver::Projection;
        order_junk = s.drop_trailing;
    } else if !s.order_by.is_empty() {
        // The "base row" of a join IS the joined row, and it is built in full
        // before the sort — so sorting it is the same operation, just wider.
        let hcolls: Vec<String> = binder.host_colls().to_vec();
        let hcolls = &hcolls[..];
        let mut keys = Vec::with_capacity(s.order_by.len());
        let mut all_cols = true;
        for (e, desc) in &s.order_by {
            // Peel an explicit `COLLATE`; the inner expression must be a bare
            // joined-row column for this fast path, and the collation rides the
            // sort tuple.
            let (e, coll) = peel_order_collate(e, hcolls)?;
            let coll = coll.unwrap_or_else(|| {
                mpedb_types::OrderColl::Native(declared_collation(e, &binder.scope))
            });
            // Same PG rule as the single-table path: an output ALIAS wins
            // over an input column of the same name.
            if let ast::Expr::Col(n) = e {
                if s.items.as_ref().is_some_and(|items| {
                    items.iter().any(|it| it.1.as_deref() == Some(n.as_str()))
                }) {
                    all_cols = false;
                    break;
                }
            }
            match binder.bind_expr(e) {
                Ok((BExpr::Col(i), _)) => keys.push((i, *desc, coll)),
                _ => {
                    all_cols = false;
                    break;
                }
            }
        }
        if all_cols && s.drop_trailing == 0 {
            order_by = keys;
        } else {
            let (keys, n_junk) =
                join_order_by(s, &full_scope, &mut projection, &mut binder, &namer)?;
            order_by = keys;
            order_over = OrderOver::Projection;
            order_junk = n_junk.saturating_add(s.drop_trailing);
        }
    }

    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Select(SelectPlan {
            table: outer_id,
            access,
            joins,
            joined_filter,
            post_filter,
            filter,
            projection,
            order_by,
            order_over,
            order_junk,
            limit: s.limit,
            offset: s.offset,
            distinct: s.distinct,
            aggregate: None,
            windows: Vec::new(),
        }),
        param_types,
        context_keys,
        list_keys,
        out_types,
        subplans,
    ))
}

/// Resolve every `NATURAL JOIN` in `s` into the equivalent `JOIN … USING (…)`
/// by filling its `using` with the columns common to the two sides. The common
/// set is the intersection of the names VISIBLE on the left so far (the outer
/// table plus every already-joined table, deduped, in first-seen / left-to-right
/// column order) with the right table's own columns — which is the order sqlite
/// coalesces them in for `SELECT *`. A column that is common but appears in
/// SEVERAL left tables is NOT an error: `using_on_expr` equates the LEFTMOST
/// occurrence, exactly as sqlite coalesces it. NO common column ⇒ `using` stays
/// empty and the join keeps its `ON true`, i.e. a cross join (sqlite's rule).
///
/// Only reached when some join is `natural`; a plain USING/ON join is copied
/// through unchanged (its own columns still join the left-visible set).
fn desugar_natural_joins(s: &ast::SelectStmt, schema: &Schema) -> Result<ast::SelectStmt> {
    let outer_table = s.table.as_deref().expect("join on a FROM-less SELECT");
    let (_, outer) = resolve_table(schema, outer_table)?;
    // Names visible on the left, first-seen (left-to-right) order, deduped so a
    // column shared by two left tables becomes ONE USING entry.
    let mut left_cols: Vec<String> = Vec::new();
    let push_col = |cols: &mut Vec<String>, name: &str| {
        if !cols.iter().any(|n| n == name) {
            cols.push(name.to_string());
        }
    };
    for c in &outer.columns {
        push_col(&mut left_cols, &c.name);
    }
    let mut joins = Vec::with_capacity(s.joins.len());
    for jc in &s.joins {
        let (_, jt) = resolve_table(schema, &jc.table)?;
        let mut jc = jc.clone();
        if jc.natural {
            jc.using = left_cols
                .iter()
                .filter(|name| jt.column_index(name.as_str()).is_some())
                .cloned()
                .collect();
        }
        // The right table's columns become visible to any join further right.
        for c in &jt.columns {
            push_col(&mut left_cols, &c.name);
        }
        joins.push(jc);
    }
    Ok(ast::SelectStmt { joins, ..s.clone() })
}

/// Desugar `JOIN … USING (cols)` into the AST predicate
/// `left.c1 = right.c1 AND …`. `acc_named` is the accumulated join scope with
/// the RIGHT (just-joined) table as its LAST entry; the tables before it are the
/// left side. Each USING column must exist in the right table AND in some table
/// to its left, else a clean bind error. The left occurrence is the LEFTMOST
/// match — the coalesce representative — so `a JOIN b USING(x) JOIN c USING(x)`
/// pins `a.x = b.x AND a.x = c.x`, exactly sqlite's single coalesced column.
fn using_on_expr(using: &[String], acc_named: &[(String, &TableDef)]) -> Result<ast::Expr> {
    let (right_name, right) = acc_named.last().expect("the joined table is present");
    let left = &acc_named[..acc_named.len() - 1];
    let mut conds: Vec<ast::Expr> = Vec::with_capacity(using.len());
    for col in using {
        if right.column_index(col).is_none() {
            return Err(bind_err(format!(
                "USING column `{col}` does not exist in table `{right_name}`"
            )));
        }
        let left_name = left
            .iter()
            .find(|(_, t)| t.column_index(col).is_some())
            .map(|(n, _)| n.clone())
            .ok_or_else(|| {
                bind_err(format!(
                    "USING column `{col}` does not exist on the left side of the join"
                ))
            })?;
        conds.push(ast::Expr::Binary(
            ast::BinOp::Eq,
            Box::new(ast::Expr::Qualified(left_name, col.clone())),
            Box::new(ast::Expr::Qualified(right_name.clone(), col.clone())),
        ));
    }
    let mut it = conds.into_iter();
    let first = it.next().expect("USING has at least one column");
    Ok(it.fold(first, |acc, c| {
        ast::Expr::Binary(ast::BinOp::And, Box::new(acc), Box::new(c))
    }))
}

/// `SELECT *` over a `USING` join: expand the star into explicit qualified
/// column items so the join columns are COALESCED — each shows once, taken from
/// the LEFT side — matching sqlite. Every column is emitted in table order, but
/// a joined table's OWN USING columns are dropped (their value equals the equal
/// left column already emitted). Returns a clone of `s` with `items = Some(…)`;
/// only reached when `s.items` is `None` and some join carries a `using` list.
fn expand_using_star(s: &ast::SelectStmt, schema: &Schema) -> Result<ast::SelectStmt> {
    let outer_table = s.table.as_deref().expect("join on a FROM-less SELECT");
    let (_, outer) = resolve_table(schema, outer_table)?;
    let outer_name = s.alias.clone().unwrap_or_else(|| outer.name.clone());
    let mut items: Vec<(ast::Expr, Option<String>)> = Vec::new();
    // The outer table contributes every VISIBLE column (a hidden implicit rowid
    // is never in `SELECT *`, #94) — a USING column keeps its natural position
    // here and is the left side of the coalesce.
    for c in outer.visible_columns() {
        items.push((ast::Expr::Qualified(outer_name.clone(), c.name.clone()), None));
    }
    for jc in &s.joins {
        let (_, jt) = resolve_table(schema, &jc.table)?;
        let jname = jc.alias.clone().unwrap_or_else(|| jt.name.clone());
        for c in jt.visible_columns() {
            // Drop this table's USING columns: they equal the already-emitted
            // left column, so `SELECT *` shows the join column once.
            if jc.using.iter().any(|u| u == &c.name) {
                continue;
            }
            items.push((ast::Expr::Qualified(jname.clone(), c.name.clone()), None));
        }
    }
    Ok(ast::SelectStmt { items: Some(items), ..s.clone() })
}

/// `SELECT *` over a plain (non-USING) join that spans at least one
/// implicit-rowid table (#94): expand the star into explicit qualified items of
/// the VISIBLE columns, in table order. Returns `None` when no joined table has
/// a hidden rowid, so an all-explicit-PK join keeps the untouched `None` fast
/// path and its behavior is unchanged.
fn expand_join_star_visible(
    s: &ast::SelectStmt,
    schema: &Schema,
    cte: Option<CteRef<'_>>,
) -> Result<Option<ast::SelectStmt>> {
    let outer_table = s.table.as_deref().expect("join on a FROM-less SELECT");
    let (_, outer) = resolve_table_cte(schema, cte, outer_table)?;
    let outer_name = s.alias.clone().unwrap_or_else(|| outer.name.clone());
    let mut any_hidden = outer.implicit_rowid;
    let mut items: Vec<(ast::Expr, Option<String>)> = Vec::new();
    for c in outer.visible_columns() {
        items.push((ast::Expr::Qualified(outer_name.clone(), c.name.clone()), None));
    }
    for jc in &s.joins {
        let (_, jt) = resolve_table_cte(schema, cte, &jc.table)?;
        any_hidden |= jt.implicit_rowid;
        let jname = jc.alias.clone().unwrap_or_else(|| jc.table.clone());
        for c in jt.visible_columns() {
            items.push((ast::Expr::Qualified(jname.clone(), c.name.clone()), None));
        }
    }
    Ok(any_hidden.then(|| ast::SelectStmt { items: Some(items), ..s.clone() }))
}

/// Name a joined-tuple slot for EXPLAIN and output columns: `<table>.<column>`,
/// because `id` alone would be a lie about which side it came from.
fn joined_namer<'a>(
    names: &'a [(String, &'a TableDef)],
) -> impl Fn(u16) -> String + use<'a> {
    move |c: u16| {
        let mut off = 0usize;
        for (nm, t) in names {
            if (c as usize) < off + t.columns.len() {
                return format!("{}.{}", nm, t.columns[c as usize - off].name);
            }
            off += t.columns.len();
        }
        format!("col#{c}")
    }
}

/// `ORDER BY` for a join that needs sort-only columns.
fn join_order_by(
    s: &ast::SelectStmt,
    _joined: &Scope<'_>,
    projection: &mut Vec<Projection>,
    binder: &mut Binder<'_>,
    namer: &dyn Fn(u16) -> String,
) -> Result<(OrderKeys, u16)> {
    let items = s.items.as_ref();
    let hcolls: Vec<String> = binder.host_colls().to_vec();
    let hcolls = &hcolls[..];
    let mut keys = Vec::with_capacity(s.order_by.len());
    let mut n_junk = 0u16;
    for (i, (e, desc)) in s.order_by.iter().enumerate() {
        // Peel an explicit `COLLATE`; the inner expression drives resolution
        // (output position or sort-only column), the collation rides the sort.
        let (e, coll) = peel_order_collate(e, hcolls)?;
        let coll = coll.unwrap_or_else(|| {
            mpedb_types::OrderColl::Native(declared_collation(e, &binder.scope))
        });
        if let Some(items) = items {
            if let Some(pos) = ordinal(e, items.len())? {
                keys.push((pos, *desc, coll));
                continue;
            }
            if let Some(pos) = items.iter().position(|it| &it.0 == e) {
                keys.push((pos as u16, *desc, coll));
                continue;
            }
            // A bare identifier naming an item's ALIAS is that output
            // position (PG/sqlite resolve the output name first).
            if let ast::Expr::Col(n) = e {
                if let Some(pos) =
                    items.iter().position(|it| it.1.as_deref() == Some(n.as_str()))
                {
                    keys.push((pos as u16, *desc, coll));
                    continue;
                }
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
        let name = render_program(&program, &namer);
        projection.push(Projection::Expr { program, name });
        keys.push(((projection.len() - 1) as u16, *desc, coll));
        n_junk += 1;
    }
    Ok((keys, n_junk))
}

/// #49: decompose a join's bound ON into an inner-side access path
/// (parametrized by the outer row) plus the residual ON — the index
/// nested-loop pushdown. Only PURE equality conjuncts are consumed:
/// `inner.col = outer.col` (column types exactly equal, so the key encoding
/// is the stored one), `inner.col = $p`, `inner.col = <lit>`. Anything that
/// can raise stays in the residual — consuming it would erase observable
/// raise behaviour, the same contract that keeps policies ahead of ON.
///
/// Preference: full-PK equality (one `get` per outer row, no index tree) >
/// unique index (at most one row) > non-unique index > FullScan
/// (read-once-and-hold, exactly the pre-#49 execution).
fn extract_join_access(
    on: BExpr,
    outer_types: &[ColumnType],
    inner: &TableDef,
    consts: &mut Vec<Value>,
) -> Result<(AccessPath, BExpr)> {
    let ow = outer_types.len() as u16;
    let iw = inner.columns.len() as u16;
    let mut conjuncts = Vec::new();
    split_and(on, &mut conjuncts);
    let mut consumed = vec![false; conjuncts.len()];
    // Per inner column: the first conjunct that pins it, and with what part.
    let mut pins: Vec<Option<(usize, KeyPart)>> = vec![None; inner.columns.len()];
    for (ci, c) in conjuncts.iter().enumerate() {
        let BExpr::Binary(BinOp::Eq, l, r) = c else { continue };
        let (icol, part) = match (&**l, &**r) {
            (BExpr::Col(i), BExpr::Col(j)) if *i >= ow && *i < ow + iw && *j < ow => {
                let icol = (*i - ow) as usize;
                if outer_types[*j as usize] != inner.columns[icol].ty {
                    continue; // cross-type equality: encodings differ, stay residual
                }
                (icol, KeyPart::OuterCol(*j))
            }
            (BExpr::Col(j), BExpr::Col(i)) if *i >= ow && *i < ow + iw && *j < ow => {
                let icol = (*i - ow) as usize;
                if outer_types[*j as usize] != inner.columns[icol].ty {
                    continue;
                }
                (icol, KeyPart::OuterCol(*j))
            }
            (BExpr::Col(i), other) if *i >= ow && *i < ow + iw => {
                let icol = (*i - ow) as usize;
                match as_atom(other) {
                    // A NULL or cross-type constant would fail plan validation
                    // (and `= NULL` is UNKNOWN anyway) — leave it residual.
                    Some(Atom::Const(v))
                        if v.is_null() || !v.fits(inner.columns[icol].ty) =>
                    {
                        continue
                    }
                    Some(a) => (icol, a.to_key_part(consts)?),
                    None => continue,
                }
            }
            (other, BExpr::Col(i)) if *i >= ow && *i < ow + iw => {
                let icol = (*i - ow) as usize;
                match as_atom(other) {
                    Some(Atom::Const(v))
                        if v.is_null() || !v.fits(inner.columns[icol].ty) =>
                    {
                        continue
                    }
                    Some(a) => (icol, a.to_key_part(consts)?),
                    None => continue,
                }
            }
            _ => continue,
        };
        // Belt and suspenders: an `any` column can never be pinned. The
        // schema already refuses `any` in every key (PK/UNIQUE/indexed), so
        // no access path could use the pin — but the probe semantics would be
        // encoding-equality rather than sql_cmp, and that must never leak in
        // through a future schema change.
        if pins[icol].is_none() && inner.columns[icol].ty != ColumnType::Any {
            pins[icol] = Some((ci, part));
        }
    }

    // Full PK pinned → PkPoint. Otherwise a UNIQUE index beats a non-unique
    // one; within each pass, declaration order picks.
    let mut chosen: Option<AccessPath> = None;
    let pk_pins: Option<Vec<(usize, KeyPart)>> = inner
        .primary_key
        .iter()
        .map(|&pc| pins[pc as usize])
        .collect();
    if let Some(pp) = pk_pins {
        for (ci, _) in &pp {
            consumed[*ci] = true;
        }
        chosen = Some(AccessPath::PkPoint(pp.into_iter().map(|(_, p)| p).collect()));
    } else {
        // #55: an index qualifies when its FULL column prefix is pinned by
        // the join equalities (k = 1 is the single-column case). Full-width
        // unique first (at most one row per outer), then longest prefix.
        let mut best: Option<(usize, Vec<usize>, Vec<KeyPart>, bool)> = None;
        for (pos, ix) in inner.indexes.iter().enumerate() {
            if pos >= 63 {
                break;
            }
            // A PARTIAL index is never the inner probe of a nested loop.
            // The evidence a §5.5 implication test would need is the OUTER
            // statement's WHERE restricted to this table, which this function
            // does not see — it is handed only the ON equalities. Declining
            // costs an inner full scan; guessing costs rows that exist.
            // (design/DESIGN-WORKLOAD-INDEXES.md §5.5; the same refusal
            // `agg_index_choice` makes.)
            if ix.predicate.is_some() {
                continue;
            }
            let mut cis = Vec::new();
            let mut parts = Vec::new();
            for &col in &ix.columns {
                match pins[col as usize] {
                    Some((ci, part)) => {
                        cis.push(ci);
                        parts.push(part);
                    }
                    None => break,
                }
            }
            // A prefix probe of a COMPOSITE index skips every row whose
            // uncovered index columns are NULL, because those rows have no
            // entry at all (membership = "no indexed column is NULL"). Sound
            // only when the uncovered suffix is declared NOT NULL — the same
            // rule `extract_access` and `plan::agg_servable_by_index` take,
            // and the same measured wrong answer (`INDEX (a, b)` probed by
            // `a = o.k` lost every row with a NULL `b`).
            if parts.is_empty()
                || !ix.columns[parts.len()..]
                    .iter()
                    .all(|&c| !inner.columns[c as usize].nullable)
            {
                continue;
            }
            let full_unique = ix.unique && parts.len() == ix.columns.len();
            let better = match &best {
                None => true,
                Some((_, _, bparts, bfull)) => {
                    (full_unique && !bfull)
                        || (full_unique == *bfull && parts.len() > bparts.len())
                }
            };
            if better {
                best = Some((pos, cis, parts, full_unique));
            }
        }
        if let Some((pos, cis, parts, _)) = best {
            for ci in cis {
                consumed[ci] = true;
            }
            chosen = Some(AccessPath::IndexPoint {
                index_no: (pos + 1) as u32,
                parts,
            });
        }
    }
    let access = chosen.unwrap_or(AccessPath::FullScan);
    // An empty residual is constant TRUE — the equality was the whole ON.
    let residual =
        rebuild_residual(conjuncts, &consumed).unwrap_or(BExpr::Const(Value::Bool(true)));
    Ok((access, residual))
}
