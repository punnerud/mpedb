//! Subquery lifting (#56). A `(SELECT …)` inside an expression is planned as
//! its own [`SubPlan`] and REPLACED by a reserved parameter: the outer
//! statement never sees a subquery, only `Param(slot)` — so every downstream
//! stage (binder typing, access extraction, DISTINCT/ORDER machinery, the
//! whole executor expression path) works unchanged, and no new instruction
//! enters the expression IR.
//!
//! Correlation is the index-nested-loop idea applied to a plan: an outer-row
//! reference inside the subquery becomes a trailing parameter of the INNER
//! plan (`outer_args[j]` names the outer slot that fills it), exactly as
//! `KeyPart::OuterCol` parametrizes an inner fetch. `outer_args` empty =
//! uncorrelated: evaluated once per execute, before access resolution — which
//! is what lets `WHERE id = (SELECT max(id) …)` still be a PK point probe.

use super::*;

/// Everything `lift_subqueries` learned about one statement.
pub(super) struct Lifted {
    /// The statement with every subquery replaced by `Param(slot)`.
    pub stmt: ast::SelectStmt,
    pub subplans: Vec<SubPlan>,
    /// Output type of each subplan's result slot, parallel to `subplans`
    /// (`None` when the inner output type could not be pinned).
    pub slot_types: Vec<Ty>,
}

/// Does any expression of this statement contain a subquery at all? Cheap
/// pre-check so the plain path pays nothing.
pub(super) fn has_subquery(s: &ast::SelectStmt) -> bool {
    let in_items = s
        .items
        .as_ref()
        .is_some_and(|items| items.iter().any(|(e, _)| expr_has_subquery(e)));
    in_items
        || s.joins.iter().any(|j| expr_has_subquery(&j.on))
        || s.where_clause.as_ref().is_some_and(expr_has_subquery)
        || s.group_by.iter().any(expr_has_subquery)
        || s.having.as_ref().is_some_and(expr_has_subquery)
        || s.order_by.iter().any(|(e, _)| expr_has_subquery(e))
}

fn expr_has_subquery(e: &ast::Expr) -> bool {
    use ast::Expr as E;
    match e {
        E::Subquery(_) | E::Exists(..) | E::InSubquery(..) => true,
        E::InParamSlot(a, _, _) => expr_has_subquery(a),
        E::Unary(_, a) | E::IsNull(a, _) | E::Cast(a, _) => expr_has_subquery(a),
        E::Binary(_, a, b) | E::Like(a, b) => expr_has_subquery(a) || expr_has_subquery(b),
        E::InContext(a, _, _) => expr_has_subquery(a),
        E::InList(a, xs, _) => expr_has_subquery(a) || xs.iter().any(expr_has_subquery),
        E::Coalesce(xs) | E::Func(_, xs) => xs.iter().any(expr_has_subquery),
        E::Case(arms, els) => {
            arms.iter()
                .any(|(c, r)| expr_has_subquery(c) || expr_has_subquery(r))
                || els.as_deref().is_some_and(expr_has_subquery)
        }
        E::Agg(_, arg, _) => arg.as_deref().is_some_and(expr_has_subquery),
        E::Lit(_) | E::Param(_) | E::Col(_) | E::ContextRef(_) | E::Excluded(_)
        | E::Qualified(..) => false,
    }
}

