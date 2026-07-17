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
/// `A RIGHT JOIN B ON c` is `B LEFT JOIN A ON c` — with one catch the swap
/// alone gets wrong: the OUTPUT still lists A's columns first. So a bare
/// `SELECT *` is pinned to the original column order as explicit qualified
/// items BEFORE the sides swap. Only the two-table form rewrites; a chain
/// would need the right side as a subtree, which left-deep plans cannot
/// express (the refusal says the manual fix).
pub(super) fn rewrite_right_join(
    s: &ast::SelectStmt,
    schema: &Schema,
) -> Result<Option<ast::SelectStmt>> {
    if !s.joins.iter().any(|j| j.kind == ast::JoinKind::Right) {
        return Ok(None);
    }
    if s.joins.len() != 1 {
        return Err(bind_err(
            "RIGHT JOIN in a multi-join chain is not supported — swap the tables \
             and write LEFT JOIN",
        ));
    }
    let j = &s.joins[0];
    // Join paths are unreachable without a FROM — the parser cannot build a
    // join clause on a FROM-less statement.
    let s_table = s.table.as_deref().expect("join on a FROM-less SELECT");
    let items = match &s.items {
        Some(items) => Some(items.clone()),
        None => {
            let (_, lt) = resolve_table(schema, s_table)?;
            let (_, rt) = resolve_table(schema, &j.table)?;
            let lname = s.alias.clone().unwrap_or_else(|| s_table.to_string());
            let rname = j.alias.clone().unwrap_or_else(|| j.table.clone());
            let mut items = Vec::with_capacity(lt.columns.len() + rt.columns.len());
            for c in &lt.columns {
                items.push((ast::Expr::Qualified(lname.clone(), c.name.clone()), None));
            }
            for c in &rt.columns {
                items.push((ast::Expr::Qualified(rname.clone(), c.name.clone()), None));
            }
            Some(items)
        }
    };
    Ok(Some(ast::SelectStmt {
        table: Some(j.table.clone()),
        alias: j.alias.clone(),
        joins: vec![ast::JoinClause {
            table: s_table.to_string(),
            alias: s.alias.clone(),
            kind: ast::JoinKind::Left,
            on: j.on.clone(),
        }],
        distinct: s.distinct,
        items,
        where_clause: s.where_clause.clone(),
        group_by: s.group_by.clone(),
        having: s.having.clone(),
        order_by: s.order_by.clone(),
        limit: s.limit,
        offset: s.offset,
    }))
}

pub(super) fn plan_join_select(
    s: &ast::SelectStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
    subplans: Vec<SubPlan>,
    slot_types: Vec<Ty>,
) -> Result<PlannedStmt> {
    // Subqueries were lifted by `plan_select` before the join dispatch; the
    // reserved result slots sit right after the user params.
    let eff_params = n_params + subplans.len() as u16;
    let correlated: Vec<bool> = subplans.iter().map(|p| !p.outer_args.is_empty()).collect();
    let outer_table = s.table.as_deref().expect("join on a FROM-less SELECT");
    let (outer_id, outer) = resolve_table(schema, outer_table)?;
    let outer_name = s.alias.clone().unwrap_or_else(|| outer.name.clone());

    // The outer's policy binds over its own row and can pin an access path.
    let mut ob = Binder::new(outer, eff_params, true);
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
        let (jid, jt) = resolve_table(schema, &jc.table)?;
        let jname = jc.alias.clone().unwrap_or_else(|| jt.name.clone());

        let mut pb = binder.rescope(Scope::single(jt));
        let policy = read_policy(&mut pb, catalog, jid, &jt.name, PolicyCmd::Select)?;
        let policy = policy.map(|e| compile_program(&e)).transpose()?;

        acc_named.push((jname, jt));
        let mut jb = pb.rescope(Scope::joined_named(acc_named.clone())?);
        let bound_on = jb.bind_predicate(&jc.on)?;
        binder = jb;

        let kind = match jc.kind {
            ast::JoinKind::Inner => JoinKind::Inner,
            ast::JoinKind::Left => JoinKind::Left,
            // RIGHT was rewritten to a swapped LEFT before planning; one
            // still here is a RIGHT the rewrite cannot express as left-deep.
            ast::JoinKind::Right => {
                return Err(bind_err(
                    "RIGHT JOIN in a multi-join chain is not supported — swap the \
                     tables and write LEFT JOIN",
                ))
            }
            ast::JoinKind::Full => {
                if s.joins.len() > 1 {
                    return Err(bind_err(
                        "FULL JOIN in a multi-join chain is not supported — it needs \
                         both sides whole, which a left-deep chain cannot give it",
                    ));
                }
                JoinKind::Full
            }
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

    let (access, outer_residual) = extract_access(
        merge_and(and_all(outer_extra), outer_policy),
        outer,
        consts,
    )?;
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
        let (jaccess, residual_on) = if draft.kind == JoinKind::Full {
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
        if correlated.iter().any(|&c| c) {
            return Err(bind_err(
                "a correlated subquery in an aggregate query is not supported yet",
            ));
        }
        return plan_aggregate_select(
            s,
            &full_scope,
            outer_id,
            access,
            filter,
            joins,
            joined_filter,
            binder,
            consts,
            subplans,
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
        let (keys, n_junk) = distinct_order_by(s, &full_scope, None)?;
        order_by = keys;
        order_over = OrderOver::Projection;
        order_junk = n_junk;
    } else if !s.order_by.is_empty() {
        // The "base row" of a join IS the joined row, and it is built in full
        // before the sort — so sorting it is the same operation, just wider.
        let mut keys = Vec::with_capacity(s.order_by.len());
        let mut all_cols = true;
        for (e, desc) in &s.order_by {
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
            let (keys, n_junk) =
                join_order_by(s, &full_scope, &mut projection, &mut binder, &namer)?;
            order_by = keys;
            order_over = OrderOver::Projection;
            order_junk = n_junk;
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
        }),
        param_types,
        context_keys,
        list_keys,
        out_types,
        subplans,
    ))
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
            if let Some(pos) = items.iter().position(|it| &it.0 == e) {
                keys.push((pos as u16, *desc));
                continue;
            }
            // A bare identifier naming an item's ALIAS is that output
            // position (PG/sqlite resolve the output name first).
            if let ast::Expr::Col(n) = e {
                if let Some(pos) =
                    items.iter().position(|it| it.1.as_deref() == Some(n.as_str()))
                {
                    keys.push((pos as u16, *desc));
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
        keys.push(((projection.len() - 1) as u16, *desc));
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
        let sec = secondary_indexes(inner);
        'passes: for unique_only in [true, false] {
            for (pos, &col) in sec.iter().enumerate() {
                if pos >= 63 || inner.columns[col as usize].unique != unique_only {
                    continue;
                }
                if let Some((ci, part)) = pins[col as usize] {
                    consumed[ci] = true;
                    chosen = Some(AccessPath::IndexPoint {
                        index_no: (pos + 1) as u32,
                        part,
                    });
                    break 'passes;
                }
            }
        }
    }
    let access = chosen.unwrap_or(AccessPath::FullScan);
    // An empty residual is constant TRUE — the equality was the whole ON.
    let residual =
        rebuild_residual(conjuncts, &consumed).unwrap_or(BExpr::Const(Value::Bool(true)));
    Ok((access, residual))
}
