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

pub(super) fn expr_has_subquery(e: &ast::Expr) -> bool {
    use ast::Expr as E;
    match e {
        E::Subquery(_) | E::Exists(..) | E::InSubquery(..) => true,
        E::InParamSlot(a, _, _) => expr_has_subquery(a),
        E::Unary(_, a) | E::IsNull(a, _) | E::Cast(a, _) => expr_has_subquery(a),
        E::Binary(_, a, b)
        | E::Like(a, b, _)
        | E::Match(a, b)
        | E::IsDistinct(a, b, _)
        | E::Glob(a, b, _)
        | E::Regexp(a, b, _) => expr_has_subquery(a) || expr_has_subquery(b),
        E::InContext(a, _, _) => expr_has_subquery(a),
        E::Collate(a, _) => expr_has_subquery(a),
        E::InList(a, xs, _) => expr_has_subquery(a) || xs.iter().any(expr_has_subquery),
        E::Coalesce(xs) | E::Func(_, xs) | E::RowValue(xs) => xs.iter().any(expr_has_subquery),
        E::Case(arms, els) => {
            arms.iter()
                .any(|(c, r)| expr_has_subquery(c) || expr_has_subquery(r))
                || els.as_deref().is_some_and(expr_has_subquery)
        }
        E::Agg(_, arg, _, filter, extra) => {
            arg.as_deref().is_some_and(expr_has_subquery)
                || filter.as_deref().is_some_and(expr_has_subquery)
                || extra.iter().any(expr_has_subquery)
        }
        // A subquery inside a window's arg/PARTITION/ORDER is not lifted in
        // stage 1 (the window planner binds those sub-expressions directly); one
        // that appears there is refused by the binder, not lifted here.
        E::Window { .. } => false,
        E::Lit(_) | E::Param(_) | E::Col(_) | E::ContextRef(_) | E::Excluded(_)
        | E::Qualified(..) => false,
    }
}

/// The FROM scope of a SELECT — its table plus any joined tables, each addressed
/// by alias. A FROM-less SELECT (`SELECT 3`) yields an EMPTY scope, which
/// resolves nothing (so nothing can correlate against it). Shared by the outer
/// scope of `lift_subqueries`, the inner scope of `plan_one`, and — for stage 3
/// — the scopes `Correlate` pushes as it descends into nested subqueries.
fn stmt_scope<'s>(schema: &'s Schema, s: &ast::SelectStmt) -> Result<Scope<'s>> {
    let mut named: Vec<(String, &TableDef)> = Vec::new();
    if let Some(t) = &s.table {
        let (_, it) = resolve_table(schema, t)?;
        named.push((s.alias.clone().unwrap_or_else(|| t.clone()), it));
    }
    for j in &s.joins {
        let (_, jt) = resolve_table(schema, &j.table)?;
        named.push((j.alias.clone().unwrap_or_else(|| j.table.clone()), jt));
    }
    Scope::joined_named(named)
}

