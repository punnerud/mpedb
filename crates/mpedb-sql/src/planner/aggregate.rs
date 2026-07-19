use super::*;
use super::select::{distinct_order_by, ordinal, push_junk};

/// Does this expression contain an aggregate anywhere?
pub(super) fn contains_agg(e: &ast::Expr) -> bool {
    use ast::Expr as E;
    match e {
        E::Agg(..) => true,
        E::Unary(_, a) | E::IsNull(a, _) | E::Cast(a, _) => contains_agg(a),
        E::Binary(_, a, b)
        | E::Like(a, b)
        | E::Match(a, b)
        | E::IsDistinct(a, b, _)
        | E::Glob(a, b, _)
        | E::Regexp(a, b, _) => contains_agg(a) || contains_agg(b),
        E::InContext(a, _, _) => contains_agg(a),
        E::Collate(a, _) => contains_agg(a),
        E::InSubquery(a, _, _) | E::InParamSlot(a, _, _) => contains_agg(a),
        E::InList(a, xs, _) => contains_agg(a) || xs.iter().any(contains_agg),
        E::Coalesce(xs) | E::Func(_, xs) | E::RowValue(xs) => xs.iter().any(contains_agg),
        E::Case(arms, els) => {
            arms.iter().any(|(c, r)| contains_agg(c) || contains_agg(r))
                || els.as_deref().is_some_and(contains_agg)
        }
        // An aggregate INSIDE a subquery aggregates the inner statement's
        // rows, not ours — the outer walk stops at the boundary.
        E::Subquery(_) | E::Exists(..) => false,
        // An aggregate inside a WINDOW is the window's own business (its `arg`
        // is accumulated over the partition, not this query's groups) — the
        // walk stops here exactly as it does at a subquery boundary. This is
        // what keeps `sum(x) OVER (…)` from being read as a plain aggregate.
        E::Window { .. } => false,
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
#[allow(clippy::type_complexity)]
fn lift_aggs(
    e: &ast::Expr,
    keys: &GroupKeys<'_>,
    // Name -> slot in the row being aggregated. A `Scope`, so this works over a
    // join's `[outer ‖ inner]` as well as one table — the rule it enforces ("a
    // bare column must be a GROUP BY key") is about the ROW, and does not care
    // how many tables built it.
    scope: &Scope<'_>,
    // GROUP BY strictness (COMPAT.md): `Postgres` refuses a bare column outright;
    // `Sqlite` gives it a slot in the grouped tuple's `bare` region and lets the
    // caller decide (after folding) whether it survives.
    mode: BareGroupBy,
    // Per aggregate: `(func, arg, distinct, filter)`. The last element is the
    // optional `FILTER (WHERE …)` predicate AST, kept in the dedup key so two
    // otherwise-identical aggregates with DIFFERENT filters stay separate slots.
    aggs: &mut Vec<(mpedb_types::AggTarget, Option<ast::Expr>, bool, Option<ast::Expr>)>,
    // sqlite bare columns: `(base-row slot, type)`, deduped by slot. A bare
    // column becomes `__b{j}` where `j` is its index here; the planner resolves
    // those names against the extended grouped tuple `[keys ‖ aggs ‖ bare]`.
    bare: &mut Vec<(u16, ColumnType)>,
) -> Result<ast::Expr> {
    use ast::Expr as E;
    let rec = |x: &ast::Expr,
               aggs: &mut Vec<(mpedb_types::AggTarget, Option<ast::Expr>, bool, Option<ast::Expr>)>,
               bare: &mut Vec<(u16, ColumnType)>| lift_aggs(x, keys, scope, mode, aggs, bare);
    let group_by = keys.asts;
    // A selected/ordered expression that IS a group key — `SELECT a+1 …
    // GROUP BY a+1` — names that key's slot in the grouped tuple. Checked
    // before anything else, so the whole key wins over its parts; aggregate
    // ARGUMENTS never get here (the Agg arm below clones them unrewritten).
    if let Some(pos) = keys.asts.iter().position(|k| k == e) {
        return Ok(E::Col(format!("__g{pos}")));
    }
    Ok(match e {
        E::Agg(f, arg, distinct, filter) => {
            // The FILTER predicate rides in the dedup key: `count(*) FILTER
            // (WHERE a)` and `count(*) FILTER (WHERE b)` are two aggregates.
            let spec = (f.clone(), arg.as_deref().cloned(), *distinct, filter.as_deref().cloned());
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
        // A bare column in an aggregate query is one that is neither a GROUP BY
        // key nor inside an aggregate. `Postgres` refuses it (SQL's rule);
        // `Sqlite` gives it a slot in the grouped tuple's `bare` region and lets
        // the planner decide — after constant folding — whether it is genuinely
        // used (must then be fixed by a single min()/max()) or folds away.
        E::Col(_) | E::Qualified(..) => {
            let (idx, ty) = match e {
                E::Col(n) => scope.resolve(n)?,
                E::Qualified(q, n) => scope.resolve_qualified(q, n)?,
                _ => unreachable!("matched above"),
            };
            // Slot-based match: `GROUP BY a` + `SELECT t.a` are the same key
            // under two spellings, which AST equality above cannot see.
            match keys.cols.iter().position(|g| *g == Some(idx)) {
                Some(pos) => E::Col(format!("__g{pos}")),
                None => match mode {
                    BareGroupBy::Postgres => {
                        return Err(bind_err(format!(
                            "column `{}` must appear in GROUP BY or be inside an aggregate \
                             — otherwise there is no single value for it in the group",
                            scope.slot_name(idx)
                        )))
                    }
                    BareGroupBy::Sqlite => {
                        let j = match bare.iter().position(|(c, _)| *c == idx) {
                            Some(j) => j,
                            None => {
                                bare.push((idx, ty));
                                bare.len() - 1
                            }
                        };
                        E::Col(format!("__b{j}"))
                    }
                },
            }
        }
        E::Unary(op, a) => E::Unary(*op, Box::new(rec(a, aggs, bare)?)),
        E::Cast(a, t) => E::Cast(Box::new(rec(a, aggs, bare)?), t.clone()),
        E::IsNull(a, n) => E::IsNull(Box::new(rec(a, aggs, bare)?), *n),
        E::Binary(op, a, b) => {
            E::Binary(*op, Box::new(rec(a, aggs, bare)?), Box::new(rec(b, aggs, bare)?))
        }
        E::IsDistinct(a, b, n) => {
            E::IsDistinct(Box::new(rec(a, aggs, bare)?), Box::new(rec(b, aggs, bare)?), *n)
        }
        E::Like(a, b) => E::Like(Box::new(rec(a, aggs, bare)?), Box::new(rec(b, aggs, bare)?)),
        E::Match(a, b) => E::Match(Box::new(rec(a, aggs, bare)?), Box::new(rec(b, aggs, bare)?)),
        E::Glob(a, b, n) => {
            E::Glob(Box::new(rec(a, aggs, bare)?), Box::new(rec(b, aggs, bare)?), *n)
        }
        E::Regexp(a, b, n) => {
            E::Regexp(Box::new(rec(a, aggs, bare)?), Box::new(rec(b, aggs, bare)?), *n)
        }
        E::InList(a, xs, n) => E::InList(
            Box::new(rec(a, aggs, bare)?),
            xs.iter().map(|x| rec(x, aggs, bare)).collect::<Result<_>>()?,
            *n,
        ),
        E::InContext(a, k, n) => E::InContext(Box::new(rec(a, aggs, bare)?), k.clone(), *n),
        // Preserve the COLLATE through the lift so the grouped ORDER BY path can
        // peel it (`ORDER BY name COLLATE NOCASE` in a GROUP BY query). The
        // collation itself carries no aggregate.
        E::Collate(a, name) => E::Collate(Box::new(rec(a, aggs, bare)?), name.clone()),
        // The IN-subquery marker: the slot side is opaque here; only the lhs
        // participates in the grouped tuple. (A raw InSubquery still present
        // means the lift refused/skipped it — pass through to the binder's
        // refusal, same as Subquery below.)
        E::InParamSlot(a, slot, n) => E::InParamSlot(Box::new(rec(a, aggs, bare)?), *slot, *n),
        other @ E::InSubquery(..) => other.clone(),
        E::Coalesce(xs) => {
            E::Coalesce(xs.iter().map(|x| rec(x, aggs, bare)).collect::<Result<_>>()?)
        }
        E::Func(f, xs) => E::Func(
            f.clone(),
            xs.iter().map(|x| rec(x, aggs, bare)).collect::<Result<_>>()?,
        ),
        // A row value's elements may hold aggregates (`HAVING (count(*), sum(x))
        // > (?, ?)`), so lift each; the binder desugars the tuple comparison
        // afterward over the grouped tuple.
        E::RowValue(xs) => {
            E::RowValue(xs.iter().map(|x| rec(x, aggs, bare)).collect::<Result<_>>()?)
        }
        E::Case(arms, els) => E::Case(
            arms.iter()
                .map(|(c, r)| Ok((rec(c, aggs, bare)?, rec(r, aggs, bare)?)))
                .collect::<Result<_>>()?,
            match els {
                Some(x) => Some(Box::new(rec(x, aggs, bare)?)),
                None => None,
            },
        ),
        // Subqueries are lifted before aggregation planning ever runs; one
        // still here is headed for the binder's clear refusal — pass through.
        other @ (E::Subquery(_) | E::Exists(..)) => other.clone(),
        // A window inside an aggregate query is refused before this runs
        // (windows + aggregate is rejected at routing); if one reaches here it
        // passes through to the binder's clear refusal rather than being lifted.
        other @ E::Window { .. } => other.clone(),
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
/// What `lift_aggs` needs to know about the GROUP BY keys.
struct GroupKeys<'a> {
    /// The keys as written — whole-expression matching.
    asts: &'a [ast::Expr],
    /// Per key: its base-row slot when it IS a plain column, for the
    /// spelling-insensitive bare-column rule.
    cols: &'a [Option<u16>],
}

fn synthetic_grouped_table(
    // One type per GROUP BY key — a plain column's declared type, or the
    // bound type of a computed key (`GROUP BY a + 1`).
    key_types: &[ColumnType],
    // One collation per GROUP BY key — a bare column's declared collation, so an
    // `ORDER BY <grouped-key>` over this synthetic tuple inherits it.
    key_collations: &[Collation],
    aggs: &[(mpedb_types::AggTarget, Option<ast::Expr>, bool, Option<ast::Expr>)],
    agg_types: &[Option<ColumnType>],
    // sqlite bare columns `(base slot, type)`: the tuple's tail
    // `[keys ‖ aggs ‖ bare]`, named `__b{j}`. Empty in postgres mode.
    bare: &[(u16, ColumnType)],
) -> TableDef {
    let mut out: Vec<mpedb_types::ColumnDef> =
        Vec::with_capacity(key_types.len() + aggs.len() + bare.len());
    for (k, &ty) in key_types.iter().enumerate() {
        out.push(mpedb_types::ColumnDef {
            name: format!("__g{k}"),
            ty,
            nullable: true, // a group key can be NULL; NULLs group together
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: key_collations.get(k).copied().unwrap_or(Collation::Binary),
            affinity: mpedb_types::Affinity::implied_by(ty),
        });
    }
    for (i, (f, _, _, _)) in aggs.iter().enumerate() {
        let ty = match f.native() {
            Some(mpedb_types::AggFn::Count) => ColumnType::Int64,
            Some(mpedb_types::AggFn::Avg | mpedb_types::AggFn::Total) => ColumnType::Float64,
            Some(mpedb_types::AggFn::GroupConcat) => ColumnType::Text,
            // SUM/MIN/MAX keep the argument's type.
            Some(_) => agg_types[i].unwrap_or(ColumnType::Int64),
            // A HOST aggregate returns whatever its `xFinal` writes — the same
            // dynamic typing a host SCALAR gets (`ColumnType::Any`). Pinning it
            // to the argument's type would reject `stddev_pop(int_col) / 2.0`.
            None => ColumnType::Any,
        };
        out.push(mpedb_types::ColumnDef {
            name: format!("__g{}", key_types.len() + i),
            ty,
            // COUNT and TOTAL are never NULL (0 / 0.0 over an empty group);
            // every other aggregate is NULL over an empty group.
            // COUNT and TOTAL are the only never-NULL aggregates; a host
            // aggregate may return NULL from `xFinal` (an empty group always
            // does), so it is nullable like the rest.
            nullable: !matches!(
                f.native(),
                Some(mpedb_types::AggFn::Count | mpedb_types::AggFn::Total)
            ),
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: Collation::Binary,
            affinity: mpedb_types::Affinity::implied_by(ty),
        });
    }
    // The bare region: named `__b{j}` (the names `lift_aggs` emitted), typed by
    // the base column. Always nullable — a bare column is carried from a witness
    // row that may hold NULL, and an empty group yields NULL.
    for (j, (_, ty)) in bare.iter().enumerate() {
        out.push(mpedb_types::ColumnDef {
            name: format!("__b{j}"),
            ty: *ty,
            nullable: true,
            unique: false,
            indexed: false,
            default: None,
            check: None,
            collation: Collation::Binary,
            affinity: mpedb_types::Affinity::implied_by(*ty),
        });
    }
    TableDef {
        // Not a table anyone named, and nothing resolves a qualifier against
        // it: `lift_aggs` has already rewritten every column reference to the
        // positional `__gN`.
        id: 0,
        name: String::new(),
        columns: out,
        primary_key: vec![0],
        indexes: Vec::new(),
        dead: false,
        implicit_rowid: false,
        kind: mpedb_types::TableKind::Standard,
    }
}

/// Plan an aggregate SELECT over `base` — one table, or a join's
/// `[outer ‖ inner]`. Everything here is about the ROW being aggregated, so all
/// the join changes is how wide that row is and how names resolve into it.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_aggregate_select(
    s: &ast::SelectStmt,
    // The row being aggregated, as the scope its names resolve in (which is
    // also where the grouped tuple's key types come from).
    base_scope: &Scope<'_>,
    table_id: u32,
    access: AccessPath,
    filter: Option<ExprProgram>,
    joins: Vec<Join>,
    joined_filter: Option<ExprProgram>,
    // The correlated WHERE residual (#73 §1): filled and applied per outer row
    // BEFORE accumulation, so aggregation still runs over the full
    // `(WHERE ∧ policy)` set. `None` for a plain aggregate.
    post_filter: Option<ExprProgram>,
    mut binder: Binder<'_>,
    // GROUP BY strictness dialect (COMPAT.md): `Postgres` refuses every bare
    // column; `Sqlite` accepts the deterministic ones — folded-away, fixed by a
    // single min/max (witness row), or (over a single INTEGER-PK table) sqlite's
    // lowest-rowid pick — and refuses only what it cannot reproduce exactly (the
    // arbitrary case over a join or a non-rowid PK).
    mode: BareGroupBy,
    _consts: &mut Vec<Value>,
    subplans: Vec<SubPlan>,
) -> Result<PlannedStmt> {
    let items = s.items.as_ref().ok_or_else(|| {
        bind_err("SELECT * with GROUP BY has no meaning — list the group keys and aggregates")
    })?;

    // 1. GROUP BY keys — a plain column becomes a base-row slot, anything
    //    else (`GROUP BY a + 1`) a computed key evaluated over the base row.
    //    Both live in the grouped tuple `[keys ‖ aggs]` like any other key.
    let mut group_by: Vec<GroupKey> = Vec::with_capacity(s.group_by.len());
    // Per key: its base-row slot when it IS a column (for the bare-column
    // rule below), its declared type (for the grouped tuple), and its AST
    // (for matching selected/ordered expressions to key positions).
    let mut key_cols: Vec<Option<u16>> = Vec::with_capacity(s.group_by.len());
    let mut key_types: Vec<ColumnType> = Vec::with_capacity(s.group_by.len());
    // Per key: the collation to GROUP/ORDER the key under — a bare column's
    // declared collation (so `GROUP BY name` on a NOCASE column collapses case),
    // BINARY for a computed key. Rides into the synthetic grouped tuple so an
    // `ORDER BY <key>` over the grouped row picks it up.
    let mut key_collations: Vec<Collation> = Vec::with_capacity(s.group_by.len());
    let mut key_asts: Vec<ast::Expr> = Vec::with_capacity(s.group_by.len());
    for g in &s.group_by {
        // `GROUP BY 2` is an OUTPUT ordinal in sqlite and PostgreSQL, so the
        // key is the second select item's expression. A non-integer constant
        // is refused as in PG: it would put every row in one group, which is
        // never what was meant.
        let ordinal_key;
        let g = if let Some(pos) = ordinal(g, items.len())? {
            ordinal_key = items[pos as usize].0.clone();
            &ordinal_key
        } else if matches!(g, ast::Expr::Lit(_)) {
            return Err(bind_err(
                "a constant in GROUP BY does nothing — write an output ordinal \
                 (`GROUP BY 1`) or a key expression",
            ));
        } else {
            g
        };
        if contains_agg(g) {
            return Err(bind_err(
                "GROUP BY cannot contain an aggregate — the key decides the \
                 groups the aggregate is computed OVER",
            ));
        }
        match g {
            ast::Expr::Col(_) | ast::Expr::Qualified(..) => {
                let (i, ty) = match g {
                    ast::Expr::Col(n) => base_scope.resolve(n)?,
                    ast::Expr::Qualified(q, n) => base_scope.resolve_qualified(q, n)?,
                    _ => unreachable!("matched above"),
                };
                // A repeated key is redundant, not wrong — `GROUP BY a, a`
                // groups exactly as `GROUP BY a` in sqlite and PostgreSQL
                // both (measured: refusing it was 14k+ corpus statements).
                if key_cols.contains(&Some(i)) {
                    continue;
                }
                group_by.push(GroupKey::Col(i));
                key_cols.push(Some(i));
                key_types.push(ty);
                key_collations.push(base_scope.column_collation(i));
            }
            expr => {
                // Same rule as repeated columns: redundant, not wrong.
                if key_asts.contains(expr) {
                    continue;
                }
                let (b, ty) = binder.bind_expr(expr)?;
                group_by.push(GroupKey::Expr(compile_program(&b)?));
                key_cols.push(None);
                // An unpinnable key type (a bare NULL) grades to Any — the
                // honest "decided per value" column type.
                key_types.push(ty.unwrap_or(ColumnType::Any));
                key_collations.push(Collation::Binary);
            }
        }
        key_asts.push(g.clone());
    }

    // 2. Lift the aggregates out of the SELECT list and HAVING. In sqlite mode
    //    `bare` collects any bare column (not a key, not an aggregate) — each
    //    gets a slot in the grouped tuple's tail; whether it SURVIVES is decided
    //    after folding (step 5). In postgres mode `lift_aggs` refuses instead.
    let keys = GroupKeys { asts: &key_asts, cols: &key_cols };
    let mut agg_specs: Vec<(mpedb_types::AggTarget, Option<ast::Expr>, bool, Option<ast::Expr>)> =
        Vec::new();
    let mut bare: Vec<(u16, ColumnType)> = Vec::new();
    let mut rewritten = Vec::with_capacity(items.len());
    for (item, _alias) in items {
        rewritten.push(lift_aggs(item, &keys, base_scope, mode, &mut agg_specs, &mut bare)?);
    }
    let rewritten_having = match &s.having {
        Some(h) => Some(lift_aggs(h, &keys, base_scope, mode, &mut agg_specs, &mut bare)?),
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
        rewritten_order.push((
            lift_aggs(e, &keys, base_scope, mode, &mut agg_specs, &mut bare)?,
            *desc,
        ));
    }

    // 3. Bind each aggregate ARGUMENT and FILTER over the BASE row. The FILTER
    //    is bound BEFORE the binder is rescoped to the grouped tuple (below), so
    //    it resolves the same base columns/params the argument does — the two
    //    always see the identical tuple. It is typed as a predicate (Bool),
    //    exactly like WHERE/HAVING. The argument is bound first so params
    //    register in source order (`sum(x) FILTER (WHERE y)`).
    let mut aggs = Vec::with_capacity(agg_specs.len());
    let mut agg_types = Vec::with_capacity(agg_specs.len());
    for (f, arg, distinct, filt) in &agg_specs {
        let (arg_prog, ty, distinct) = match arg {
            None => (None, Some(ColumnType::Int64), false),
            Some(a) => {
                let (b, ty) = binder.bind_expr(a)?;
                (Some(compile_program(&b)?), ty, *distinct)
            }
        };
        let filter = match filt {
            Some(fe) => {
                let b = binder.bind_predicate(fe)?;
                Some(compile_program(&b)?)
            }
            None => None,
        };
        agg_types.push(ty);
        aggs.push(AggCall {
            func: f.clone(),
            arg: arg_prog,
            distinct,
            filter,
        });
    }

    // 4. Bind the rewritten projection/HAVING over the GROUPED tuple — a
    //    different tuple from the base row, carrying the same parameter table.
    //    The tuple is `[keys ‖ aggs ‖ bare]`; folding runs here, so a bare column
    //    in a dead branch (`COALESCE(-24, col)`) drops out and never reaches a
    //    program.
    let grouped =
        synthetic_grouped_table(&key_types, &key_collations, &agg_specs, &agg_types, &bare);
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

    // sqlite bare columns: which of `bare`'s slots SURVIVED folding, and are they
    // legal? A bare slot at `k_aggs + j` is live iff some bound program (a
    // projection item, HAVING, or an ORDER BY key that indexes the grouped tuple)
    // actually reads it. A folded-away reference (`COALESCE(-24, col)`) leaves
    // none → carry nothing (the deterministic never-evaluated case). A live bare
    // column is deterministic in one of two ways, both matching sqlite EXACTLY:
    //   (a) a SINGLE min()/max() fixes it to the extremum's WITNESS row (#87); or
    //   (b) otherwise sqlite's "arbitrary" pick is really the group's LOWEST-ROWID
    //       row (verified vs sqlite 3.45), which the executor reproduces by
    //       tracking the minimum PK per group — but ONLY where mpedb's row
    //       identity IS that rowid (`rowid_pick_ok`).
    // Anything else is refused rather than risk a value that differs from sqlite
    // (the core never-a-wrong-answer guarantee). `grouped_order` names the ORDER
    // BY slots ONLY when they index the grouped tuple (`OrderOver::Grouped`);
    // projection ordinals and junk columns are already covered by the projection
    // walk.
    //
    // `rowid_pick_ok`: the lowest-rowid pick matches sqlite only when mpedb reads
    // rows FROM its rowid — a single INTEGER-PK table, no join. Over a join the
    // aggregated "row" is `[outer ‖ inner]` with no single rowid, and a
    // composite/text PK is not sqlite's implicit rowid, so the arbitrary case
    // stays refused there (never a wrong answer; documented in COMPAT.md).
    let rowid_pick_ok = joins.is_empty() && {
        let t = base_scope.only();
        t.primary_key.len() == 1
            && t.columns.get(t.primary_key[0] as usize).map(|c| c.ty) == Some(ColumnType::Int64)
    };
    let k_aggs = (group_by.len() + agg_specs.len()) as u16;
    let decide_bare_cols = |projection: &[Projection],
                            having: &Option<ExprProgram>,
                            grouped_order: &[u16]|
     -> Result<Vec<u16>> {
        if bare.is_empty() {
            return Ok(Vec::new());
        }
        let mut refs: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
        for p in projection {
            if let Projection::Expr { program, .. } = p {
                for instr in &program.instrs {
                    if let Instr::PushCol(i) = instr {
                        refs.insert(*i);
                    }
                }
            }
        }
        if let Some(h) = having {
            for instr in &h.instrs {
                if let Instr::PushCol(i) = instr {
                    refs.insert(*i);
                }
            }
        }
        refs.extend(grouped_order.iter().copied());
        let Some(first_live) = (0..bare.len()).find(|&j| refs.contains(&(k_aggs + j as u16)))
        else {
            // Every bare column folded away — nothing to carry, plan is
            // byte-identical to the postgres one.
            return Ok(Vec::new());
        };
        // sqlite's bare-column rule turns on how many min()/max() aggregates the
        // query has (the OTHER aggregates — count/sum/avg — do not matter):
        //   1  → every bare column takes that extremum's WITNESS row. sqlite's
        //        documented, deterministic rule (#87), and it holds even over a
        //        join, so no `rowid_pick_ok` gate.
        //   0  → sqlite's "arbitrary" pick is really the group's LOWEST-ROWID row
        //        (verified vs sqlite 3.45). The executor reproduces it with the
        //        per-group min-PK witness, EXACTLY when mpedb reads rows from that
        //        rowid — `rowid_pick_ok` (single INTEGER-PK table, no join).
        //   ≥2 → sqlite takes the LAST min()/max()'s witness row, an order-
        //        dependent pick its own docs call "arbitrary" — refuse it rather
        //        than reproduce version-fragile behavior (never a wrong answer).
        let n_minmax = agg_specs
            .iter()
            .filter(|(f, _, _, _)| {
                matches!(f.native(), Some(mpedb_types::AggFn::Min | mpedb_types::AggFn::Max))
            })
            .count();
        let reproducible = match n_minmax {
            1 => true,
            0 => rowid_pick_ok,
            _ => false,
        };
        if !reproducible {
            let reason = if n_minmax == 0 {
                "over a join or a non-rowid primary key sqlite would take it from an \
                 arbitrary row"
            } else {
                "with two or more min()/max() aggregates sqlite's pick is order-dependent"
            };
            return Err(bind_err(format!(
                "column `{}` must appear in GROUP BY, be inside an aggregate, or be \
                 determined by a single min()/max() — {reason}, which mpedb refuses \
                 rather than return a value that might differ from sqlite",
                base_scope.slot_name(bare[first_live].0)
            )));
        }
        // Keep EVERY recorded bare column (live and folded-away alike) so the
        // grouped-tuple slot positions the projection was bound against stay put;
        // the executor fills them all from the group's witness row — the single
        // min/max extremum, or the lowest-rowid row — inferred from the aggregate set.
        Ok(bare.iter().map(|(c, _)| *c).collect())
    };

    // 5. ORDER BY. Preferred form: every key is a bare column of the GROUPED
    //    tuple — a group key or an aggregate slot — so the sort runs there and
    //    `ORDER BY count(*)` works even unselected. A key computed FROM those
    //    (`ORDER BY count(*) + 1`) is not a column of any tuple that exists
    //    yet, so it gets a sort-only column appended to the projection, exactly
    //    as the plain path does for `ORDER BY amt + 1`.
    if s.distinct {
        // DISTINCT sorts over the PROJECTION, so no ORDER BY key indexes the
        // grouped tuple directly (`grouped_order` is empty).
        let bare_cols = decide_bare_cols(&projection, &having, &[])?;
        let (order_by, _) = distinct_order_by(s, base_scope, None)?;
        let (param_types, context_keys, list_keys) = binder.into_parts();
        return Ok((
            PlanStmt::Select(SelectPlan {
                table: table_id,
                access,
                joins,
                joined_filter,
                post_filter,
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
                    bare_cols,
                }),
                windows: Vec::new(),
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
        // The lift preserved any COLLATE (`ORDER BY name COLLATE NOCASE` in a
        // GROUP BY query); peel it so the inner resolves to a grouped-tuple
        // column and the collation rides the sort.
        let (e, coll) = peel_collate(e)?;
        // The lifted key names a grouped-tuple column (`__gN`); its declared
        // collation was carried onto that synthetic column, so an `ORDER BY name`
        // over a NOCASE group key sorts case-insensitively.
        let coll = coll.unwrap_or_else(|| declared_collation(e, &binder.scope));
        match binder.bind_expr(e)? {
            (BExpr::Col(i), _) => grouped_keys.push((i, *desc, coll)),
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
            // Collation comes from the ORIGINAL key text; peel both the original
            // (for the ordinal test) and the lifted expr (for the item match /
            // junk column).
            let (orig, coll) = peel_collate(orig)?;
            let coll = coll.unwrap_or_else(|| declared_collation(orig, base_scope));
            let (e, _) = peel_collate(e)?;
            // An ordinal or a repeat of a selected item needs no new column.
            if let Some(pos) = ordinal(orig, items.len())? {
                keys.push((pos, *desc, coll));
                continue;
            }
            match rewritten.iter().position(|it| it == e) {
                Some(pos) => keys.push((pos as u16, *desc, coll)),
                None => {
                    let mut junk = Some((&mut projection, &mut binder));
                    let (pos, added) = push_junk(&mut junk, e, &Scope::single(&grouped), i)?;
                    keys.push((pos, *desc, coll));
                    n_junk += added;
                }
            }
        }
        (keys, OrderOver::Projection, n_junk)
    };

    // An ORDER BY over the grouped tuple (`OrderOver::Grouped`) may itself be the
    // only reader of a bare column (`… ORDER BY name`), so feed those slots into
    // the liveness decision; a projection-indexed sort is already covered.
    let grouped_order: Vec<u16> = if order_over == OrderOver::Grouped {
        order_by.iter().map(|(i, _, _)| *i).collect()
    } else {
        Vec::new()
    };
    let bare_cols = decide_bare_cols(&projection, &having, &grouped_order)?;

    let (param_types, context_keys, list_keys) = binder.into_parts();
    Ok((
        PlanStmt::Select(SelectPlan {
            table: table_id,
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
            aggregate: Some(Aggregation {
                group_by,
                aggs,
                having,
                bare_cols,
            }),
            windows: Vec::new(),
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
        ast::Expr::Agg(f, None, _, _) => format!("{}(*)", f.name()),
        ast::Expr::Agg(f, Some(a), distinct, _) => format!(
            "{}({}{})",
            f.name(),
            if *distinct { "DISTINCT " } else { "" },
            agg_item_name(a)
        ),
        _ => "?column?".to_string(),
    }
}
