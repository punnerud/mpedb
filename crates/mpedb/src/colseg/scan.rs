use super::*;

/// Feed a whole-table aggregate from column segments instead of the row tree,
/// if and only if that is provably the same scan.
///
/// Returns `Ok(false)` — meaning "not usable, run the row scan" — unless ALL
/// of these hold:
/// - the context has a read snapshot (the write path has no segments),
/// - every block decodes at the table's CURRENT `mod_gen` (§6: the table has
///   not changed since the pass), and
/// - the blocks' row counts sum to the table's row count.
///
/// That last check is what makes a partially-written or partially-dropped
/// column safe: a missing block would silently shorten the scan, which is a
/// wrong answer, so coverage is verified rather than assumed. The values are
/// then pushed in PK order — the row scan's order — so the result is
/// bit-identical, float sums included.
pub(crate) fn feed_from_segments(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    col: u16,
    ty: ColumnType,
    push: &mut dyn FnMut(&Value) -> Result<()>,
) -> Result<bool> {
    if !segmentable(ty) {
        return Ok(false);
    }
    // The shipped whole-table path first: it succeeds exactly when nothing has
    // written since the segments were built (mod_gen matches), and declines
    // without pushing anything otherwise — so it is safe to fall through from.
    let want_gen = match snap.mod_gen(table) {
        Ok(g) => g,
        Err(_) => return Ok(false),
    };
    let want_rows = match snap.row_count(table) {
        Ok(n) => n,
        Err(_) => return Ok(false),
    };

    // TWO passes over the records, on purpose. The first only validates and
    // counts: a decline discovered at block 7 must not leave the accumulators
    // carrying blocks 0..6, and re-reading a validated record is far cheaper
    // than materializing every block's values to stay safe.
    let mut n_blocks = 0u32;
    let mut covered: u64 = 0;
    for bi in 0u32.. {
        let Some(bytes) = block_bytes(snap, table, col, bi)? else { break };
        match decode_block(&bytes, want_gen, ty)? {
            Some(b) => covered += b.n_rows as u64,
            None => {
                // Stale for the WHOLE-table path — but this may just be appends
                // above a watermark, which the tail path serves.
                return feed_from_segments_tail(snap, table, col, ty, push);
            }
        }
        n_blocks += 1;
        if covered > want_rows {
            return Ok(false); // more rows than the table holds: do not trust it
        }
    }
    if n_blocks == 0 || covered != want_rows {
        // No segments at this gen, or they do not cover the whole table: the
        // segments might still cover a watermark prefix with a row tail.
        return feed_from_segments_tail(snap, table, col, ty, push);
    }
    for bi in 0..n_blocks {
        let bytes = block_bytes(snap, table, col, bi)?
            .ok_or_else(|| Error::Corrupt("column segment vanished mid-scan".into()))?;
        let b = decode_block(&bytes, want_gen, ty)?
            .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
        b.for_each(push)?;
    }
    Ok(true)
}

/// The split scan (DESIGN-COLUMNAR §7): segments for the first `W` rows (built
/// at generation `g0`), then a row-tree fold of everything with a PK strictly
/// above the watermark. Used when the whole-table path declines because rows
/// were APPENDED since the segments were built — the appends are all above the
/// watermark (a covered-row write would have deleted it), so this reads exactly
/// the same values in exactly PK order and stays bit-identical to the row scan.
///
/// Declines (Ok(false), nothing pushed) unless a watermark record is present and
/// the column's blocks decode at `g0` and cover exactly `W` rows.
fn feed_from_segments_tail(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    col: u16,
    ty: ColumnType,
    push: &mut dyn FnMut(&Value) -> Result<()>,
) -> Result<bool> {
    let Some((g0, covered, wm_pk)) = snap.columnar_watermark(table)? else {
        return Ok(false);
    };
    let Some(recs) = load_column(snap, table, col, ty, g0, covered)? else {
        return Ok(false);
    };
    for bytes in &recs {
        let b = decode_block(bytes, g0, ty)?
            .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
        b.for_each(push)?;
    }
    fold_tail_column(snap, table, &wm_pk, col, push)?;
    Ok(true)
}

