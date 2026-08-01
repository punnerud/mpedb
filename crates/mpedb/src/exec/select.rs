use super::*;

/// Execute one statement plan against `ctx`. `params` are validated first
/// (count, then declared types; NULL always passes the type check —
/// nullability is enforced by the engine where it matters).
///
/// `partial` is an out-flag for statement-level atomicity: when the returned
/// value is an `Err`, `*partial == true` means the failed statement may
/// already have applied some of its effects to `ctx` (e.g. a multi-row
/// INSERT that violated a constraint on its third row inserted the first
/// two). Callers that keep the transaction alive across statement failures
/// ([`crate::WriteSession`]) must then poison it; the autocommit path aborts
/// the whole transaction on any error and can ignore the flag. The flag is
/// never set spuriously *false* (never under-reports), but it may be
/// conservatively *true* for failures whose partial effects cannot be ruled
/// out.
pub(crate) fn exec_stmt(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
) -> Result<ExecResult> {
    // Read paths and any caller that cannot fire triggers use the trigger-free
    // set — one empty-map lookup per written row, no allocation.
    exec_stmt_triggered(ctx, schema, plan, params, partial, &WriteRules::empty(), 0)
}

/// Maximum depth of the trigger cascade (DESIGN-TRIGGERS §4.4). Each level is a
/// full statement execution, so this is deliberately conservative — far below
/// sqlite's 1000. Exceeding it aborts the whole statement.
pub(crate) const MAX_TRIGGER_DEPTH: u32 = 32;

/// Like [`exec_stmt`], but with the trigger set to fire from (and the current
/// cascade `depth`). The write paths pass the leader's/session's gen-gated
/// [`WriteRules`]; a trigger body re-enters here with `depth + 1` on the SAME
/// `ctx`, never through the facade (DESIGN-TRIGGERS §4.3).
pub(crate) fn exec_stmt_triggered(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
    triggers: &WriteRules,
    depth: u32,
) -> Result<ExecResult> {
    // #40 instrument: statement-total time, so resolve + stmt reconciles
    // against execute()'s wall clock and nothing hides between the phases.
    #[cfg(feature = "leakstat")]
    {
        let t0 = mpedb_core::Instant::now();
        let r = exec_stmt_impl(ctx, schema, plan, params, partial, triggers, depth);
        mpedb_core::engine::leakstat::add(
            &mpedb_core::engine::leakstat::EXEC_NS_STMT,
            t0.elapsed().as_nanos() as u64,
        );
        r
    }
    #[cfg(not(feature = "leakstat"))]
    exec_stmt_impl(ctx, schema, plan, params, partial, triggers, depth)
}

fn exec_stmt_impl(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
    triggers: &WriteRules,
    depth: u32,
) -> Result<ExecResult> {
    let coerced = coerce_params(plan, params)?;
    let params: &[Value] = &coerced;
    // Uncorrelated subplans evaluate ONCE per execute, into their reserved
    // slots — before dispatch, so a PK probe built on `id = (SELECT max…)`
    // resolves like any other param. Correlated ones wait for their row.
    let filled;
    let params: &[Value] = if plan.subplans.iter().any(|s| s.outer_args.is_empty()) {
        let base = plan.subplan_base() as usize;
        let n_user = base;
        let mut buf = params.to_vec();
        for (i, sub) in plan.subplans.iter().enumerate() {
            if !sub.outer_args.is_empty() {
                continue;
            }
            // `run_subplan` fills this subplan's OWN uncorrelated nested lifts
            // (#73 §3) before running it — the recursion the flat two levels
            // became.
            let inner = run_subplan(ctx, schema, plan, &buf[..n_user], sub)?;
            buf[base + i] = subplan_value(inner, sub.kind)?;
        }
        filled = buf;
        &filled
    } else {
        params
    };
    match &plan.stmt {
        PlanStmt::Select(sp) => exec_select_top(ctx, schema, plan, params, sp),
        PlanStmt::Compound(c) => exec_compound(ctx, schema, plan, params, c),
        PlanStmt::RecursiveCte(rc) => recursive::exec_recursive_cte(ctx, schema, plan, params, rc),
        PlanStmt::Derived(dp) => recursive::exec_derived(ctx, schema, plan, params, dp),
        _other => exec_stmt_rest(ctx, schema, plan, params, partial, triggers, depth),
    }
}


/// The rows a DML statement's WHERE keeps, with the CORRELATED half applied.
///
/// `filter` (the uncorrelated conjuncts) is already pushed into the gather;
/// what is left reads a subplan slot that only exists once a row is in hand.
/// This is the SELECT path's per-row fill (`correlated_survivors`) applied to
/// the rows a DELETE/UPDATE has collected — the same function, so the two can
/// never disagree about when a slot is filled or what it is filled from.
pub(super) fn dml_survivors(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    rows: Vec<Vec<Value>>,
    post_filter: Option<&ExprProgram>,
) -> Result<Vec<Vec<Value>>> {
    let correlated: Vec<(usize, &SubPlan)> = plan
        .subplans
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.outer_args.is_empty())
        .collect();
    if correlated.is_empty() && post_filter.is_none() {
        return Ok(rows);
    }
    let base = plan.subplan_base() as usize;
    Ok(
        correlated_survivors(ctx, schema, plan, params, base, rows, &correlated, post_filter)?
            .into_iter()
            .map(|(row, _)| row)
            .collect(),
    )
}

/// A subquery's rows, reduced to the VALUE its reserved slot carries.
pub(super) fn subplan_value(r: ExecResult, kind: mpedb_sql::SubPlanKind) -> Result<Value> {
    use mpedb_sql::SubPlanKind as K;
    let ExecResult::Rows { rows, .. } = r else {
        return Err(internal("subplan produced no row set"));
    };
    match kind {
        K::Exists => return Ok(Value::Bool(!rows.is_empty())),
        K::List => {
            // `x IN (SELECT …)`: every value of the single output column,
            // order-irrelevant (membership). Bounded so a runaway subquery
            // cannot balloon one param slot unobserved.
            if rows.len() > 1_000_000 {
                return Err(Error::Unsupported(format!(
                    "an IN subquery returned {} rows — the membership list is \
                     capped at 1,000,000",
                    rows.len()
                )));
            }
            let mut items = Vec::with_capacity(rows.len());
            for mut r in rows {
                match (r.pop(), r.is_empty()) {
                    (Some(v), true) => items.push(v),
                    _ => return Err(internal("IN subplan output arity")),
                }
            }
            return Ok(Value::List(items));
        }
        K::Scalar => {}
    }
    match rows.len() {
        0 => Ok(Value::Null),
        1 => rows
            .into_iter()
            .next()
            .and_then(|mut r| if r.len() == 1 { r.pop() } else { None })
            .ok_or_else(|| internal("scalar subplan output arity")),
        // sqlite silently takes the first row; saying so is the strict line.
        // (The planner caps a scalar subplan at 2 rows — enough to detect this —
        // so `n` is the capped count, i.e. "at least 2", not the true total.)
        _ => Err(Error::Unsupported(
            "a scalar subquery returned more than one row — it must return at most one".into(),
        )),
    }
}

