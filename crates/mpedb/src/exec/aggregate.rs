use super::*;
use mpedb_types::{HostAggState, HostAggs};

/// One aggregate's running state over one group: a built-in [`Accum`], or a HOST
/// aggregate's `xStep`/`xFinal` accumulator (design/DESIGN-UDF.md stage 2).
///
/// The two differ on NULL, deliberately. A built-in SKIPS NULL arguments
/// ([`Accum::push`] is where that rule lives); sqlite hands a USER aggregate
/// every row, NULLs included, and lets `xStep` decide — Django's `StdDevPop`
/// depends on it. DISTINCT and `FILTER (WHERE …)` are applied identically to
/// both: the filter by the caller before `push`, the dedup here.
enum Acc {
    Native(Accum),
    Host {
        state: Box<dyn HostAggState>,
        /// `f(DISTINCT x)`: values already stepped in this group, keyed by their
        /// storage-class grouping encoding — the same mechanism
        /// [`Accum::new_distinct`] uses.
        /// `None` for a non-DISTINCT call, so the common case pays nothing.
        seen: Option<std::collections::BTreeSet<Vec<u8>>>,
        /// The argument column's declared collation, folded into the dedup key
        /// exactly as [`Accum`] does for a native aggregate.
        coll: Collation,
    },
}

impl Acc {
    /// Feed one row. `None` is `count(*)`'s "the row itself" — never produced for
    /// a host aggregate (the plan decoder rejects a host call with no argument).
    fn push(&mut self, v: Option<&Value>) -> Result<()> {
        match self {
            Acc::Native(a) => a.push(v),
            Acc::Host { state, seen, coll } => {
                let args = match v {
                    Some(v) => std::slice::from_ref(v),
                    None => &[][..],
                };
                if let Some(seen) = seen {
                    if !seen.insert(keycode::encode_group_key(args, std::slice::from_ref(coll))) {
                        return Ok(());
                    }
                }
                state.step(args)
            }
        }
    }

    fn finish(self) -> Result<Value> {
        match self {
            Acc::Native(a) => Ok(a.finish()),
            // sqlite frees the aggregate context right after `xFinal`; consuming
            // the boxed state here is that free.
            Acc::Host { state, .. } => state.finish(),
        }
    }
}

/// Mint a fresh accumulator for one aggregate over one group. A host aggregate
/// needs the connection's factory table, so this is where a plan naming one
/// executed OUT of scope (the write path, the streaming/overlay read paths)
/// surfaces a clean error instead of a wrong answer.
fn new_accum(a: &AggCall, host: Option<&dyn HostAggs>, coll: Collation) -> Result<Acc> {
    let Some(name) = a.func.host() else {
        let f = a.func.native().ok_or_else(|| internal("aggregate with no target"))?;
        return Ok(Acc::Native(if a.distinct {
            Accum::new_distinct_collated(f, coll)
        } else {
            Accum::new(f)
        }));
    };
    let host = host.ok_or_else(|| {
        Error::Unsupported(format!(
            "host aggregate {name}() is not in scope for this execution"
        ))
    })?;
    // The plan's shape IS the arity: a host call always carries its one argument
    // (decode enforces it), so the registry lookup asks for exactly that.
    let argc = a.arg.is_some() as i32;
    Ok(Acc::Host {
        state: host.create(name, argc)?,
        seen: a.distinct.then(std::collections::BTreeSet::new),
        coll,
    })
}