/// Lift every subquery out of `s`. `n_params` is the user parameter count;
/// subplan result slots are allocated at `n_params + i` (the binder is later
/// created with `n_params + subplans.len()` slots, and context slots append
/// after — the `[user ‖ sub ‖ context]` layout).
pub(super) fn lift_subqueries(
    s: &ast::SelectStmt,
    schema: &Schema,
    n_params: u16,
    catalog: &PolicyCatalog,
    consts: &mut Vec<Value>,
) -> Result<Lifted> {
    // The OUTER scope, for correlation: the same `[table0 ‖ … ‖ tableN]`
    // tuple the outer statement's own expressions bind over.
    let mut named: Vec<(String, &TableDef)> = Vec::new();
    // FROM-less outer: an EMPTY outer scope — with no outer columns, nothing
    // can correlate, and every unresolved name inside a subquery stays that
    // subquery's own error.
    if let Some(t) = &s.table {
        let (_, outer_t) = resolve_table(schema, t)?;
        named.push((s.alias.clone().unwrap_or_else(|| t.clone()), outer_t));
    }
    for j in &s.joins {
        let (_, jt) = resolve_table(schema, &j.table)?;
        named.push((j.alias.clone().unwrap_or_else(|| j.table.clone()), jt));
    }
    let outer_scope = Scope::joined_named(named)?;

    let mut lift = Lift {
        schema,
        n_params,
        catalog,
        consts,
        outer_scope,
        subplans: Vec::new(),
        slot_types: Vec::new(),
    };
    let stmt = ast::SelectStmt {
        table: s.table.clone(),
        alias: s.alias.clone(),
        joins: s
            .joins
            .iter()
            .map(|j| {
                Ok(ast::JoinClause {
                    table: j.table.clone(),
                    alias: j.alias.clone(),
                    kind: j.kind,
                    // A subquery in an ON condition would run gather-side,
                    // where correlated slots are not yet filled — refuse
                    // rather than misread (uncorrelated-in-ON can come later).
                    on: {
                        if expr_has_subquery(&j.on) {
                            return Err(bind_err(
                                "a subquery in a JOIN's ON condition is not supported yet",
                            ));
                        }
                        j.on.clone()
                    },
                })
            })
            .collect::<Result<_>>()?,
        distinct: s.distinct,
        items: match &s.items {
            None => None,
            Some(items) => Some(
                items
                    .iter()
                    .map(|(e, a)| Ok((lift.rewrite(e)?, a.clone())))
                    .collect::<Result<_>>()?,
            ),
        },
        where_clause: s.where_clause.as_ref().map(|e| lift.rewrite(e)).transpose()?,
        // GROUP BY keys must BE columns (the aggregate planner's rule) — a
        // subquery there could never plan; leave it to error naturally.
        group_by: s.group_by.clone(),
        having: match &s.having {
            // HAVING runs over the grouped tuple inside the aggregate phase,
            // where per-row slot filling cannot reach.
            Some(h) if expr_has_subquery(h) => {
                return Err(bind_err(
                    "a subquery in HAVING is not supported yet",
                ))
            }
            other => other.clone(),
        },
        order_by: s
            .order_by
            .iter()
            .map(|(e, d)| Ok((lift.rewrite(e)?, *d)))
            .collect::<Result<_>>()?,
        limit: s.limit,
        offset: s.offset,
    };
    Ok(Lifted {
        stmt,
        subplans: lift.subplans,
        slot_types: lift.slot_types,
    })
}

struct Lift<'a> {
    schema: &'a Schema,
    n_params: u16,
    catalog: &'a PolicyCatalog,
    consts: &'a mut Vec<Value>,
    outer_scope: Scope<'a>,
    subplans: Vec<SubPlan>,
    slot_types: Vec<Ty>,
}