/// Run one subplan, first filling its OWN nested lifts (#73 §3).
///
/// `base_params` is `[user ‖ this subplan's correlation args]` — of length
/// `sub.sub_base` — so a plain leaf subplan (no nested lifts) runs exactly as
/// before. A leaf subplan's body may be a plain SELECT or a whole compound
/// (#56/format 31), run through [`exec_subbody`]. When `sub` HAS nested lifts
/// (only a SELECT body ever does):
///
/// - UNCORRELATED children depend only on `base_params`, so each is evaluated
///   ONCE here, bottom-up, into `[.. ‖ children results]` at `sub_base + i`,
///   before the select body's own gather.
/// - CORRELATED children (stage 2: correlated to THIS subplan's row) are NOT
///   filled here — they are filled PER ROW of the select body by
///   [`exec_select_leveled`], the same machinery the top level uses for its own
///   correlated subplans, plus the body's `post_filter` when the correlated
///   child feeds `sub`'s WHERE.
///
/// This generalizes the flat two-level fill (`exec_stmt_impl` once + top per-row)
/// into a recursion that bottoms out at the leaf case.
pub(super) fn run_subplan(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    base_params: &[Value],
    sub: &SubPlan,
) -> Result<ExecResult> {
    // A leaf subplan (no nested lifts) runs its body directly — a plain SELECT or
    // a whole compound (#56/format 31). A compound body is always a leaf.
    if sub.subplans.is_empty() {
        return exec_subbody(ctx, schema, plan, base_params, &sub.body);
    }
    // With nested lifts the body is guaranteed a plain SELECT (a compound body
    // never carries children — validate/planner enforce it).
    let Some(sp) = sub.body.as_select() else {
        return Err(internal("compound subplan body with nested lifts"));
    };
    let base = sub.sub_base as usize;
    let mut buf = base_params.to_vec();
    buf.resize(base + sub.subplans.len(), Value::Null);
    for (i, child) in sub.subplans.iter().enumerate() {
        // Only the uncorrelated children fill once here (into `sub_base + i`); a
        // correlated child correlates to `sp`'s row and is filled per row below.
        // `base_params` (== `buf[..base]`) is the `[user ‖ correlation]` prefix
        // each uncorrelated child inherits.
        if child.outer_args.is_empty() {
            let r = run_subplan(ctx, schema, plan, base_params, child)?;
            buf[base + i] = subplan_value(r, child.kind)?;
        }
    }
    exec_select_leveled(ctx, schema, plan, &buf, sp, base, &sub.subplans)
}

/// Execute a lifted subquery's body — a plain `SELECT` or a whole compound
/// `SELECT … UNION/… …` (#56/format 31) — into the row set its consumer
/// (`subplan_value`) reduces to a value / list / existence.
fn exec_subbody(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    body: &SubBody,
) -> Result<ExecResult> {
    match body {
        SubBody::Select(sp) => exec_select(ctx, schema, plan, params, sp),
        SubBody::Compound(c) => exec_compound(ctx, schema, plan, params, c),
        // Format 65: the subquery's FROM was a non-flattenable derived table.
        SubBody::Derived(dp) => recursive::exec_derived(ctx, schema, plan, params, dp),
    }
}

/// The top-level SELECT: routes to the leveled executor with the statement's
/// own lifts (result slots at `subplan_base + i`). See [`exec_select_leveled`].
fn exec_select_top(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<ExecResult> {
    exec_select_leveled(
        ctx,
        schema,
        plan,
        params,
        sp,
        plan.subplan_base() as usize,
        &plan.subplans,
    )
}

/// Execute one SELECT whose CORRELATED subplans (and any `post_filter`) are
/// handled PER ROW. `subplans` is this level's lift list, with result slots at
/// `base + i` in `params` — every UNCORRELATED slot already filled by the
/// caller. A correlated subplan is the ONLY place its result slot is filled:
/// per row, after the gather (and therefore after every policy) has produced
/// the row.
///
/// Shared by the top level (`base = subplan_base`, `subplans = plan.subplans`)
/// and — via [`run_subplan`] — each NESTED subplan (`base = sub.sub_base`,
/// `subplans = sub.subplans`). That is the recursion #73 §3 stage 2 turns the
/// two hardcoded levels into: a nested subquery correlated to its immediate
/// parent is filled per parent row here, exactly as the top level fills its
/// correlated subplans per outer row. Compound arms and leaf subplans instead
/// go through the plain [`exec_select`], which never fills slots.
pub(super) fn exec_select_leveled(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
    base: usize,
    subplans: &[SubPlan],
) -> Result<ExecResult> {
    let correlated: Vec<(usize, &SubPlan)> = subplans
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.outer_args.is_empty())
        .collect();
    if correlated.is_empty() && sp.post_filter.is_none() {
        return exec_select(ctx, schema, plan, params, sp);
    }
    // #73 §1: an aggregate over a correlated filter. The aggregate path consumes
    // rows in its gather, so the per-row correlated pre-filter must run BETWEEN
    // the gather and the grouping — `exec_aggregate` takes the correlated
    // subplans and the post-filter and runs the shared `correlated_survivors`
    // there. Everything downstream (empty-group zero row, HAVING, ORDER BY,
    // LIMIT-bounds-groups) is unchanged.
    if sp.aggregate.is_some() {
        return run_aggregate(
            ctx, schema, plan, params, sp, base, &correlated, sp.post_filter.as_ref(),
        );
    }
    exec_select_with(ctx, schema, plan, params, sp, base, &correlated)
}

/// Combine already-projected rows under one set operator, left-associatively.
/// `UNION`/`EXCEPT`/`INTERSECT` are SET ops: the result is deduplicated (and
/// NULLs count as equal — the set-op rule, same as DISTINCT); only
/// `UNION ALL` keeps duplicates. Keys are the storage-class GROUP encoding, for
/// the same reason DISTINCT uses it: Value is neither Hash nor Ord, the
/// encoding is total even across types, and set membership is decided by
/// sqlite's comparison — `SELECT 1 UNION SELECT 1.0` is one row.
fn apply_set_op(acc: Vec<Vec<Value>>, right: Vec<Vec<Value>>, op: SetOp) -> Vec<Vec<Value>> {
    use std::collections::HashSet;
    let dedup = |rows: Vec<Vec<Value>>| {
        let mut seen = HashSet::new();
        rows.into_iter()
            .filter(|r| seen.insert(keycode::encode_group_key(r, &[])))
            .collect::<Vec<_>>()
    };
    match op {
        SetOp::UnionAll => {
            let mut acc = acc;
            acc.extend(right);
            acc
        }
        SetOp::Union => {
            let mut acc = acc;
            acc.extend(right);
            dedup(acc)
        }
        SetOp::Except | SetOp::Intersect => {
            let rset: std::collections::HashSet<Vec<u8>> =
                right.iter().map(|r| keycode::encode_group_key(r, &[])).collect();
            let keep_present = matches!(op, SetOp::Intersect);
            dedup(acc)
                .into_iter()
                .filter(|r| rset.contains(&keycode::encode_group_key(r, &[])) == keep_present)
                .collect()
        }
    }
}