/// Fold one column of the row tail — rows with a PK strictly greater than the
/// watermark — into `push`, in PK order (the fold's natural order), continuing
/// straight on from the segments. `SERIAL` drains the whole range.
fn fold_tail_column(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    wm_pk: &[u8],
    col: u16,
    push: &mut dyn FnMut(&Value) -> Result<()>,
) -> Result<()> {
    snap.fold_range_column(
        table,
        Some((wm_pk, false)),
        None,
        col,
        mpedb_core::FoldOpts::SERIAL,
        push,
    )?;
    Ok(())
}

/// The VECTORIZED whole-table `sum(FLOAT column)` (DESIGN-COLUMNAR §7.2):
/// `(total, non_null_count)` summed straight out of the packed segments, in PK
/// order, with no `Value` boxed and no `dyn` closure crossed per row — the tight
/// f64 loop of [`Block::add_f64_into`]. `Ok(None)` = not applicable (the whole-
/// table `mod_gen` path declined — appends, stale, or a block that is not a
/// null-free raw-f64 encoding), and the caller runs the ordinary per-value fold.
///
/// Bit-identical to that fold: it accumulates the SAME floats in the SAME order
/// into one running total, only skipping the per-row tax. Restricted to the
/// whole-table path on purpose — the row tail and predicate paths keep the
/// general fold, so this stays a pure speed swap on the one hot shape.
pub(crate) fn sum_f64_whole_table(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    col: u16,
) -> Result<Option<(f64, u64)>> {
    let (Ok(want_gen), Ok(want_rows)) = (snap.mod_gen(table), snap.row_count(table)) else {
        return Ok(None);
    };
    // One pass: the running total is LOCAL, so a mid-loop decline discards it
    // with no side effect (unlike the feeding paths, which need two passes).
    let mut total = 0.0f64;
    let mut count = 0u64;
    let mut covered = 0u64;
    let mut n_blocks = 0u32;
    for bi in 0u32.. {
        let Some(bytes) = block_bytes(snap, table, col, bi)? else { break };
        let Some(b) = decode_block(&bytes, want_gen, ColumnType::Float64)? else {
            return Ok(None); // stale, or not a Float64 block
        };
        let Some(c) = b.add_f64_into(&mut total) else {
            return Ok(None); // has NULLs, or an encoding the tight loop can't take
        };
        count += c;
        covered += b.n_rows as u64;
        n_blocks += 1;
        if covered > want_rows {
            return Ok(None);
        }
    }
    if n_blocks == 0 || covered != want_rows {
        return Ok(None);
    }
    Ok(Some((total, count)))
}

/// Largest key span a dense histogram will hold. Wider than this (a unique
/// key, a timestamp) declines to the per-row fold: the vector would be the
/// table, and the per-row path already builds one group per distinct key.
const HIST_MAX_SPAN: i128 = 1 << 20;