/// Lift every subquery out of `s`. `n_params` is the user parameter count;
/// subplan result slots are allocated at `n_params + i` (the binder is later
/// created with `n_params + subplans.len()` slots, and context slots append
/// after — the `[user ‖ sub ‖ context]` layout).
#[allow(clippy::too_many_arguments)]
pub(super) fn lift_subqueries<'a>(
    s: &ast::SelectStmt,
    schema: &'a Schema,
    n_params: u16,
    catalog: &'a PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &'a HostUdfSet,
    row_count: RowCountFn<'a>,
    consts: &'a mut Vec<Value>,
) -> Result<Lifted> {
    // The OUTER scope, for correlation: the same `[table0 ‖ … ‖ tableN]`
    // tuple the outer statement's own expressions bind over. FROM-less outer:
    // an EMPTY scope — with no outer columns, nothing can correlate, and every
    // unresolved name inside a subquery stays that subquery's own error.
    let outer_scope = stmt_scope(schema, s)?;

    let mut lift = Lift {
        schema,
        n_params,
        catalog,
        mode,
        host_udfs,
        row_count,
        consts,
        outer_scope,
        subplans: Vec::new(),
        slot_types: Vec::new(),
    };
    let stmt = ast::SelectStmt {
        table: s.table.clone(),
        from_derived: None,
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
                    using: j.using.clone(),
                    natural: j.natural,
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
        // GROUP BY keys lift like any other clause (#97). A key is computed PER
        // ROW in `exec_aggregate`'s loop, against that row's filled scratch, so
        // both an uncorrelated subquery (filled once, before dispatch) and a
        // correlated one (filled per row) are meaningful there. An ordinal
        // (`GROUP BY 1`) is a literal and rewrites to itself, so the
        // select-item/key AST match `lift_aggs` performs is unaffected.
        //
        // NOTE the one shape this deliberately does NOT unify: writing the SAME
        // subquery in both the select list and the GROUP BY (`SELECT (S), … GROUP
        // BY (S)`) lifts it TWICE, into two distinct slots, so `lift_aggs`'s AST
        // match sees `Param(0)` vs `Param(1)`, the item is not recognised as the
        // key, and it is refused as a grouped projection reading a correlated
        // slot. A clean refusal — and `GROUP BY 1`, which every ORM emits, takes
        // the matching path.
        group_by: s
            .group_by
            .iter()
            .map(|e| lift.rewrite(e))
            .collect::<Result<_>>()?,
        having: match &s.having {
            // HAVING runs over the GROUPED tuple, inside the aggregate phase,
            // which no per-row slot fill reaches. An UNCORRELATED subquery does
            // not need one: `exec_stmt_impl` fills every uncorrelated result
            // slot once, before dispatch, for every statement kind — so by the
            // time HAVING is evaluated the slot holds the answer, and the
            // predicate sees an ordinary parameter. A CORRELATED one is refused
            // by name: its slot would still be holding whatever the last base
            // row put there, which is a wrong answer rather than a refusal.
            Some(h) if expr_has_subquery(h) => {
                let before = lift.subplans.len();
                let rewritten = lift.rewrite(h)?;
                if lift.subplans[before..].iter().any(|p| !p.outer_args.is_empty()) {
                    return Err(bind_err(
                        "a CORRELATED subquery in HAVING is not supported yet — HAVING is \
                         evaluated over the collapsed group, where no single row's \
                         correlation applies",
                    ));
                }
                Some(rewritten)
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

/// Lift every subquery out of an UPDATE/DELETE `WHERE` clause (#97).
///
/// The write planners bind their `WHERE` directly — no lift ran, so a
/// `(SELECT …)` reached the binder and was refused ("this expression position
/// does not support subqueries yet"). This is the same lift `plan_select`
/// performs, applied to the one expression a DML statement has: each subquery
/// becomes a [`SubPlan`] on the plan and is replaced by `Param(slot)`, so the
/// write planner's `extract_access` / `compile_program` see only a parameter
/// and need no change at all. `exec_stmt_impl` already fills every UNCORRELATED
/// result slot once, before dispatch, for EVERY statement kind — so the
/// executor needs no change either.
///
/// **Uncorrelated only, and that is load-bearing.** `outer_scope` is the write
/// target's own row, so a reference to it RESOLVES here and is refused BY NAME
/// instead of silently becoming an "unknown column" inside the subquery. A
/// correlated DML subquery would need the per-row fill (`post_filter`) that
/// only the SELECT executor has; the write path has no such phase, so admitting
/// one would read an unfilled hole. Refused, never answered.
///
/// **Snapshot semantics.** An uncorrelated subplan is evaluated ONCE, before
/// the write begins, against the transaction's stable MVCC snapshot. So
/// `DELETE FROM t WHERE id IN (SELECT id FROM t WHERE …)` — a subquery over the
/// very table being written — reads the PRE-write state, which is both SQL's
/// rule and what sqlite does (it materializes the `IN` set into an ephemeral
/// index first). The Halloween problem cannot arise.
#[allow(clippy::too_many_arguments)]
pub(super) fn lift_dml_where<'a>(
    where_clause: &ast::Expr,
    target: &'a TableDef,
    target_name: &str,
    schema: &'a Schema,
    n_params: u16,
    catalog: &'a PolicyCatalog,
    mode: BareGroupBy,
    host_udfs: &'a HostUdfSet,
    row_count: RowCountFn<'a>,
    consts: &'a mut Vec<Value>,
    op: &str,
) -> Result<(ast::Expr, Vec<SubPlan>, Vec<Ty>)> {
    let mut lift = Lift {
        schema,
        n_params,
        catalog,
        mode,
        host_udfs,
        row_count,
        consts,
        outer_scope: Scope::single_named(target_name.to_string(), target),
        subplans: Vec::new(),
        slot_types: Vec::new(),
    };
    let rewritten = lift.rewrite(where_clause)?;
    if lift.subplans.iter().any(|s| !s.outer_args.is_empty()) {
        return Err(bind_err(format!(
            "a correlated subquery in {op} … WHERE is not supported yet — the \
             correlated value is filled per row, which only the SELECT path does"
        )));
    }
    Ok((rewritten, lift.subplans, lift.slot_types))
}

struct Lift<'a> {
    schema: &'a Schema,
    n_params: u16,
    catalog: &'a PolicyCatalog,
    /// GROUP BY strictness dialect (COMPAT.md), carried so a subquery's own
    /// aggregate is planned under the SAME mode as the outer statement.
    mode: BareGroupBy,
    /// Host-registered scalar UDFs (design/DESIGN-UDF.md), carried so a subquery
    /// can call the same UDFs as the outer statement.
    host_udfs: &'a HostUdfSet,
    /// Catalog row counts for the MPEE join solver, carried so a subquery
    /// body's own join chain is solved with the same inputs as the outer
    /// statement's (design/DESIGN-MPEE-SOLVER.md §5).
    row_count: RowCountFn<'a>,
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
            E::Cast(a, t) => E::Cast(Box::new(self.rewrite(a)?), t.clone()),
            E::Binary(op, a, b) => E::Binary(
                *op,
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
            ),
            E::Like(a, b, esc) => {
                E::Like(Box::new(self.rewrite(a)?), Box::new(self.rewrite(b)?), *esc)
            }
            E::Match(a, b) => E::Match(Box::new(self.rewrite(a)?), Box::new(self.rewrite(b)?)),
            E::IsDistinct(a, b, n) => E::IsDistinct(
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
                *n,
            ),
            E::Glob(a, b, n) => E::Glob(
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
                *n,
            ),
            E::Regexp(a, b, n) => E::Regexp(
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
                *n,
            ),
            E::InContext(a, k, n) => {
                E::InContext(Box::new(self.rewrite(a)?), k.clone(), *n)
            }
            E::Collate(a, name) => E::Collate(Box::new(self.rewrite(a)?), name.clone()),
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
            // A row value's elements can themselves contain subqueries — rewrite
            // each (the desugar to scalar boolean logic happens later, in the
            // binder). The RowValue node survives the lift untouched otherwise.
            E::RowValue(xs) => {
                E::RowValue(xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?)
            }
            E::Coalesce(xs) => {
                E::Coalesce(xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?)
            }
            E::Func(f, xs) => {
                // The JSON functions that take VALUE arguments read sqlite's
                // per-value JSON subtype, which propagates out of a scalar
                // subquery. This is the LAST place that shape is visible —
                // after the lift, the subquery is a reserved parameter the
                // binder cannot tell from a user one.
                crate::binder::reject_subquery_in_json_value(f, xs)?;
                E::Func(
                    f.clone(),
                    xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?,
                )
            }
            E::Case(arms, els) => E::Case(
                arms.iter()
                    .map(|(c, r)| Ok((self.rewrite(c)?, self.rewrite(r)?)))
                    .collect::<Result<_>>()?,
                match els {
                    Some(x) => Some(Box::new(self.rewrite(x)?)),
                    None => None,
                },
            ),
            E::Agg(f, arg, d, filter, extra) => E::Agg(
                f.clone(),
                match arg {
                    Some(a) => Some(Box::new(self.rewrite(a)?)),
                    None => None,
                },
                *d,
                // A subquery inside `FILTER (WHERE …)` lifts exactly like one in
                // the aggregate argument.
                match filter {
                    Some(a) => Some(Box::new(self.rewrite(a)?)),
                    None => None,
                },
                // …and so does one in a host aggregate's later arguments.
                extra.iter().map(|x| self.rewrite(x)).collect::<Result<Vec<_>>>()?,
            ),
            // Windows are not descended into for subquery lifting (stage 1); a
            // subquery inside one reaches the binder's refusal unchanged.
            other @ E::Window { .. } => other.clone(),
            other @ (E::Lit(_) | E::Param(_) | E::Col(_) | E::ContextRef(_)
            | E::Excluded(_) | E::Qualified(..)) => other.clone(),
        })
    }

    /// Plan one subquery, dispatching on its body: a plain `SELECT` (the
    /// correlation-aware path below) or a whole compound `SELECT … UNION …`
    /// (#56/format 31, always uncorrelated). Hands back the reserved slot its
    /// result will occupy.
    fn plan_one(&mut self, inner: &ast::SubqueryBody, kind: SubPlanKind) -> Result<u16> {
        if self.subplans.len() >= 16 {
            return Err(bind_err("too many subqueries in one statement (max 16)"));
        }
        match inner {
            ast::SubqueryBody::Select(sel) => self.plan_one_select(sel, kind),
            ast::SubqueryBody::Compound(cs) => self.plan_one_compound(cs, kind),
        }
    }

    /// Plan one lifted subquery whose body is a whole compound (#56/format 31).
    /// A compound subquery body is UNCORRELATED: each arm binds only the outer's
    /// user params, and an outer-column reference inside an arm resolves to
    /// nothing and errors as an unknown column (a correlated compound subquery is
    /// not supported yet — a clean refusal, never a wrong answer). So it is
    /// planned standalone, exactly like a top-level compound, and evaluated ONCE
    /// per execute.
    fn plan_one_compound(&mut self, cs: &ast::CompoundStmt, kind: SubPlanKind) -> Result<u16> {
        let (stmt, _ptypes, ctx, _lists, out, subs) =
            plan_compound(cs, self.schema, self.n_params, self.catalog, self.mode, self.host_udfs, self.row_count, self.consts)?;
        if !ctx.is_empty() {
            return Err(bind_err(
                "current_setting() inside a subquery is not supported yet",
            ));
        }
        // A top-level compound may now carry (uncorrelated) arm subplans, but a
        // compound in a SUBQUERY position may not: its arms' slots were numbered
        // against THIS statement's user params and would collide with the outer
        // lift's own reserved slots. Refuse by name — the format-31 subplan
        // shape (no nested lifts under a compound body) is unchanged.
        if !subs.is_empty() {
            return Err(bind_err(
                "a subquery inside a compound subquery body is not supported yet",
            ));
        }
        let PlanStmt::Compound(mut cp) = stmt else {
            return Err(Error::Internal("compound body planned to a non-compound".into()));
        };
        // A scalar/IN subquery must output exactly one column; EXISTS ignores it.
        if kind != SubPlanKind::Exists && out.len() != 1 {
            return Err(bind_err(match kind {
                SubPlanKind::List => "an IN subquery must select exactly one column",
                _ => "a scalar subquery must select exactly one column",
            }));
        }
        // Consumer cap, mirroring the select path: EXISTS needs one surviving row,
        // a scalar at most two (one value, or two to detect the >1-row error); IN
        // needs every value. Applied to the COMPOUND-level LIMIT, which the
        // executor honors after the set ops — a smaller user LIMIT wins via `min`.
        let cap = match kind {
            SubPlanKind::Exists => Some(1u64),
            SubPlanKind::Scalar => Some(2),
            SubPlanKind::List => None,
        };
        if let Some(cap) = cap {
            cp.limit = Some(cp.limit.map_or(cap, |l| l.min(cap)));
        }
        let ty = match kind {
            SubPlanKind::Exists => Some(ColumnType::Bool),
            SubPlanKind::Scalar => out.first().copied().flatten(),
            // The slot holds a LIST at runtime; membership is runtime-typed.
            SubPlanKind::List => None,
        };
        let slot = self.n_params + self.subplans.len() as u16;
        self.subplans.push(SubPlan {
            body: SubBody::Compound(cp),
            outer_args: Vec::new(),
            kind,
            subplans: Vec::new(),
            // Uncorrelated, no nested lifts ⇒ the reserved region begins right
            // after the user params (mirrors the select path's `inner_n`).
            sub_base: self.n_params,
            slot_type: ty,
        });
        self.slot_types.push(ty);
        Ok(slot)
    }

    /// Plan one subquery whose body is a plain `SELECT`: resolve its correlation
    /// against the outer scope, plan the rewritten inner select, and hand back
    /// the reserved slot its result will occupy.
    fn plan_one_select(&mut self, inner: &ast::SelectStmt, kind: SubPlanKind) -> Result<u16> {
        // #73 §3: a subquery MAY now contain subqueries, and (stage 3) a nested
        // one may correlate to a MIDDLE or the outermost scope, not only its
        // immediate parent. `Correlate` below resolves this subquery's OWN
        // references against the outer scope AND descends into its nested
        // subqueries to collect their references to THIS subquery's parent —
        // TRANSIT correlations this level forwards to the nested level (§3.3).
        // The INNER scope decides which names stay put; what it cannot resolve
        // (here or in a nested subquery) is tried against the OUTER scope and
        // becomes a correlation parameter. Bare names prefer the inner table —
        // SQL's rule. A FROM-less subquery (`SELECT (SELECT 3)`) has an empty
        // inner scope: every name falls through to the outer and correlates, or
        // errors there — the same rule as any other unresolved inner name.
        let inner_scope = stmt_scope(self.schema, inner)?;

        let mut corr = Correlate {
            schema: self.schema,
            inner_scope,
            nested: Vec::new(),
            outer_scope: &self.outer_scope,
            n_params: self.n_params,
            outer_args: Vec::new(),
            arg_types: Vec::new(),
        };
        // MPEE-style pruning: cap each subplan to the minimum rows its CONSUMER
        // can possibly read, so the LIMIT pushdown stops the scan there instead
        // of materializing rows that are then discarded ("don't compute the
        // distances you won't use"). EXISTS needs one surviving row (existence);
        // a scalar subquery needs at most two (one value, or two to detect the
        // >1-row error); `IN` needs every value, so it is uncapped. OFFSET is
        // preserved — the pushdown cap is offset+limit, so existence/value "after
        // the offset" is still computed — and a smaller user LIMIT wins via `min`.
        let consumer_cap = match kind {
            SubPlanKind::Exists => Some(1),
            SubPlanKind::Scalar => Some(2),
            SubPlanKind::List => None,
        };
        let inner_limit = match consumer_cap {
            Some(cap) => Some(inner.limit.map_or(cap, |l| l.min(cap))),
            None => inner.limit,
        };
        // Rewrite every correlation-bearing clause (descending into nested
        // subqueries for transit correlations, §3.3), then apply the
        // consumer-cap LIMIT the un-capped `rewrite_select` leaves untouched.
        let mut rewritten = corr.rewrite_select(inner)?;
        rewritten.limit = inner_limit;
        let outer_args = corr.outer_args;
        let arg_types = corr.arg_types;

        // Plan the inner with its own parameter space: user params, then one
        // slot per correlation arg. Its context keys are refused (the
        // reserved-slot layouts would have to be reconciled across levels).
        let inner_n = self.n_params + outer_args.len() as u16;
        let (stmt, inner_ptypes, inner_ctx, _inner_lists, inner_out, inner_subs) =
            plan_select(&rewritten, self.schema, inner_n, self.catalog, self.mode, self.host_udfs, self.row_count, self.consts, None)?;
        // #73 §3 stage 3: a nested subquery may correlate to its IMMEDIATE
        // parent (stage 2), to a MIDDLE scope, or to the OUTERMOST scope. A
        // reference to a non-immediate ancestor was captured above as a TRANSIT
        // correlation arg of THIS subquery (`Correlate::descend` turned the
        // nested reference into a `Param` pointing into this subplan's own
        // correlation region and registered the source column in `outer_args`).
        // At exec, this subplan is filled per parent row, its correlation region
        // — INCLUDING the transit values — is inherited by the nested subplan's
        // param buffer, and the nested level reads the ancestor value as a plain
        // (already-filled) param. A child correlated to THIS row rides the
        // recursive per-row fill exactly as in stage 2; `plan.post_filter`
        // (from `split_correlated`) carries the correlated WHERE conjunct.
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
        // (#97) A CORRELATED `IN (SELECT …)` was refused here — "rewrite as
        // EXISTS" — since #70, when the List kind landed before the per-row fill
        // existed. It needs nothing the other kinds do not: `split_correlated`
        // already classifies `BExpr::InParam(_, slot)` as a correlated reference
        // and routes the conjunct into `post_filter`; the executor's per-row
        // phase fills a List slot with the same `subplan_value` call it uses for
        // Exists/Scalar, memoized by the same correlation tuple; `validate`'s
        // `gather_ok` already treats `Instr::InParam` as a slot read, so the
        // gather-side discipline covers it. The refusal is gone; the shape is
        // sqlite-differentially tested in `correlated_in.rs`.
        let ty = match kind {
            SubPlanKind::Exists => Some(ColumnType::Bool),
            SubPlanKind::Scalar => inner_out.first().copied().flatten(),
            // The slot holds a LIST at runtime; pinning a scalar type on it
            // would make resolve reject the fill. Membership is runtime-typed
            // (the same 3VL core session-context lists use).
            SubPlanKind::List => None,
        };
        let slot = self.n_params + self.subplans.len() as u16;
        // `sub_base = inner_n`: the inner was planned with `[user ‖ correlation]`
        // as its param space, and its OWN lifts (`inner_subs`) sit right after —
        // at `inner_n + i` — exactly the "results after user + trailing reserved"
        // shape the top level uses one layer up.
        self.subplans.push(SubPlan {
            body: SubBody::Select(plan),
            outer_args,
            kind,
            subplans: inner_subs,
            sub_base: inner_n,
            slot_type: ty,
        });
        self.slot_types.push(ty);
        Ok(slot)
    }
}

