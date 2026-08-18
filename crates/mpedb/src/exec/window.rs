//! Window-function execution (design/DESIGN-WINDOW.md stage 1).
//!
//! A post-pass over the materialized base rows: each row is EXTENDED in place
//! with one value per window (in window order, at slots `base_width..`), then the
//! projection — compiled over the extended tuple — reads those slots. Rows keep
//! their gather order; only the window VALUES are computed, via a per-window
//! index sort that never reorders the rows themselves (so the outer ORDER BY,
//! over the projection, decides the final order).
//!
//! This is a pure in-process, read-only feature: nothing here touches the
//! engine, the commit path, or footprints (a window is key-neutral, so
//! `select_footprint` never sees it).

use super::*;
use mpedb_sql::{Frame, FrameBound, FrameExclude, FrameMode, WinInt, WindowFunc, WindowSpec};
use mpedb_types::{Accum, HostAggState, HostAggs};
use std::cmp::Ordering;

/// Execute a windowed SELECT: gather the base rows in full, compute every window,
/// project over the extended rows, then sort/trim/bound. A windowed plan always
/// carries `order_over = Projection` (the sort must follow the window phase) and
/// no correlated subplans (the planner refuses that combination), so this is the
/// only executor path a window ever reaches.
pub(super) fn exec_select_windowed(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<ExecResult> {
    // Gather the base rows in full — a window needs every row before it can
    // assign any value, so no scan bound and no top-K apply (the plan forces
    // `order_over = Projection`, which already disables both).
    let mut rows = if !sp.joins.is_empty() {
        gather_joined(
            ctx,
            plan,
            params,
            schema,
            sp.table,
            &sp.access,
            sp.filter.as_ref(),
            &sp.joins,
            sp.joined_filter.as_ref(),
            // No #125 pruning under a window: the projection here indexes the
            // EXTENDED tuple `[base ‖ w0..wk]` and `compute_windows` reads the
            // base row through side vectors the analysis does not model, so
            // `row_prune` refuses a windowed plan outright.
            None,
        )?
    } else {
        gather_rows(ctx, sp.table, &sp.access, sp.filter.as_ref(), plan, params, None)?
    };

    // Declared collations of the base row, from which every window clause's
    // collation follows (see `program_coll`).
    let base_colls = base_row_collations(schema, plan, sp.table, &sp.joins);
    compute_windows(
        &mut rows,
        &sp.windows,
        &base_colls,
        params,
        ctx.host_fns(),
        ctx.host_aggs(),
    )?;

    // Project over the extended rows `[base ‖ w0..wk]`. DISTINCT dedups the
    // projected tuples AFTER the window phase (the same key encoding the plain
    // path uses, so NULLs compare equal).
    let mut out: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    for row in &rows {
        let mut orow = Vec::with_capacity(sp.projection.len());
        for p in &sp.projection {
            orow.push(match p {
                Projection::Column(i) => row
                    .get(*i as usize)
                    .cloned()
                    .ok_or_else(|| internal("window projection column"))?,
                // A window frame is defined over a fixed row set; expanding a
                // row inside one would change the frame it was computed for.
                Projection::SetReturning { .. } => {
                    return Err(Error::Unsupported(
                        "a set-returning function is not supported alongside a window \
                         function"
                            .into(),
                    ))
                }
                Projection::Expr { program, .. } => {
                    program.eval_host(row, params, ctx.host_fns())?
                }
            });
        }
        if sp.distinct && !seen.insert(keycode::encode_group_key(&orow, &[])) {
            continue;
        }
        out.push(orow);
    }

    // The outer ORDER BY runs over the projection (windows force it there).
    if !sp.order_by.is_empty() {
        super::gather::check_order_colls(&sp.order_by, ctx.host_colls())?;
        sort_rows(&mut out, &sp.order_by, ctx.host_colls());
    }
    // Sort-only junk columns are trailing; trim them after the sort.
    if sp.order_junk > 0 {
        let keep = sp.projection.len() - sp.order_junk as usize;
        for row in &mut out {
            row.truncate(keep);
        }
    }
    let (l, o) = super::resolve_limit_offset(sp.limit, sp.offset, params)?;
    let skip = o.min(usize::MAX as u64) as usize;
    let take = l.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    if skip > 0 || take != usize::MAX {
        out = out.into_iter().skip(skip).take(take).collect();
    }
    let columns = select_output_columns(schema, plan, sp)?;
    Ok(ExecResult::Rows { columns, rows: out })
}

/// Compute every window over the materialized rows, extending each row with one
/// result value per window (at `base_width + k`). Rows are never reordered — the
/// The DECLARED collation of each slot of the GROUPED tuple
/// `[keys ‖ aggs ‖ bare]`, so a window over a grouped result compares the same
/// way the ungrouped one does.
///
/// A key that IS a base column carries that column's collation (`GROUP BY
/// dept` on a NOCASE column, then `PARTITION BY dept`); a computed key does
/// not. A bare column likewise carries the column it is read from. An
/// AGGREGATE result carries none — it is a computed value, and sqlite gives a
/// computed expression BINARY.
pub(super) fn grouped_collations(
    schema: &Schema,
    plan: &CompiledPlan,
    sp: &mpedb_sql::SelectPlan,
    agg: &mpedb_sql::Aggregation,
) -> Vec<Collation> {
    if sp.windows.is_empty() {
        return Vec::new();
    }
    let base = super::base_row_collations(schema, plan, sp.table, &sp.joins);
    let at = |i: u16| base.get(i as usize).copied().unwrap_or(Collation::Binary);
    let mut out: Vec<Collation> = agg
        .group_by
        .iter()
        .map(|k| match k {
            mpedb_sql::GroupKey::Col(i) => at(*i),
            mpedb_sql::GroupKey::Expr(_) => Collation::Binary,
        })
        .collect();
    out.resize(out.len() + agg.aggs.len(), Collation::Binary);
    out.extend(agg.bare_cols.iter().map(|&i| at(i)));
    out
}

/// One window ORDER BY key's comparison inputs: DESCENDING, and the COLLATION.
/// Bundled because every peer test, every sort and every frame boundary must
/// use the same pair — carrying them as two parallel slices is how the
/// collation went missing from three of the four comparison sites.
type OrdKey = (bool, Collation);

/// The collating sequence an expression compares under: the DECLARED collation
/// of the bare column it names, and BINARY for anything computed.
///
/// This is sqlite's rule and the one `min(x) OVER (…)` already used for a
/// window ARGUMENT (probed against the bundled 3.45.0). An explicit `COLLATE`
/// inside a window clause is refused at bind, so a `PushCol` is the whole rule.
fn program_coll(p: Option<&ExprProgram>, base_colls: &[Collation]) -> Collation {
    match p.map(|p| p.instrs.as_slice()) {
        Some([mpedb_types::Instr::PushCol(i)]) => {
            base_colls.get(*i as usize).copied().unwrap_or(Collation::Binary)
        }
        _ => Collation::Binary,
    }
}

/// index vector is sorted and each result is written back at the row's ORIGINAL
/// index — so the base rows stay in gather order for the outer sort.
pub(super) fn compute_windows(
    rows: &mut [Vec<Value>],
    windows: &[WindowSpec],
    // The DECLARED collation of each base-row column, from which every window
    // clause's collation follows (`program_coll`). May be shorter than the row
    // (defensive): a missing entry is Binary.
    base_colls: &[Collation],
    params: &[Value],
    // The connection's HOST SCALAR functions. A window's PARTITION BY, its
    // ORDER BY, its argument and a lag/lead default are all ordinary
    // expressions and may call one — `PARTITION BY django_date_extract('year',
    // d)` is what Django's ORM writes for `.annotate(…, partition_by=…__year)`.
    // These four evaluations used `eval` (no host), so every such window
    // failed with "host function …() is not in scope for this execution" while
    // the SAME call in the projection, in GROUP BY and in the statement's
    // ORDER BY worked.
    host_fns: Option<&dyn HostFns>,
    // The connection's HOST window-aggregate registry, for
    // `WindowFunc::Host` (design/DESIGN-UDF.md stage 4). `None` wherever no
    // host registration can be in scope — the mechanism stays inert.
    host_aggs: Option<&dyn HostAggs>,
) -> Result<()> {
    if rows.is_empty() || windows.is_empty() {
        return Ok(());
    }
    let base_width = rows[0].len();
    // Reserve the K result slots on every row up front (NULL placeholders); the
    // window sub-programs only read base slots, so evaluating them over the
    // extended row is identical to evaluating over the base row.
    for row in rows.iter_mut() {
        row.resize(base_width + windows.len(), Value::Null);
    }
    let n = rows.len();

    for (k, w) in windows.iter().enumerate() {
        // A window's PARTITION BY and ORDER BY compare under the DECLARED
        // collation of a bare column, exactly as the statement's ORDER BY and
        // GROUP BY do — `PARTITION BY s` on a `TEXT COLLATE NOCASE` column puts
        // 'A' and 'a' in ONE partition, and `ORDER BY s` makes them peers.
        // Both used BINARY, which was not a refusal but a WRONG ANSWER: a
        // different partitioning and a different rank than sqlite's.
        // A computed key carries no declared collation (the `[PushCol]` rule
        // `arg_colls` already uses), and an explicit `COLLATE` in a window
        // clause is refused at bind — so this is the whole rule.
        let part_colls: Vec<Collation> =
            w.partition_by.iter().map(|p| program_coll(Some(p), base_colls)).collect();
        let ord_colls: Vec<Collation> =
            w.order_by.iter().map(|(p, _)| program_coll(Some(p), base_colls)).collect();
        // Per-row partition key, ordering values, and (for an aggregate window)
        // the argument value — all evaluated over the base row.
        let mut part_key: Vec<Vec<u8>> = Vec::with_capacity(n);
        let mut order_vals: Vec<Vec<Value>> = Vec::with_capacity(n);
        let mut arg_vals: Vec<Option<Value>> = Vec::with_capacity(n);
        // `lag`/`lead` out-of-range default, evaluated at each (current) row —
        // NULL for every other function and for a lag/lead with no default.
        let mut default_vals: Vec<Value> = Vec::with_capacity(n);
        for row in rows.iter() {
            let mut pk = Vec::with_capacity(w.partition_by.len());
            for p in &w.partition_by {
                pk.push(p.eval_host(row, params, host_fns)?);
            }
            // NULLs group together (SQL's PARTITION BY rule) and so do `1`
            // and `1.0` (partition membership is sqlite's comparison) — the
            // total, NULL-equal GROUP key is exactly the GROUP BY keying.
            part_key.push(keycode::encode_group_key(&pk, &part_colls));
            let mut ov = Vec::with_capacity(w.order_by.len());
            for (p, _) in &w.order_by {
                ov.push(p.eval_host(row, params, host_fns)?);
            }
            order_vals.push(ov);
            arg_vals.push(match &w.arg {
                None => None,
                Some(p) => Some(p.eval_host(row, params, host_fns)?),
            });
            default_vals.push(match &w.default {
                Some(p) => p.eval_host(row, params, host_fns)?,
                None => Value::Null,
            });
        }

        // Stable sort of indices by (partition key, window ORDER BY). Stability
        // keeps ties in gather (PK/scan) order — matching row_number's tiebreak
        // and the top-K path's tiebreak elsewhere in the executor.
        let ord_keys: Vec<OrdKey> = w
            .order_by
            .iter()
            .enumerate()
            .map(|(k, (_, d))| (*d, ord_colls.get(k).copied().unwrap_or(Collation::Binary)))
            .collect();
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| {
            part_key[a]
                .cmp(&part_key[b])
                .then_with(|| order_cmp(&order_vals[a], &order_vals[b], &ord_keys))
        });

        assign_window(
            k,
            base_width,
            &idx,
            rows,
            w,
            // Resolved once per window, before any row is touched (format 63).
            resolve_win_int(w.func, params)?,
            // The frame's boundary offsets, likewise: a boundary is consulted
            // per ROW, and its offset may be a parameter.
            match &w.frame {
                Some(f) => resolve_frame_offsets(f, params)?,
                None => FrameOffsets { start: 0, end: 0 },
            },
            program_coll(w.arg.as_ref(), base_colls),
            &part_key,
            &order_vals,
            &arg_vals,
            &default_vals,
            &ord_keys,
            host_aggs,
        )?;
    }
    Ok(())
}