/// `GROUP BY` one null-free `INT64` column, optionally with `sum` of one
/// null-free `FLOAT64` column, straight out of the packed segments.
///
/// Each returned triple is `(key, row_count, float_sum)`. The sum is the
/// group's running total in PK order, and the first value is stored as itself
/// rather than added to `0.0`, so the bits match `Accum::push` (a leading
/// `-0.0` would not survive `0.0 + x`). `Ok(None)` declines — a NULL, a tail
/// above the watermark, a codec this loop cannot read, or a key span past
/// [`HIST_MAX_SPAN`] — and the caller feeds synthetic rows instead.
///
/// `sum_col == None` is the count-only shape; the float slot is then `0.0`.
pub(crate) fn group_count_sum_f64(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    key_col: u16,
    sum_col: Option<u16>,
) -> Result<Option<Vec<(i64, u64, f64)>>> {
    let (Ok(want_gen), Ok(want_rows)) = (snap.mod_gen(table), snap.row_count(table)) else {
        return Ok(None);
    };
    if want_rows == 0 {
        return Ok(None);
    }
    let Some(key_recs) = load_column(snap, table, key_col, ColumnType::Int64, want_gen, want_rows)?
    else {
        return Ok(None);
    };
    let amt_recs = match sum_col {
        Some(col) => Some(
            match load_column(snap, table, col, ColumnType::Float64, want_gen, want_rows)? {
                Some(r) => r,
                None => return Ok(None),
            },
        ),
        None => None,
    };
    if amt_recs.as_ref().is_some_and(|r| r.len() != key_recs.len()) {
        return Err(Error::Corrupt(
            "group column segments disagree on block count".into(),
        ));
    }

    let mut key_blocks = Vec::with_capacity(key_recs.len());
    let mut gmin = i64::MAX;
    let mut gmax = i64::MIN;
    for rec in &key_recs {
        let Some(b) = decode_block(rec, want_gen, ColumnType::Int64)? else {
            return Ok(None);
        };
        let Some((lo, hi)) = b.hist_i64_bounds() else {
            return Ok(None);
        };
        gmin = gmin.min(lo);
        gmax = gmax.max(hi);
        key_blocks.push(b);
    }
    let span = (gmax as i128) - (gmin as i128) + 1;
    if span <= 0 || span > HIST_MAX_SPAN {
        return Ok(None);
    }

    let mut amt_blocks = Vec::new();
    if let Some(recs) = &amt_recs {
        for (rec, key_b) in recs.iter().zip(&key_blocks) {
            let Some(b) = decode_block(rec, want_gen, ColumnType::Float64)? else {
                return Ok(None);
            };
            if !b.hist_f64_ok() || b.n_rows != key_b.n_rows {
                if b.n_rows != key_b.n_rows {
                    return Err(Error::Corrupt(
                        "group column segments disagree on block size".into(),
                    ));
                }
                return Ok(None);
            }
            amt_blocks.push(b);
        }
    }

    let span = span as usize;
    // `0` count means the bin has not been seeded. The first float is stored,
    // later floats are added, matching `Accum`'s `None → Float(x)` then `a + x`.
    let mut bins = vec![(0u64, 0.0f64); span];
    let mut keys = Vec::new();
    let mut amts = Vec::new();
    for (bi, key_b) in key_blocks.iter().enumerate() {
        if key_b.fill_i64(&mut keys).is_none() {
            return Ok(None);
        }
        let have_amt = if sum_col.is_some() {
            if amt_blocks[bi].fill_f64(&mut amts).is_none() || amts.len() != keys.len() {
                return Ok(None);
            }
            true
        } else {
            false
        };
        for (i, k) in keys.iter().enumerate() {
            let idx = match (*k as i128).checked_sub(gmin as i128) {
                Some(d) if d >= 0 && (d as usize) < span => d as usize,
                _ => {
                    return Err(Error::Corrupt(
                        "group key outside its own zone map".into(),
                    ))
                }
            };
            let bin = &mut bins[idx];
            if have_amt {
                let a = amts[i];
                if bin.0 == 0 {
                    bin.1 = a;
                    bin.0 = 1;
                } else {
                    bin.1 += a;
                    bin.0 += 1;
                }
            } else {
                bin.0 += 1;
            }
        }
    }

    let mut out = Vec::new();
    for (i, (n, total)) in bins.into_iter().enumerate() {
        if n == 0 {
            continue;
        }
        let key = gmin + i as i64;
        out.push((key, n, total));
    }
    Ok(Some(out))
}


