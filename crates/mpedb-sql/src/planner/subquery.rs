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
use crate::plan::LimitVal;

/// A lifted WHERE clause: the rewritten expression, its subplans, and — per
/// subplan result slot — the pinned output type and the rung-4 collation.
pub(super) type LiftedWhere = (ast::Expr, Vec<SubPlan>, Vec<Ty>, Vec<Option<Collation>>);

/// Everything `lift_subqueries` learned about one statement.
pub(super) struct Lifted {
    /// The statement with every subquery replaced by `Param(slot)`.
    pub stmt: ast::SelectStmt,
    pub subplans: Vec<SubPlan>,
    /// Output type of each subplan's result slot, parallel to `subplans`
    /// (`None` when the inner output type could not be pinned).
    pub slot_types: Vec<Ty>,
    /// Declared collation of each subplan's output column, parallel to
    /// `subplans`. sqlite's rung 4 — see `Lift::body_collation`.
    pub slot_colls: Vec<Option<Collation>>,
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
        E::Agg(_, arg, _, filter, extra, _) => {
            arg.as_deref().is_some_and(expr_has_subquery)
                || filter.as_deref().is_some_and(expr_has_subquery)
                || extra.iter().any(expr_has_subquery)
        }
        // A subquery inside a window's arg/PARTITION/ORDER is not lifted in
        // stage 1 (the window planner binds those sub-expressions directly); one
        // that appears there is refused by the binder, not lifted here.
        E::Window { .. } => false,
        E::TableStar(_)
        | E::Lit(_) | E::Param(_) | E::Col(..) | E::ContextRef(_) | E::Excluded(_)
        | E::Qualified(..) | E::Raise(..) => false,
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
    mode: Dialect,
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
        slot_colls: Vec::new(),
    };
    let stmt = ast::SelectStmt {
        table: s.table.clone(),
        // Carried for the same reason `Correlate::rewrite_select` carries it.
        from_derived: s.from_derived.clone(),
        from_series: s.from_series.clone(),
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
                    derived: None,
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
            // HAVING runs over the GROUPED tuple. Uncorrelated subqueries are
            // filled once before dispatch. Correlated ones lift normally; the
            // executor supplies the group's first-row param scratch so OuterRef
            // on a group key (Django) is correct.
            Some(h) if expr_has_subquery(h) => Some(lift.rewrite(h)?),
            other => other.clone(),
        },
        order_by: s
            .order_by
            .iter()
            .map(|(e, d)| Ok((lift.rewrite(e)?, *d)))
            .collect::<Result<_>>()?,
        limit: s.limit,
        offset: s.offset,
        drop_trailing: s.drop_trailing,
    };
    Ok(Lifted {
        stmt,
        subplans: lift.subplans,
        slot_types: lift.slot_types,
        slot_colls: lift.slot_colls,
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
    mode: Dialect,
    host_udfs: &'a HostUdfSet,
    row_count: RowCountFn<'a>,
    consts: &'a mut Vec<Value>,
    op: &str,
) -> Result<LiftedWhere> {
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
        slot_colls: Vec::new(),
    };
    let _ = op;
    // A CORRELATED subplan is no longer refused here: the caller splits the
    // bound predicate with `split_correlated` — the same function the SELECT
    // path uses — so the correlated conjuncts become a per-row residual and
    // never reach the access-path extractor.
    let rewritten = lift.rewrite(where_clause)?;
    Ok((rewritten, lift.subplans, lift.slot_types, lift.slot_colls))
}

struct Lift<'a> {
    schema: &'a Schema,
    n_params: u16,
    catalog: &'a PolicyCatalog,
    /// GROUP BY strictness dialect (COMPAT.md), carried so a subquery's own
    /// aggregate is planned under the SAME mode as the outer statement.
    mode: Dialect,
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
    /// (see `Lifted::slot_colls`)
    /// The DECLARED collation each subplan's output column carries, parallel to
    /// `slot_types`. sqlite's collation rung 4: an `IN (SELECT …)` whose PROBE
    /// supplies no collation takes the SUBQUERY's. Measured over 62 forms —
    /// 21 of them were live wrong answers before this existed.
    slot_colls: Vec<Option<Collation>>,
}