/// Assign one window's values along the sorted index, resetting at each
/// partition boundary. Ranking functions and the default-frame aggregate all
/// walk the same partition/peer-group structure.
#[allow(clippy::too_many_arguments)]
fn assign_window(
    k: usize,
    base_width: usize,
    idx: &[usize],
    rows: &mut [Vec<Value>],
    w: &WindowSpec,
    // The value function's integer argument (`lag`/`lead` offset, `nth_value`'s
    // n, `ntile`'s bucket count), resolved ONCE for this execution by
    // `resolve_win_int`. It is a literal or a PARAMETER, never per-row — which
    // is why one read serves every row here.
    win_int: i64,
    // The frame's two boundary offsets, resolved for this execution alongside
    // `win_int` and for the same reason.
    fo: FrameOffsets,
    // The argument's collating sequence — the min/max compare (and the DISTINCT
    // dedup, had windows allowed one) of a `WindowFunc::Agg` accumulator.
    coll: Collation,
    part_key: &[Vec<u8>],
    order_vals: &[Vec<Value>],
    arg_vals: &[Option<Value>],
    default_vals: &[Value],
    ord_keys: &[OrdKey],
    host_aggs: Option<&dyn HostAggs>,
) -> Result<()> {
    let slot = base_width + k;
    let has_order = !w.order_by.is_empty();
    // With no ORDER BY the whole partition is ONE peer group (sqlite gives every
    // row rank 1 / dense_rank 1); with ORDER BY, peers are rows equal on all
    // keys. The aggregate cumulative branch only consults this when `has_order`.
    let peers = |i: usize, j: usize| -> bool {
        !has_order || order_cmp(&order_vals[i], &order_vals[j], ord_keys) == Ordering::Equal
    };

    let mut p = 0usize;
    while p < idx.len() {
        // One partition: the contiguous run of equal partition keys.
        let mut q = p + 1;
        while q < idx.len() && part_key[idx[q]] == part_key[idx[p]] {
            q += 1;
        }
        let part = &idx[p..q];
        // An explicit frame overrides the default-frame logic below for the
        // functions whose result depends on it (aggregates and
        // first_value/last_value/nth_value — the planner refuses a frame on any
        // other function). lag/lead stay frame-independent; ranking/distribution
        // never carry a frame.
        if let Some(frame) = &w.frame {
            assign_framed(
                slot,
                part,
                rows,
                w.func,
                win_int,
                coll,
                frame,
                fo,
                arg_vals,
                order_vals,
                ord_keys,
                has_order,
                w.host.as_deref(),
                host_aggs,
            )?;
            p = q;
            continue;
        }
        match w.func {
            WindowFunc::RowNumber => {
                for (off, &i) in part.iter().enumerate() {
                    rows[i][slot] = Value::Int((off + 1) as i64);
                }
            }
            // Ranking with gaps: at a new peer group the rank jumps to the
            // 1-based position; peers share it (1,1,3).
            WindowFunc::Rank => {
                let mut rank = 1i64;
                for (off, &i) in part.iter().enumerate() {
                    if off > 0 && !peers(i, part[off - 1]) {
                        rank = (off + 1) as i64;
                    }
                    rows[i][slot] = Value::Int(rank);
                }
            }
            // Dense ranking: ++ at each new peer group, no gaps (1,1,2).
            WindowFunc::DenseRank => {
                let mut dense = 1i64;
                for (off, &i) in part.iter().enumerate() {
                    if off > 0 && !peers(i, part[off - 1]) {
                        dense += 1;
                    }
                    rows[i][slot] = Value::Int(dense);
                }
            }
            // Default-frame aggregate. With ORDER BY it is cumulative and — the
            // RANGE-vs-ROWS distinction — every row of a peer group gets the
            // SAME value: the running total THROUGH THE END of that group. With
            // no ORDER BY the whole partition is one group.
            WindowFunc::Agg(f) => {
                let mut acc = Accum::new_collated(f, coll);
                if !has_order {
                    for &i in part {
                        push_arg(&mut acc, &arg_vals[i])?;
                    }
                    let v = acc.finish();
                    for &i in part {
                        rows[i][slot] = v.clone();
                    }
                } else {
                    let mut g = 0usize;
                    while g < part.len() {
                        // One peer group within the partition.
                        let mut h = g + 1;
                        while h < part.len() && peers(part[h], part[g]) {
                            h += 1;
                        }
                        for &i in &part[g..h] {
                            push_arg(&mut acc, &arg_vals[i])?;
                        }
                        // A non-consuming snapshot of the cumulative value.
                        let v = acc.clone().finish();
                        for &i in &part[g..h] {
                            rows[i][slot] = v.clone();
                        }
                        g = h;
                    }
                }
            }
            // A HOST window aggregate under the DEFAULT frame: the same
            // cumulative-through-the-peer-group rule as the built-in above,
            // driven through the caller's xStep/xValue instead of `Accum`.
            WindowFunc::Host => {
                assign_host_default(
                    slot,
                    part,
                    rows,
                    arg_vals,
                    &peers,
                    has_order,
                    w.host.as_deref(),
                    host_aggs,
                )?;
            }
            // lag/lead: frame-INDEPENDENT. A PHYSICAL row offset in window order
            // (not a peer-group hop) — the value `offset` rows before (lag) /
            // after (lead) the current row; out of range ⇒ the per-row default
            // (or NULL). A negative constant offset is legal and simply looks the
            // other way (`p - offset`), exactly as sqlite computes it.
            WindowFunc::Lag(_) | WindowFunc::Lead(_) => {
                let offset = win_int;
                let forward = matches!(w.func, WindowFunc::Lead(_));
                for (off, &i) in part.iter().enumerate() {
                    let cur = off as i64;
                    let target = if forward {
                        cur.checked_add(offset)
                    } else {
                        cur.checked_sub(offset)
                    };
                    rows[i][slot] = match target {
                        Some(t) if (0..part.len() as i64).contains(&t) => {
                            arg_vals[part[t as usize]].clone().unwrap_or(Value::Null)
                        }
                        _ => default_vals[i].clone(),
                    };
                }
            }
            // first_value: the frame START is UNBOUNDED PRECEDING, so it is the
            // partition's FIRST row for every row — constant across the partition.
            WindowFunc::FirstValue => {
                let fv = arg_vals[part[0]].clone().unwrap_or(Value::Null);
                for &i in part {
                    rows[i][slot] = fv.clone();
                }
            }
            // last_value: the frame END is the current row's peer-group end (or
            // the partition end with no ORDER BY). Every row of a peer group sees
            // the group's FINAL row — the RANGE-frame default, matching sqlite.
            WindowFunc::LastValue => {
                let mut g = 0usize;
                while g < part.len() {
                    let mut h = g + 1;
                    while h < part.len() && peers(part[h], part[g]) {
                        h += 1;
                    }
                    let lv = arg_vals[part[h - 1]].clone().unwrap_or(Value::Null);
                    for &i in &part[g..h] {
                        rows[i][slot] = lv.clone();
                    }
                    g = h;
                }
            }
            // nth_value: the n-th row (1-based) of the frame, else NULL. The frame
            // for a peer group ends at that group's last row (exclusive index
            // `h`), so the FIXED row `part[n-1]` is in-frame once `h >= n` — it
            // appears at the peer group that first reaches it and stays for the
            // rest of the partition.
            WindowFunc::NthValue(_) => {
                let nn = win_int;
                let mut g = 0usize;
                while g < part.len() {
                    let mut h = g + 1;
                    while h < part.len() && peers(part[h], part[g]) {
                        h += 1;
                    }
                    // `nn >= 1` (validated); present ⇒ 1 <= nn <= h <= part.len(),
                    // so `nn - 1` is a valid index. Compare in i64 to stay correct
                    // for an absurdly large constant n (which just yields NULL).
                    let v = if (h as i64) >= nn {
                        arg_vals[part[(nn - 1) as usize]].clone().unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    };
                    for &i in &part[g..h] {
                        rows[i][slot] = v.clone();
                    }
                    g = h;
                }
            }
            // ntile: distribute the partition's `sz` rows into `nb` buckets
            // (1-based) along the window order. sqlite's rule: the first `sz % nb`
            // buckets get `ceil(sz/nb)` rows, the rest `floor(sz/nb)`. The planner
            // guarantees an ORDER BY (so the order is deterministic) and `nb >= 1`.
            WindowFunc::Ntile(_) => {
                let nb = win_int;
                let sz = part.len() as i64;
                let nb = nb.max(1); // validated ≥ 1; guard division regardless
                let floor = sz / nb;
                let rem = sz % nb;
                // The first `rem` buckets each hold `floor + 1` rows; together they
                // cover the leading `large` rows. Beyond that, buckets hold `floor`
                // (only reached when floor >= 1, so the division below is safe).
                let large = rem * (floor + 1);
                for (off, &i) in part.iter().enumerate() {
                    let off = off as i64;
                    let bucket = if off < large {
                        off / (floor + 1) + 1
                    } else {
                        rem + (off - large) / floor + 1
                    };
                    rows[i][slot] = Value::Int(bucket);
                }
            }
            // percent_rank: (rank - 1) / (sz - 1), or 0.0 for a one-row partition.
            // Uses rank() semantics — ties share, the next rank skips — so it walks
            // the same peer-group boundary as `Rank`. With no ORDER BY every row is
            // one peer group ⇒ rank 1 ⇒ 0.0 everywhere (matching sqlite).
            WindowFunc::PercentRank => {
                let sz = part.len();
                let denom = (sz as f64) - 1.0;
                let mut rank = 1i64;
                for (off, &i) in part.iter().enumerate() {
                    if off > 0 && !peers(i, part[off - 1]) {
                        rank = (off + 1) as i64;
                    }
                    let pr = if sz <= 1 {
                        0.0
                    } else {
                        (rank - 1) as f64 / denom
                    };
                    rows[i][slot] = Value::Float(pr);
                }
            }
            // cume_dist: (rows whose order key is <= the current row's, peers
            // included) / sz. Every row of a peer group shares the value: the index
            // just past that group (`h`) over `sz`. With no ORDER BY the whole
            // partition is one peer group ⇒ 1.0 everywhere (matching sqlite).
            WindowFunc::CumeDist => {
                let sz = part.len() as f64;
                let mut g = 0usize;
                while g < part.len() {
                    let mut h = g + 1;
                    while h < part.len() && peers(part[h], part[g]) {
                        h += 1;
                    }
                    let cd = h as f64 / sz;
                    for &i in &part[g..h] {
                        rows[i][slot] = Value::Float(cd);
                    }
                    g = h;
                }
            }
        }
        p = q;
    }
    Ok(())
}

