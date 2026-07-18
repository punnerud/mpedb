use super::*;

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
pub(super) fn distinct_order_by(
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
    let item_slots: Vec<Option<u16>> = items.iter().map(|it| col_slot(&it.0, scope)).collect();
    let mut out = Vec::with_capacity(s.order_by.len());
    let mut n_junk = 0u16;
    for (i, (key, desc)) in s.order_by.iter().enumerate() {
        if let Some(pos) = ordinal(key, items.len())? {
            out.push((pos, *desc));
            continue;
        }
        // A bare identifier that matches a select-item's ALIAS names that
        // output position — PostgreSQL and sqlite both resolve the output
        // name before the input column, so `SELECT a AS b … ORDER BY b`
        // sorts by the output even when the table has its own `b`.
        if let ast::Expr::Col(n) = key {
            if let Some(pos) = items.iter().position(|it| it.1.as_deref() == Some(n.as_str())) {
                out.push((pos as u16, *desc));
                continue;
            }
        }
        let pos = match col_slot(key, scope) {
            Some(slot) => item_slots.iter().position(|s| *s == Some(slot)),
            // Not a column: fall back to comparing the expressions, which is
            // what makes `SELECT amt * 2 … ORDER BY amt * 2` match.
            None => items.iter().position(|it| &it.0 == key),
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
pub(super) fn push_junk(
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
pub(super) fn describe_key(e: &ast::Expr, pos: usize) -> String {
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
pub(super) fn ordinal(key: &ast::Expr, n_items: usize) -> Result<Option<u16>> {
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
        // A key naming an item's alias IS in the SELECT list.
        if let ast::Expr::Col(n) = key {
            if items.iter().any(|it| it.1.as_deref() == Some(n.as_str())) {
                continue;
            }
        }
        let stripped = strip(key);
        if !items.iter().any(|it| strip(&it.0) == stripped) {
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

pub(super) fn plan_select(
    s: &ast::SelectStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<PlannedStmt> {
    // RIGHT rewrites to a swapped LEFT before anything else looks at the
    // statement — the subquery lift's correlation scope and every stage
    // below must see the FINAL table order.
    let rewritten_right;
    let s = match super::join::rewrite_right_join(s, schema)? {
        Some(r) => {
            rewritten_right = r;
            &rewritten_right
        }
        None => s,
    };
    // Subqueries lift out FIRST: every one becomes a subplan plus a reserved
    // parameter slot, and the rest of planning sees only `Param(slot)` — no
    // stage below knows subqueries exist (see planner/subquery.rs).
    let lifted;
    let (s, subplans, slot_types): (&ast::SelectStmt, Vec<SubPlan>, Vec<Ty>) =
        if subquery::has_subquery(s) {
            lifted = subquery::lift_subqueries(s, schema, n_params, catalog, consts)?;
            (&lifted.stmt, lifted.subplans, lifted.slot_types)
        } else {
            (s, Vec::new(), Vec::new())
        };
    // The binder's parameter table covers `[user ‖ subplan results]`; the
    // planner KNOWS each result slot's type, so pin it instead of inferring.
    let eff_params = n_params + subplans.len() as u16;
    let correlated: Vec<bool> = subplans.iter().map(|p| !p.outer_args.is_empty()).collect();

    // A derived table must have been flattened by the view-inline pass before
    // planning (crate::view). If one survives to here it is an unsupported body
    // (aggregate/join/DISTINCT/…) — refuse, never silently drop the source.
    if s.from_derived.is_some() {
        return Err(bind_err(
            "this derived table `FROM (SELECT …)` is not supported yet — only a \
             simple single-table projection/filter subquery can be used as a FROM source",
        ));
    }
    let (table_id, table) = match &s.table {
        Some(name) => resolve_table(schema, name)?,
        // FROM-less: the DUAL sentinel and a zero-column def. The whole plain
        // pipeline below works over width 0 — access degrades to FullScan
        // (no columns can pin a key), the executor yields ONE empty row, and
        // ORDER BY by name errors exactly as sqlite's "no such column".
        None => {
            if s.items.is_none() {
                return Err(bind_err("SELECT * without a FROM clause — no tables specified"));
            }
            (crate::plan::DUAL_TABLE, crate::plan::dual_def())
        }
    };
    if !s.joins.is_empty() {
        return plan_join_select(s, schema, n_params, catalog, consts, subplans, slot_types);
    }
    let mut binder = match &s.alias {
        Some(a) => Binder::with_scope(Scope::single_named(a.clone(), table), eff_params, true),
        None => Binder::new(table, eff_params, true),
    };
    for (i, ty) in slot_types.iter().enumerate() {
        binder.pin_param(n_params + i as u16, *ty);
    }
    let bound_where = s
        .where_clause
        .as_ref()
        .map(|e| binder.bind_predicate(e))
        .transpose()?;
    // WHERE conjuncts that read a CORRELATED slot cannot run in the gather
    // (the slot is filled per row, after the policies) — they split off into
    // `post_filter`. Everything else, policy included, keeps today's path.
    let (bound_where, post_where) =
        subquery::split_correlated(bound_where, n_params, &correlated);
    // Inject the SELECT visibility policy AND-ed with the user WHERE, *before*
    // access extraction, so a policy conjunct that pins the PK/unique column
    // still becomes a Point/Range access and footprints only narrow (§3.3).
    // A FROM-less statement reads no table, so there is no table whose policy
    // could apply — and `table_id` is the DUAL sentinel, not a catalog key.
    let policy = if s.table.is_some() {
        read_policy(&mut binder, catalog, table_id, &table.name, PolicyCmd::Select)?
    } else {
        None
    };
    // Never run access extraction over the dual def: its EMPTY primary key
    // satisfies a point probe vacuously (zero of zero parts pinned), and a
    // `PkPoint([])` on a table that does not exist is exactly the "keyed
    // access on a FROM-less select" shape validate refuses. FullScan + the
    // whole predicate as residual is the only honest plan.
    let (access, residual) = if s.table.is_some() {
        extract_access(merge_and(bound_where, policy), table, consts)?
    } else {
        (AccessPath::FullScan, merge_and(bound_where, policy))
    };
    let filter = residual.map(|e| compile_program(&e)).transpose()?;
    let post_filter = post_where.map(|e| compile_program(&e)).transpose()?;
    let joined_filter: Option<ExprProgram> = None;

    check_distinct_order_by(s, table)?;

    // Is this an aggregate query? Either an aggregate appears, or GROUP BY does.
    let has_agg = s
        .items
        .as_ref()
        .is_some_and(|items| items.iter().any(|(e, _)| contains_agg(e)))
        || s.having.as_ref().is_some_and(contains_agg)
        // ORDER BY too: `SELECT dept FROM t ORDER BY count(*)` is an aggregate
        // query even though no aggregate appears in the SELECT list, and
        // routing it to the plain planner would report the wrong problem.
        || s.order_by.iter().any(|(e, _)| contains_agg(e))
        || !s.group_by.is_empty();
    if has_agg {
        // Aggregation consumes rows in the gather phase; the per-row filling
        // that correlated slots need happens after it. Uncorrelated slots are
        // filled once up front and pass through untouched.
        if correlated.iter().any(|&c| c) {
            return Err(bind_err(
                "a correlated subquery in an aggregate query is not supported yet",
            ));
        }
        return plan_aggregate_select(
            s,
            // The scope carries the ALIAS when the query gave one — dropping
            // it here is how `SELECT cor0.c FROM t AS cor0 GROUP BY cor0.c`
            // spent months failing with "no table named cor0".
            &match &s.alias {
                Some(a) => Scope::single_named(a.clone(), table),
                None => Scope::single(table),
            },
            table_id,
            access,
            filter,
            Vec::new(),
            None,
            binder,
            consts,
            subplans,
        );
    }

    let mut out_types: Vec<Option<ColumnType>> = Vec::new();
    let mut projection: Vec<Projection> = match &s.items {
        None => {
            out_types = table.columns.iter().map(|c| Some(c.ty)).collect();
            (0..table.columns.len() as u16).map(Projection::Column).collect()
        }
        Some(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (item, alias) in items {
                let (b, ty) = binder.bind_expr(item)?;
                out_types.push(ty);
                out.push(match (b, alias) {
                    (BExpr::Col(i), None) => Projection::Column(i),
                    // An alias must survive as the output name; a bare column
                    // wears it via a one-instruction program (PROJ_EXPR is
                    // already on the wire — no format change, and only
                    // aliased items pay the indirection).
                    (other, alias) => {
                        let program = compile_program(&other)?;
                        let name = alias.clone().unwrap_or_else(|| {
                            render_program(&program, &|c| {
                                table
                                    .columns
                                    .get(c as usize)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_else(|| format!("col#{c}"))
                            })
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
        // An unqualified name matching a select-item ALIAS orders the OUTPUT
        // (the PostgreSQL rule) — never the base row, even when a table
        // column shares the name. Route it to the projection-sort path.
        if let ast::Expr::Col(n) = e {
            if s.items.as_ref().is_some_and(|items| {
                items.iter().any(|it| it.1.as_deref() == Some(n.as_str()))
            }) {
                break;
            }
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
    // INDEX accesses deliver INDEX order (value, then pk WITHIN one value),
    // not PK order — eliding the sort over them returns rows in the wrong
    // order, silently. The differential caught exactly this on IndexRange
    // the day it was built; the guard is load-bearing.
    let pk_ordered_access = !matches!(
        access,
        AccessPath::IndexPoint { .. } | AccessPath::IndexRange { .. }
    ) && order_over == OrderOver::BaseRow;
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
        PlanStmt::Select(SelectPlan {
            aggregate: None,
            joins: Vec::new(),
            joined_filter,
            post_filter,
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
        }),
        param_types,
        context_keys,
        list_keys,
        out_types,
        subplans,
    ))
}