impl Lift<'_> {
    /// Rung 4: the collation of a lifted subquery's single output column.
    ///
    /// `None` means Binary, and `None` is the answer for anything this cannot
    /// resolve with certainty — an unresolvable form keeps today's behaviour,
    /// so widening the table later can only ever REMOVE a wrong answer, never
    /// introduce one.
    ///
    /// Every rule here is MEASURED against 3.45.1, and two of them are
    /// counterintuitive enough that reasoning would have got them wrong:
    ///
    /// * `CAST(a AS TEXT)` KEEPS the collation and so does unary `+a` (sqlite
    ///   descends TK_CAST/TK_UPLUS in `sqlite3ExprCollSeq`), while `a || ''`
    ///   LOSES it. A whitelist built from "expressions lose it" is wrong in the
    ///   answering-more direction. (Unary plus needs no arm — the parser folds
    ///   it away, so it arrives as the bare column.)
    /// * The compound rule INVERTS with nesting: a BARE compound takes its LAST
    ///   arm's collation, a DERIVED-WRAPPED one takes its FIRST. Measured
    ///   two-arm and three-arm, both directions, EXCEPT included.
    fn body_collation(&self, body: &ast::SubqueryBody, nested: bool, depth: u32) -> Option<Collation> {
        if depth > crate::view::MAX_VIEW_DEPTH as u32 {
            return None;
        }
        match body {
            ast::SubqueryBody::Select(s) => self.select_collation(s, depth),
            ast::SubqueryBody::Compound(c) => {
                let arm = if nested { c.arms.first() } else { c.arms.last() };
                self.select_collation(arm?, depth + 1)
            }
        }
    }

    fn select_collation(&self, s: &ast::SelectStmt, depth: u32) -> Option<Collation> {
        if depth > crate::view::MAX_VIEW_DEPTH as u32 {
            return None;
        }
        // `SELECT *` and multi-column projections: not a single-column probe
        // target this path can reason about.
        let items = s.items.as_ref()?;
        if items.len() != 1 {
            return None;
        }
        let (qual, name) = peel_to_column(&items[0].0)?;
        // A derived FROM has ONE output (checked above), so the outer's single
        // reference must be to it; take the body's own collation.
        if let Some(d) = &s.from_derived {
            return self.body_collation(d, true, depth + 1);
        }
        // Resolve the name across the FROM table and every join operand. An
        // AMBIGUOUS name yields None — the binder reports it far better, and a
        // guess here is the one thing this must not do.
        let mut found: Option<Collation> = None;
        let mut candidates: Vec<(Option<&str>, &str)> = Vec::new();
        if let Some(t) = &s.table {
            candidates.push((s.alias.as_deref(), t.as_str()));
        }
        for j in &s.joins {
            candidates.push((j.alias.as_deref(), j.table.as_str()));
        }
        for (alias, tname) in candidates {
            if let Some(q) = qual {
                let addressed = alias.unwrap_or(tname);
                if !mpedb_types::ident_eq(addressed, q) {
                    continue;
                }
            }
            let Some(def) = self
                .schema
                .table_id(tname)
                .and_then(|id| self.schema.table(id))
            else {
                continue;
            };
            let Some(i) = def.column_index(name) else { continue };
            let c = def.columns[i as usize].collation;
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(c);
        }
        found
    }
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
            E::Binary(op, a, b) => {
                // `(a, b) = (SELECT x, y …)` / `<>`, either way round —
                // rewritten so the comparison moves INTO the subquery's
                // projection (see `row_value_eq_subquery`); the result is an
                // ordinary scalar subquery the arm above then lifts.
                if matches!(op, ast::BinOp::Eq | ast::BinOp::Ne) {
                    let hit = match (a.as_ref(), b.as_ref()) {
                        (E::RowValue(_), E::Subquery(inner)) => Some((a.as_ref(), inner, true)),
                        (E::Subquery(inner), E::RowValue(_)) => Some((b.as_ref(), inner, false)),
                        _ => None,
                    };
                    if let Some((probe, inner, probe_on_left)) = hit {
                        if let Some(rewritten) = row_value_eq_subquery(
                            *op,
                            probe_on_left,
                            probe,
                            inner,
                            &self.outer_scope,
                        )? {
                            return self.rewrite(&rewritten);
                        }
                    }
                }
                E::Binary(*op, Box::new(self.rewrite(a)?), Box::new(self.rewrite(b)?))
            }
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
                // A ROW-VALUE probe against a subquery — `(a, b) IN (SELECT x, y
                // FROM s)`. The `List` subplan carries ONE column, so this used
                // to be refused; it is rewritten instead, into the two
                // correlated `EXISTS` that ARE its 3-valued definition. No plan
                // format and no executor change: the pieces already exist.
                if let E::RowValue(_) = lhs.as_ref() {
                    if let Some(rewritten) =
                        row_value_in_subquery(lhs, inner, *negated, &self.outer_scope)?
                    {
                        return self.rewrite(&rewritten);
                    }
                }
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
            E::Agg(f, arg, d, filter, extra, ob) => E::Agg(
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
                // An AGGREGATE ORDER BY key is an ordinary expression over the
                // same base row, so a subquery in one lifts like any other.
                ob.iter()
                    .map(|(e, d)| Ok((self.rewrite(e)?, *d)))
                    .collect::<Result<Vec<_>>>()?,
            ),
            // Windows are not descended into for subquery lifting (stage 1); a
            // subquery inside one reaches the binder's refusal unchanged.
            other @ E::Window { .. } => other.clone(),
            other @ (E::TableStar(_) | E::Lit(_) | E::Param(_) | E::Col(..) | E::ContextRef(_)
            | E::Excluded(_) | E::Qualified(..) | E::Raise(..)) => other.clone(),
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
    ///
    /// **Correlation (format 56).** A compound body used to be UNCORRELATED by
    /// construction: its arms were planned standalone, so an outer-column
    /// reference inside an arm resolved to nothing and errored as
    /// `no table named V0 in this statement`. It now correlates exactly as a
    /// plain-SELECT body does, and for the same reason the arm-owned lift works:
    /// the correlation region belongs to the SUBPLAN, not to the compound. Every
    /// arm is rewritten by ONE shared [`Correlate`] — one arg list, deduped by
    /// outer slot, so two arms naming the same outer column share a slot — and
    /// the compound is then planned over the param space
    /// `[user ‖ correlation args]`. At exec the whole compound is run per outer
    /// row by the ordinary correlated fill, with the region ALREADY filled, so
    /// each arm reads it as a plain parameter and nothing inside the compound
    /// needs a per-row phase of its own.
    ///
    /// Each arm's `inner_scope` replaces the previous one while the shared
    /// `outer_args` accumulate: a name an arm binds itself is that arm's, and
    /// only a name no arm binds yet the OUTER scope does becomes a correlation.
    fn plan_one_compound(&mut self, cs: &ast::CompoundStmt, kind: SubPlanKind) -> Result<u16> {
        let mut corr = Correlate {
            schema: self.schema,
            // Replaced per arm below; a compound has no FROM of its own.
            inner_scope: Scope::joined_named(Vec::new())?,
            nested: Vec::new(),
            outer_scope: &self.outer_scope,
            n_params: self.n_params,
            outer_args: Vec::new(),
            arg_types: Vec::new(),
            arg_colls: Vec::new(),
        };
        let mut rewritten = cs.clone();
        for arm in &mut rewritten.arms {
            corr.inner_scope = stmt_scope(self.schema, arm)?;
            *arm = corr.rewrite_select(arm)?;
        }
        let outer_args = corr.outer_args;
        let arg_types = corr.arg_types;
        let inner_n = self.n_params + outer_args.len() as u16;
        let (stmt, inner_ptypes, ctx, _lists, out, subs) =
            plan_compound(&rewritten, self.schema, inner_n, self.catalog, self.mode, self.host_udfs, self.row_count, self.consts)?;
        if !ctx.is_empty() {
            return Err(bind_err(
                "current_setting() inside a subquery is not supported yet",
            ));
        }
        // The arms OWN their lifts (format 56), so `plan_compound` hands back an
        // EMPTY statement-level list — there is nothing here that could collide
        // with this lift's own reserved slots.
        if !subs.is_empty() {
            return Err(Error::Internal("compound arms did not own their lifts".into()));
        }
        let PlanStmt::Compound(mut cp) = stmt else {
            return Err(Error::Internal("compound body planned to a non-compound".into()));
        };
        // The inner binder saw each correlation slot in real use — a type it
        // pinned must MATCH the outer column feeding the slot (the same check
        // the select path makes).
        for (j, &want) in arg_types.iter().enumerate() {
            let slot = self.n_params as usize + j;
            if let Some(Some(t)) = inner_ptypes.get(slot) {
                if *t != want {
                    return Err(bind_err(format!(
                        "correlated reference is {want} in the outer query but used as {t} \
                         inside the subquery"
                    )));
                }
            }
        }
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
            // A parameterized LIMIT cannot fold with the consumer cap at plan
            // time — keep the parameter (correctness beats the cap, which is
            // only an optimization: the consumer stops reading anyway).
            cp.limit = Some(match cp.limit {
                None => LimitVal::Lit(cap),
                Some(LimitVal::Lit(l)) => LimitVal::Lit(l.min(cap)),
                Some(p @ LimitVal::Param(_)) => p,
            });
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
            outer_args,
            kind,
            // The ARMS own the compound's lifts, not this subplan (format 56):
            // `SubPlan::subplans` are filled per row of a SELECT body, which a
            // compound body is not.
            subplans: Vec::new(),
            // `[user ‖ correlation]`, and the arms' own reserved slots start
            // right after — mirrors the select path's `inner_n`.
            sub_base: inner_n,
            slot_type: ty,
        });
        self.slot_types.push(ty);
        // A BARE compound takes its LAST arm's collation (measured).
        self.slot_colls
            .push(cs.arms.last().and_then(|a| self.select_collation(a, 1)));
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
            arg_colls: Vec::new(),
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
            // A parameterized inner LIMIT cannot fold with the consumer cap at
            // plan time — keep the parameter (correctness) and forgo the cap
            // (it was an optimization; the consumer stops reading anyway).
            Some(cap) => Some(match inner.limit {
                None => LimitVal::Lit(cap),
                Some(LimitVal::Lit(l)) => LimitVal::Lit(l.min(cap)),
                Some(p @ LimitVal::Param(_)) => p,
            }),
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
        // A subquery whose FROM is a non-flattenable DERIVED table is
        // materialized, exactly as a compound ARM already is: `plan_select`
        // refuses one it did not intercept, and the intercept only ever ran at
        // the outermost FROM. Nothing about the subplan boundary makes the
        // materialization harder — the body is planned standalone either way —
        // so the refusal here was a missing route, not a missing capability.
        let planned = if rewritten.from_derived.is_some() {
            super::derived::plan_derived_select(
                &rewritten, self.schema, inner_n, self.catalog, self.mode, self.host_udfs,
                self.row_count, self.consts,
            )?
        } else {
            plan_select(&rewritten, self.schema, inner_n, self.catalog, self.mode, self.host_udfs, self.row_count, self.consts, None, &corr.arg_colls)?
        };
        let (stmt, inner_ptypes, inner_ctx, _inner_lists, inner_out, inner_subs) = planned;
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
        // The body is a plain SELECT, or — when the subquery's own FROM had to
        // be materialized — a whole DerivedPlan (format 65). Everything below
        // reads the shape through `out_sel`: a derived's OUTER select is what
        // the subquery projects, exactly as a compound's first arm is.
        let body = match stmt {
            PlanStmt::Select(sp) => SubBody::Select(sp),
            PlanStmt::Derived(dp) => SubBody::Derived(Box::new(dp)),
            _ => return Err(Error::Internal("subquery planned to a non-select".into())),
        };
        let plan: &SelectPlan = match &body {
            SubBody::Select(sp) => sp,
            SubBody::Derived(dp) => &dp.outer,
            SubBody::Compound(_) => unreachable!("compound is not planned here"),
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
            body,
            outer_args,
            kind,
            subplans: inner_subs,
            sub_base: inner_n,
            slot_type: ty,
        });
        self.slot_types.push(ty);
        // Rung 4 read from the ORIGINAL body, not the correlation-rewritten
        // one: the rewrite turns outer references into params, and a param has
        // no declared collation — reading the rewritten form would silently
        // drop the collation for exactly the correlated shapes.
        self.slot_colls.push(self.select_collation(inner, 0));
        Ok(slot)
    }
}