/// Assign one window's values under an EXPLICIT frame, for the frame-sensitive
/// functions (aggregate + first/last/nth_value). For each row of the partition
/// (in window order) the frame resolves to a contiguous half-open range
/// `part[lo..hi]`, and the function is computed over exactly those rows. This is
/// a straightforward re-aggregation per row — O(partition · frame) — which is
/// always correct (no incremental removal, so `min`/`max` stay exact); window
/// partitions are small in practice and the default-frame fast paths are
/// untouched.
/// A window value function's integer argument for THIS execution (format 63).
///
/// A literal is itself; a parameter is read once here. The value is one value
/// for the whole execution — never per row — which is the entire reason a
/// parameter is admissible where a column reference is not.
///
/// A bound non-integer is refused BY NAME. sqlite coerces instead (MEASURED at
/// 3.45.1: `lag(a, 1.5)` yields all-NULL, `lag(a, 'x')` behaves as offset 0),
/// and those rules are the version-specific guesswork the 0-wrong-answer
/// contract forbids reproducing. The binder types such a slot `Int64`, so in
/// practice the ordinary parameter check refuses it first; this is the
/// backstop for a plan that reached the executor another way.
///
/// `nth_value`/`ntile` additionally require n ≥ 1, and raise HERE when the
/// value came from a parameter — which is where sqlite raises too (MEASURED:
/// `nth_value(a, 0)` and `ntile(0)` are runtime errors, not parse errors).
fn resolve_win_int(func: WindowFunc, params: &[Value]) -> Result<i64> {
    let (arg, positive, what) = match func {
        WindowFunc::Lag(v) | WindowFunc::Lead(v) => (v, false, "lag/lead offset"),
        WindowFunc::NthValue(v) => (v, true, "second argument to nth_value"),
        WindowFunc::Ntile(v) => (v, true, "argument of ntile"),
        _ => return Ok(0),
    };
    let n = match arg {
        WinInt::Lit(n) => n,
        WinInt::Param(i) => match params.get(i as usize) {
            Some(Value::Int(n)) => *n,
            Some(other) => {
                return Err(Error::TypeMismatch(format!(
                    "{what} must be an integer, got {}",
                    other.type_name()
                )))
            }
            None => {
                return Err(Error::Internal(format!(
                    "window parameter ${} is not bound",
                    i + 1
                )))
            }
        },
    };
    if positive && n < 1 {
        return Err(Error::TypeMismatch(format!(
            "{what} must be a positive integer"
        )));
    }
    Ok(n)
}

