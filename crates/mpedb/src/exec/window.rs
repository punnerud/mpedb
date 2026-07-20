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
use mpedb_sql::{Frame, FrameBound, FrameMode, WindowFunc, WindowSpec};
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
        )?
    } else {
        gather_rows(ctx, sp.table, &sp.access, sp.filter.as_ref(), plan, params, None)?
    };

    compute_windows(&mut rows, &sp.windows, params, ctx.host_aggs())?;

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
    let skip = sp.offset.unwrap_or(0).min(usize::MAX as u64) as usize;
    let take = sp.limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    if skip > 0 || take != usize::MAX {
        out = out.into_iter().skip(skip).take(take).collect();
    }
    let columns = select_output_columns(schema, plan, sp)?;
    Ok(ExecResult::Rows { columns, rows: out })
}

/// Compute every window over the materialized rows, extending each row with one
/// result value per window (at `base_width + k`). Rows are never reordered — the
/// index vector is sorted and each result is written back at the row's ORIGINAL
/// index — so the base rows stay in gather order for the outer sort.
pub(super) fn compute_windows(
    rows: &mut [Vec<Value>],
    windows: &[WindowSpec],
    params: &[Value],
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
                pk.push(p.eval(row, params)?);
            }
            // NULLs group together (SQL's PARTITION BY rule) and so do `1`
            // and `1.0` (partition membership is sqlite's comparison) — the
            // total, NULL-equal GROUP key is exactly the GROUP BY keying.
            part_key.push(keycode::encode_group_key(&pk, &[]));
            let mut ov = Vec::with_capacity(w.order_by.len());
            for (p, _) in &w.order_by {
                ov.push(p.eval(row, params)?);
            }
            order_vals.push(ov);
            arg_vals.push(match &w.arg {
                None => None,
                Some(p) => Some(p.eval(row, params)?),
            });
            default_vals.push(match &w.default {
                Some(p) => p.eval(row, params)?,
                None => Value::Null,
            });
        }

        // Stable sort of indices by (partition key, window ORDER BY). Stability
        // keeps ties in gather (PK/scan) order — matching row_number's tiebreak
        // and the top-K path's tiebreak elsewhere in the executor.
        let dirs: Vec<bool> = w.order_by.iter().map(|(_, d)| *d).collect();
        let mut idx: Vec<usize> = (0..n).collect();
        idx.sort_by(|&a, &b| {
            part_key[a]
                .cmp(&part_key[b])
                .then_with(|| order_cmp(&order_vals[a], &order_vals[b], &dirs))
        });

        assign_window(
            k,
            base_width,
            &idx,
            rows,
            w,
            &part_key,
            &order_vals,
            &arg_vals,
            &default_vals,
            &dirs,
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
    part_key: &[Vec<u8>],
    order_vals: &[Vec<Value>],
    arg_vals: &[Option<Value>],
    default_vals: &[Value],
    dirs: &[bool],
    host_aggs: Option<&dyn HostAggs>,
) -> Result<()> {
    let slot = base_width + k;
    let has_order = !w.order_by.is_empty();
    // With no ORDER BY the whole partition is ONE peer group (sqlite gives every
    // row rank 1 / dense_rank 1); with ORDER BY, peers are rows equal on all
    // keys. The aggregate cumulative branch only consults this when `has_order`.
    let peers = |i: usize, j: usize| -> bool {
        !has_order || order_cmp(&order_vals[i], &order_vals[j], dirs) == Ordering::Equal
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
                frame,
                arg_vals,
                order_vals,
                dirs,
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
                let mut acc = Accum::new(f);
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
            WindowFunc::Lag(offset) | WindowFunc::Lead(offset) => {
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
            WindowFunc::NthValue(nn) => {
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
            WindowFunc::Ntile(nb) => {
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
#[allow(clippy::too_many_arguments)]
fn assign_framed(
    slot: usize,
    part: &[usize],
    rows: &mut [Vec<Value>],
    func: WindowFunc,
    frame: &Frame,
    arg_vals: &[Option<Value>],
    order_vals: &[Vec<Value>],
    dirs: &[bool],
    has_order: bool,
    host: Option<&str>,
    host_aggs: Option<&dyn HostAggs>,
) -> Result<()> {
    let len = part.len();
    // A HOST window aggregate is the one function here that is NOT re-aggregated
    // per row: it SLIDES (see `assign_host_framed`).
    if matches!(func, WindowFunc::Host) {
        return assign_host_framed(
            slot, part, rows, frame, arg_vals, order_vals, dirs, has_order, host, host_aggs,
        );
    }
    // Peer-group structure is needed only for GROUPS/RANGE (they count / span
    // peer groups); ROWS is a purely physical offset and skips it.
    let (group_of, group_starts) = if matches!(frame.mode, FrameMode::Rows) {
        (Vec::new(), Vec::new())
    } else {
        build_groups(part, order_vals, dirs, has_order)
    };
    for off in 0..len {
        let (lo, hi) = frame_bounds(off, len, frame, &group_of, &group_starts);
        let target = part[off];
        rows[target][slot] = match func {
            WindowFunc::Agg(f) => {
                let mut acc = Accum::new(f);
                for &i in &part[lo..hi] {
                    push_arg(&mut acc, &arg_vals[i])?;
                }
                acc.finish()
            }
            // The frame's FIRST / LAST row (in window order), or NULL for an
            // empty frame.
            WindowFunc::FirstValue => {
                if lo < hi {
                    arg_vals[part[lo]].clone().unwrap_or(Value::Null)
                } else {
                    Value::Null
                }
            }
            WindowFunc::LastValue => {
                if lo < hi {
                    arg_vals[part[hi - 1]].clone().unwrap_or(Value::Null)
                } else {
                    Value::Null
                }
            }
            // The n-th row (1-based) WITHIN the frame, or NULL if the frame is
            // shorter than n. `nn >= 1` is validated; compute in i64 so an absurd
            // constant n just yields NULL rather than overflowing.
            WindowFunc::NthValue(nn) => {
                let idx = lo as i64 + (nn - 1);
                if idx >= lo as i64 && idx < hi as i64 {
                    arg_vals[part[idx as usize]].clone().unwrap_or(Value::Null)
                } else {
                    Value::Null
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
    arg_vals: &[Option<Value>],
    order_vals: &[Vec<Value>],
    dirs: &[bool],
    has_order: bool,
    host: Option<&str>,
    host_aggs: Option<&dyn HostAggs>,
) -> Result<()> {
    let len = part.len();
    let (group_of, group_starts) = if matches!(frame.mode, FrameMode::Rows) {
        (Vec::new(), Vec::new())
    } else {
        build_groups(part, order_vals, dirs, has_order)
    };
    let arg_at = |p: usize| arg_vals[part[p]].clone().unwrap_or(Value::Null);
    let mut state = new_host_state(host, host_aggs)?;
    let (mut cur_lo, mut cur_hi) = (0usize, 0usize);
    for off in 0..len {
        let (lo, hi) = frame_bounds(off, len, frame, &group_of, &group_starts);
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
    dirs: &[bool],
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
                || order_cmp(&order_vals[i], &order_vals[prev], dirs) == Ordering::Equal;
            if !same {
                g += 1;
                group_starts.push(pos);
            }
        }
        group_of.push(g);
    }
    (group_of, group_starts)
}

/// Resolve a frame to the half-open range `[lo, hi)` of positions within the
/// partition slice for the row at position `off`. `lo <= hi <= len` always; an
/// empty frame is `lo == hi`. ROWS uses physical offsets; RANGE/GROUPS use the
/// peer-group structure (`group_of`/`group_starts`). Offsets and positions are
/// computed in i64 with saturating/clamping arithmetic, so a huge constant
/// offset simply pins the boundary to the partition edge.
fn frame_bounds(
    off: usize,
    len: usize,
    frame: &Frame,
    group_of: &[usize],
    group_starts: &[usize],
) -> (usize, usize) {
    let n = len as i64;
    match frame.mode {
        FrameMode::Rows => {
            let off = off as i64;
            // Inclusive start `s` and inclusive end `e`; an illegal-as-a-bound
            // value (UNBOUNDED FOLLOWING as start, UNBOUNDED PRECEDING as end)
            // maps to an empty side and is rejected before exec anyway.
            let s = match frame.start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(k) => off.saturating_sub(k as i64),
                FrameBound::CurrentRow => off,
                FrameBound::Following(k) => off.saturating_add(k as i64),
                FrameBound::UnboundedFollowing => n,
            };
            let e = match frame.end {
                FrameBound::UnboundedPreceding => -1,
                FrameBound::Preceding(k) => off.saturating_sub(k as i64),
                FrameBound::CurrentRow => off,
                FrameBound::Following(k) => off.saturating_add(k as i64),
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
                FrameBound::Preceding(k) => start_pos(g.saturating_sub(k as i64)),
                FrameBound::CurrentRow => start_pos(g),
                FrameBound::Following(k) => start_pos(g.saturating_add(k as i64)),
                FrameBound::UnboundedFollowing => n,
            };
            let hi = match frame.end {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(k) => end_excl(g.saturating_sub(k as i64)),
                FrameBound::CurrentRow => end_excl(g),
                FrameBound::Following(k) => end_excl(g.saturating_add(k as i64)),
                FrameBound::UnboundedFollowing => n,
            };
            let lo = lo.clamp(0, n);
            let hi = hi.clamp(0, n).max(lo);
            (lo as usize, hi as usize)
        }
    }
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
fn order_cmp(a: &[Value], b: &[Value], dirs: &[bool]) -> Ordering {
    for (k, &desc) in dirs.iter().enumerate() {
        let (Some(x), Some(y)) = (a.get(k), b.get(k)) else {
            continue;
        };
        let ord = value_cmp(x, y);
        if ord != Ordering::Equal {
            return if desc { ord.reverse() } else { ord };
        }
    }
    Ordering::Equal
}

fn value_cmp(a: &Value, b: &Value) -> Ordering {
    // Storage-class order, as `ORDER BY` uses: a window key can be an `any`
    // column, which really does hold more than one class.
    match a.sort_cmp(b, mpedb_types::Collation::Binary) {
        Some(o) => o,
        // NULL involved: NULLS FIRST in ascending order (two NULLs are peers).
        None => match (a.is_null(), b.is_null()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => Ordering::Equal,
        },
    }
}