/// The column a rung-4 output expression names, as `(qualifier, name)` — or
/// `None` when the expression is not a bare column reference.
///
/// `CAST` is transparent here because sqlite makes it transparent for
/// collation (measured), which is the opposite of what "a cast produces a new
/// value" suggests. Everything else — concatenation, arithmetic, aggregates,
/// window functions — yields `None`, i.e. Binary, which is also measured.
fn peel_to_column(e: &ast::Expr) -> Option<(Option<&str>, &str)> {
    match e {
        ast::Expr::Col(n, _) => Some((None, n.as_str())),
        ast::Expr::Qualified(q, n) => Some((Some(q.as_str()), n.as_str())),
        ast::Expr::Cast(inner, _) => peel_to_column(inner),
        _ => None,
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
    /// The DECLARED collation of the outer column behind each correlation slot,
    /// parallel to `arg_types`. Handed to the inner binder so a comparison
    /// against the slot keeps the collation the column has — without it the
    /// inner binder sees a bare parameter and the comparison falls to BINARY,
    /// which is a wrong answer for a NOCASE column and not a refusal.
    arg_colls: Vec<Collation>,
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
                self.arg_colls.push(self.outer_scope.column_collation(outer_slot));
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
            // CARRIED, not dropped: a derived FROM source in a nested position
            // is refused by `plan_select`, and it can only refuse what still
            // reaches it. Dropping it turned the statement FROM-less, which is
            // a wrong answer (one synthetic row) rather than a refusal.
            from_derived: s.from_derived.clone(),
            from_series: s.from_series.clone(),
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
                        derived: None,
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
            drop_trailing: s.drop_trailing,
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
    /// for transit correlations (as above); a compound body (format 56) may now
    /// correlate too, so EACH ARM is descended into on its own — an arm binds
    /// its own tables, and a reference it makes to THIS subquery's parent is a
    /// transit correlation of this level exactly as one from a select body is.
    /// The rewritten body is then lifted one level down by that level's own
    /// `plan_one_compound`, which resolves what is left against ITS outer scope.
    fn descend_body(&mut self, inner: &ast::SubqueryBody) -> Result<ast::SubqueryBody> {
        Ok(match inner {
            ast::SubqueryBody::Select(sel) => ast::SubqueryBody::Select(self.descend(sel)?),
            ast::SubqueryBody::Compound(cs) => {
                let mut out = cs.clone();
                for arm in &mut out.arms {
                    *arm = self.descend(arm)?;
                }
                ast::SubqueryBody::Compound(out)
            }
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
            E::Col(n, _) => {
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
            E::Agg(f, arg, d, filter, extra, ob) => E::Agg(
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
                // An AGGREGATE ORDER BY key is an ordinary expression over the
                // same base row, so a subquery in one lifts like any other.
                ob.iter()
                    .map(|(e, d)| Ok((self.rewrite(e)?, *d)))
                    .collect::<Result<Vec<_>>>()?,
            ),
            // A window is not descended into for correlation rewriting (stage 1);
            // a window inside a subquery that references an enclosing row reaches
            // the binder's "unknown column" / window refusal unchanged.
            other @ E::Window { .. } => other.clone(),
            other @ (E::TableStar(_) | E::Lit(_) | E::Param(_) | E::ContextRef(_) | E::Excluded(_)
            | E::Raise(..)) => other.clone(),
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
        | BExpr::Cast(a, _)
        | BExpr::CastPg(a, _) => refs_correlated(a, sub_base, correlated),
        BExpr::Binary(_, a, bx)
        | BExpr::IsDistinct(a, bx, _)
        | BExpr::CollateCmp(_, a, bx, _)
        | BExpr::RegexpDyn(a, bx)
        | BExpr::LikeDyn(a, bx, _, _)
        | BExpr::GlobDyn(a, bx)
        | BExpr::ClassCmp(_, a, bx, _, _) => {
            refs_correlated(a, sub_base, correlated) || refs_correlated(bx, sub_base, correlated)
        }
        BExpr::InParam(a, i) | BExpr::InParamColl(a, i, _) => {
            is_corr(*i) || refs_correlated(a, sub_base, correlated)
        }
        BExpr::InList(a, xs) | BExpr::InListColl(a, xs, _) => {
            refs_correlated(a, sub_base, correlated)
                || xs.iter().any(|x| refs_correlated(x, sub_base, correlated))
        }
        BExpr::ConcatN(xs) => xs.iter().any(|x| refs_correlated(x, sub_base, correlated)),
        BExpr::Case(arms, els) => {
            arms.iter().any(|(c, r)| {
                refs_correlated(c, sub_base, correlated)
                    || refs_correlated(r, sub_base, correlated)
            }) || els
                .as_deref()
                .is_some_and(|e| refs_correlated(e, sub_base, correlated))
        }
        BExpr::Call(_, xs)
        | BExpr::CallColl(_, xs, _)
        | BExpr::Coalesce(xs)
        | BExpr::HostCall { args: xs, .. }
        | BExpr::SpellCall { args: xs, .. } => {
            xs.iter().any(|x| refs_correlated(x, sub_base, correlated))
        }
    }
}

/// Qualify every unqualified column of the PROBE against the OUTER scope, and
/// collect the table qualifiers the result carries — in ONE walk.
///
/// One walk, not two, because the two halves have OPPOSITE safety polarities:
/// the qualifier must cover every node kind or a column slips through bare, and
/// the collector must cover every node kind or the shadow test under-reports.
/// Two lists to keep in sync is precisely the drift this file has been bitten
/// by; a node kind can now only be missing from both at once, which the
/// whitelist below turns into a refusal.
///
/// `Ok(None)` means the probe cannot be spliced safely and the CALLER MUST
/// REFUSE BY NAME. Two ways to get there, and both were measured answering
/// where sqlite refuses:
///
///  * a bare column the outer scope does not bind UNIQUELY. `Scope::owner_name`
///    reports unknown and ambiguous the same way, which is exactly right here:
///    leaving such a name bare lets the SUBQUERY resolve it. With r(a,b) joined
///    to s(a,c), `(a, r.b) IN (SELECT q.a,q.b FROM q)` returned rows where
///    sqlite says "ambiguous column name: a"; with q carrying a column the
///    outer lacks, `(zz, r.b) IN (…)` returned rows where sqlite says "no such
///    column: zz". Both spellings refuse correctly OUTSIDE a row value, so the
///    leak is this splice and nothing else.
///  * any node kind not on the whitelist. The catch-all used to clone the node
///    and trust the shadow test to notice — but the shadow test only sees
///    qualifiers a probe already CARRIES, and an unqualified column inside an
///    unwalked node carries none. A whitelist cannot have that hole: a node
///    this does not understand is a refusal, and adding a kind is a deliberate
///    act with a measurement behind it.
fn qualify_probe(
    e: &ast::Expr,
    outer: &Scope<'_>,
    quals: &mut Vec<String>,
) -> Option<ast::Expr> {
    use ast::Expr as E;
    let sub = |a: &ast::Expr, q: &mut Vec<String>| qualify_probe(a, outer, q);
    Some(match e {
        E::Col(name, _) => {
            let owner = outer.owner_name(name)?;
            quals.push(owner.to_string());
            E::Qualified(owner.to_string(), name.clone())
        }
        E::Qualified(q, n) => {
            quals.push(q.clone());
            E::Qualified(q.clone(), n.clone())
        }
        // Column-free by construction: nothing to qualify, nothing to shadow.
        E::Lit(_) | E::Param(_) => e.clone(),
        E::Binary(op, a, b) => {
            E::Binary(*op, Box::new(sub(a, quals)?), Box::new(sub(b, quals)?))
        }
        E::Unary(op, a) => E::Unary(*op, Box::new(sub(a, quals)?)),
        E::Cast(a, t) => E::Cast(Box::new(sub(a, quals)?), t.clone()),
        E::Collate(a, c) => E::Collate(Box::new(sub(a, quals)?), c.clone()),
        E::Func(n, args) => {
            let mut out = Vec::with_capacity(args.len());
            for a in args {
                out.push(sub(a, quals)?);
            }
            E::Func(n.clone(), out)
        }
        _ => return None,
    })
}

/// `(a, b) [NOT] IN (SELECT x, y FROM s WHERE p)` as the two correlated
/// `EXISTS` that are its definition:
///
/// ```text
/// CASE WHEN EXISTS (SELECT 1 FROM s WHERE p AND (x = a AND y = b))       THEN TRUE
///      WHEN EXISTS (SELECT 1 FROM s WHERE p AND ((x = a AND y = b) IS NULL)) THEN NULL
///      ELSE FALSE END
/// ```
///
/// That IS the 3-valued rule — TRUE if any row matches, else NULL if any row's
/// comparison was NULL, else FALSE — and it is why an `EXISTS` alone would be a
/// WRONG ANSWER here: `EXISTS` collapses the NULL case to FALSE, so a probe
/// carrying a NULL would report "not a member" where sqlite reports unknown.
/// `NOT IN` negates the whole thing, for the same reason the list form does.
///
/// `Ok(None)` — keep the ordinary path and its refusal — when the subquery is
/// not a shape this can move a condition INTO: a compound, an aggregate
/// projection, or anything whose row set the added WHERE would change
/// (DISTINCT, GROUP BY/HAVING, LIMIT/OFFSET). `ORDER BY` is not one of those:
/// `EXISTS` does not care about order.
fn row_value_in_subquery(
    lhs: &ast::Expr,
    inner: &ast::SubqueryBody,
    negated: bool,
    outer: &Scope<'_>,
) -> Result<Option<ast::Expr>> {
    use ast::{BinOp, Expr, SubqueryBody, UnOp};
    let Expr::RowValue(probe) = lhs else {
        return Ok(None);
    };
    let SubqueryBody::Select(sel) = inner else {
        return Ok(None);
    };
    if sel.distinct
        || !sel.group_by.is_empty()
        || sel.having.is_some()
        || sel.limit.is_some()
        || sel.offset.is_some()
        || sel.from_derived.is_some()
    {
        return Ok(None);
    }
    let Some(items) = &sel.items else {
        // `SELECT *`: the column count is not known until the schema is in
        // hand, so the arity check below cannot be made here.
        return Ok(None);
    };
    if items.len() != probe.len() {
        return Err(bind_err(format!(
            "row value has {} column(s) but the subquery selects {}",
            probe.len(),
            items.len()
        )));
    }
    if items.iter().any(|(e, _)| crate::view::expr_aggregates(e)) {
        return Ok(None);
    }
    // THE PROBE MOVES INTO THE INNER SCOPE, so every unqualified column in it
    // must be QUALIFIED against the outer scope first — otherwise the inner
    // query resolves it, and a column of the same name there silently captures
    // the outer reference. That is a wrong answer, not an error, and it was a
    // shipped one: measured against 3.45.1 with r(a,b) and q(a,b),
    //
    //   (a,b) IN (SELECT a,b FROM q)  ->  oracle {y,z}, mpedb {x,y,z}
    //   (a,b) IN (SELECT b,a FROM q)  ->  oracle {y,z}, mpedb {}
    //
    // because `a = a` is trivially true and `a = b AND b = a` trivially false
    // once BOTH sides bind inside q. Only the fully-qualified spelling escaped.
    let qualified = qualify_row_probe(probe, sel, outer)?;
    // `a = x AND b = y`, over the inner row's scope — the items move into the
    // WHERE rather than the other way round.
    //
    // THE PROBE GOES ON THE LEFT, and that is not cosmetic. sqlite resolves a
    // comparison's collation from the LEFT operand when it has a declared one,
    // and the left operand of `IN` is the row value — the OUTER columns. Built
    // the other way round the inner column's collation won, and a
    // `COLLATE NOCASE` probe stopped matching: measured, nc(a,b) NOCASE holding
    // ('AB','CD') against nq holding ('ab','cd') gave the oracle one row and
    // mpedb none. A wrong answer, in both the bare and the qualified spelling.
    let mut eq: Option<Expr> = None;
    for ((item, _), p) in items.iter().zip(&qualified) {
        let cmp = Expr::Binary(BinOp::Eq, Box::new(p.clone()), Box::new(item.clone()));
        eq = Some(match eq {
            None => cmp,
            Some(prev) => Expr::Binary(BinOp::And, Box::new(prev), Box::new(cmp)),
        });
    }
    let eq = eq.expect("arity checked non-zero by the parser");
    let one_row = |cond: Expr| -> SubqueryBody {
        let mut s = sel.clone();
        s.items = Some(vec![(Expr::Lit(Value::Int(1)), None)]);
        s.order_by = Vec::new();
        s.where_clause = Some(match s.where_clause.take() {
            None => cond,
            Some(w) => Expr::Binary(BinOp::And, Box::new(w), Box::new(cond)),
        });
        SubqueryBody::Select(s)
    };
    let matched = Expr::Exists(Box::new(one_row(eq.clone())), false);
    let unknown = Expr::Exists(
        Box::new(one_row(Expr::IsNull(Box::new(eq), false))),
        false,
    );
    let case = Expr::Case(
        vec![
            (matched, Expr::Lit(Value::Bool(true))),
            (unknown, Expr::Lit(Value::Null)),
        ],
        Some(Box::new(Expr::Lit(Value::Bool(false)))),
    );
    Ok(Some(if negated {
        Expr::Unary(UnOp::Not, Box::new(case))
    } else {
        case
    }))
}

/// Qualify every element of a row-value probe against the OUTER scope, so the
/// probe can move INTO a subquery without an inner column of the same name
/// silently capturing it (the S22 rules). ONE copy, shared by row-value `IN`
/// and row-value `=`/`<>` — two copies of these fences would drift, and the
/// fences are the correctness.
fn qualify_row_probe(
    probe: &[ast::Expr],
    sel: &ast::SelectStmt,
    outer: &Scope<'_>,
) -> Result<Vec<ast::Expr>> {
    use ast::Expr;
    let mut probe_quals: Vec<String> = Vec::new();
    let mut qualified: Vec<Expr> = Vec::with_capacity(probe.len());
    for p in probe {
        match qualify_probe(p, outer, &mut probe_quals) {
            Some(q) => qualified.push(q),
            // Cannot be qualified safely — see `qualify_probe`. Refusing by
            // name is the only answer that is not a guess.
            None => {
                return Err(bind_err(
                    "a row value compared with a subquery must name outer columns \
                     the outer query binds unambiguously — an unqualified name that \
                     the outer query does not resolve would be resolved INSIDE the \
                     subquery instead, which silently changes what is compared"
                        .to_string(),
                ))
            }
        }
    }
    // Qualification cannot save a probe whose outer table is ADDRESSED by the
    // same name inside the subquery (`FROM r` in both). Refuse the rewrite —
    // the ordinary path then reports the row-value limit by name, which is the
    // right answer where this one cannot be given safely.
    let inner_names: Vec<&str> = sel
        .alias
        .as_deref()
        .into_iter()
        .chain(if sel.alias.is_none() { sel.table.as_deref() } else { None })
        .chain(
            sel.joins
                .iter()
                .map(|j| j.alias.as_deref().unwrap_or(j.table.as_str())),
        )
        .collect();
    let shadowed = probe_quals
        .iter()
        .any(|q| inner_names.iter().any(|n| mpedb_types::ident_eq(n, q)));
    if shadowed {
        return Err(bind_err(
            "a row value cannot be compared with a subquery that reads a table \
             addressed by the SAME name as the outer one — qualifying the probe \
             is what keeps the outer columns from being resolved inside the \
             subquery, and it cannot distinguish two identical names. Alias one \
             of them."
                .to_string(),
        ));
    }
    Ok(qualified)
}

/// `(a, b) = (SELECT x, y FROM s …)` and `<>` — plan §2, the last Django
/// label. The comparison MOVES INTO the subquery's projection: the body is
/// planned ONCE as an ordinary scalar subplan whose single output column IS
/// the row-value comparison (bound later by the binder's proven-3VL row-value
/// desugar), and the probe columns become correlation parameters under
/// [`qualify_row_probe`]'s fences. No plan-format change, ONE evaluation —
/// never one scalar subquery per column, which could see different rows.
///
/// Why not the `IN` desugar above: `EXISTS` answers where `=` must refuse.
/// The scalar-subplan shape keeps every measured semantic: 0 inner rows →
/// slot NULL (sqlite: NULL); 1 row → the pairwise rule (FALSE if any pair
/// definitely unequal, else NULL if any pair NULL, else TRUE — 3VL AND is
/// exactly that, measured both ways round); >1 rows → mpedb's DOCUMENTED
/// multi-row scalar refusal stands, where sqlite silently takes the first
/// row. Operand order is PRESERVED (`(SELECT…) = (a,b)` keeps the subquery
/// on the left) — sqlite takes a comparison's collation from the LEFT
/// operand, the same lesson the `IN` desugar above carries.
///
/// `Ok(None)` — fall back to the ordinary named refusal — for shapes the
/// projection swap would CHANGE: a compound body, `SELECT *`, a derived
/// FROM, DISTINCT (it would dedup the comparison's value instead of the
/// pair), GROUP BY/HAVING, and an ORDINAL `ORDER BY` (it would silently
/// re-bind to the ONE swapped column — with LIMIT that picks a different
/// row: a wrong answer, not a refusal). Order comparisons (`<` `<=` `>`
/// `>=`) stay refusals this round.
fn row_value_eq_subquery(
    op: BinOp,
    probe_on_left: bool,
    probe: &ast::Expr,
    inner: &ast::SubqueryBody,
    outer: &Scope<'_>,
) -> Result<Option<ast::Expr>> {
    use ast::{Expr, SubqueryBody};
    let Expr::RowValue(probe_items) = probe else {
        return Ok(None);
    };
    let SubqueryBody::Select(sel) = inner else {
        return Ok(None);
    };
    if sel.distinct
        || !sel.group_by.is_empty()
        || sel.having.is_some()
        || sel.from_derived.is_some()
    {
        return Ok(None);
    }
    let Some(items) = &sel.items else {
        return Ok(None); // `SELECT *`: arity unknown until the schema is in hand
    };
    if sel.order_by.iter().any(|(e, _)| matches!(e, Expr::Lit(Value::Int(_)))) {
        return Ok(None);
    }
    if items.len() != probe_items.len() {
        return Err(bind_err(format!(
            "row value has {} column(s) but the subquery selects {}",
            probe_items.len(),
            items.len()
        )));
    }
    // The probe is checked for aggregates/windows BEFORE it moves: inside the
    // subquery `max(a)` becomes LEGAL — and aggregates over the INNER rows,
    // a wrong answer the refutation round measured. Refused by name instead.
    if probe_items.iter().any(crate::view::expr_aggregates) {
        return Err(bind_err(
            "a row value carrying an aggregate or window function cannot be \
             compared with a subquery: the comparison is evaluated inside the \
             subquery, where the aggregate would run over the subquery's rows \
             instead of the outer group"
                .to_string(),
        ));
    }
    let qualified = qualify_row_probe(probe_items, sel, outer)?;
    let inner_vals: Vec<Expr> = items.iter().map(|(e, _)| e.clone()).collect();
    let (l, r) = if probe_on_left {
        (Expr::RowValue(qualified), Expr::RowValue(inner_vals))
    } else {
        (Expr::RowValue(inner_vals), Expr::RowValue(qualified))
    };
    let mut s = sel.clone();
    s.items = Some(vec![(Expr::Binary(op, Box::new(l), Box::new(r)), None)]);
    Ok(Some(Expr::Subquery(Box::new(SubqueryBody::Select(s)))))
}
