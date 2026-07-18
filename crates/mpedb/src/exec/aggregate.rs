use super::*;

fn new_accum(a: &AggCall) -> Accum {
    if a.distinct {
        Accum::new_distinct(a.func)
    } else {
        Accum::new(a.func)
    }
}

/// `GROUP BY` / aggregates / `HAVING`.
///
/// **The first line is the invariant.** DESIGN-MULTIDB §4: aggregation must
/// consume rows only AFTER the merged `(WHERE ∧ effective-policy)` predicate.
/// `gather_rows` applies exactly that — the access path plus `filter`, which is
/// where the planner AND-folded the policy — so accumulating over its output
/// satisfies §4 by construction. Reading the raw scan instead would make
/// `count(*)` report rows the caller cannot see, and a count leaks existence
/// whether or not the rows come back. §4 calls it "a natural mistake, since some
/// policy conjuncts land in the residual"; the only defence is to never hold the
/// unfiltered stream, which is why there is no cursor here.
///
/// The other trap: **LIMIT applies to GROUPS, not rows.** The non-aggregate path
/// bounds `gather_rows` by offset+limit, which would be silently wrong here —
/// `LIMIT 1` on a grouped query means one group, not one input row. So this
/// gathers unbounded and bounds at the end.
#[allow(clippy::too_many_arguments)]
pub(super) fn exec_aggregate(
    ctx: &mut dyn TxnCtx,
    plan: &CompiledPlan,
    params: &[Value],
    schema: &Schema,
    t: &TableDef,
    table: u32,
    access: &AccessPath,
    filter: Option<&ExprProgram>,
    joins: &[Join],
    joined_filter: Option<&ExprProgram>,
    agg: &Aggregation,
    projection: &[Projection],
    order_by: &[(u16, bool, Collation)],
    order_over: OrderOver,
    order_junk: u16,
    limit: Option<u64>,
    offset: Option<u64>,
    distinct: bool,
    // First reserved result slot of THIS level (`subplan_base` at the top,
    // `sub.sub_base` for a nested aggregate subplan) — where the per-row
    // correlated fill writes. Unused when `correlated` is empty and
    // `post_filter` is `None`.
    base: usize,
    // #73 §1: the correlated subplans (per-row filled) and the correlated WHERE
    // residual. Empty/`None` for a plain aggregate, which behaves exactly as
    // before.
    correlated: &[(usize, &SubPlan)],
    post_filter: Option<&ExprProgram>,
) -> Result<ExecResult> {
    // Unbounded on purpose: see the LIMIT note above. Over a join the row being
    // aggregated is the JOINED row — same rule, wider row.
    let rows = match joins.is_empty() {
        true => gather_rows(ctx, table, access, filter, plan, params, None)?,
        false => {
            gather_joined(ctx, plan, params, schema, table, access, filter, joins, joined_filter)?
        }
    };

    // #73 §1: aggregate over a correlated filter. Fill each correlated slot per
    // gathered row and apply the correlated WHERE residual BEFORE grouping, so
    // accumulation still consumes only the full `(WHERE ∧ policy)` set
    // (DESIGN-MULTIDB §4 — the same ordering the plain gather guarantees). The
    // shared `correlated_survivors` keeps this byte-identical to the
    // non-aggregate correlated path, memo included. The grouped programs never
    // read a correlated slot (planner + validate forbid it), so grouping below
    // reads `params` and the per-row scratch is discarded here.
    let rows: Vec<Vec<Value>> = if correlated.is_empty() && post_filter.is_none() {
        rows
    } else {
        correlated_survivors(ctx, schema, plan, params, base, rows, correlated, post_filter)?
            .into_iter()
            .map(|(row, _scratch)| row)
            .collect()
    };

    // sqlite "bare columns" (COMPAT.md): each group also carries the values of
    // `agg.bare_cols` from its single min()/max() WITNESS row. The planner +
    // `validate` guarantee that a non-empty `bare_cols` comes with exactly one
    // aggregate and it is Min/Max, so `agg.aggs[0]` is the governing extremum.
    let has_bare = !agg.bare_cols.is_empty();
    let mm = if has_bare { agg.aggs.first() } else { None };

    // Group. The key is the memcmp-ordered keycode of the group columns, so
    // groups come out in a deterministic order for free and NULL keys group
    // together (SQL treats NULLs as one group in GROUP BY, unlike `=`).
    //
    // The optional third element is the bare-column witness: `(extreme, bare)`
    // where `extreme` is the running min/max value (None until the first non-NULL
    // arg) and `bare` is the extremum row's values for `agg.bare_cols`.
    type Group = (Vec<Value>, Vec<Accum>, Option<(Option<Value>, Vec<Value>)>);
    let mut groups: std::collections::BTreeMap<Vec<u8>, Group> = Default::default();
    for row in &rows {
        let key_vals: Vec<Value> = agg
            .group_by
            .iter()
            .map(|k| match k {
                GroupKey::Col(c) => Ok(row.get(*c as usize).cloned().unwrap_or(Value::Null)),
                // A computed key — `GROUP BY a+1` — evaluated over the base row.
                GroupKey::Expr(p) => p.eval(row, params),
            })
            .collect::<Result<_>>()?;
        let key = keycode::encode_key(&key_vals);
        let entry = groups.entry(key).or_insert_with(|| {
            (
                key_vals,
                agg.aggs.iter().map(new_accum).collect(),
                if has_bare { Some((None, Vec::new())) } else { None },
            )
        });
        for (i, call) in agg.aggs.iter().enumerate() {
            match &call.arg {
                // count(*): the ROW is the input, so nothing is evaluated and
                // NULL cannot arise.
                None => entry.1[i].push(None)?,
                Some(p) => {
                    let v = p.eval(row, params)?;
                    entry.1[i].push(Some(&v))?;
                }
            }
        }
        // Update the bare-column witness. sqlite's rule, reproduced exactly: the
        // bare columns come from the row that achieved the min/max; on a tie the
        // FIRST such row wins (`min_max_prefers` replaces only on a strict beat);
        // and until any non-NULL extremum is seen the witness tracks the LATEST
        // row, so an all-NULL-argument group takes its bare values from its last
        // row — the behavior differential-tested against sqlite 3.45.
        if let (Some(mm), Some(w)) = (mm, entry.2.as_mut()) {
            let v = match &mm.arg {
                Some(p) => p.eval(row, params)?,
                None => Value::Null,
            };
            let capture = || -> Vec<Value> {
                agg.bare_cols
                    .iter()
                    .map(|&c| row.get(c as usize).cloned().unwrap_or(Value::Null))
                    .collect()
            };
            match &w.0 {
                None => {
                    w.1 = capture();
                    if !v.is_null() {
                        w.0 = Some(v);
                    }
                }
                Some(e) => {
                    if !v.is_null() && mm.func.min_max_prefers(e, &v)? {
                        w.0 = Some(v);
                        w.1 = capture();
                    }
                }
            }
        }
    }

    // `SELECT count(*) FROM t` over an EMPTY table must return one row (0), not
    // zero rows — there is one group when there is nothing to group by. With a
    // GROUP BY, an empty input means no groups at all.
    let mut out: Vec<Vec<Value>> = Vec::new();
    if groups.is_empty() && agg.group_by.is_empty() {
        let accs: Vec<Accum> = agg.aggs.iter().map(new_accum).collect();
        let mut tuple: Vec<Value> = accs.into_iter().map(|a| a.finish()).collect();
        // No rows means no witness row, so a bare column is NULL — sqlite:
        // `SELECT name, max(x) FROM empty` yields one row `(NULL, NULL)`.
        tuple.resize(tuple.len() + agg.bare_cols.len(), Value::Null);
        out.push(tuple);
    }
    for (_, (keys, accs, witness)) in groups {
        let mut tuple = keys;
        tuple.extend(accs.into_iter().map(|a| a.finish()));
        // Every group has at least one row, so the witness `bare` is populated;
        // extend the grouped tuple to `[keys ‖ aggs ‖ bare]`.
        if let Some((_, bare)) = witness {
            tuple.extend(bare);
        }
        out.push(tuple);
    }

    // HAVING — over the GROUPED tuple, which is why it can see aggregates and
    // WHERE cannot.
    if let Some(h) = &agg.having {
        let mut keep = Vec::with_capacity(out.len());
        for tuple in out {
            if h.eval_filter(&mut Vec::new(), &tuple, params)? {
                keep.push(tuple);
            }
        }
        out = keep;
    }

    // Sort the GROUPED tuple only when the indices refer to it; otherwise the
    // sort waits for the projection below.
    if order_over == OrderOver::Grouped && !order_by.is_empty() {
        sort_rows(&mut out, order_by);
    }

    let skip = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
    let take = limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    let mut projected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tuple in out {
        let mut orow = Vec::with_capacity(projection.len());
        for p in projection {
            orow.push(match p {
                Projection::Column(i) => tuple
                    .get(*i as usize)
                    .cloned()
                    .ok_or_else(|| internal("grouped projection column"))?,
                Projection::Expr { program, .. } => program.eval(&tuple, params)?,
            });
        }
        // `SELECT DISTINCT dept, count(*) … GROUP BY dept` — the groups are
        // already distinct by key, but the PROJECTION need not be (two groups
        // can share a count), so this still has work to do.
        if distinct && !seen.insert(keycode::encode_key(&orow)) {
            continue;
        }
        projected.push(orow);
    }
    if order_over == OrderOver::Projection {
        sort_rows(&mut projected, order_by);
    }
    // Sort-only columns come off after the sort, exactly as in the plain path —
    // `ORDER BY count(*) + 1` computes a column nobody asked to see.
    if order_junk > 0 {
        let keep = projection.len() - order_junk as usize;
        for row in &mut projected {
            row.truncate(keep);
        }
    }
    let projected: Vec<Vec<Value>> = projected.into_iter().skip(skip).take(take).collect();
    let columns = projection
        .iter()
        .take(projection.len() - order_junk as usize)
        .map(|p| match p {
            Projection::Column(i) => t
                .columns
                .get(*i as usize)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("col{i}")),
            Projection::Expr { name, .. } => name.clone(),
        })
        .collect();
    Ok(ExecResult::Rows {
        columns,
        rows: projected,
    })
}
