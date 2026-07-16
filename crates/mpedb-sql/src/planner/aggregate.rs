use super::*;
use super::select::{distinct_order_by, ordinal, push_junk};

/// Does this expression contain an aggregate anywhere?
pub(super) fn contains_agg(e: &ast::Expr) -> bool {
    use ast::Expr as E;
    match e {
        E::Agg(..) => true,
        E::Unary(_, a) | E::IsNull(a, _) | E::Cast(a, _) => contains_agg(a),
        E::Binary(_, a, b) | E::Like(a, b) => contains_agg(a) || contains_agg(b),
        E::InContext(a, _, _) => contains_agg(a),
        E::InList(a, xs, _) => contains_agg(a) || xs.iter().any(contains_agg),
        E::Coalesce(xs) | E::Func(_, xs) => xs.iter().any(contains_agg),
        E::Case(arms, els) => {
            arms.iter().any(|(c, r)| contains_agg(c) || contains_agg(r))
                || els.as_deref().is_some_and(contains_agg)
        }
        // An aggregate INSIDE a subquery aggregates the inner statement's
        // rows, not ours — the outer walk stops at the boundary.
        E::Subquery(_) | E::Exists(..) => false,
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
        E::Cast(a, t) => E::Cast(Box::new(rec(a, aggs)?), *t),
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
        // Subqueries are lifted before aggregation planning ever runs; one
        // still here is headed for the binder's clear refusal — pass through.
        other @ (E::Subquery(_) | E::Exists(..)) => other.clone(),
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
            indexed: false,
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
            indexed: false,
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

/// Plan an aggregate SELECT over `base` — one table, or a join's
/// `[outer ‖ inner]`. Everything here is about the ROW being aggregated, so all
/// the join changes is how wide that row is and how names resolve into it.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_aggregate_select(
    s: &ast::SelectStmt,
    // The row being aggregated: its columns (for the grouped tuple's types) and
    // its scope (for name resolution and messages).
    base_columns: &[mpedb_types::ColumnDef],
    base_scope: &Scope<'_>,
    table_id: u32,
    access: AccessPath,
    filter: Option<ExprProgram>,
    joins: Vec<Join>,
    joined_filter: Option<ExprProgram>,
    mut binder: Binder<'_>,
    _consts: &mut Vec<Value>,
    subplans: Vec<SubPlan>,
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
    for (item, _alias) in items {
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

    let mut out_types: Vec<Option<ColumnType>> = Vec::with_capacity(rewritten.len());
    let mut projection: Vec<Projection> = Vec::with_capacity(rewritten.len());
    for (item, (orig, alias)) in rewritten.iter().zip(items) {
        let (b, ty) = binder.bind_expr(item)?;
        out_types.push(ty);
        // The alias, when present, IS the output name — otherwise the
        // canonical rendering of the original item.
        let name = alias.clone().unwrap_or_else(|| agg_item_name(orig));
        projection.push(match b {
            BExpr::Col(i) => Projection::Expr {
                program: compile_program(&BExpr::Col(i))?,
                name,
            },
            other => Projection::Expr {
                program: compile_program(&other)?,
                name,
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
            PlanStmt::Select(SelectPlan {
                table: table_id,
                access,
                joins,
                joined_filter,
                post_filter: None,
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
            }),
            param_types,
            context_keys,
            list_keys,
            out_types,
            subplans,
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
        PlanStmt::Select(SelectPlan {
            table: table_id,
            access,
            joins,
            joined_filter,
            post_filter: None,
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
        }),
        param_types,
        context_keys,
        list_keys,
        out_types,
        subplans,
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