impl Lift<'_> {
    /// Replace every subquery in `e` with `Param(slot)`, planning it into
    /// `self.subplans` on the way.
    fn rewrite(&mut self, e: &ast::Expr) -> Result<ast::Expr> {
        use ast::Expr as E;
        Ok(match e {
            E::Subquery(inner) => E::Param(self.plan_one(inner, SubPlanKind::Scalar)?),
            E::Exists(inner, negated) => {
                let p = E::Param(self.plan_one(inner, SubPlanKind::Exists)?);
                if *negated {
                    E::Unary(ast::UnOp::Not, Box::new(p))
                } else {
                    p
                }
            }
            E::Unary(op, a) => E::Unary(*op, Box::new(self.rewrite(a)?)),
            E::IsNull(a, n) => E::IsNull(Box::new(self.rewrite(a)?), *n),
            E::Cast(a, t) => E::Cast(Box::new(self.rewrite(a)?), *t),
            E::Binary(op, a, b) => E::Binary(
                *op,
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
            ),
            E::Like(a, b) => E::Like(Box::new(self.rewrite(a)?), Box::new(self.rewrite(b)?)),
            E::InContext(a, k, n) => {
                E::InContext(Box::new(self.rewrite(a)?), k.clone(), *n)
            }
            // `x IN (SELECT …)` (#70): the subquery becomes a LIST-kind
            // subplan; the node becomes the InParam membership marker over
            // its slot. Uncorrelated only in this step — a correlated IN
            // wants the post-filter machinery and is refused by name.
            E::InSubquery(lhs, inner, negated) => {
                let lhs = self.rewrite(lhs)?;
                let slot = self.plan_one(inner, SubPlanKind::List)?;
                E::InParamSlot(Box::new(lhs), slot, *negated)
            }
            E::InParamSlot(a, slot, n) => {
                E::InParamSlot(Box::new(self.rewrite(a)?), *slot, *n)
            }
            E::InList(a, xs, n) => E::InList(
                Box::new(self.rewrite(a)?),
                xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?,
                *n,
            ),
            E::Coalesce(xs) => {
                E::Coalesce(xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?)
            }
            E::Func(f, xs) => E::Func(
                f.clone(),
                xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?,
            ),
            E::Case(arms, els) => E::Case(
                arms.iter()
                    .map(|(c, r)| Ok((self.rewrite(c)?, self.rewrite(r)?)))
                    .collect::<Result<_>>()?,
                match els {
                    Some(x) => Some(Box::new(self.rewrite(x)?)),
                    None => None,
                },
            ),
            E::Agg(f, arg, d) => E::Agg(
                *f,
                match arg {
                    Some(a) => Some(Box::new(self.rewrite(a)?)),
                    None => None,
                },
                *d,
            ),
            other @ (E::Lit(_) | E::Param(_) | E::Col(_) | E::ContextRef(_)
            | E::Excluded(_) | E::Qualified(..)) => other.clone(),
        })
    }

    /// Plan one subquery: resolve its correlation against the outer scope,
    /// plan the rewritten inner select, and hand back the reserved slot its
    /// result will occupy.
    fn plan_one(&mut self, inner: &ast::SelectStmt, kind: SubPlanKind) -> Result<u16> {
        if self.subplans.len() >= 16 {
            return Err(bind_err("too many subqueries in one statement (max 16)"));
        }
        if has_subquery(inner) {
            return Err(bind_err("nested subqueries are not supported yet"));
        }
        // The INNER scope decides which names stay put; what it cannot
        // resolve is tried against the OUTER scope and becomes a correlation
        // parameter. Bare names prefer the inner table — SQL's rule.
        let mut inner_named: Vec<(String, &TableDef)> = Vec::new();
        // A FROM-less subquery (`SELECT (SELECT 3)`) has an empty inner
        // scope: every name falls through to the outer and correlates, or
        // errors there — the same rule as any other unresolved inner name.
        if let Some(t) = &inner.table {
            let (_, it) = resolve_table(self.schema, t)?;
            inner_named.push((inner.alias.clone().unwrap_or_else(|| t.clone()), it));
        }
        for j in &inner.joins {
            let (_, jt) = resolve_table(self.schema, &j.table)?;
            inner_named.push((j.alias.clone().unwrap_or_else(|| j.table.clone()), jt));
        }
        let inner_scope = Scope::joined_named(inner_named)?;

        let mut corr = Correlate {
            inner_scope,
            outer_scope: &self.outer_scope,
            n_params: self.n_params,
            outer_args: Vec::new(),
            arg_types: Vec::new(),
        };
        let rewritten = ast::SelectStmt {
            table: inner.table.clone(),
            alias: inner.alias.clone(),
            joins: inner
                .joins
                .iter()
                .map(|j| {
                    Ok(ast::JoinClause {
                        table: j.table.clone(),
                        alias: j.alias.clone(),
                        kind: j.kind,
                        on: corr.rewrite(&j.on)?,
                    })
                })
                .collect::<Result<_>>()?,
            distinct: inner.distinct,
            items: match &inner.items {
                None => None,
                Some(items) => Some(
                    items
                        .iter()
                        .map(|(e, a)| Ok((corr.rewrite(e)?, a.clone())))
                        .collect::<Result<_>>()?,
                ),
            },
            where_clause: inner
                .where_clause
                .as_ref()
                .map(|e| corr.rewrite(e))
                .transpose()?,
            group_by: inner
                .group_by
                .iter()
                .map(|e| corr.rewrite(e))
                .collect::<Result<_>>()?,
            having: inner.having.as_ref().map(|e| corr.rewrite(e)).transpose()?,
            order_by: inner
                .order_by
                .iter()
                .map(|(e, d)| Ok((corr.rewrite(e)?, *d)))
                .collect::<Result<_>>()?,
            limit: inner.limit,
            offset: inner.offset,
        };
        let outer_args = corr.outer_args;
        let arg_types = corr.arg_types;

        // Plan the inner with its own parameter space: user params, then one
        // slot per correlation arg. Its context keys are refused (the
        // reserved-slot layouts would have to be reconciled across levels).
        let inner_n = self.n_params + outer_args.len() as u16;
        let (stmt, inner_ptypes, inner_ctx, _inner_lists, inner_out, inner_subs) =
            plan_select(&rewritten, self.schema, inner_n, self.catalog, self.consts)?;
        debug_assert!(inner_subs.is_empty(), "nesting refused above");
        if !inner_ctx.is_empty() {
            return Err(bind_err(
                "current_setting() inside a subquery is not supported yet",
            ));
        }
        let PlanStmt::Select(plan) = stmt else {
            return Err(Error::Internal("subquery planned to a non-select".into()));
        };
        // The inner binder saw each correlation slot in real use — a type it
        // pinned must MATCH the outer column feeding the slot.
        for (j, &want) in arg_types.iter().enumerate() {
            let slot = self.n_params as usize + j;
            if let Some(t) = inner_ptypes[slot] {
                if t != want {
                    return Err(bind_err(format!(
                        "correlated reference is {want} in the outer query but used as {t} \
                         inside the subquery"
                    )));
                }
            }
        }
        if kind != SubPlanKind::Exists
            && plan.projection.len() - plan.order_junk as usize != 1
        {
            return Err(bind_err(match kind {
                SubPlanKind::List => "an IN subquery must select exactly one column",
                _ => "a scalar subquery must select exactly one column",
            }));
        }
        if kind == SubPlanKind::List && !outer_args.is_empty() {
            return Err(bind_err(
                "a correlated IN subquery is not supported yet — rewrite as EXISTS",
            ));
        }
        let ty = match kind {
            SubPlanKind::Exists => Some(ColumnType::Bool),
            SubPlanKind::Scalar => inner_out.first().copied().flatten(),
            // The slot holds a LIST at runtime; pinning a scalar type on it
            // would make resolve reject the fill. Membership is runtime-typed
            // (the same 3VL core session-context lists use).
            SubPlanKind::List => None,
        };
        let slot = self.n_params + self.subplans.len() as u16;
        self.subplans.push(SubPlan { plan, outer_args, kind });
        self.slot_types.push(ty);
        Ok(slot)
    }
}