#[allow(clippy::too_many_arguments)]
fn assign_framed(
    slot: usize,
    part: &[usize],
    rows: &mut [Vec<Value>],
    func: WindowFunc,
    // `nth_value`'s n, already resolved for this execution (see
    // `resolve_win_int`) — the framed path never sees the parameters.
    win_int: i64,
    // The argument's collating sequence, for a min/max aggregate over the frame.
    coll: Collation,
    frame: &Frame,
    // The frame's two boundary offsets, likewise already resolved — a boundary
    // is consulted per ROW, and its offset may be a parameter.
    fo: FrameOffsets,
    arg_vals: &[Option<Value>],
    order_vals: &[Vec<Value>],
    ord_keys: &[OrdKey],
    has_order: bool,
    host: Option<&str>,
    host_aggs: Option<&dyn HostAggs>,
) -> Result<()> {
    let len = part.len();
    // A HOST window aggregate is the one function here that is NOT re-aggregated
    // per row: it SLIDES (see `assign_host_framed`).
    if matches!(func, WindowFunc::Host) {
        return assign_host_framed(
            slot, part, rows, frame, fo, arg_vals, order_vals, ord_keys, has_order, host, host_aggs,
        );
    }
    // Peer-group structure is needed for GROUPS/RANGE (they count / span peer
    // groups) — and, since format 66, for `EXCLUDE GROUP`/`TIES` under ANY
    // mode: the frame may be purely physical while the exclusion is by peers.
    let (group_of, group_starts) = if matches!(frame.mode, FrameMode::Rows)
        && !needs_peers(frame.exclude)
    {
        (Vec::new(), Vec::new())
    } else {
        build_groups(part, order_vals, ord_keys, has_order)
    };
    // A RANGE offset boundary is a VALUE, so it needs the single ORDER BY key
    // at each POSITION (not each row) and the sort direction. Built only for
    // that frame shape; `check` has refused any other key count.
    let range_keys: Vec<Value> = if matches!(frame.mode, FrameMode::Range) && frame_has_offset(frame)
    {
        part.iter()
            .map(|&i| order_vals[i].first().cloned().unwrap_or(Value::Null))
            .collect()
    } else {
        Vec::new()
    };
    let range_desc = ord_keys.first().is_some_and(|k| k.0);
    for off in 0..len {
        let (lo, hi) = frame_bounds(
            off, len, frame, fo, &group_of, &group_starts, &range_keys, range_desc,
        );
        let keep = exclusion(off, len, frame.exclude, &group_of, &group_starts);
        let target = part[off];
        rows[target][slot] = match func {
            WindowFunc::Agg(f) => {
                let mut acc = Accum::new_collated(f, coll);
                for p in (lo..hi).filter(|&p| keep(p)) {
                    push_arg(&mut acc, &arg_vals[part[p]])?;
                }
                acc.finish()
            }
            // The frame's FIRST / LAST row (in window order), or NULL for an
            // empty frame. `EXCLUDE` can drop either end, so both walk the
            // KEPT positions rather than indexing `lo` / `hi - 1` — a hole at
            // the edge would otherwise return an excluded row's value.
            WindowFunc::FirstValue => match (lo..hi).find(|&p| keep(p)) {
                Some(p) => arg_vals[part[p]].clone().unwrap_or(Value::Null),
                None => Value::Null,
            },
            WindowFunc::LastValue => match (lo..hi).rev().find(|&p| keep(p)) {
                Some(p) => arg_vals[part[p]].clone().unwrap_or(Value::Null),
                None => Value::Null,
            },
            // The n-th row (1-based) WITHIN the frame, or NULL if the frame is
            // shorter than n. `nn >= 1` is validated; the count is over the
            // KEPT rows, so an excluded row does not consume an ordinal.
            WindowFunc::NthValue(_) => {
                let nn = win_int;
                match usize::try_from(nn - 1)
                    .ok()
                    .and_then(|k| (lo..hi).filter(|&p| keep(p)).nth(k))
                {
                    Some(p) => arg_vals[part[p]].clone().unwrap_or(Value::Null),
                    None => Value::Null,
                }
            }
            // The planner refuses a frame on any other function, so this is
            // unreachable for a valid plan; be defensive rather than panic.
            _ => return Err(internal("explicit frame on an unsupported window function")),
        };
    }
    Ok(())
}