/// Execute compound ARM `k`, with the lifts that arm OWNS (format 56) filled
/// the way every other level fills its own: the UNCORRELATED ones once, up
/// front, and the CORRELATED ones per ARM row by [`exec_select_leveled`].
///
/// This is `exec_stmt_impl`'s discipline and `exec_derived`'s, applied to an
/// arm — the ownership move, not a new mechanism. An arm's correlated lift
/// names the ARM's row, which is the only row it CAN name: a compound has no
/// outer row of its own, which is exactly why hoisting these onto the statement
/// could never fill them.
///
/// The buffer is rebuilt from `params` for every arm, so another arm's reserved
/// slots are NULL rather than stale here — a forged cross-arm read (which
/// `validate_compound` rejects) can then only see NULL, never another row's
/// correlated value.
fn exec_compound_arm(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    c: &CompoundPlan,
    k: usize,
) -> Result<ExecResult> {
    let arm = c.arms.get(k).ok_or_else(|| internal("compound arm out of range"))?;
    match arm {
        mpedb_sql::CompoundArm::Derived(dp) => {
            // Nested derived as a compound arm: materialise body, scan outer.
            // Body slots live at dp.body_sub_base in the shared param buffer.
            recursive::exec_derived(ctx, schema, plan, params, dp)
        }
        mpedb_sql::CompoundArm::Select(sp) => {
            let lifts = c.arm_lifts(k);
            if lifts.is_empty() && c.n_arm_slots() == 0 {
                return exec_select(ctx, schema, plan, params, sp);
            }
            let base = c.arm_base(k) as usize;
            let mut buf = params.to_vec();
            buf.resize(
                buf.len()
                    .max(c.arm_sub_base as usize + c.n_arm_slots() as usize)
                    .max(params.len()),
                Value::Null,
            );
            for (i, sub) in lifts.iter().enumerate() {
                if !sub.outer_args.is_empty() {
                    continue;
                }
                let inner = run_subplan(ctx, schema, plan, &buf[..base], sub)?;
                buf[base + i] = subplan_value(inner, sub.kind)?;
            }
            exec_select_leveled(ctx, schema, plan, &buf, sp, base, lifts)
        }
    }
}

pub(super) fn exec_compound(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    c: &CompoundPlan,
) -> Result<ExecResult> {
    // Arms carry no ORDER BY/LIMIT of their own (validate enforces it), so
    // each arm materializes exactly its projected rows. The FIRST arm names
    // the output — sqlite's and PG's rule.
    if c.arms.is_empty() {
        return Err(internal("compound with no arms"));
    }
    let ExecResult::Rows { columns, rows } = exec_compound_arm(ctx, schema, plan, params, c, 0)?
    else {
        return Err(internal("compound arm produced no rows"));
    };
    let mut acc = rows;
    for (k, op) in c.ops.iter().enumerate() {
        let ExecResult::Rows { rows, .. } = exec_compound_arm(ctx, schema, plan, params, c, k + 1)?
        else {
            return Err(internal("compound arm produced no rows"));
        };
        acc = apply_set_op(acc, rows, *op);
    }
    if !c.order_by.is_empty() {
        gather::check_order_colls(&c.order_by, ctx.host_colls())?;
        sort_rows(&mut acc, &c.order_by, ctx.host_colls());
    }
    let (l, o) = resolve_limit_offset(c.limit, c.offset, params)?;
    let skip = o.min(usize::MAX as u64) as usize;
    let take = l.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    if skip > 0 || take != usize::MAX {
        acc = acc.into_iter().skip(skip).take(take).collect();
    }
    Ok(ExecResult::Rows { columns, rows: acc })
}

/// **What the statement must PRODUCE, turned into a bound on what the pipeline
/// carries** (#125).
///
/// The row pipeline for one SELECT is `[table0 ‖ table1 ‖ …]`, and every column
/// of every table in it is materialized today whether or not anything
/// downstream can see it — `SELECT count(*)` over a join holds the entire
/// product to produce one integer. [`mpedb_sql::row_prune`] computes the slots
/// a later stage can observe; `None` means every slot is observed and the
/// executor's paths stay byte-for-byte what they were.
///
/// Two base-row reads go through no expression and so are passed in
/// explicitly: the outer table's PRIMARY KEY (sqlite's bare-column witness
/// picks a group's lowest-rowid row by reading it) and each correlated
/// subplan's `outer_args` (filled per row by [`correlated_survivors`]).
fn select_prune(
    schema: &Schema,
    plan: &CompiledPlan,
    sp: &SelectPlan,
    correlated: &[(usize, &SubPlan)],
) -> Result<Option<RowPrune>> {
    let t = table_def(schema, plan, sp.table)?;
    // One width per stage of the pipeline: the outer table, then each join's.
    let mut widths = Vec::with_capacity(sp.joins.len() + 1);
    widths.push(t.columns.len());
    for j in &sp.joins {
        widths.push(table_def(schema, plan, j.table)?.columns.len());
    }
    let mut args: Vec<u16> = Vec::new();
    for (_, s) in correlated {
        args.extend_from_slice(&s.outer_args);
    }
    Ok(mpedb_sql::row_prune(sp, &widths, &t.primary_key, &args))
}