/// Rewrites OUTER references inside a subquery into correlation parameters.
struct Correlate<'a, 'b> {
    inner_scope: Scope<'a>,
    outer_scope: &'b Scope<'a>,
    n_params: u16,
    /// Outer base-row slots, one per correlation parameter, in slot order.
    outer_args: Vec<u16>,
    arg_types: Vec<ColumnType>,
}

impl Correlate<'_, '_> {
    fn arg_param(&mut self, outer_slot: u16, ty: ColumnType) -> ast::Expr {
        // The same outer slot referenced twice is ONE parameter.
        let j = match self.outer_args.iter().position(|&a| a == outer_slot) {
            Some(j) => j,
            None => {
                self.outer_args.push(outer_slot);
                self.arg_types.push(ty);
                self.outer_args.len() - 1
            }
        };
        ast::Expr::Param(self.n_params + j as u16)
    }

    fn rewrite(&mut self, e: &ast::Expr) -> Result<ast::Expr> {
        use ast::Expr as E;
        Ok(match e {
            // The names are the whole point. Inner resolution wins (SQL's
            // innermost-scope rule); only a name the subquery CANNOT see is
            // tried against the outer row and becomes a parameter.
            E::Col(n) => {
                if self.inner_scope.resolve(n).is_ok() {
                    e.clone()
                } else if let Ok((slot, ty)) = self.outer_scope.resolve(n) {
                    self.arg_param(slot, ty)
                } else {
                    // Neither scope knows it — let the inner binder produce
                    // its usual "unknown column" with the inner context.
                    e.clone()
                }
            }
            E::Qualified(q, n) => {
                if self.inner_scope.resolve_qualified(q, n).is_ok() {
                    e.clone()
                } else if let Ok((slot, ty)) = self.outer_scope.resolve_qualified(q, n) {
                    self.arg_param(slot, ty)
                } else {
                    e.clone()
                }
            }
            E::Subquery(_) | E::Exists(..) => {
                return Err(bind_err("nested subqueries are not supported yet"))
            }
            E::Unary(op, a) => E::Unary(*op, Box::new(self.rewrite(a)?)),
            E::IsNull(a, n) => E::IsNull(Box::new(self.rewrite(a)?), *n),
            E::Cast(a, t) => E::Cast(Box::new(self.rewrite(a)?), *t),
            E::Binary(op, a, b) => E::Binary(
                *op,
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
            ),
            E::Like(a, b) => E::Like(Box::new(self.rewrite(a)?), Box::new(self.rewrite(b)?)),
            E::InContext(a, k, n) => {
                E::InContext(Box::new(self.rewrite(a)?), k.clone(), *n)
            }
            // Inside a subquery, another subquery is NESTING — refused,
            // the same line plan_one draws for Subquery/Exists.
            E::InSubquery(..) | E::InParamSlot(..) => {
                return Err(bind_err("nested subqueries are not supported yet"))
            }
            E::InList(a, xs, n) => E::InList(
                Box::new(self.rewrite(a)?),
                xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?,
                *n,
            ),
            E::Coalesce(xs) => {
                E::Coalesce(xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?)
            }
            E::Func(f, xs) => E::Func(
                f.clone(),
                xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?,
            ),
            E::Case(arms, els) => E::Case(
                arms.iter()
                    .map(|(c, r)| Ok((self.rewrite(c)?, self.rewrite(r)?)))
                    .collect::<Result<_>>()?,
                match els {
                    Some(x) => Some(Box::new(self.rewrite(x)?)),
                    None => None,
                },
            ),
            E::Agg(f, arg, d) => E::Agg(
                *f,
                match arg {
                    Some(a) => Some(Box::new(self.rewrite(a)?)),
                    None => None,
                },
                *d,
            ),
            other @ (E::Lit(_) | E::Param(_) | E::ContextRef(_) | E::Excluded(_)) => {
                other.clone()
            }
        })
    }
}