/// Rewrites OUTER references inside a subquery into correlation parameters.
///
/// **Stage 3 (#73 §3).** `rewrite` descends into NESTED subqueries to capture
/// their references to THIS subquery's parent (`outer_scope`) — a correlation
/// that skips the intervening level(s). Such a reference becomes an
/// `outer_arg`/`Param` of THIS subquery exactly like a direct one: the executor
/// pulls the ancestor column into this subplan's correlation region per parent
/// row, and the nested subplan inherits it in its param buffer and reads it as a
/// plain (already-filled) param. `nested` is the stack of scopes introduced by
/// the subqueries we are currently descending through; a name resolvable in
/// `inner_scope` OR any `nested` scope is bound at this level or a deeper one and
/// is left for that level's own lift, so only a name bound by NEITHER, yet
/// resolvable in `outer_scope`, is a (possibly transit) correlation.
struct Correlate<'a, 'b> {
    schema: &'a Schema,
    inner_scope: Scope<'a>,
    /// Scopes of the nested subqueries currently being descended through
    /// (innermost last). Empty while rewriting this subquery's OWN clauses.
    nested: Vec<Scope<'a>>,
    outer_scope: &'b Scope<'a>,
    n_params: u16,
    /// Outer base-row slots, one per correlation parameter, in slot order.
    outer_args: Vec<u16>,
    arg_types: Vec<ColumnType>,
}