/// Resolve a HOST window aggregate's name to a fresh accumulation state.
fn new_host_state(
    host: Option<&str>,
    host_aggs: Option<&dyn HostAggs>,
) -> Result<Box<dyn HostAggState>> {
    let (Some(name), Some(reg)) = (host, host_aggs) else {
        // The plan named one and the connection has no registry (or the plan
        // carries no name at all) — a plan/registry mismatch, not a data error.
        return Err(internal("host window aggregate is not registered on this connection"));
    };
    reg.create(name, 1)
}

/// Assign a HOST window aggregate's values under an EXPLICIT frame, by SLIDING.
///
/// This is the one place mpedb's window executor does not simply re-aggregate
/// per row, and the reason is sqlite's contract rather than performance: a host
/// window function is registered with `xStep`/`xFinal` **plus** `xValue` and
/// `xInverse` precisely so the frame can move, and a consumer's callbacks are
/// written expecting exactly that call sequence. Re-aggregating would never
/// invoke `xInverse` and would call `xFinal` once per row — observably different
/// for any implementation that counts its calls or holds state.
///
/// The slide is legal because every frame shape mpedb accepts is MONOTONE in
/// window order: `lo` and `hi` are both non-decreasing as the current row
/// advances. The loop keeps the half-open range `[cur_lo, cur_hi)` stepped into
/// the state, extends it on the right with `step` and retracts it on the left
/// with `inverse`. A non-monotone move would be a bug elsewhere; rather than
/// trust that, it is detected and the state rebuilt from scratch — correct
/// under any frame, at worst quadratic.
///
/// `xFinal` runs once per PARTITION, at the end, and its error is swallowed:
/// sqlite does not propagate a finalizer failure out of `sqlite3_step`, and
/// CPython's suite pins that (`test_win_exception_in_finalize`).
#[allow(clippy::too_many_arguments)]
fn assign_host_framed(
    slot: usize,
    part: &[usize],
    rows: &mut [Vec<Value>],
    frame: &Frame,
    fo: FrameOffsets,
    arg_vals: &[Option<Value>],
    order_vals: &[Vec<Value>],
    ord_keys: &[OrdKey],
    has_order: bool,
    host: Option<&str>,
    host_aggs: Option<&dyn HostAggs>,
) -> Result<()> {
    let len = part.len();
    let (group_of, group_starts) = if matches!(frame.mode, FrameMode::Rows)
        && !needs_peers(frame.exclude)
    {
        (Vec::new(), Vec::new())
    } else {
        build_groups(part, order_vals, ord_keys, has_order)
    };
    let arg_at = |p: usize| arg_vals[part[p]].clone().unwrap_or(Value::Null);
    // `EXCLUDE` (format 66) punches a HOLE in the frame, so the slide below —
    // whose whole argument is that `[cur_lo, cur_hi)` is a contiguous range
    // moving monotonically — cannot express it. `xInverse` retracts from the
    // LEFT EDGE only; there is no callback for "remove a row from the middle".
    // Rebuild per row instead: still `xStep`/`xValue` in window order over
    // exactly the frame's rows, just without the incremental saving.
    // A RANGE offset boundary is a VALUE, so it needs the single ORDER BY key
    // at each POSITION (not each row) and the sort direction. Built only for
    // that frame shape; `check` has refused any other key count.
    let range_keys: Vec<Value> = if matches!(frame.mode, FrameMode::Range) && frame_has_offset(frame)
    {
        part.iter()
            .map(|&i| order_vals[i].first().cloned().unwrap_or(Value::Null))
            .collect()
    } else {
        Vec::new()
    };
    let range_desc = ord_keys.first().is_some_and(|k| k.0);
    if !matches!(frame.exclude, FrameExclude::NoOthers) {
        for off in 0..len {
            let (lo, hi) = frame_bounds(
                off, len, frame, fo, &group_of, &group_starts, &range_keys, range_desc,
            );
            let keep = exclusion(off, len, frame.exclude, &group_of, &group_starts);
            let mut st = new_host_state(host, host_aggs)?;
            for p in (lo..hi).filter(|&p| keep(p)) {
                st.step(&[arg_at(p)])?;
            }
            rows[part[off]][slot] = st.value()?;
            let _ = st.finish();
        }
        return Ok(());
    }
    let mut state = new_host_state(host, host_aggs)?;
    let (mut cur_lo, mut cur_hi) = (0usize, 0usize);
    for off in 0..len {
        let (lo, hi) = frame_bounds(
            off, len, frame, fo, &group_of, &group_starts, &range_keys, range_desc,
        );
        if lo < cur_lo || hi < cur_hi {
            // Not monotone: start this row's frame from an empty state.
            state = new_host_state(host, host_aggs)?;
            cur_lo = lo;
            cur_hi = lo;
        }
        while cur_hi < hi {
            state.step(&[arg_at(cur_hi)])?;
            cur_hi += 1;
        }
        while cur_lo < lo {
            state.inverse(&[arg_at(cur_lo)])?;
            cur_lo += 1;
        }
        rows[part[off]][slot] = state.value()?;
    }
    let _ = state.finish();
    Ok(())
}