/// The collation an `f(DISTINCT x)` dedup folds TEXT under: the DECLARED
/// collation of the column `x` names, sqlite's rung-2 rule and exactly what
/// `SELECT DISTINCT` already applies through `output_collations`.
///
/// Only a BARE column reference carries one — `count(DISTINCT lower(name))`
/// dedups an expression's result, which has no column and therefore BINARY,
/// as in sqlite. The argument program for a bare column is the single
/// instruction `PushCol(i)`, so that is the shape matched.
fn arg_collation(a: &AggCall, base_colls: &[Collation]) -> Collation {
    if !a.distinct {
        return Collation::Binary;
    }
    match a.arg.as_ref().map(|p| p.instrs.as_slice()) {
        Some([mpedb_types::Instr::PushCol(i)]) => {
            base_colls.get(*i as usize).copied().unwrap_or(Collation::Binary)
        }
        _ => Collation::Binary,
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
    order_by: &[(u16, SortDir, Collation)],
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
    // non-aggregate correlated path, memo included.
    //
    // Each survivor keeps its SCRATCH — the `[user ‖ subplan results]` vector
    // with this row's correlated slots filled. The row loop below evaluates the
    // PER-ROW programs (`FILTER (WHERE …)`, an aggregate argument, a computed
    // GROUP BY key) against that scratch, not against `params`: `params` still
    // holds NULL in every correlated slot, and a filter that reads one would
    // evaluate to NULL and — 3VL — DROP the row rather than test it. That is the
    // wrong answer `count(*) FILTER (WHERE EXISTS (…correlated…))` used to give,
    // 0 for both the positive and the negated form. The GROUPED programs (HAVING,
    // the projection, ORDER BY) still read `params`: they run over a collapsed
    // group, where no single row's correlation applies, and both the planner
    // (`reject_correlated_in_aggregate`) and validate refuse a correlated slot
    // there.
    let (rows, scratches): (Vec<Vec<Value>>, Option<Vec<Vec<Value>>>) =
        if correlated.is_empty() && post_filter.is_none() {
            (rows, None)
        } else {
            let survivors =
                correlated_survivors(ctx, schema, plan, params, base, rows, correlated, post_filter)?;
            let mut kept = Vec::with_capacity(survivors.len());
            let mut scratch = Vec::with_capacity(survivors.len());
            for (row, s) in survivors {
                kept.push(row);
                scratch.push(s);
            }
            (kept, Some(scratch))
        };

    // sqlite "bare columns" (COMPAT.md): each group also carries the values of
    // `agg.bare_cols` from a WITNESS row. Which row is sqlite's — and so mpedb's —
    // is inferable from the aggregate set (no plan byte, so PLAN_FORMAT stays 30):
    //   * EXACTLY ONE min()/max() (even alongside count/sum/avg) → that extremum's
    //     row, sqlite's documented rule (#87, verified: `min(x), count(*)` follows
    //     the min);
    //   * ZERO min()/max() → sqlite's "arbitrary" pick, which is deterministic: the
    //     group's LOWEST-ROWID row. mpedb identifies a single-table row by its PK,
    //     so it tracks the minimum PK per group and takes that row. The planner
    //     only emits this shape over a single INTEGER-PK table (`rowid_pick_ok`),
    //     where PK == sqlite's rowid, so the pick matches sqlite EXACTLY.
    // The planner refuses the ≥2 min/max case (sqlite follows the LAST min/max — an
    // order-dependent, undocumented pick), so a legitimately compiled plan never
    // reaches here with it; a forged one falls into the safe min-PK branch below.
    let has_bare = !agg.bare_cols.is_empty();
    // A HOST aggregate is never a min/max: `AggTarget::native()` is None for one,
    // so it can neither govern the witness nor be miscounted here.
    let mm: Option<(&AggCall, mpedb_types::AggFn)> = {
        let mut it = agg.aggs.iter().filter_map(|c| match c.func.native() {
            Some(f @ (mpedb_types::AggFn::Min | mpedb_types::AggFn::Max)) => Some((c, f)),
            _ => None,
        });
        match (it.next(), it.next()) {
            // Exactly one min/max → its witness row governs the bare columns.
            (Some(c), None) => Some(c),
            // Zero (→ lowest rowid) or two-plus (planner-refused) → min-PK path.
            _ => None,
        }
    };

    // Group. The key is the memcmp-ordered keycode of the group columns, so
    // groups come out in a deterministic order for free and NULL keys group
    // together (SQL treats NULLs as one group in GROUP BY, unlike `=`).
    //
    // The optional third element is the bare-column witness, in one of two shapes
    // (chosen by `mm`): the min/max extremum row, or the lowest-rowid row.
    enum Witness {
        // #87: `extreme` is the running min/max (None until the first non-NULL
        // arg); `bare` is the extremum row's values for `agg.bare_cols`.
        MinMax { extreme: Option<Value>, bare: Vec<Value> },
        // sqlite's arbitrary pick: `pk` is the encoded PK of the lowest-rowid row
        // seen so far (empty = no row yet); `bare` is that row's bare values.
        MinRowid { pk: Vec<u8>, bare: Vec<Value> },
    }
    // The collation each GROUP BY key groups under: a bare NOCASE/RTRIM column
    // collapses case-/space-variants into ONE group (sqlite parity); a computed
    // key is BINARY. Folded before encoding, so `'abc'` and `'ABC'` (NOCASE) hash
    // to the same bucket — the equality half of the column's declared collation.
    let base_colls = super::base_row_collations(schema, table, joins);
    let group_collations: Vec<Collation> = agg
        .group_by
        .iter()
        .map(|k| match k {
            GroupKey::Col(c) => base_colls.get(*c as usize).copied().unwrap_or(Collation::Binary),
            GroupKey::Expr(_) => Collation::Binary,
        })
        .collect();
    type Group = (Vec<Value>, Vec<Acc>, Option<Witness>);
    let mut groups: std::collections::BTreeMap<Vec<u8>, Group> = Default::default();
    // Bound once: the factory table for host aggregates is read per GROUP (one
    // fresh accumulator each), and both this and `ctx.host_fns()` are shared
    // reborrows of the same context.
    let host_aggs = ctx.host_aggs();
    for (ri, row) in rows.iter().enumerate() {
        // The parameter vector for THIS row's per-row programs: the correlated
        // scratch when this plan has correlated subplans, otherwise `params`
        // itself (no clone, no correlated slot to fill). The two agree on every
        // user and uncorrelated-subplan slot — the scratch is a copy of `params`
        // with only the correlated slots overwritten — so this is a no-op except
        // exactly where a correlated slot is read.
        let row_params: &[Value] = match &scratches {
            Some(s) => &s[ri],
            None => params,
        };
        let key_vals: Vec<Value> = agg
            .group_by
            .iter()
            .map(|k| match k {
                GroupKey::Col(c) => Ok(row.get(*c as usize).cloned().unwrap_or(Value::Null)),
                // A computed key — `GROUP BY a+1` — evaluated over the base row.
                GroupKey::Expr(p) => p.eval_host(row, row_params, ctx.host_fns()),
            })
            .collect::<Result<_>>()?;
        // The grouping key is sqlite's storage-class key, NOT the on-disk one:
        // over a typeless column `1` and `1.0` are ONE group and the text `'1'`
        // another. Its byte order is also sqlite's class order, so this
        // `BTreeMap` still iterates groups the way sqlite emits them.
        let key = keycode::encode_group_key(&key_vals, &group_collations);
        // Not `or_insert_with`: minting a HOST accumulator can FAIL (an
        // out-of-scope or unregistered aggregate), and a closure cannot carry
        // that error out.
        let entry = match groups.entry(key) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                let witness = if !has_bare {
                    None
                } else if mm.is_some() {
                    Some(Witness::MinMax { extreme: None, bare: Vec::new() })
                } else {
                    Some(Witness::MinRowid { pk: Vec::new(), bare: Vec::new() })
                };
                let accs = agg
                    .aggs
                    .iter()
                    .map(|c| new_accum(c, host_aggs, arg_collation(c, &base_colls)))
                    .collect::<Result<Vec<Acc>>>()?;
                e.insert((key_vals, accs, witness))
            }
        };
        for (i, call) in agg.aggs.iter().enumerate() {
            // `agg(x) FILTER (WHERE cond)`: this row feeds THIS aggregate only
            // when `cond` is TRUE over the base row (3VL — NULL/FALSE skip). The
            // filter is per-aggregate, so two aggregates in one SELECT can have
            // different filters (or none). For DISTINCT this runs BEFORE the
            // dedupe inside `Accum` — filter first, then dedupe.
            if let Some(f) = &call.filter {
                if !f.eval_filter_host(&mut Vec::new(), row, row_params, ctx.host_fns())? {
                    continue;
                }
            }
            match &call.arg {
                // count(*): the ROW is the input, so nothing is evaluated and
                // NULL cannot arise.
                None => entry.1[i].push(None)?,
                Some(p) => {
                    let v = p.eval_host(row, row_params, ctx.host_fns())?;
                    entry.1[i].push(Some(&v))?;
                }
            }
        }
        // Update the bare-column witness, reproducing sqlite's rule EXACTLY.
        if let Some(w) = entry.2.as_mut() {
            let capture = || -> Vec<Value> {
                agg.bare_cols
                    .iter()
                    .map(|&c| row.get(c as usize).cloned().unwrap_or(Value::Null))
                    .collect()
            };
            match w {
                // The bare columns come from the row that achieved the min/max; on
                // a tie the FIRST such row wins (`min_max_prefers` replaces only on
                // a strict beat); and until any non-NULL extremum is seen the
                // witness tracks the LATEST row, so an all-NULL-argument group
                // takes its bare values from its last row — differential-tested
                // against sqlite 3.45.
                Witness::MinMax { extreme, bare } => {
                    let (mm, mm_fn) =
                        mm.expect("MinMax witness implies a single min/max aggregate");
                    // A FILTER on the governing min/max restricts BOTH the
                    // extremum AND the witness row to the rows it accepts
                    // (verified vs sqlite 3.45: the bare column follows the
                    // FILTERED extremum). A rejected row contributes nothing —
                    // except that when the filter rejects EVERY row in the group,
                    // sqlite falls back to the group's FIRST row for the bare
                    // values, so seed `bare` from the first row while it is still
                    // empty. With no filter this is byte-identical to before
                    // (`passes` is always true, and the extreme=None branch already
                    // captured every row, so the last row wins as documented).
                    let passes = match &mm.filter {
                        Some(fp) => {
                            fp.eval_filter_host(&mut Vec::new(), row, row_params, ctx.host_fns())?
                        }
                        None => true,
                    };
                    if !passes {
                        if bare.is_empty() {
                            *bare = capture();
                        }
                    } else {
                        let v = match &mm.arg {
                            Some(p) => p.eval_host(row, row_params, ctx.host_fns())?,
                            None => Value::Null,
                        };
                        match extreme {
                            None => {
                                *bare = capture();
                                if !v.is_null() {
                                    *extreme = Some(v);
                                }
                            }
                            Some(e) => {
                                if !v.is_null() && mm_fn.min_max_prefers(e, &v)? {
                                    *extreme = Some(v);
                                    *bare = capture();
                                }
                            }
                        }
                    }
                }
                // sqlite's "arbitrary" pick is the group's lowest-rowid row. The
                // row's PK identifies it; over a single INTEGER-PK table (the only
                // shape the planner emits this for) the PK IS sqlite's rowid, so
                // the smallest encoded PK is sqlite's pick. Encoding gives memcmp
                // order = ascending PK, so tracking the running minimum matches
                // sqlite even when the scan is NOT PK-ordered (an index or
                // descending-range access path).
                Witness::MinRowid { pk, bare } => {
                    let pk_vals: Vec<Value> = t
                        .primary_key
                        .iter()
                        .map(|&c| row.get(c as usize).cloned().unwrap_or(Value::Null))
                        .collect();
                    let this = keycode::encode_key(&pk_vals);
                    if pk.is_empty() || this < *pk {
                        *pk = this;
                        *bare = capture();
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
        // An EMPTY group still FINISHES a fresh accumulator — for a host
        // aggregate that is `xFinal` on a never-stepped (NULL) aggregate
        // context, which is exactly sqlite's rule and why Django's
        // `STDDEV_POP` over no rows is NULL rather than an error.
        let accs = agg
            .aggs
            .iter()
            .map(|c| new_accum(c, host_aggs, arg_collation(c, &base_colls)))
            .collect::<Result<Vec<Acc>>>()?;
        let mut tuple: Vec<Value> =
            accs.into_iter().map(Acc::finish).collect::<Result<_>>()?;
        // No rows means no witness row, so a bare column is NULL — sqlite:
        // `SELECT name, max(x) FROM empty` yields one row `(NULL, NULL)`.
        tuple.resize(tuple.len() + agg.bare_cols.len(), Value::Null);
        out.push(tuple);
    }
    for (_, (keys, accs, witness)) in groups {
        let mut tuple = keys;
        for a in accs {
            tuple.push(a.finish()?);
        }
        // Every group has at least one row, so the witness `bare` is populated;
        // extend the grouped tuple to `[keys ‖ aggs ‖ bare]`.
        if let Some(w) = witness {
            let bare = match w {
                Witness::MinMax { bare, .. } | Witness::MinRowid { bare, .. } => bare,
            };
            tuple.extend(bare);
        }
        out.push(tuple);
    }

    // HAVING — over the GROUPED tuple, which is why it can see aggregates and
    // WHERE cannot.
    if let Some(h) = &agg.having {
        let mut keep = Vec::with_capacity(out.len());
        for tuple in out {
            if h.eval_filter_host(&mut Vec::new(), &tuple, params, ctx.host_fns())? {
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
                Projection::Expr { program, .. } => program.eval_host(&tuple, params, ctx.host_fns())?,
            });
        }
        // `SELECT DISTINCT dept, count(*) … GROUP BY dept` — the groups are
        // already distinct by key, but the PROJECTION need not be (two groups
        // can share a count), so this still has work to do.
        if distinct && !seen.insert(keycode::encode_group_key(&orow, &[])) {
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