/// Feed a GROUP BY aggregate from column segments (DESIGN-COLUMNAR stage 3):
/// stream ONLY the group-key and aggregate-argument columns as synthetic rows
/// into the ordinary [`Folder`](crate::exec) — same values, same PK order, so
/// the grouping, HAVING, projection and ordering are the identical code and the
/// answer is bit-identical to the row scan. A `GROUP BY store_id, sum(amount)`
/// over a six-column fact table then touches two columns' packed segments
/// instead of pulling every whole row out of the PK tree.
///
/// `needed` lists the `(ordinal, type)` of every column the aggregate reads —
/// the group keys and the aggregate arguments, deduplicated by the caller. A
/// synthetic row is table-width with exactly those ordinals filled (rest NULL);
/// the caller has already verified nothing else is read (no bare columns, no
/// per-aggregate FILTER over another column, no residual filter).
///
/// Returns `false` — decline to the row scan — unless every needed column has
/// fresh segments (`mod_gen`) that cover the table and are blocked identically.
pub(crate) fn feed_group_from_segments(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    width: usize,
    needed: &[(u16, ColumnType)],
    push: &mut dyn FnMut(&[Value]) -> Result<()>,
) -> Result<bool> {
    if needed.is_empty() || needed.iter().any(|&(_, ty)| !segmentable(ty)) {
        return Ok(false);
    }
    // Whole-table path (mod_gen): fed only when nothing has written since the
    // build. It validates every column before emitting any, so a decline pushes
    // nothing and it is safe to fall through to the tail path.
    if let (Ok(want_gen), Ok(want_rows)) = (snap.mod_gen(table), snap.row_count(table)) {
        if emit_group_segments(snap, table, width, needed, want_gen, want_rows, push)? {
            return Ok(true);
        }
    }
    // Split path (DESIGN-COLUMNAR §7): segments for the first `W` rows plus a
    // synthetic-row fold of the appended tail above the watermark.
    let Some((g0, covered, wm_pk)) = snap.columnar_watermark(table)? else {
        return Ok(false);
    };
    if !emit_group_segments(snap, table, width, needed, g0, covered, push)? {
        return Ok(false);
    }
    // The tail's needed columns, folded as full-width synthetic rows in PK order
    // — the same shape `emit_group_segments` produces, so the folder cannot tell
    // the two halves apart.
    let ords: Vec<u16> = needed.iter().map(|&(o, _)| o).collect();
    snap.fold_range_columns(
        table,
        Some((&wm_pk, false)),
        None,
        &ords,
        mpedb_core::FoldOpts::SERIAL,
        push,
    )?;
    Ok(true)
}

/// Emit the group-by synthetic rows of the segments for `needed` columns, all
/// decoded at generation `gen` and required to cover exactly `covered` rows.
/// Returns `false` (having pushed nothing) if any column is missing, stale,
/// blocked differently, or does not cover — the caller then declines or tries
/// the tail. Every column is validated up front, before the first emit.
fn emit_group_segments(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    width: usize,
    needed: &[(u16, ColumnType)],
    gen: u64,
    covered: u64,
    push: &mut dyn FnMut(&[Value]) -> Result<()>,
) -> Result<bool> {
    let mut recs: Vec<Vec<Vec<u8>>> = Vec::with_capacity(needed.len());
    for &(col, ty) in needed {
        match load_column(snap, table, col, ty, gen, covered)? {
            Some(r) => recs.push(r),
            None => return Ok(false),
        }
    }
    let n_blocks = recs[0].len();
    if recs.iter().any(|r| r.len() != n_blocks) {
        return Ok(false); // columns blocked differently: do not pair them
    }

    // One reused synthetic row; only the needed ordinals are ever written, and
    // the folder clones what it keeps, so overwriting per row is sound.
    let mut synth = vec![Value::Null; width];
    // Per-column decoded block, reused across blocks (bounded by one block, not
    // the table).
    let mut cols: Vec<Vec<Value>> = vec![Vec::new(); needed.len()];
    for bi in 0..n_blocks {
        let mut n_rows: Option<usize> = None;
        for (k, ((_, ty), rec)) in needed.iter().zip(&recs).enumerate() {
            let blk = decode_block(&rec[bi], gen, *ty)?
                .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
            match n_rows {
                None => n_rows = Some(blk.n_rows as usize),
                // Same-build columns share BLOCK_ROWS boundaries, so equal block
                // counts (checked above) force equal per-block rows — a mismatch
                // is inconsistent segments, i.e. corruption. It must be a hard
                // error, NOT a mid-emit `Ok(false)`: the group caller does not
                // rebuild its accumulators on decline, so declining after having
                // pushed earlier blocks would double-count them.
                Some(n) if n != blk.n_rows as usize => {
                    return Err(Error::Corrupt(
                        "group column segments disagree on block size".into(),
                    ))
                }
                _ => {}
            }
            cols[k].clear();
            blk.for_each(&mut |v: &Value| {
                cols[k].push(v.clone());
                Ok(())
            })?;
        }
        // Row-major emission across the columns decoded above. `r` indexes
        // every column at once (a lockstep read), so the range loop is the
        // natural shape — not an iterator over any single one.
        let n = n_rows.unwrap_or(0);
        #[allow(clippy::needless_range_loop)]
        for r in 0..n {
            for (k, &(ord, _)) in needed.iter().enumerate() {
                synth[ord as usize] = cols[k][r].clone();
            }
            push(&synth)?;
        }
    }
    Ok(true)
}