/// One SELECT — shared verbatim between a top-level SELECT and each compound
/// arm, so the two can never drift.
pub(super) fn exec_select(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<ExecResult> {
    // Window functions are their own phase: materialize the base rows, compute
    // each window, project over the extended rows, then sort/trim/bound. Kept in
    // its own function so this executor's other paths stay untouched.
    //
    // An AGGREGATE plan that also carries windows runs the phase over the
    // GROUPED tuple instead, inside `exec_aggregate` — the rows this path would
    // hand the window phase are the ungrouped ones, and projecting them would
    // answer the wrong question rather than fail.
    if !sp.windows.is_empty() && sp.aggregate.is_none() {
        return window::exec_select_windowed(ctx, schema, plan, params, sp);
    }
    // Micro-executor: single-table PK point probe with column-only projection.
    // The generic path materializes gather → project → sort/limit machinery;
    // for `SELECT cols FROM t WHERE pk = $1` that is pure overhead (~half of
    // prepare+bind SELECT wall time). Same result as the general path.
    if let Some(out) = try_exec_pk_point_hot(ctx, schema, plan, params, sp)? {
        return Ok(out);
    }
    let SelectPlan {
        table,
        access,
        joins,
        joined_filter,
        // Only the TOP-level statement routes post-filter/correlated work
        // (to `exec_select_with`); arms and subplans never carry one — the
        // planner cannot produce it there and validate refuses it.
        post_filter: _,
        filter,
        projection,
        order_by,
        limit,
        offset,
        aggregate,
        distinct,
        order_over,
        order_junk,
        windows: _,
    } = sp;
    {
        {
            // DISTINCT makes LIMIT bound DISTINCT rows, so the scan bound (and
            // the top-K path, which is the same bound wearing a hat) must not
            // apply — the same trap the aggregate path has. Forcing it to None
            // here keeps that in one place rather than at each use.
            // The scan bound only applies when the sort (and the dedup, if any)
            // happen on the base row — otherwise LIMIT bounds a tuple further
            // down the pipeline and cutting the scan short would drop input
            // that later stages still need.
            let (limit_val, offset_val) = resolve_limit_offset(*limit, *offset, params)?;
            let skip_take_bound = || {
                // A join is gathered whole (the LIMIT bounds joined rows, not
                // outer rows), and any sort below the base row moves the bound
                // down the pipeline too.
                if !joins.is_empty() || *order_over != OrderOver::BaseRow {
                    return None;
                }
                limit_val.map(|l| {
                    let l = l.min(usize::MAX as u64) as usize;
                    let o = offset_val.min(usize::MAX as u64) as usize;
                    l.saturating_add(o)
                })
            };
            // Exact kNN (stage D, design/DESIGN-MPEE-GENERAL.md §3): `ORDER BY
            // vec_l2(col, $q) LIMIT k` over one table selects under a k-sized
            // heap with per-dimension early abandonment, instead of computing
            // every full distance and sorting every row.
            if let Some(out) = try_exec_knn(ctx, schema, plan, params, sp)? {
                return Ok(out);
            }
            if aggregate.is_some() {
                // Plain aggregate: no correlated subplans and no post-filter.
                // This function is the fill-free LEAF — every level that owns
                // correlated lifts (the statement, a derived body, a compound
                // ARM, a nested subplan) routes to `run_aggregate` from
                // `exec_select_leveled` with ITS own base. `base` is unused with
                // an empty correlated set.
                return run_aggregate(
                    ctx, schema, plan, params, sp, plan.subplan_base() as usize, &[], None,
                );
            }
            let rows = if !joins.is_empty() {
                // A join materializes: the sort, the dedup and the LIMIT all
                // apply to JOINED rows, so none of them can bound the scan.
                // #125: the join's product is the biggest thing this path
                // holds, and the projection above it is usually a handful of
                // columns. Computed only for a join — a single-table read
                // materializes exactly one row set and pruning it would rebuild
                // every row to save a slot the projection was about to read.
                let prune = select_prune(schema, plan, sp, &[])?;
                let mut r = gather_joined(
                    ctx,
                    plan,
                    params,
                    schema,
                    *table,
                    access,
                    filter.as_ref(),
                    joins,
                    joined_filter.as_ref(),
                    prune.as_ref(),
                )?;
                // `OrderOver::BaseRow` means "the tuple the scan produced", and
                // for a join that tuple IS the joined row — so the sort belongs
                // HERE, before the projection narrows it. Sorting the projected
                // rows instead would index the wrong tuple.
                if *order_over == OrderOver::BaseRow && !order_by.is_empty() {
                    gather::check_order_colls(order_by, ctx.host_colls())?;
                    sort_rows(&mut r, order_by, ctx.host_colls());
                }
                r
            } else if *order_over != OrderOver::BaseRow {
                // The sort indexes a tuple further down (the projection), so the
                // base rows are left unsorted and unbounded here.
                gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?
            } else if order_by.is_empty() {
                // No surviving sort (the planner elides ORDER BY that matches
                // PK scan order): stream and stop after offset+limit rows.
                gather_rows(ctx, *table, access, filter.as_ref(), plan, params, skip_take_bound())?
            } else if let Some(keep) = skip_take_bound() {
                // ORDER BY … LIMIT: bounded top-K, O(offset+limit) memory
                // instead of materializing every match (already sorted).
                gather_topk(ctx, *table, access, filter.as_ref(), plan, params, order_by, keep)?
            } else {
                // ORDER BY with no LIMIT: must materialize and sort in full.
                let mut r = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
                gather::check_order_colls(order_by, ctx.host_colls())?;
                sort_rows(&mut r, order_by, ctx.host_colls());
                r
            };
            let skip = offset_val.min(usize::MAX as u64) as usize;
            let take = limit_val.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
            // Without DISTINCT, skip/take applies to base rows and there is no
            // reason to project the ones being skipped. With it, the projection
            // is what gets deduplicated, so it must happen first and skip/take
            // moves to the end.
            let (row_skip, row_take) = if *order_over == OrderOver::BaseRow {
                (skip, take)
            } else {
                (0, usize::MAX)
            };
            let mut out = Vec::new();
            let mut seen = std::collections::HashSet::new();
            // Per-output-column collation for DISTINCT: a NOCASE/RTRIM column
            // deduplicates case-/space-insensitively (`SELECT DISTINCT name`),
            // sqlite parity. Only built when DISTINCT (else unused).
            let distinct_colls = if *distinct {
                output_collations(schema, plan, *table, joins, projection)
            } else {
                Vec::new()
            };
            for row in rows.into_iter().skip(row_skip).take(row_take) {
                let mut orow = Vec::with_capacity(projection.len());
                for p in projection {
                    orow.push(match p {
                        Projection::Column(i) => row
                            .get(*i as usize)
                            .cloned()
                            .ok_or_else(|| internal("projection column"))?,
                        Projection::Expr { program, name, .. } => {
                            program
                                .eval_host(&row, params, ctx.host_fns())
                                .map_err(|e| name_decode_error(e, name))?
                        }
                    });
                }
                // Keying on the storage-class GROUP encoding rather than on
                // Value: DISTINCT must treat NULLs as equal to each other
                // (unlike `=`), which is exactly what the key encoding does,
                // and Value is neither Hash nor Ord. It must ALSO treat `1` and
                // `1.0` as one value (sqlite's DISTINCT asks its comparison,
                // and the on-disk encoder answers by mpedb type — 3 values
                // where sqlite sees 2). Text keys are folded under the output
                // column's declared collation.
                if *distinct
                    && !seen.insert(keycode::encode_group_key(&orow, &distinct_colls))
                {
                    continue;
                }
                out.push(orow);
            }
            if *order_over != OrderOver::BaseRow {
                gather::check_order_colls(order_by, ctx.host_colls())?;
                gather::check_order_colls(order_by, ctx.host_colls())?;
        sort_rows(&mut out, order_by, ctx.host_colls());
                // Sort-only columns come off AFTER the sort and before the
                // caller sees anything. They are always trailing, so the trim
                // is a truncate — and it must reach `columns` below too, or the
                // header would name a column the rows no longer carry.
                if *order_junk > 0 {
                    let keep = projection.len() - *order_junk as usize;
                    for row in &mut out {
                        row.truncate(keep);
                    }
                }
                out = out.into_iter().skip(skip).take(take).collect();
            }
            let columns = select_output_columns(schema, plan, sp)?;
            Ok(ExecResult::Rows { columns, rows: out })
        }
    }
}

/// Exact kNN under a bounded heap with early abandonment — `Some` when the
/// plan is `SELECT … FROM t [WHERE …] ORDER BY vec_l2(col, <query>) LIMIT k`
/// (single table, single ascending sort key, the key a projection expression
/// of exactly that shape), `None` otherwise.
///
/// The abandonment is the monotone-bound argument of DESIGN-MPEE-GENERAL §3
/// made concrete: squared-difference terms are non-negative, so the partial
/// sum is a lower bound on the full distance, and a candidate is dropped the
/// moment its partial sum exceeds the current k-th best. **Exactness and
/// errors are both preserved:** the SHAPE of every row's blob is validated
/// before any summing (a malformed embedding raises exactly as the generic
/// projection would), only the arithmetic is skipped — and the skipped
/// arithmetic could only have grown the sum further. Ordering matches the
/// generic path's stable sort bit-exactly: candidates compare by
/// `(distance², scan order)`, and `sqrt` is monotone, so the selected set and
/// its order are the ones the full sort would produce.
///
/// NULL keys (a NULL embedding or a NULL query) sort BEFORE every real
/// distance — ascending storage-class order, sqlite's rule — and are kept in
/// scan order, exactly as `sort_rows` would place them.
fn try_exec_knn(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<Option<ExecResult>> {
    use mpedb_types::{Instr, ScalarFn};
    let SelectPlan {
        table,
        access,
        joins,
        filter,
        projection,
        order_by,
        limit,
        offset,
        distinct,
        order_over,
        order_junk,
        ..
    } = sp;

    // Shape gates — anything unproven falls back to the generic path, which
    // is the semantics of record.
    if !joins.is_empty()
        || *distinct
        || *order_over == OrderOver::BaseRow
        || order_by.len() != 1
        || *table == mpedb_sql::DUAL_TABLE
        || *table == mpedb_sql::CTE_TABLE
    {
        return Ok(None);
    }
    let (key_col, dir, coll) = (&order_by[0].0, order_by[0].1, &order_by[0].2);
    if dir != SortDir::ASC || !matches!(coll, OrderColl::Native(_)) {
        return Ok(None);
    }
    let Some(limit) = limit else { return Ok(None) };
    let Some(Projection::Expr { program, .. }) = projection.get(*key_col as usize) else {
        return Ok(None);
    };
    // vec_l2 is symmetric, so both argument orders qualify.
    let (emb_col, query) = match program.instrs.as_slice() {
        [Instr::PushCol(c), Instr::PushParam(p), Instr::Call(ScalarFn::VecL2, 2)] => {
            (*c, params.get(*p as usize))
        }
        [Instr::PushParam(p), Instr::PushCol(c), Instr::Call(ScalarFn::VecL2, 2)] => {
            (*c, params.get(*p as usize))
        }
        [Instr::PushCol(c), Instr::PushConst(ci), Instr::Call(ScalarFn::VecL2, 2)] => {
            (*c, program.consts.get(*ci as usize))
        }
        [Instr::PushConst(ci), Instr::PushCol(c), Instr::Call(ScalarFn::VecL2, 2)] => {
            (*c, program.consts.get(*ci as usize))
        }
        _ => return Ok(None),
    };
    // The query vector, validated ONCE. NULL or malformed → generic path, so
    // the NULL-key ordering and the canonical refusal message both come from
    // the code that owns them.
    let Some(Value::Blob(qb)) = query else { return Ok(None) };
    if qb.len() % 4 != 0 {
        return Ok(None);
    }
    let q: Vec<f64> = qb
        .chunks_exact(4)
        .map(|c| f64::from(f32::from_le_bytes([c[0], c[1], c[2], c[3]])))
        .collect();

    let keep = {
        let (l, o) = resolve_limit_offset(Some(*limit), *offset, params)?;
        let l = l.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
        let o = o.min(usize::MAX as u64) as usize;
        l.saturating_add(o)
    };

    // The scan: same gather, same charges, same filter as the generic path.
    let rows = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;

    // NULL keys sort first (kept in scan order, capped at `keep`); real
    // distances go through the max-heap of the k best (d², seq) pairs.
    struct Cand {
        d2: f64,
        seq: usize,
        row: Vec<Value>,
    }
    impl PartialEq for Cand {
        fn eq(&self, other: &Self) -> bool {
            self.d2 == other.d2 && self.seq == other.seq
        }
    }
    impl Eq for Cand {}
    impl PartialOrd for Cand {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Cand {
        fn cmp(&self, other: &Self) -> Ordering {
            self.d2.total_cmp(&other.d2).then(self.seq.cmp(&other.seq))
        }
    }
    let mut nulls: Vec<Vec<Value>> = Vec::new();
    // Same discipline as `scan_rows_topk`: `keep` is runtime data (a negative
    // `LIMIT $k` resolves to no-bound = usize::MAX here), so it bounds what
    // the heap holds, never the preallocation — `keep + 1` alone overflows.
    let mut heap: BinaryHeap<Cand> = BinaryHeap::with_capacity(keep.saturating_add(1).min(65_536));
    for (seq, row) in rows.into_iter().enumerate() {
        let emb = row.get(emb_col as usize);
        let eb = match emb {
            Some(Value::Null) => {
                if nulls.len() < keep {
                    nulls.push(row);
                }
                continue;
            }
            Some(Value::Blob(b)) => b,
            // Not a blob: the canonical refusal, from the canonical code.
            Some(other) => {
                return Err(Error::TypeMismatch(format!(
                    "vec_l2() argument 1 must be a blob of little-endian f32, got {}",
                    other.type_name()
                )))
            }
            None => return Err(Error::Corrupt("kNN embedding column out of range".into())),
        };
        // Shape validation is NEVER abandoned — a malformed row must raise
        // here exactly as the generic projection would have raised.
        if eb.len() % 4 != 0 {
            return Err(Error::TypeMismatch(format!(
                "vec_l2() argument 1: blob length {} is not a multiple of 4",
                eb.len()
            )));
        }
        if eb.len() / 4 != q.len() {
            return Err(Error::TypeMismatch(format!(
                "vec_l2(): dimension mismatch ({} vs {})",
                eb.len() / 4,
                q.len()
            )));
        }
        let bound = if heap.len() == keep {
            match heap.peek() {
                Some(worst) => worst.d2,
                None => f64::INFINITY,
            }
        } else {
            f64::INFINITY
        };
        let mut d2 = 0.0f64;
        let mut abandoned = false;
        for (chunk, qv) in eb.chunks_exact(4).zip(&q) {
            let x = f64::from(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            let d = x - qv;
            d2 += d * d;
            // The partial sum only grows: past the current k-th best, the
            // remaining dimensions cannot un-lose.
            if d2 > bound {
                abandoned = true;
                break;
            }
        }
        if abandoned || (heap.len() == keep && keep > 0 && d2 >= bound) {
            // `>=`: an exact tie keeps the EARLIER row — the stable sort's
            // answer, since this row's seq is the largest so far.
            continue;
        }
        if keep == 0 {
            break;
        }
        heap.push(Cand { d2, seq, row });
        if heap.len() > keep {
            heap.pop();
        }
    }

    // NULLs first (scan order), then ascending distance — `sort_rows`' order.
    let mut chosen = nulls;
    let mut ranked: Vec<Cand> = heap.into_vec();
    ranked.sort();
    chosen.extend(ranked.into_iter().map(|c| c.row));
    chosen.truncate(keep);

    // The generic tail: project, trim sort-only columns, skip/take.
    let mut out = Vec::with_capacity(chosen.len());
    for row in &chosen {
        let mut orow = Vec::with_capacity(projection.len());
        for p in projection {
            orow.push(match p {
                Projection::Column(i) => row
                    .get(*i as usize)
                    .cloned()
                    .ok_or_else(|| internal("projection column"))?,
                Projection::Expr { program, name, .. } => {
                    program
                        .eval_host(row, params, ctx.host_fns())
                        .map_err(|e| name_decode_error(e, name))?
                }
            });
        }
        out.push(orow);
    }
    if *order_junk > 0 {
        let width = projection.len() - *order_junk as usize;
        for row in &mut out {
            row.truncate(width);
        }
    }
    let (l, o) = resolve_limit_offset(Some(*limit), *offset, params)?;
    let skip = o.min(usize::MAX as u64) as usize;
    let take = l.unwrap_or(u64::MAX).min(usize::MAX as u64) as usize;
    let out: Vec<Vec<Value>> = out.into_iter().skip(skip).take(take).collect();

    let columns = select_output_columns(schema, plan, sp)?;
    Ok(Some(ExecResult::Rows { columns, rows: out }))
}

/// Precomputed shape for the PkPoint micro-executor. Built once at
/// [`crate::PreparedSelect`] prepare (or on first execute) so the hot path
/// never rebuilds column names or projection ordinals — SQLite's stmt keeps
/// the same state on the `sqlite3_stmt`.
#[derive(Debug, Clone)]
pub(crate) struct PkPointHot {
    pub table: u32,
    pub col_idxs: Vec<u16>,
    pub columns: std::sync::Arc<[String]>,
}

/// Hot path for the common point lookup:
/// `SELECT c1, c2, … FROM t WHERE pk0 = $a [AND pk1 = $b …]`
///
/// Shape (all required — anything else falls through to the general SELECT):
/// - single table, no join / residual filter / post_filter / aggregate /
///   DISTINCT / ORDER BY / windows / dual
/// - `AccessPath::PkPoint` with only `Param`/`Const` parts (no OuterCol)
/// - projection is only base columns (no Expr)
/// - `offset` is 0 or absent; `limit` is absent or ≥ 1 (PkPoint yields ≤ 1 row)
///
/// Correctness: same `get_by_pk` + column projection the gather path uses;
/// column names match [`select_output_columns`].
fn try_exec_pk_point_hot(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<Option<ExecResult>> {
    let Some(hot) = try_build_pk_point_hot(schema, plan, sp)? else {
        return Ok(None);
    };
    Ok(Some(exec_pk_point_hot(ctx, plan, params, sp, &hot)?))
}

/// Build [`PkPointHot`] if `sp` is eligible; `Ok(None)` means use the general
/// SELECT path (not an error).
pub(crate) fn try_build_pk_point_hot(
    schema: &Schema,
    plan: &CompiledPlan,
    sp: &SelectPlan,
) -> Result<Option<PkPointHot>> {
    if sp.table == mpedb_sql::DUAL_TABLE
        || !sp.joins.is_empty()
        || sp.filter.is_some()
        || sp.joined_filter.is_some()
        || sp.post_filter.is_some()
        || sp.aggregate.is_some()
        || sp.distinct
        || !sp.windows.is_empty()
        || !sp.order_by.is_empty()
        || sp.order_junk != 0
        || !matches!(sp.offset, None | Some(LimitVal::Lit(0)))
        || matches!(sp.limit, Some(LimitVal::Lit(0)) | Some(LimitVal::Param(_)))
    {
        return Ok(None);
    }
    let AccessPath::PkPoint(parts) = &sp.access else {
        return Ok(None);
    };
    if parts.is_empty() {
        return Ok(None);
    }
    for p in parts {
        match p {
            KeyPart::Param(_) | KeyPart::Const(_) => {}
            KeyPart::OuterCol(_) => return Ok(None),
        }
    }
    for p in &sp.projection {
        if !matches!(p, Projection::Column(_)) {
            return Ok(None);
        }
    }
    let mut col_idxs = Vec::with_capacity(sp.projection.len());
    for p in &sp.projection {
        let Projection::Column(i) = p else {
            unreachable!("filtered above");
        };
        col_idxs.push(*i);
    }
    let columns = pk_point_output_columns(schema, plan, sp)?;
    Ok(Some(PkPointHot {
        table: sp.table,
        col_idxs,
        columns: std::sync::Arc::from(columns),
    }))
}

/// Run the PkPoint micro-executor with precomputed column metadata.
///
/// `sp` is the SelectPlan `hot` was built from, and it is NOT always
/// `plan.stmt`: `exec_select_impl` runs compound arms and lifted subplans
/// through this same path, and for those the top-level `plan.stmt` is a
/// `Compound` — or an `Insert` whose VALUES carry a scalar subquery — while the
/// eligible PkPoint select is nested inside it. Re-deriving `sp` from
/// `plan.stmt` here is what made `SELECT a FROM t WHERE id = 1 UNION …` fail
/// with "pk-point hot needs a Select plan". `plan` is still needed, for its
/// const pool and table ids.
pub(crate) fn exec_pk_point_hot(
    ctx: &mut dyn TxnCtx,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
    hot: &PkPointHot,
) -> Result<ExecResult> {
    let AccessPath::PkPoint(parts) = &sp.access else {
        return Err(internal("pk-point hot needs PkPoint access"));
    };

    // Resolve PK. Common case: one Param — borrow the caller's Value, no clone.
    let owned_pk: Vec<Value>;
    let pk: &[Value] = if parts.len() == 1 {
        match &parts[0] {
            KeyPart::Param(i) => {
                let Some(v) = params.get(*i as usize) else {
                    return Err(internal("key param"));
                };
                std::slice::from_ref(v)
            }
            KeyPart::Const(i) => {
                let Some(v) = plan.consts.get(*i as usize) else {
                    return Err(internal("key const"));
                };
                std::slice::from_ref(v)
            }
            KeyPart::OuterCol(_) => return Err(internal("outer-col in pk-point hot")),
        }
    } else {
        owned_pk = parts
            .iter()
            .map(|p| gather::resolve_part(p, plan, params))
            .collect::<Result<Vec<_>>>()?;
        &owned_pk
    };

    let projected = ctx.get_by_pk_cols(hot.table, pk, &hot.col_idxs)?;
    // One Arc clone + one Vec from Arc — cheaper than rebuilding names from schema.
    let columns: Vec<String> = hot.columns.iter().cloned().collect();
    let rows = match projected {
        None => Vec::new(),
        Some(row) => vec![row],
    };
    Ok(ExecResult::Rows { columns, rows })
}

/// Column names for the PkPoint hot path: single-table, column projections only.
/// Same naming as [`select_output_columns`] for that shape, without join logic.
fn pk_point_output_columns(
    schema: &Schema,
    plan: &CompiledPlan,
    sp: &SelectPlan,
) -> Result<Vec<String>> {
    let t = table_def(schema, plan, sp.table)?;
    let mut cols = Vec::with_capacity(sp.projection.len());
    for p in &sp.projection {
        let Projection::Column(i) = p else {
            return Err(internal("pk-point hot projection"));
        };
        let name = t
            .columns
            .get(*i as usize)
            .map(|c| c.name.clone())
            .ok_or_else(|| internal("projection column name"))?;
        cols.push(name);
    }
    Ok(cols)
}

/// Output column names of one SELECT. A joined slot past the outer's width
/// belongs to an inner table and is named `<table>.<column>` (`id` alone would
/// not say which side); a single-table read keeps plain column names.
pub(super) fn select_output_columns(schema: &Schema, plan: &CompiledPlan, sp: &SelectPlan) -> Result<Vec<String>> {
    // FROM-less: no table to name columns from. Every projection is an Expr
    // carrying its own name — the binder cannot produce a Column over the
    // zero-column dual row.
    if sp.table == mpedb_sql::DUAL_TABLE {
        return sp
            .projection
            .iter()
            .take(sp.projection.len() - sp.order_junk as usize)
            .map(|p| match p {
                Projection::Expr { name, .. } => Ok(name.clone()),
                Projection::Column(_) => Err(internal("column projection on a FROM-less select")),
            })
            .collect();
    }
    let t = table_def(schema, plan, sp.table)?;
    let joined_tables: Vec<std::borrow::Cow<TableDef>> = if sp.joins.is_empty() {
        vec![t]
    } else {
        let mut v = vec![t];
        for j in &sp.joins {
            v.push(table_def(schema, plan, j.table)?);
        }
        v
    };
    let name_slot = |mut i: usize| -> Result<String> {
        if joined_tables.len() == 1 {
            return joined_tables[0]
                .columns
                .get(i)
                .map(|c| c.name.clone())
                .ok_or_else(|| internal("projection column name"));
        }
        for jt in &joined_tables {
            if i < jt.columns.len() {
                return Ok(format!("{}.{}", jt.name, jt.columns[i].name));
            }
            i -= jt.columns.len();
        }
        Err(internal("projection column name"))
    };
    sp.projection
        .iter()
        .take(sp.projection.len() - sp.order_junk as usize)
        .map(|p| match p {
            Projection::Column(i) => name_slot(*i as usize),
            Projection::Expr { name, .. } => Ok(name.clone()),
        })
        .collect()
}

/// The correlated pipeline: gather UNBOUNDED (a per-row filter downstream
/// means no scan bound and no top-K is sound), then per row — fill each
/// correlated slot by running its subplan with the row's correlation args,
/// apply the post-filter, project, dedup — and only THEN sort/trim/bound.
/// The policies all ran inside the gather, so no subplan ever executes
/// against a row the caller was not allowed to see (the raise contract).
#[allow(clippy::too_many_arguments)]
fn exec_select_with(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
    // First reserved result slot of THIS level (`subplan_base` at the top,
    // `sub.sub_base` for a nested subplan) — where correlated slots are filled.
    base: usize,
    correlated: &[(usize, &SubPlan)],
) -> Result<ExecResult> {
    let SelectPlan {
        table,
        access,
        joins,
        joined_filter,
        post_filter,
        filter,
        projection,
        order_by,
        limit,
        offset,
        aggregate,
        distinct,
        order_over,
        order_junk,
        windows,
    } = sp;
    if aggregate.is_some() {
        // A correlated aggregate is routed to `run_aggregate` from
        // `exec_select_top`; reaching here with one is a routing bug.
        return Err(internal("correlated subplans in an aggregate plan"));
    }
    // The planner refuses windows together with a correlated subquery, so a
    // windowed plan never reaches this correlated path — its window results
    // would be silently dropped here. Reaching it with one is a routing bug.
    if !windows.is_empty() {
        return Err(internal("windows in a correlated select plan"));
    }
    // #125. Unlike the uncorrelated path this narrows the SINGLE-TABLE gather
    // too: `correlated_survivors` keeps a per-row scratch beside every gathered
    // row, so this shape holds the whole input at its widest and the columns
    // the correlation actually names are typically one or two.
    let prune = select_prune(schema, plan, sp, correlated)?;
    let mut rows = if !joins.is_empty() {
        gather_joined(
            ctx,
            plan,
            params,
            schema,
            *table,
            access,
            filter.as_ref(),
            joins,
            joined_filter.as_ref(),
            prune.as_ref(),
        )?
    } else {
        match &prune {
            Some(p) => {
                let t = table_def(schema, plan, *table)?;
                gather::gather_narrowed(
                    ctx,
                    *table,
                    access,
                    filter.as_ref(),
                    plan,
                    params,
                    &t,
                    p.stage(0),
                )?
            }
            None => gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?,
        }
    };
    if *order_over == OrderOver::BaseRow && !order_by.is_empty() {
        gather::check_order_colls(order_by, ctx.host_colls())?;
        sort_rows(&mut rows, order_by, ctx.host_colls());
    }

    // Fill every correlated slot per row and apply the post-filter, keeping each
    // survivor WITH the scratch that produced it — the projection may read a
    // correlated slot (a correlated scalar subquery in the SELECT list), so it
    // is evaluated against that scratch.
    let survivors = correlated_survivors(
        ctx, schema, plan, params, base, rows, correlated, post_filter.as_ref(),
    )?;

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // DISTINCT folds each output column under its declared collation (as in the
    // uncorrelated path above), so `SELECT DISTINCT name` on a NOCASE column
    // deduplicates case-insensitively.
    let distinct_colls = if *distinct {
        output_collations(schema, plan, *table, joins, projection)
    } else {
        Vec::new()
    };
    for (row, scratch) in survivors {
        let mut orow = Vec::with_capacity(projection.len());
        for p in projection {
            orow.push(match p {
                Projection::Column(i) => row
                    .get(*i as usize)
                    .cloned()
                    .ok_or_else(|| internal("projection column"))?,
                Projection::Expr { program, .. } => {
                    program.eval_host(&row, &scratch, ctx.host_fns())?
                }
            });
        }
        if *distinct && !seen.insert(keycode::encode_group_key(&orow, &distinct_colls)) {
            continue;
        }
        out.push(orow);
    }
    if *order_over != OrderOver::BaseRow {
        gather::check_order_colls(order_by, ctx.host_colls())?;
        sort_rows(&mut out, order_by, ctx.host_colls());
        if *order_junk > 0 {
            let keep = projection.len() - *order_junk as usize;
            for row in &mut out {
                row.truncate(keep);
            }
        }
    }
    // The post-filter changed the counts, so LIMIT/OFFSET bound the SURVIVING
    // rows — always applied here, whatever tuple the sort ran over.
    let (l, o) = resolve_limit_offset(*limit, *offset, params)?;
    let skip = o.min(usize::MAX as u64) as usize;
    let take = l.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    if skip > 0 || take != usize::MAX {
        out = out.into_iter().skip(skip).take(take).collect();
    }
    let columns = select_output_columns(schema, plan, sp)?;
    Ok(ExecResult::Rows { columns, rows: out })
}

/// Run the aggregate path for one SELECT, threading the per-row correlated
/// pre-filter. Shared by the plain aggregate dispatch ([`exec_select`], empty
/// correlated / no post-filter) and the correlated-aggregate dispatch
/// ([`exec_select_top`]) so the long argument wiring cannot drift.
#[allow(clippy::too_many_arguments)]
fn run_aggregate(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
    // First reserved result slot of THIS level — threaded to `correlated_survivors`
    // (unused when `correlated` is empty and `post_filter` is `None`).
    base: usize,
    correlated: &[(usize, &SubPlan)],
    post_filter: Option<&ExprProgram>,
) -> Result<ExecResult> {
    let t = table_def(schema, plan, sp.table)?;
    let agg = sp
        .aggregate
        .as_ref()
        .ok_or_else(|| internal("aggregate dispatch on a non-aggregate plan"))?;
    // #125: an aggregate is the shape whose output requirement is furthest from
    // its input width — `count(*)` observes NO column at all. The
    // materializing paths inside `exec_aggregate` narrow what they hold with
    // this, and the streaming fold (#123) pushes it into the SCAN so an
    // unobserved column is never even decoded (`gather::scan_keep`).
    let prune = select_prune(schema, plan, sp, correlated)?;
    // The parallel fold's shape gate — decided HERE, where the whole
    // SelectPlan is in hand, by the same predicate EXPLAIN prints. The
    // correlated machinery must also be absent: a correlated aggregate's
    // per-row scratch is exactly what the workers do not carry.
    let parallel_shape = correlated.is_empty()
        && post_filter.is_none()
        && sp.windows.is_empty()
        && mpedb_sql::parallel_fold_shape(sp, schema);
    exec_aggregate(
        ctx,
        plan,
        params,
        schema,
        &t,
        sp.table,
        &sp.access,
        sp.filter.as_ref(),
        &sp.joins,
        sp.joined_filter.as_ref(),
        agg,
        &sp.projection,
        &sp.order_by,
        sp.order_over,
        sp.order_junk,
        sp.limit,
        sp.offset,
        sp.distinct,
        base,
        correlated,
        post_filter,
        prune.as_ref(),
        parallel_shape,
        // A WINDOW over the grouped result: the phase runs between HAVING and
        // the projection, so the grouped tuple is widened to
        // `[keys ‖ aggs ‖ bare ‖ w0..wk]` before anything reads it.
        &sp.windows,
        window::grouped_collations(schema, plan, sp, agg),
    )
}

/// Per-row correlated pre-filter shared by the plain correlated SELECT
/// ([`exec_select_with`]) and the aggregate path ([`exec_aggregate`]) so the two
/// cannot drift (#73 §1). For each gathered row it fills every correlated
/// subplan slot into a scratch buffer — memoized per subplan by the encoded
/// correlation tuple, so two rows with the SAME tuple run the inner subplan once
/// (MPEE "buy the inner cells once, then only stream probes"; the memo is bounded
/// by the distinct tuples, itself ≤ `rows`, and `MPEDB_NO_SUBPLAN_MEMO=1`
/// restores per-row re-execution for A/B measurement) — then keeps the row iff
/// `post_filter` accepts it.
///
/// Each survivor is returned WITH the scratch that produced it, because a
/// non-aggregate projection may read a correlated slot (a correlated scalar
/// subquery in the SELECT list). The aggregate path discards the scratch:
/// validate and the planner forbid a correlated slot in any grouped program, so
/// grouping there reads `params`.
///
/// A scalar subplan's >1-row error still fires on the first occurrence of a key
/// (the miss path, before any memo insert), so error semantics are
/// byte-identical to per-row re-execution.
#[allow(clippy::too_many_arguments)]
pub(super) fn correlated_survivors(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    // First reserved result slot of THIS level: `subplan_base` at the top,
    // `sub.sub_base` for a nested subplan. `params[..base]` is `[user ‖ this
    // level's correlation args]` — the prefix a correlated child inherits — and a
    // correlated subplan `i`'s result is written to `scratch[base + i]`.
    base: usize,
    rows: Vec<Vec<Value>>,
    correlated: &[(usize, &SubPlan)],
    post_filter: Option<&ExprProgram>,
) -> Result<Vec<(Vec<Value>, Vec<Value>)>> {
    let n_user = base;
    let mut scratch: Vec<Value> = params.to_vec();
    let mut stack: Vec<Value> = Vec::new();
    let mut memo: Vec<std::collections::HashMap<Vec<u8>, Value>> =
        vec![std::collections::HashMap::new(); correlated.len()];
    let use_memo = std::env::var_os("MPEDB_NO_SUBPLAN_MEMO").is_none();
    // #74: attribute this driver to the (first) correlated subquery's inner
    // table. The inner subplan's own scans additionally charge through the scan
    // layer, so an N-outer × M-inner correlated bomb is counted as ~N·M. A
    // correlated body may be a plain SELECT or (format 56) a whole compound —
    // then the first arm names it; either way the charge must not be skipped.
    let corr_table = correlated.first().and_then(|(_, s)| match &s.body {
        SubBody::Select(sp) => Some(sp.table),
        SubBody::Compound(c) => c.arms.first().map(|a| a.output_select().table),
        // A derived body's own scans charge through the scan layer; naming its
        // WORKING table here would attribute the driver to a table that does
        // not exist in the schema, so leave the attribution to those.
        SubBody::Derived(_) => None,
    });
    let mut out = Vec::new();
    for row in rows {
        // One work-row per outer row this correlated subquery re-evaluates over.
        // Charged BEFORE the memo lookup, so the count is memo- (and
        // `MPEDB_NO_SUBPLAN_MEMO`-) independent and therefore deterministic.
        if let Some(t) = corr_table {
            ctx.charge_work(1, &|| {
                format!("correlated subquery over \"{}\"", table_name(schema, t))
            })?;
        }
        for (ci, &(i, sub)) in correlated.iter().enumerate() {
            let mut key_vals = Vec::with_capacity(sub.outer_args.len());
            for &a in &sub.outer_args {
                key_vals.push(
                    row.get(a as usize)
                        .cloned()
                        .ok_or_else(|| internal("correlation arg out of row"))?,
                );
            }
            // `encode_key_exact`, and neither of the other two encoders: this
            // is a CACHE keyed by the outer row's exact values, and the
            // subquery may distinguish what they merge (`typeof($1)`,
            // `printf`). The grouping key folds `1` and `1.0` on purpose; the
            // ORDERED key drops the mpedb type, so over a typeless (`any`)
            // column it collided the text `'1'` with the blob `x'31'` and the
            // integer `0` with the real `0.0` — and served one's result for the
            // other, which the differential caught as
            // `SELECT id, (SELECT typeof(o.v) FROM m) FROM o` answering "text"
            // where sqlite says "blob".
            let memo_key = keycode::encode_key_exact(&key_vals);
            scratch[base + i] = if let Some(v) = memo[ci].get(&memo_key) {
                v.clone()
            } else {
                let mut inner_params = Vec::with_capacity(n_user + key_vals.len());
                inner_params.extend_from_slice(&params[..n_user]);
                inner_params.extend(key_vals);
                // `inner_params` = `[user ‖ this subplan's correlation args]`,
                // width == `sub.sub_base`; `run_subplan` extends it with the
                // subplan's own (uncorrelated) nested lifts before running it.
                let r = run_subplan(ctx, schema, plan, &inner_params, sub)?;
                let v = subplan_value(r, sub.kind)?;
                if use_memo {
                    memo[ci].insert(memo_key, v.clone());
                }
                v
            };
        }
        let keep = match post_filter {
            Some(pf) => pf.eval_filter_host(&mut stack, &row, &scratch, ctx.host_fns())?,
            None => true,
        };
        if keep {
            out.push((row, scratch.clone()));
        }
    }
    Ok(out)
}