/// Assign a HOST window aggregate's values under the DEFAULT frame: the whole
/// partition when the window has no ORDER BY, else cumulative through the end of
/// each peer group (`RANGE UNBOUNDED PRECEDING → CURRENT ROW`, the same rule the
/// built-in aggregate window follows).
///
/// No `xInverse` here — the frame's left edge never moves — so this is `xStep`
/// plus a per-group `xValue`, and one `xFinal` at the partition's end.
#[allow(clippy::too_many_arguments)]
fn assign_host_default(
    slot: usize,
    part: &[usize],
    rows: &mut [Vec<Value>],
    arg_vals: &[Option<Value>],
    peers: &dyn Fn(usize, usize) -> bool,
    has_order: bool,
    host: Option<&str>,
    host_aggs: Option<&dyn HostAggs>,
) -> Result<()> {
    let mut state = new_host_state(host, host_aggs)?;
    if !has_order {
        for &i in part {
            state.step(&[arg_vals[i].clone().unwrap_or(Value::Null)])?;
        }
        let v = state.value()?;
        for &i in part {
            rows[i][slot] = v.clone();
        }
    } else {
        let mut g = 0usize;
        while g < part.len() {
            let mut h = g + 1;
            while h < part.len() && peers(part[h], part[g]) {
                h += 1;
            }
            for &i in &part[g..h] {
                state.step(&[arg_vals[i].clone().unwrap_or(Value::Null)])?;
            }
            let v = state.value()?;
            for &i in &part[g..h] {
                rows[i][slot] = v.clone();
            }
            g = h;
        }
    }
    let _ = state.finish();
    Ok(())
}

/// Peer-group structure for one partition (already in window order): `group_of[p]`
/// is the 0-based peer-group index of `part[p]`, and `group_starts[g]` is the
/// position of group `g`'s first row. Peers are rows equal on every ORDER BY key
/// (NULLs equal); with NO ORDER BY the whole partition is one group — exactly the
/// grouping sqlite uses for RANGE/GROUPS framing.
fn build_groups(
    part: &[usize],
    order_vals: &[Vec<Value>],
    ord_keys: &[OrdKey],
    has_order: bool,
) -> (Vec<usize>, Vec<usize>) {
    let mut group_of = Vec::with_capacity(part.len());
    let mut group_starts = Vec::new();
    let mut g = 0usize;
    for (pos, &i) in part.iter().enumerate() {
        if pos == 0 {
            group_starts.push(0);
        } else {
            let prev = part[pos - 1];
            let same = !has_order
                || order_cmp(&order_vals[i], &order_vals[prev], ord_keys) == Ordering::Equal;
            if !same {
                g += 1;
                group_starts.push(pos);
            }
        }
        group_of.push(g);
    }
    (group_of, group_starts)
}

/// A frame's two boundary OFFSETS, resolved once per execution.
///
/// The offsets may be PARAMETERS (`ROWS BETWEEN ? PRECEDING AND CURRENT ROW`),
/// and a frame boundary is consulted per ROW — so they are resolved before the
/// row loop rather than inside it. One value for the whole execution is exactly
/// the property that lets a parameter live here at all.
#[derive(Clone, Copy)]
struct FrameOffsets {
    start: i64,
    end: i64,
}

fn resolve_frame_offsets(frame: &Frame, params: &[Value]) -> Result<FrameOffsets> {
    let one = |b: FrameBound| -> Result<i64> {
        let arg = match b {
            FrameBound::Preceding(v) | FrameBound::Following(v) => v,
            _ => return Ok(0),
        };
        match arg {
            WinInt::Lit(n) => Ok(n),
            WinInt::Param(i) => match params.get(i as usize) {
                Some(Value::Int(n)) if *n >= 0 => Ok(*n),
                // sqlite raises at RUN time for a negative frame offset, not at
                // parse — measured — so a parameter carrying one does too.
                Some(Value::Int(n)) => Err(Error::TypeMismatch(format!(
                    "frame offset must not be negative, got {n}"
                ))),
                Some(other) => Err(Error::TypeMismatch(format!(
                    "frame offset must be an integer, got {}",
                    other.type_name()
                ))),
                None => Err(Error::Internal(format!(
                    "frame parameter ${} is not bound",
                    i + 1
                ))),
            },
        }
    };
    Ok(FrameOffsets { start: one(frame.start)?, end: one(frame.end)? })
}