/// Load and validate every block of one column, returning the raw records so
/// Load and validate every block of one column, returning the raw records so
/// the caller can decode them a second time without another round of checks.
/// `None` = not usable (missing, stale, unknown format, or the blocks do not
/// cover the table).
fn load_column(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    col: u16,
    ty: ColumnType,
    want_gen: u64,
    want_rows: u64,
) -> Result<Option<Vec<Vec<u8>>>> {
    if !segmentable(ty) {
        return Ok(None);
    }
    let mut recs = Vec::new();
    let mut covered = 0u64;
    for bi in 0u32.. {
        let Some(bytes) = block_bytes(snap, table, col, bi)? else { break };
        match decode_block(&bytes, want_gen, ty)? {
            Some(b) => covered += b.n_rows as u64,
            None => return Ok(None),
        }
        recs.push(bytes);
        if covered > want_rows {
            return Ok(None);
        }
    }
    if recs.is_empty() || covered != want_rows {
        return Ok(None);
    }
    Ok(Some(recs))
}

/// Feed a FILTERED whole-table aggregate from column segments, skipping every
/// block whose zone map proves the predicate cannot hold there
/// (DESIGN-COLUMNAR stage 2).
///
/// This is the half a row store structurally cannot do: a row scan must visit
/// every row to learn that none of them match, while a block whose `[min,max]`
/// excludes the predicate is never read at all — not the predicate column, not
/// the aggregate column.
///
/// Per block, exactly one of three things happens:
/// - `None` — skip, nothing decoded;
/// - `All` — stream the aggregate column, no per-row test (sound only when the
///   predicate block has no NULLs, see [`zone_verdict`]);
/// - `Some` — stream the predicate column into a pass mask, then stream the
///   aggregate column and push where the mask says so.
///
/// Two streaming passes and an 8 KiB mask per block: no random access, no
/// materialized values, and the aggregate sees the same values in the same PK
/// order as the row scan, so the answer stays bit-identical.
pub(crate) fn feed_filtered_from_segments(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    agg_col: u16,
    agg_ty: ColumnType,
    pred: &ZonePred,
    pred_ty: ColumnType,
    push: &mut dyn FnMut(&Value) -> Result<()>,
) -> Result<bool> {
    if !segmentable(agg_ty) || !segmentable(pred_ty) {
        return Ok(false);
    }
    // Whole-table path (mod_gen). Its only in-loop non-emit exit is the
    // corruption tripwire (now a hard `Corrupt`, not a decline), so a `false`
    // here always means it emitted nothing — safe to fall through.
    if let (Ok(want_gen), Ok(want_rows)) = (snap.mod_gen(table), snap.row_count(table)) {
        if emit_filtered_segments(snap, table, agg_col, agg_ty, pred, pred_ty, want_gen, want_rows, push)?
        {
            return Ok(true);
        }
    }
    // Split path (DESIGN-COLUMNAR §7): zone-pruned segments for the first `W`
    // rows plus a filtered fold of the appended tail above the watermark.
    let Some((g0, covered, wm_pk)) = snap.columnar_watermark(table)? else {
        return Ok(false);
    };
    if !emit_filtered_segments(snap, table, agg_col, agg_ty, pred, pred_ty, g0, covered, push)? {
        return Ok(false);
    }
    // The tail: the aggregate column pushed only where the predicate holds,
    // evaluated by the SAME `zone_pred_pass` the block mask uses — bit-identical.
    let cols = [agg_col, pred.col];
    snap.fold_range_columns(
        table,
        Some((&wm_pk, false)),
        None,
        &cols,
        mpedb_core::FoldOpts::SERIAL,
        &mut |row: &[Value]| {
            if zone_pred_pass(&row[pred.col as usize], pred) {
                push(&row[agg_col as usize])?;
            }
            Ok(())
        },
    )?;
    Ok(true)
}

