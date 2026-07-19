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
use mpedb_sql::{WindowFunc, WindowSpec};
use mpedb_types::Accum;
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

    compute_windows(&mut rows, &sp.windows, params)?;

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
                Projection::Expr { program, .. } => program.eval(row, params)?,
            });
        }
        if sp.distinct && !seen.insert(keycode::encode_key(&orow)) {
            continue;
        }
        out.push(orow);
    }

    // The outer ORDER BY runs over the projection (windows force it there).
    if !sp.order_by.is_empty() {
        sort_rows(&mut out, &sp.order_by);
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
            // NULLs group together (SQL's PARTITION BY rule) — the total,
            // NULL-equal keycode is exactly the GROUP BY keying.
            part_key.push(keycode::encode_key(&pk));
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
    match a.sql_cmp(b) {
        Ok(Some(o)) => o,
        // NULL involved: NULLS FIRST in ascending order (two NULLs are peers).
        Ok(None) => match (a.is_null(), b.is_null()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => Ordering::Equal,
        },
        // Cross-type comparison cannot happen within one rigidly-typed key.
        Err(_) => Ordering::Equal,
    }
}