/// Does this exclusion need the peer-group structure? `GROUP`/`TIES` drop the
/// current row's ORDER BY ties, which ROWS mode does not otherwise compute.
fn needs_peers(e: FrameExclude) -> bool {
    matches!(e, FrameExclude::Group | FrameExclude::Ties)
}

/// A predicate over positions of the partition slice: `true` for a row the
/// frame KEEPS after `EXCLUDE` (format 66).
///
/// Exclusion is a FILTER, not a narrowing of `[lo, hi)`: `EXCLUDE TIES` keeps
/// the current row while dropping its peers on both sides of it, so the kept
/// row stays in its window-order position. MEASURED against sqlite 3.45.1
/// before implementing, including the two cases a reading of the standard
/// alone gets wrong: with NO ORDER BY the whole partition is one peer group
/// (`GROUP` empties the frame, `TIES` leaves exactly the current row), and
/// peers never cross a PARTITION boundary because `group_of` is per-partition.
fn exclusion(
    off: usize,
    len: usize,
    exclude: FrameExclude,
    group_of: &[usize],
    group_starts: &[usize],
) -> impl Fn(usize) -> bool + use<> {
    let (ex_lo, ex_hi, keep_current) = match exclude {
        FrameExclude::NoOthers => (0, 0, false),
        FrameExclude::CurrentRow => (off, off + 1, false),
        FrameExclude::Group | FrameExclude::Ties => {
            // `group_of` is built whenever `needs_peers`; a missing entry would
            // be a caller bug, and dropping nothing is the safe reading.
            match group_of.get(off) {
                Some(&g) => {
                    let s = group_starts.get(g).copied().unwrap_or(0);
                    let e = group_starts.get(g + 1).copied().unwrap_or(len);
                    (s, e, matches!(exclude, FrameExclude::Ties))
                }
                None => (0, 0, false),
            }
        }
    };
    move |p: usize| !(p >= ex_lo && p < ex_hi) || (keep_current && p == off)
}

/// Resolve a frame to the half-open range `[lo, hi)` of positions within the
/// partition slice for the row at position `off`. `lo <= hi <= len` always; an
/// empty frame is `lo == hi`. ROWS uses physical offsets; RANGE/GROUPS use the
/// peer-group structure (`group_of`/`group_starts`). Offsets and positions are
/// computed in i64 with saturating/clamping arithmetic, so a huge constant
/// offset simply pins the boundary to the partition edge.
#[allow(clippy::too_many_arguments)]
fn frame_bounds(
    off: usize,
    len: usize,
    frame: &Frame,
    fo: FrameOffsets,
    group_of: &[usize],
    group_starts: &[usize],
    // The single ORDER BY key's value at each POSITION, and whether the sort is
    // descending — read only by RANGE with an offset bound, whose boundary is a
    // VALUE (`x ± offset`) rather than a count. `check` has already refused a
    // RANGE offset with anything but exactly one key.
    keys: &[Value],
    desc: bool,
) -> (usize, usize) {
    let n = len as i64;
    if matches!(frame.mode, FrameMode::Range) && frame_has_offset(frame) {
        return range_offset_bounds(off, len, frame, fo, group_of, group_starts, keys, desc);
    }
    match frame.mode {
        FrameMode::Rows => {
            let off = off as i64;
            // Inclusive start `s` and inclusive end `e`; an illegal-as-a-bound
            // value (UNBOUNDED FOLLOWING as start, UNBOUNDED PRECEDING as end)
            // maps to an empty side and is rejected before exec anyway.
            let s = match frame.start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(_) => off.saturating_sub(fo.start),
                FrameBound::CurrentRow => off,
                FrameBound::Following(_) => off.saturating_add(fo.start),
                FrameBound::UnboundedFollowing => n,
            };
            let e = match frame.end {
                FrameBound::UnboundedPreceding => -1,
                FrameBound::Preceding(_) => off.saturating_sub(fo.end),
                FrameBound::CurrentRow => off,
                FrameBound::Following(_) => off.saturating_add(fo.end),
                FrameBound::UnboundedFollowing => n - 1,
            };
            let lo = s.clamp(0, n);
            // `saturating_add` guards a pathologically huge FOLLOWING offset
            // (`e` may already be `i64::MAX`); the clamp pins it to the partition.
            let hi = e.saturating_add(1).clamp(0, n).max(lo);
            (lo as usize, hi as usize)
        }
        FrameMode::Range | FrameMode::Groups => {
            let g = group_of[off] as i64;
            let n_groups = group_starts.len() as i64;
            // First position of group `tg` (clamped: below range → 0, above → n).
            let start_pos = |tg: i64| -> i64 {
                if tg < 0 {
                    0
                } else if tg >= n_groups {
                    n
                } else {
                    group_starts[tg as usize] as i64
                }
            };
            // Exclusive end position just past group `tg` (below → 0, at/above the
            // last group → n).
            let end_excl = |tg: i64| -> i64 {
                if tg < 0 {
                    0
                } else if tg + 1 >= n_groups {
                    n
                } else {
                    group_starts[(tg + 1) as usize] as i64
                }
            };
            let lo = match frame.start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(_) => start_pos(g.saturating_sub(fo.start)),
                FrameBound::CurrentRow => start_pos(g),
                FrameBound::Following(_) => start_pos(g.saturating_add(fo.start)),
                FrameBound::UnboundedFollowing => n,
            };
            let hi = match frame.end {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(_) => end_excl(g.saturating_sub(fo.end)),
                FrameBound::CurrentRow => end_excl(g),
                FrameBound::Following(_) => end_excl(g.saturating_add(fo.end)),
                FrameBound::UnboundedFollowing => n,
            };
            let lo = lo.clamp(0, n);
            let hi = hi.clamp(0, n).max(lo);
            (lo as usize, hi as usize)
        }
    }
}

/// Does either boundary carry an offset? (`UNBOUNDED`/`CURRENT ROW` do not.)
fn frame_has_offset(frame: &Frame) -> bool {
    matches!(frame.start, FrameBound::Preceding(_) | FrameBound::Following(_))
        || matches!(frame.end, FrameBound::Preceding(_) | FrameBound::Following(_))
}