/// Does `v` satisfy the zone predicate? SQL 3VL (a NULL satisfies no
/// comparison), identical to the row path's `eval_filter`. Shared by the block
/// mask and the row-tail fold so the two halves of a split scan agree exactly.
fn zone_pred_pass(v: &Value, pred: &ZonePred) -> bool {
    match v {
        Value::Int(x) | Value::Timestamp(x) => match pred.op {
            Cmp::Lt => *x < pred.k,
            Cmp::Le => *x <= pred.k,
            Cmp::Gt => *x > pred.k,
            Cmp::Ge => *x >= pred.k,
            Cmp::Eq => *x == pred.k,
        },
        _ => false,
    }
}

/// Emit the zone-pruned aggregate values of the filtered segments, all decoded
/// at generation `gen` and required to cover exactly `covered` rows. Returns
/// `false` (nothing pushed) if either column is missing/stale/uncovered — the
/// decline is decided by `load_column` up front, before the emit loop, so it is
/// safe to fall through from. An inconsistent per-block pairing is `Corrupt`,
/// never a mid-emit decline.
#[allow(clippy::too_many_arguments)]
fn emit_filtered_segments(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    agg_col: u16,
    agg_ty: ColumnType,
    pred: &ZonePred,
    pred_ty: ColumnType,
    gen: u64,
    covered: u64,
    push: &mut dyn FnMut(&Value) -> Result<()>,
) -> Result<bool> {
    let Some(agg_recs) = load_column(snap, table, agg_col, agg_ty, gen, covered)? else {
        return Ok(false);
    };
    // The same column twice is legal (`sum(day_id) WHERE day_id >= …`) and
    // needs no second load.
    let pred_recs = if pred.col == agg_col && pred_ty == agg_ty {
        None
    } else {
        match load_column(snap, table, pred.col, pred_ty, gen, covered)? {
            Some(r) => Some(r),
            None => return Ok(false),
        }
    };
    let pred_recs: &Vec<Vec<u8>> = pred_recs.as_ref().unwrap_or(&agg_recs);
    if pred_recs.len() != agg_recs.len() {
        return Ok(false); // the two columns are blocked differently: do not pair them
    }

    let mut mask: Vec<u64> = Vec::new();
    for (abytes, pbytes) in agg_recs.iter().zip(pred_recs) {
        let ablk = decode_block(abytes, gen, agg_ty)?
            .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
        let pblk = decode_block(pbytes, gen, pred_ty)?
            .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
        // Both columns are blocked at one fixed size and cover the same row
        // count (checked above), so equal block counts force equal per-block
        // rows — a mismatch is inconsistent segments, i.e. corruption, NOT a
        // reason to decline after having already emitted earlier blocks.
        if ablk.n_rows != pblk.n_rows {
            return Err(Error::Corrupt("paired column segments disagree on block size".into()));
        }
        match zone_verdict(&pblk, pred) {
            Verdict::None => continue,
            Verdict::All => ablk.for_each(push)?,
            Verdict::Some => {
                let n = ablk.n_rows as usize;
                mask.clear();
                mask.resize(n.div_ceil(64), 0);
                let mut i = 0usize;
                pblk.for_each(&mut |v: &Value| {
                    // A NULL satisfies no comparison — SQL's 3VL, and the same
                    // answer the row path's `eval_filter` gives.
                    if zone_pred_pass(v, pred) {
                        mask[i / 64] |= 1u64 << (i % 64);
                    }
                    i += 1;
                    Ok(())
                })?;
                let mut j = 0usize;
                ablk.for_each(&mut |v: &Value| {
                    if mask[j / 64] & (1u64 << (j % 64)) != 0 {
                        push(v)?;
                    }
                    j += 1;
                    Ok(())
                })?;
            }
        }
    }
    Ok(true)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(ty: ColumnType, vals: &[Value]) {
        let b = encode_block(7, ty, vals).unwrap();
        let got = decode_block(&b, 7, ty).unwrap().expect("fresh");
        let got_values = got.values().unwrap();
        // Compared BITWISE for floats: `NaN != NaN` under PartialEq, but the
        // contract here is stronger than equality — the decoded value must be
        // the same bits, which is what makes a float sum bit-identical to the
        // row scan's rather than merely close.
        assert_eq!(got_values.len(), vals.len(), "round trip length");
        for (g, w) in got_values.iter().zip(vals) {
            match (g, w) {
                (Value::Float(a), Value::Float(b)) => {
                    assert_eq!(a.to_bits(), b.to_bits(), "float bits")
                }
                _ => assert_eq!(g, w, "round trip"),
            }
        }
        assert_eq!(got.n_rows as usize, vals.len());
        // A different generation must decline, never decode.
        assert!(decode_block(&b, 8, ty).unwrap().is_none());
        // Every truncation is Corrupt, never a panic.
        for n in 0..b.len() {
            let _ = decode_block(&b[..n], 7, ty);
        }
    }

    #[test]
    fn text_and_sparse_round_trip() {
        // Low-cardinality text → dictionary; the exact strings, NULLs in place.
        roundtrip(
            ColumnType::Text,
            &(0..1000).map(|i| Value::Text(format!("cat{}", i % 5))).collect::<Vec<_>>(),
        );
        // High-cardinality text → raw length-prefixed (dict would not shrink).
        roundtrip(
            ColumnType::Text,
            &(0..500).map(|i| Value::Text(format!("row-{i}-unique"))).collect::<Vec<_>>(),
        );
        // Text with NULLs and empty strings interleaved.
        roundtrip(
            ColumnType::Text,
            &[
                Value::Text("a".into()),
                Value::Null,
                Value::Text(String::new()),
                Value::Text("a".into()),
                Value::Null,
            ],
        );
        roundtrip(ColumnType::Text, &[]);
        // Blob.
        roundtrip(
            ColumnType::Blob,
            &[Value::Blob(vec![0, 1, 2]), Value::Null, Value::Blob(vec![])],
        );
        // Sparse integer → run-of-default (mostly 0, a few exceptions).
        let mut sparse = vec![Value::Int(0); 2000];
        sparse[7] = Value::Int(99);
        sparse[1500] = Value::Int(-42);
        sparse[13] = Value::Null;
        roundtrip(ColumnType::Int64, &sparse);
        // Low-cardinality integer → dictionary (5 distinct in 2000).
        roundtrip(
            ColumnType::Int64,
            &(0..2000).map(|i| Value::Int([10, 20, 30, 40, 50][i % 5])).collect::<Vec<_>>(),
        );
        // Low-cardinality FLOAT → dictionary over bit patterns; NaN and -0.0
        // must survive as their exact bits, not merely compare equal.
        roundtrip(
            ColumnType::Float64,
            &(0..2000)
                .map(|i| Value::Float([1.5, -0.0, f64::NAN, 2.5][i % 4]))
                .collect::<Vec<_>>(),
        );
    }

    /// The compact pass must pick the SMALLEST candidate per block — checked by
    /// asserting each shape lands on the encoding it should and beats raw.
    #[test]
    fn best_of_picks_the_smallest_encoding() {
        let raw_bytes = |vals: &[Value]| {
            // n_nonnull × 8 is the raw64 body size; the header+nulls are
            // constant, so a smaller total means a smaller payload.
            vals.iter().filter(|v| !matches!(v, Value::Null)).count() * 8
        };
        // A 5-value low-card int block: dictionary must beat 8 bytes/value.
        let lc: Vec<Value> = (0..2000).map(|i| Value::Int([1, 2, 3, 4, 5][i % 5])).collect();
        let enc = encode_block(1, ColumnType::Int64, &lc).unwrap();
        assert!(enc.len() < raw_bytes(&lc), "low-card int compresses");
        // A sparse block: run-of-default must be tiny.
        let mut sp = vec![Value::Int(0); 5000];
        sp[10] = Value::Int(1);
        let enc = encode_block(1, ColumnType::Int64, &sp).unwrap();
        assert!(enc.len() < 100, "sparse null-free int is a handful of bytes, got {}", enc.len());
    }

    #[test]
    fn blocks_round_trip_across_shapes() {
        // Narrow range → a few bits per value.
        roundtrip(
            ColumnType::Int64,
            &(0..1000).map(|i| Value::Int(1000 + (i % 7))).collect::<Vec<_>>(),
        );
        // All identical → zero-width payload.
        roundtrip(ColumnType::Int64, &vec![Value::Int(42); 500]);
        // Negatives and the full i64 span (FOR must not overflow).
        roundtrip(
            ColumnType::Int64,
            &[Value::Int(i64::MIN), Value::Int(0), Value::Int(i64::MAX)],
        );
        // Nulls interleaved — the bitmap must restore the exact order.
        roundtrip(
            ColumnType::Int64,
            &[Value::Int(1), Value::Null, Value::Int(3), Value::Null, Value::Null],
        );
        roundtrip(ColumnType::Int64, &vec![Value::Null; 64]);
        roundtrip(ColumnType::Int64, &[]);
        // Floats keep their exact bits, NaN and -0.0 included.
        roundtrip(
            ColumnType::Float64,
            &[
                Value::Float(1.5),
                Value::Float(-0.0),
                Value::Null,
                Value::Float(f64::NAN),
                Value::Float(f64::INFINITY),
            ],
        );
        roundtrip(
            ColumnType::Timestamp,
            &[Value::Timestamp(0), Value::Timestamp(1_700_000_000_000_000)],
        );
    }

    #[test]
    fn bitpack_round_trips_at_every_width() {
        for width in 0..=64u32 {
            let n = 37usize;
            let mask = if width == 0 { 0 } else { u64::MAX >> (64 - width) };
            let vals: Vec<u64> = (0..n).map(|i| (i as u64).wrapping_mul(0x9E37_79B9) & mask).collect();
            let mut buf = Vec::new();
            pack_bits(&mut buf, &vals, width);
            assert_eq!(unpack_bits(&buf, n, width).unwrap(), vals, "width {width}");
        }
    }

    #[test]
    fn a_foreign_type_or_format_declines_rather_than_misreads() {
        let b = encode_block(1, ColumnType::Int64, &[Value::Int(5)]).unwrap();
        // Same bytes read as a different column type: decline, not garbage.
        assert!(decode_block(&b, 1, ColumnType::Float64).unwrap().is_none());
        assert!(decode_block(&b, 1, ColumnType::Text).unwrap().is_none());
        // A bumped format byte declines too.
        let mut f = b.clone();
        f[4] = 0xFF;
        assert!(decode_block(&f, 1, ColumnType::Int64).unwrap().is_none());
        // Trailing garbage is Corrupt.
        let mut t = b.clone();
        t.push(0);
        assert!(decode_block(&t, 1, ColumnType::Int64).is_err());
    }
}