/// Split a bound WHERE into (gather-safe part, correlated part) by top-level
/// AND conjuncts. A conjunct reads a correlated slot ⇒ it moves to the
/// post-filter; ANDs under OR do not split (the whole OR moves if any leg
/// reads a slot — an OR is one predicate).
pub(super) fn split_correlated(
    bound: Option<BExpr>,
    sub_base: u16,
    correlated: &[bool],
) -> (Option<BExpr>, Option<BExpr>) {
    let Some(b) = bound else { return (None, None) };
    if correlated.iter().all(|&c| !c) {
        return (Some(b), None);
    }
    let mut gather: Option<BExpr> = None;
    let mut post: Option<BExpr> = None;
    let mut stack = vec![b];
    let and = |acc: Option<BExpr>, e: BExpr| match acc {
        None => Some(e),
        Some(a) => Some(BExpr::Binary(ast::BinOp::And, Box::new(a), Box::new(e))),
    };
    while let Some(e) = stack.pop() {
        match e {
            BExpr::Binary(ast::BinOp::And, a, bx) => {
                stack.push(*a);
                stack.push(*bx);
            }
            other => {
                if refs_correlated(&other, sub_base, correlated) {
                    post = and(post, other);
                } else {
                    gather = and(gather, other);
                }
            }
        }
    }
    (gather, post)
}

fn refs_correlated(b: &BExpr, sub_base: u16, correlated: &[bool]) -> bool {
    let is_corr = |i: u16| {
        i >= sub_base
            && ((i - sub_base) as usize) < correlated.len()
            && correlated[(i - sub_base) as usize]
    };
    match b {
        BExpr::Param(i) => is_corr(*i),
        BExpr::Const(_) | BExpr::Col(_) => false,
        BExpr::Unary(_, a) | BExpr::Like(a, _) | BExpr::Cast(a, _) => {
            refs_correlated(a, sub_base, correlated)
        }
        BExpr::Binary(_, a, bx) => {
            refs_correlated(a, sub_base, correlated) || refs_correlated(bx, sub_base, correlated)
        }
        BExpr::InParam(a, i) => is_corr(*i) || refs_correlated(a, sub_base, correlated),
        BExpr::InList(a, xs) => {
            refs_correlated(a, sub_base, correlated)
                || xs.iter().any(|x| refs_correlated(x, sub_base, correlated))
        }
        BExpr::Case(arms, els) => {
            arms.iter().any(|(c, r)| {
                refs_correlated(c, sub_base, correlated)
                    || refs_correlated(r, sub_base, correlated)
            }) || els
                .as_deref()
                .is_some_and(|e| refs_correlated(e, sub_base, correlated))
        }
        BExpr::Call(_, xs) | BExpr::Coalesce(xs) => {
            xs.iter().any(|x| refs_correlated(x, sub_base, correlated))
        }
    }
}