/// `RANGE BETWEEN <n> PRECEDING AND <m> FOLLOWING` and friends: the boundary is
/// the current row's ORDER BY VALUE shifted by the offset, and the frame is
/// every row whose value falls in that interval.
///
/// MEASURED against sqlite 3.45.1 before implementing; three rules are not what
/// the shape suggests:
///
///   * DESCENDING FLIPS THE SIGN. `n PRECEDING` means "earlier in window
///     order", which under DESC is a LARGER value. The interval is the same
///     `[x - n, x + m]` only when the frame is symmetric.
///   * A NON-NUMERIC CURRENT VALUE (NULL, TEXT, BLOB) degenerates each OFFSET
///     bound to that value's PEER GROUP — `x ± n` is not a value of its class,
///     so nothing outside the peers can be inside the interval. Applied PER
///     BOUND: `RANGE BETWEEN 2 PRECEDING AND UNBOUNDED FOLLOWING` on a NULL row
///     still runs to the partition's end.
///   * The comparison is the SORT's, storage classes and all, so a numeric
///     interval can never reach a TEXT row even though `'x' > 5` is false under
///     ordinary comparison — TEXT simply sorts past the interval's top.
#[allow(clippy::too_many_arguments)]
fn range_offset_bounds(
    off: usize,
    len: usize,
    frame: &Frame,
    fo: FrameOffsets,
    group_of: &[usize],
    group_starts: &[usize],
    keys: &[Value],
    desc: bool,
) -> (usize, usize) {
    let peer = |p: usize| -> (usize, usize) {
        match group_of.get(p) {
            Some(&g) => (
                group_starts.get(g).copied().unwrap_or(0),
                group_starts.get(g + 1).copied().unwrap_or(len),
            ),
            None => (p, p + 1),
        }
    };
    let (peer_lo, peer_hi) = peer(off);
    let x = keys.get(off).unwrap_or(&Value::Null);
    // `shift(o)`: the boundary VALUE `x` displaced by `o` in the direction the
    // sort runs. `None` where the displacement is meaningless (non-numeric).
    let shift = |o: i64, earlier: bool| -> Option<Value> {
        // "earlier in window order" is DOWN in value for ASC, UP for DESC.
        let sign: i64 = if earlier == desc { 1 } else { -1 };
        match x {
            Value::Int(v) => Some(Value::Int(v.saturating_add(sign.saturating_mul(o)))),
            Value::Float(v) => Some(Value::Float(v + (sign as f64) * (o as f64))),
            // Everything else — NULL, TEXT, BLOB, BOOL, TIMESTAMP — has no
            // meaningful `± offset`. sqlite (which has no timestamp type; the
            // C-API shim's date columns arrive as TEXT) gives exactly the peer
            // group for those, and so does this.
            _ => None,
        }
    };
    // Positions are sorted by `keys` under the window's direction, so a
    // boundary is a partition point — found by scanning from the peer group
    // outward, which is O(frame) and needs no total order on mixed classes.
    let inside = |p: usize, bound: &Value, lower: bool| -> bool {
        let v = keys.get(p).unwrap_or(&Value::Null);
        match v.sort_cmp(bound, Collation::Binary) {
            // `sort_cmp` is None when either side is NULL: a NULL row is never
            // inside a numeric interval.
            None => false,
            Some(o) => {
                // For ASC, "lower" means value >= bound; DESC reverses it.
                let o = if desc { o.reverse() } else { o };
                if lower {
                    o != Ordering::Less
                } else {
                    o != Ordering::Greater
                }
            }
        }
    };
    let lo = match frame.start {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::CurrentRow => peer_lo,
        FrameBound::Preceding(_) | FrameBound::Following(_) => {
            let earlier = matches!(frame.start, FrameBound::Preceding(_));
            match shift(fo.start, earlier) {
                None => peer_lo,
                Some(bound) => {
                    // Walk out from the peer group while rows stay inside, then
                    // in while they do not — one pass, both directions covered.
                    let mut p = peer_lo;
                    while p > 0 && inside(p - 1, &bound, true) {
                        p -= 1;
                    }
                    while p < len && !inside(p, &bound, true) {
                        p += 1;
                    }
                    p
                }
            }
        }
        FrameBound::UnboundedFollowing => len,
    };
    let hi = match frame.end {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::CurrentRow => peer_hi,
        FrameBound::Preceding(_) | FrameBound::Following(_) => {
            let earlier = matches!(frame.end, FrameBound::Preceding(_));
            match shift(fo.end, earlier) {
                None => peer_hi,
                Some(bound) => {
                    let mut p = peer_hi;
                    while p < len && inside(p, &bound, false) {
                        p += 1;
                    }
                    while p > 0 && !inside(p - 1, &bound, false) {
                        p -= 1;
                    }
                    p
                }
            }
        }
        FrameBound::UnboundedFollowing => len,
    };
    (lo.min(len), hi.clamp(lo.min(len), len))
}

/// Push one row's argument into an aggregate accumulator. `None` is `count(*)`
/// (the row itself — always counts); `Some(v)` may be NULL, which every other
/// aggregate skips — exactly the grouped path's rule.
fn push_arg(acc: &mut Accum, arg: &Option<Value>) -> Result<()> {
    match arg {
        None => acc.push(None),
        Some(v) => acc.push(Some(v)),
    }
}

/// Total order over two rows' window ORDER BY values: `Value::sql_cmp` per key,
/// NULLS FIRST ascending, reversed for a descending key — the exact `cmp_order`
/// semantics `sort_rows` uses, so the window ORDER BY matches sqlite's default.
fn order_cmp(a: &[Value], b: &[Value], ord_keys: &[OrdKey]) -> Ordering {
    for (k, &(desc, coll)) in ord_keys.iter().enumerate() {
        let (Some(x), Some(y)) = (a.get(k), b.get(k)) else {
            continue;
        };
        let ord = value_cmp(x, y, coll);
        if ord != Ordering::Equal {
            return if desc { ord.reverse() } else { ord };
        }
    }
    Ordering::Equal
}

fn value_cmp(a: &Value, b: &Value, coll: Collation) -> Ordering {
    // Storage-class order, as `ORDER BY` uses: a window key can be an `any`
    // column, which really does hold more than one class.
    match a.sort_cmp(b, coll) {
        Some(o) => o,
        // NULL involved: NULLS FIRST in ascending order (two NULLs are peers).
        None => match (a.is_null(), b.is_null()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => Ordering::Equal,
        },
    }
}