impl<'a> Correlate<'a, '_> {
    fn arg_param(&mut self, outer_slot: u16, ty: ColumnType) -> ast::Expr {
        // The same outer slot referenced twice is ONE parameter. This dedup is
        // what makes a column referenced BOTH directly by this subquery and by a
        // transit from a nested one collapse to a single correlation arg — and,
        // crucially, `arg_param` registers AND returns the slot in one step, so
        // direct and transit references are numbered consistently with no
        // separate collection pass to drift.
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

    /// Is this unqualified name bound at THIS subquery's level or a nested one
    /// we are descending through? Then it is NOT a correlation to the outer.
    fn bound_here(&self, name: &str) -> bool {
        self.inner_scope.resolve(name).is_ok()
            || self.nested.iter().any(|s| s.resolve(name).is_ok())
    }

    fn bound_here_qualified(&self, qual: &str, name: &str) -> bool {
        self.inner_scope.resolve_qualified(qual, name).is_ok()
            || self
                .nested
                .iter()
                .any(|s| s.resolve_qualified(qual, name).is_ok())
    }

    /// Rewrite every correlation-bearing clause of `s` (items, join `ON`s,
    /// WHERE, GROUP BY, HAVING, ORDER BY) with the current correlation state.
    /// `limit`/`offset` carry no expressions and are copied verbatim (the
    /// consumer-cap is applied by the caller). Used both for this subquery's own
    /// clauses (`plan_one`) and, recursively, for a nested subquery's clauses
    /// while descending (`descend`).
    fn rewrite_select(&mut self, s: &ast::SelectStmt) -> Result<ast::SelectStmt> {
        Ok(ast::SelectStmt {
            table: s.table.clone(),
            from_derived: None,
            alias: s.alias.clone(),
            joins: s
                .joins
                .iter()
                .map(|j| {
                    Ok(ast::JoinClause {
                        table: j.table.clone(),
                        alias: j.alias.clone(),
                        kind: j.kind,
                        on: self.rewrite(&j.on)?,
                        using: j.using.clone(),
                        natural: j.natural,
                    })
                })
                .collect::<Result<_>>()?,
            distinct: s.distinct,
            items: match &s.items {
                None => None,
                Some(items) => Some(
                    items
                        .iter()
                        .map(|(e, a)| Ok((self.rewrite(e)?, a.clone())))
                        .collect::<Result<_>>()?,
                ),
            },
            where_clause: s.where_clause.as_ref().map(|e| self.rewrite(e)).transpose()?,
            group_by: s.group_by.iter().map(|e| self.rewrite(e)).collect::<Result<_>>()?,
            having: s.having.as_ref().map(|e| self.rewrite(e)).transpose()?,
            order_by: s
                .order_by
                .iter()
                .map(|(e, d)| Ok((self.rewrite(e)?, *d)))
                .collect::<Result<_>>()?,
            limit: s.limit,
            offset: s.offset,
        })
    }

    /// Descend INTO a nested subquery (#73 §3 stage 3) to capture TRANSIT
    /// correlations: a reference inside `inner` (or deeper still) that resolves
    /// to THIS subquery's parent — skipping every level in between — becomes a
    /// correlation arg of THIS subquery, so the executor threads the value down
    /// through the intervening level. `inner`'s own tables join the `nested`
    /// bound-here set while we recurse, so a reference to `inner` itself, or to
    /// the intervening level, stays put and is resolved at its own level's lift.
    /// The rewritten `inner` (with ancestor references turned into params) is
    /// then lifted as usual by the intervening level's own `plan_select`.
    fn descend(&mut self, inner: &ast::SelectStmt) -> Result<ast::SelectStmt> {
        let ns = stmt_scope(self.schema, inner)?;
        self.nested.push(ns);
        let out = self.rewrite_select(inner);
        self.nested.pop();
        out
    }

    /// Descend into a nested subquery BODY. A plain `SELECT` is descended into
    /// for transit correlations (as above); a compound body (#56/format 31) is
    /// UNCORRELATED and carries no reference to an enclosing row, so there is
    /// nothing to capture — it is left verbatim, and the compound is lifted one
    /// level down by that level's own `plan_one_compound`.
    fn descend_body(&mut self, inner: &ast::SubqueryBody) -> Result<ast::SubqueryBody> {
        Ok(match inner {
            ast::SubqueryBody::Select(sel) => ast::SubqueryBody::Select(self.descend(sel)?),
            ast::SubqueryBody::Compound(cs) => ast::SubqueryBody::Compound(cs.clone()),
        })
    }

    fn rewrite(&mut self, e: &ast::Expr) -> Result<ast::Expr> {
        use ast::Expr as E;
        Ok(match e {
            // The names are the whole point. Inner resolution wins (SQL's
            // innermost-scope rule), and a name bound in a nested subquery we are
            // descending through is likewise NOT ours; only a name bound by no
            // inner-or-nested scope is tried against the outer row and becomes a
            // (possibly transit) correlation parameter.
            E::Col(n) => {
                if self.bound_here(n) {
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
                if self.bound_here_qualified(q, n) {
                    e.clone()
                } else if let Ok((slot, ty)) = self.outer_scope.resolve_qualified(q, n) {
                    self.arg_param(slot, ty)
                } else {
                    e.clone()
                }
            }
            // A subquery nested inside THIS subquery: DESCEND (#73 §3 stage 3) to
            // capture any reference it (or a deeper subquery) makes to THIS
            // subquery's parent as a transit correlation of this level. The
            // nested SELECT itself is still lifted one level down, by the
            // intervening level's own `plan_select` — descent only rewrites the
            // ancestor references it carries, leaving references to the nested or
            // intervening levels for those levels' own `Correlate`.
            E::Subquery(inner) => E::Subquery(Box::new(self.descend_body(inner)?)),
            E::Exists(inner, negated) => E::Exists(Box::new(self.descend_body(inner)?), *negated),
            E::Unary(op, a) => E::Unary(*op, Box::new(self.rewrite(a)?)),
            E::IsNull(a, n) => E::IsNull(Box::new(self.rewrite(a)?), *n),
            E::Cast(a, t) => E::Cast(Box::new(self.rewrite(a)?), t.clone()),
            E::Binary(op, a, b) => E::Binary(
                *op,
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
            ),
            E::Like(a, b, esc) => {
                E::Like(Box::new(self.rewrite(a)?), Box::new(self.rewrite(b)?), *esc)
            }
            E::Match(a, b) => E::Match(Box::new(self.rewrite(a)?), Box::new(self.rewrite(b)?)),
            E::IsDistinct(a, b, n) => E::IsDistinct(
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
                *n,
            ),
            E::Glob(a, b, n) => E::Glob(
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
                *n,
            ),
            E::Regexp(a, b, n) => E::Regexp(
                Box::new(self.rewrite(a)?),
                Box::new(self.rewrite(b)?),
                *n,
            ),
            E::InContext(a, k, n) => {
                E::InContext(Box::new(self.rewrite(a)?), k.clone(), *n)
            }
            E::Collate(a, name) => E::Collate(Box::new(self.rewrite(a)?), name.clone()),
            // `x IN (SELECT …)` nested inside this subquery: rewrite the LHS (it
            // lives in the inner's scope, so it may correlate to the outer) and
            // DESCEND into the nested SELECT for transit correlations — same rule
            // as `Subquery`/`Exists` above.
            E::InSubquery(lhs, inner, negated) => E::InSubquery(
                Box::new(self.rewrite(lhs)?),
                Box::new(self.descend_body(inner)?),
                *negated,
            ),
            E::InParamSlot(a, slot, negated) => {
                E::InParamSlot(Box::new(self.rewrite(a)?), *slot, *negated)
            }
            E::InList(a, xs, n) => E::InList(
                Box::new(self.rewrite(a)?),
                xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?,
                *n,
            ),
            // A row value's elements can themselves contain subqueries — rewrite
            // each (the desugar to scalar boolean logic happens later, in the
            // binder). The RowValue node survives the lift untouched otherwise.
            E::RowValue(xs) => {
                E::RowValue(xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?)
            }
            E::Coalesce(xs) => {
                E::Coalesce(xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?)
            }
            E::Func(f, xs) => {
                // The JSON functions that take VALUE arguments read sqlite's
                // per-value JSON subtype, which propagates out of a scalar
                // subquery. This is the LAST place that shape is visible —
                // after the lift, the subquery is a reserved parameter the
                // binder cannot tell from a user one.
                crate::binder::reject_subquery_in_json_value(f, xs)?;
                E::Func(
                    f.clone(),
                    xs.iter().map(|x| self.rewrite(x)).collect::<Result<_>>()?,
                )
            }
            E::Case(arms, els) => E::Case(
                arms.iter()
                    .map(|(c, r)| Ok((self.rewrite(c)?, self.rewrite(r)?)))
                    .collect::<Result<_>>()?,
                match els {
                    Some(x) => Some(Box::new(self.rewrite(x)?)),
                    None => None,
                },
            ),
            E::Agg(f, arg, d, filter, extra) => E::Agg(
                f.clone(),
                match arg {
                    Some(a) => Some(Box::new(self.rewrite(a)?)),
                    None => None,
                },
                *d,
                // A subquery inside `FILTER (WHERE …)` lifts exactly like one in
                // the aggregate argument.
                match filter {
                    Some(a) => Some(Box::new(self.rewrite(a)?)),
                    None => None,
                },
                // …and so does one in a host aggregate's later arguments.
                extra.iter().map(|x| self.rewrite(x)).collect::<Result<Vec<_>>>()?,
            ),
            // A window is not descended into for correlation rewriting (stage 1);
            // a window inside a subquery that references an enclosing row reaches
            // the binder's "unknown column" / window refusal unchanged.
            other @ E::Window { .. } => other.clone(),
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
        BExpr::Unary(_, a)
        | BExpr::Like(a, _, _, _)
        | BExpr::Glob(a, _)
        | BExpr::Regexp(a, _)
        | BExpr::Cast(a, _) => refs_correlated(a, sub_base, correlated),
        BExpr::Binary(_, a, bx)
        | BExpr::IsDistinct(a, bx, _)
        | BExpr::CollateCmp(_, a, bx, _)
        | BExpr::RegexpDyn(a, bx)
        | BExpr::LikeDyn(a, bx, _, _)
        | BExpr::GlobDyn(a, bx)
        | BExpr::ClassCmp(_, a, bx, _, _) => {
            refs_correlated(a, sub_base, correlated) || refs_correlated(bx, sub_base, correlated)
        }
        BExpr::InParam(a, i) => is_corr(*i) || refs_correlated(a, sub_base, correlated),
        BExpr::InList(a, xs) | BExpr::InListColl(a, xs, _) => {
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
        BExpr::Call(_, xs) | BExpr::Coalesce(xs) | BExpr::HostCall { args: xs, .. } => {
            xs.iter().any(|x| refs_correlated(x, sub_base, correlated))
        }
    }
}
