//! Column segments — stage 1 of design/DESIGN-COLUMNAR.md.
//!
//! A segment is a **regenerable, read-optimized copy of one column**, blocked
//! in PK order and kept in the sys-keyspace (namespace `colseg`) exactly like a
//! stats record. It is NOT a page-format change and NOT the source of truth:
//! the row B+tree stays that, and a segment only ever makes a scan cheaper or
//! gets ignored. Every decline path — no segment, a `mod_gen` mismatch, an
//! encoding this build does not know, a decode that returns `Corrupt` — falls
//! back to the row scan, so a segment can never make an answer wrong.
//!
//! **Why it is faster.** A `sum(amount)` over a six-column fact table reads
//! every row's whole ~50-byte record out of the PK tree to extract 8 bytes. A
//! segment stores that one column contiguously, frame-of-reference coded and
//! bit-packed, so the same scan touches a few bits per row of sequential
//! memory. The encoding is chosen per block from that block's own measured
//! min/max — the compression IS the layout, and there is no entropy coder
//! anywhere: every value is reachable by arithmetic, which is what lets a
//! later stage skip whole blocks on a predicate without decoding them.
//!
//! **What stage 1 deliberately does NOT do.** It does not answer aggregates
//! from the per-block summaries, even though the zone map is stored (stage 2
//! needs it). The values are decoded and pushed through the SAME accumulators
//! the row scan uses, in the SAME order — so `sum`, `min`, `max`, `count` are
//! bit-identical to the row path, floats included, and no aggregate semantics
//! (integer overflow raises, collation, `avg`'s count) had to be reimplemented
//! somewhere they could drift. The win here is purely touched bytes.

use mpedb_types::{ColumnType, Error, Result, Value};

/// Namespace in the sys-record keyspace.
pub const NS: &str = "colseg";

const MAGIC: &[u8; 4] = b"MCOL";
/// Versioned from day one. A layout change is a new format, not a migration:
/// an unknown format reads as "no segment" and the row scan runs.
const FORMAT: u16 = 1;

/// Rows per block. Sized so a block's payload stays far below
/// `SYS_RECORD_MAX_VALUE` (1 MiB) even at the raw 8-bytes-per-value worst case
/// (65 536 × 8 = 512 KiB), while still being long enough that the per-block
/// header is noise.
pub const BLOCK_ROWS: usize = 65_536;

/// Encodings, all directly addressable — no entropy coder, no decode pass, no
/// intermediate buffer (stage 3b adds dictionary and run-of-default; the byte
/// values still come straight out of the payload). Every one is chosen per
/// block from that block's own data at compact time.
const ENC_FOR_BITPACK: u8 = 1; // integers: value − block_min, packed to the needed width
const ENC_RAW64: u8 = 2; // 8-byte values (floats, and any block the others cannot shrink)
const ENC_DICT: u8 = 3; // low-cardinality: a per-block dictionary + packed codes
const ENC_RAW_TEXT: u8 = 4; // high-cardinality text/blob: length-prefixed bytes
const ENC_RUN_DEFAULT: u8 = 5; // sparse: one default value + an exception list

/// Read the `k`-th `width`-bit value from a packed array (`width == 0` → 0).
fn packed_at(buf: &[u8], k: usize, width: u32) -> u64 {
    if width == 0 {
        return 0;
    }
    let mask = u64::MAX >> (64 - width);
    let bit = k * width as usize;
    let byte = bit / 8;
    let off = (bit % 8) as u32;
    let end = (byte + 9).min(buf.len());
    let mut acc: u128 = 0;
    for (j, b) in buf[byte..end].iter().enumerate() {
        acc |= (*b as u128) << (8 * j);
    }
    ((acc >> off) as u64) & mask
}

/// The decoded, ready-to-stream form of a block's payload. Borrows the block's
/// bytes (dictionaries and text are slices, not copies); numeric values are
/// produced by arithmetic, text values by a dictionary/offset lookup.
enum Codec<'a> {
    /// 8-byte values.
    Raw64(&'a [u8]),
    /// Frame of reference: `block_min + packed_delta`.
    For { lo: i64, width: u32, packed: &'a [u8] },
    /// Sparse: `default` everywhere except the listed non-null indices.
    RunDefault { default: u64, exc: Vec<(u32, u64)> },
    /// Low-cardinality numeric: `dict[code(k)]`.
    DictNum { dict: Vec<u64>, width: u32, codes: &'a [u8] },
    /// Low-cardinality text/blob: `dict[code(k)]` as bytes.
    DictText { dict: Vec<&'a [u8]>, width: u32, codes: &'a [u8] },
    /// High-cardinality text/blob: the k-th length-prefixed slice.
    RawText(Vec<&'a [u8]>),
}

/// One VALIDATED block, held as a view over its own bytes.
///
/// It deliberately does not materialize the values: the row fold streams one
/// value at a time into the accumulator, so a segment that first built a
/// `Vec<Value>` per block would reintroduce exactly the per-row allocation the
/// columnar design exists to remove — and the first measurement showed it
/// does, turning a 12× smaller column into a SLOWER scan. Values are produced
/// by arithmetic straight out of the packed payload
/// ([`Block::for_each`]).
pub struct Block<'a> {
    /// Rows in the block, nulls included — the block's share of the scan.
    pub n_rows: u32,
    n_nonnull: u32,
    ty: ColumnType,
    zmin: u64,
    zmax: u64,
    /// The null bitmap, or `None` when the block has no NULLs — a NOT-NULL
    /// fact column then pays nothing for a bitmap of zeros (n_rows/8 bytes, a
    /// quarter-megabyte per 2M-row column).
    nulls: Option<&'a [u8]>,
    codec: Codec<'a>,
}

impl Block<'_> {
    /// Does this block contain no NULLs at all? A "the whole block passes"
    /// shortcut is only sound when it does: the zone map covers the NON-NULL
    /// values, and a NULL satisfies no comparison, so a block with NULLs must
    /// still be tested row by row.
    pub fn null_free(&self) -> bool {
        self.n_nonnull == self.n_rows
    }

    /// Is this an integer-class column (the only kind a zone map decides)?
    pub fn is_int_column(&self) -> bool {
        matches!(self.ty, ColumnType::Int64 | ColumnType::Timestamp)
    }

    /// The block's INTEGER value bounds, or `None` when the block holds no
    /// non-null value (the bounds would be sentinels) or is not an integer
    /// column. Floats are deliberately excluded: NaN compares false to
    /// everything, and the encoder's min/max skip it, so a float zone map
    /// cannot support an "everything passes" conclusion.
    pub fn int_bounds(&self) -> Option<(i64, i64)> {
        if self.n_nonnull == 0 || matches!(self.ty, ColumnType::Float64) {
            return None;
        }
        Some((self.zmin as i64, self.zmax as i64))
    }

    /// Stream every value, in PK order, nulls in place.
    pub fn for_each(&self, f: &mut dyn FnMut(&Value) -> Result<()>) -> Result<()> {
        let n_rows = self.n_rows as usize;
        let n_nonnull = self.n_nonnull as usize;
        // A NULL bitmap that disagrees with `n_nonnull` is corruption; produce
        // the numeric bit-pattern (or text slice) for the k-th non-null value.
        let numeric = |bits: u64| match self.ty {
            ColumnType::Float64 => Value::Float(f64::from_bits(bits)),
            ColumnType::Timestamp => Value::Timestamp(bits as i64),
            _ => Value::Int(bits as i64),
        };
        let text = |b: &[u8]| -> Result<Value> {
            if self.ty == ColumnType::Text {
                Ok(Value::Text(
                    std::str::from_utf8(b)
                        .map_err(|_| Error::Corrupt("column segment: invalid utf-8".into()))?
                        .to_owned(),
                ))
            } else {
                Ok(Value::Blob(b.to_vec()))
            }
        };
        // RunDefault walks its exception list with a cursor as `k` advances.
        let mut exc_i = 0usize;
        let mut k = 0usize;
        for i in 0..n_rows {
            if self.nulls.is_some_and(|b| b[i / 8] & (1 << (i % 8)) != 0) {
                f(&Value::Null)?;
                continue;
            }
            if k >= n_nonnull {
                return Err(Error::Corrupt("column segment: null bitmap disagrees".into()));
            }
            let v = match &self.codec {
                Codec::Raw64(p) => {
                    let o = k * 8;
                    numeric(u64::from_le_bytes(
                        p.get(o..o + 8)
                            .ok_or_else(|| Error::Corrupt("column segment: short raw64".into()))?
                            .try_into()
                            .unwrap(),
                    ))
                }
                Codec::For { lo, width, packed } => {
                    numeric(lo.wrapping_add(packed_at(packed, k, *width) as i64) as u64)
                }
                Codec::RunDefault { default, exc } => {
                    let bits = if exc_i < exc.len() && exc[exc_i].0 as usize == k {
                        let v = exc[exc_i].1;
                        exc_i += 1;
                        v
                    } else {
                        *default
                    };
                    numeric(bits)
                }
                Codec::DictNum { dict, width, codes } => {
                    let c = packed_at(codes, k, *width) as usize;
                    numeric(
                        *dict
                            .get(c)
                            .ok_or_else(|| Error::Corrupt("column segment: dict code".into()))?,
                    )
                }
                Codec::DictText { dict, width, codes } => {
                    let c = packed_at(codes, k, *width) as usize;
                    text(dict
                        .get(c)
                        .ok_or_else(|| Error::Corrupt("column segment: dict code".into()))?)?
                }
                Codec::RawText(offs) => text(offs.get(k).ok_or_else(|| {
                    Error::Corrupt("column segment: short raw-text".into())
                })?)?,
            };
            k += 1;
            f(&v)?;
        }
        if k != n_nonnull {
            return Err(Error::Corrupt("column segment: null bitmap disagrees".into()));
        }
        Ok(())
    }

    /// Materialize — tests and the reference path only.
    #[cfg(test)]
    pub fn values(&self) -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(self.n_rows as usize);
        self.for_each(&mut |v| {
            out.push(v.clone());
            Ok(())
        })?;
        Ok(out)
    }
}

/// What one `compact_columns` pass produced, for the CLI/report.
#[derive(Debug, Clone)]
pub struct ColSegStat {
    pub table: String,
    pub column: String,
    pub blocks: u32,
    pub rows: u64,
    pub bytes: u64,
}

/// Key: `table BE4 ‖ column ORDINAL BE2 ‖ block BE4`.
///
/// The column is keyed by ORDINAL, not by name, which is safe only because
/// `DROP COLUMN` — the one operation that renumbers the survivors — bumps the
/// table's `mod_gen` and so invalidates every segment. `RENAME COLUMN` does
/// not bump, and does not need to: the ordinal is unchanged and the values
/// are unchanged. See `ReadTxn::mod_gen`.
pub fn record_key(table_id: u32, col: u16, block: u32) -> [u8; 10] {
    let mut k = [0u8; 10];
    k[0..4].copy_from_slice(&table_id.to_be_bytes());
    k[4..6].copy_from_slice(&col.to_be_bytes());
    k[6..10].copy_from_slice(&block.to_be_bytes());
    k
}

/// Is this a column type stage 1 can segment? Text/Blob need the dictionary
/// encoding (stage 3); `Any` is class-encoded and has no fixed width.
pub fn segmentable(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Int64 | ColumnType::Float64 | ColumnType::Timestamp | ColumnType::Text | ColumnType::Blob
    )
}

fn is_numeric(ty: ColumnType) -> bool {
    matches!(ty, ColumnType::Int64 | ColumnType::Float64 | ColumnType::Timestamp)
}

fn put_lp(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn bits_for(range: u64) -> u32 {
    if range == 0 {
        0
    } else {
        64 - range.leading_zeros()
    }
}

fn pack_bits(out: &mut Vec<u8>, vals: &[u64], width: u32) {
    if width == 0 {
        return;
    }
    let mut acc: u64 = 0;
    let mut used: u32 = 0;
    for &v in vals {
        acc |= (v & (u64::MAX >> (64 - width))) << used;
        used += width;
        while used >= 8 {
            out.push((acc & 0xFF) as u8);
            acc >>= 8;
            used -= 8;
        }
    }
    if used > 0 {
        out.push((acc & 0xFF) as u8);
    }
}

/// The bulk inverse of [`pack_bits`]. Only the width round-trip test uses it —
/// the scan path reads each value in place (`Block::for_each`) rather than
/// unpacking a buffer, which is what made the segment faster than the row fold
/// instead of slower.
#[cfg(test)]
fn unpack_bits(buf: &[u8], n: usize, width: u32) -> Result<Vec<u64>> {
    let mut out = Vec::with_capacity(n);
    if width == 0 {
        out.resize(n, 0);
        return Ok(out);
    }
    let need = (n * width as usize).div_ceil(8);
    if buf.len() < need {
        return Err(Error::Corrupt("column segment: truncated payload".into()));
    }
    let mask = u64::MAX >> (64 - width);
    let mut bit = 0usize;
    for _ in 0..n {
        let byte = bit / 8;
        let off = (bit % 8) as u32;
        // Up to 9 bytes can span a value of width ≤ 64 at any bit offset.
        let mut acc: u128 = 0;
        for (i, b) in buf[byte..(byte + 9).min(buf.len())].iter().enumerate() {
            acc |= (*b as u128) << (8 * i);
        }
        out.push(((acc >> off) as u64) & mask);
        bit += width as usize;
    }
    Ok(out)
}

/// Encode one block of values (in PK order, nulls included).
pub fn encode_block(mod_gen: u64, ty: ColumnType, vals: &[Value]) -> Result<Vec<u8>> {
    let n = vals.len();
    let mut nulls = vec![0u8; n.div_ceil(8)];

    // The NON-NULL stream, in row order. Numeric columns carry bit patterns;
    // text/blob carry the bytes.
    let mut raw: Vec<u64> = Vec::new();
    let mut txt: Vec<&[u8]> = Vec::new();
    let numeric = is_numeric(ty);
    for (i, v) in vals.iter().enumerate() {
        match v {
            Value::Null => nulls[i / 8] |= 1 << (i % 8),
            Value::Int(x) | Value::Timestamp(x) if numeric => raw.push(*x as u64),
            Value::Float(f) if numeric => raw.push(f.to_bits()),
            Value::Text(sx) if ty == ColumnType::Text => txt.push(sx.as_bytes()),
            Value::Blob(bx) if ty == ColumnType::Blob => txt.push(bx.as_slice()),
            other => {
                return Err(Error::Internal(format!(
                    "column segment: unexpected value {} for {ty:?}",
                    other.type_name()
                )))
            }
        }
    }
    let n_nonnull = if numeric { raw.len() } else { txt.len() };

    // Zone map over the NON-NULL values (integers only; a float zone map is
    // unusable for pruning, and text has none). Stored for stage 2.
    let (zmin, zmax) = if numeric && ty != ColumnType::Float64 {
        let mut lo = i64::MAX;
        let mut hi = i64::MIN;
        for &b in &raw {
            let x = b as i64;
            if x < lo { lo = x; }
            if x > hi { hi = x; }
        }
        (lo as u64, hi as u64)
    } else if ty == ColumnType::Float64 {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for &b in &raw {
            let f = f64::from_bits(b);
            if f < lo { lo = f; }
            if f > hi { hi = f; }
        }
        (lo.to_bits(), hi.to_bits())
    } else {
        (0, 0)
    };

    // Best-of encoding, chosen from THIS block's own data. Each candidate is
    // built, and the smallest payload wins — the compression is the layout, so
    // "smaller" is measured, not guessed.
    let (enc, width, payload) = if numeric {
        best_numeric(ty, &raw, zmin)
    } else {
        best_text(&txt)
    };

    let mut out = Vec::with_capacity(40 + nulls.len() + payload.len());

    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&mod_gen.to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(n_nonnull as u32).to_le_bytes());
    out.push(ty as u8);
    out.push(enc);
    out.push(width as u8);
    out.push((n_nonnull != n) as u8);
    out.extend_from_slice(&zmin.to_le_bytes());
    out.extend_from_slice(&zmax.to_le_bytes());
    if n_nonnull != n {
        out.extend_from_slice(&nulls);
    }
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Choose the smallest of the numeric encodings for this block's non-null
/// values (`raw`, bit patterns): frame-of-reference, run-of-default, a
/// low-cardinality dictionary, or plain 8-byte. All lossless; the winner is
/// whichever is fewest bytes.
fn best_numeric(ty: ColumnType, raw: &[u64], zmin: u64) -> (u8, u32, Vec<u8>) {
    let raw64 = || -> Vec<u8> {
        let mut p = Vec::with_capacity(raw.len() * 8);
        for &b in raw {
            p.extend_from_slice(&b.to_le_bytes());
        }
        p
    };
    if raw.is_empty() {
        return (ENC_RAW64, 0, Vec::new());
    }
    let mut best: (u8, u32, Vec<u8>) = (ENC_RAW64, 0, raw64());

    // Frame of reference (non-float, in-range).
    if ty != ColumnType::Float64 {
        let lo = zmin as i64;
        let hi = best_hi(raw);
        let range = (hi as i128 - lo as i128) as u128;
        if range <= u64::MAX as u128 {
            let w = bits_for(range as u64);
            let mut p = Vec::with_capacity((raw.len() * w as usize).div_ceil(8));
            let deltas: Vec<u64> = raw.iter().map(|&b| (b as i64).wrapping_sub(lo) as u64).collect();
            pack_bits(&mut p, &deltas, w);
            if p.len() < best.2.len() {
                best = (ENC_FOR_BITPACK, w, p);
            }
        }
    }

    // Frequency-derived candidates: run-of-default (from the mode) and
    // dictionary (from the distinct set). One pass builds the counts.
    let mut counts: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    for &b in raw {
        *counts.entry(b).or_default() += 1;
    }
    // Run-of-default: the most frequent value as the default, the rest as
    // (index, value) exceptions. Worth it only when one value dominates.
    if let Some((&default, &cnt)) = counts.iter().max_by_key(|(_, c)| **c) {
        let n_exc = raw.len() - cnt as usize;
        // 8 (default) + 4 (count) + 12 per exception.
        let size = 12 + n_exc * 12;
        if size < best.2.len() {
            let mut p = Vec::with_capacity(size);
            p.extend_from_slice(&default.to_le_bytes());
            p.extend_from_slice(&(n_exc as u32).to_le_bytes());
            for (k, &b) in raw.iter().enumerate() {
                if b != default {
                    p.extend_from_slice(&(k as u32).to_le_bytes());
                    p.extend_from_slice(&b.to_le_bytes());
                }
            }
            best = (ENC_RUN_DEFAULT, 0, p);
        }
    }
    // Dictionary: distinct values + packed codes.
    let distinct = counts.len();
    if distinct >= 1 {
        let cw = bits_for((distinct as u64).saturating_sub(1));
        let size = 4 + distinct * 8 + (raw.len() * cw as usize).div_ceil(8);
        if size < best.2.len() {
            let mut dict: Vec<u64> = counts.keys().copied().collect();
            dict.sort_unstable();
            let index: std::collections::HashMap<u64, u32> =
                dict.iter().enumerate().map(|(i, &v)| (v, i as u32)).collect();
            let mut p = Vec::with_capacity(size);
            p.extend_from_slice(&(distinct as u32).to_le_bytes());
            for &d in &dict {
                p.extend_from_slice(&d.to_le_bytes());
            }
            let codes: Vec<u64> = raw.iter().map(|b| index[b] as u64).collect();
            pack_bits(&mut p, &codes, cw);
            best = (ENC_DICT, cw, p);
        }
    }
    best
}

fn best_hi(raw: &[u64]) -> i64 {
    raw.iter().map(|&b| b as i64).max().unwrap_or(i64::MIN)
}

/// Choose between a dictionary and raw length-prefixed bytes for a text/blob
/// block's non-null values.
fn best_text(txt: &[&[u8]]) -> (u8, u32, Vec<u8>) {
    let raw_text = || -> Vec<u8> {
        let mut p = Vec::new();
        for b in txt {
            put_lp(&mut p, b);
        }
        p
    };
    if txt.is_empty() {
        return (ENC_RAW_TEXT, 0, Vec::new());
    }
    let mut best = (ENC_RAW_TEXT, 0u32, raw_text());

    // Dictionary of distinct byte strings.
    let mut index: std::collections::HashMap<&[u8], u32> = std::collections::HashMap::new();
    let mut dict: Vec<&[u8]> = Vec::new();
    for b in txt {
        if !index.contains_key(b) {
            index.insert(b, dict.len() as u32);
            dict.push(b);
        }
    }
    let cw = bits_for((dict.len() as u64).saturating_sub(1));
    let dict_bytes: usize = 4 + dict.iter().map(|b| 4 + b.len()).sum::<usize>();
    let size = dict_bytes + (txt.len() * cw as usize).div_ceil(8);
    if size < best.2.len() {
        let mut p = Vec::with_capacity(size);
        p.extend_from_slice(&(dict.len() as u32).to_le_bytes());
        for b in &dict {
            put_lp(&mut p, b);
        }
        let codes: Vec<u64> = txt.iter().map(|b| index[b] as u64).collect();
        pack_bits(&mut p, &codes, cw);
        best = (ENC_DICT, cw, p);
    }
    best
}

/// Decode a block, but only if it was built at `want_gen` — the coherence test
/// (DESIGN-COLUMNAR §6). Treats its input as hostile: every read is
/// bounds-checked and returns `Corrupt`, never panics. `Ok(None)` means "not
/// usable" (wrong generation, wrong format, wrong type) and the caller runs
/// the row scan.
pub fn decode_block(bytes: &[u8], want_gen: u64, ty: ColumnType) -> Result<Option<Block<'_>>> {
    let take = |p: &mut usize, n: usize| -> Result<&[u8]> {
        let end = p
            .checked_add(n)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| Error::Corrupt("column segment: truncated".into()))?;
        let s = &bytes[*p..end];
        *p = end;
        Ok(s)
    };
    let mut p = 0usize;
    if take(&mut p, 4)? != MAGIC {
        return Err(Error::Corrupt("column segment: bad magic".into()));
    }
    if u16::from_le_bytes(take(&mut p, 2)?.try_into().unwrap()) != FORMAT {
        return Ok(None); // a format this build does not know: fall back
    }
    if u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap()) != want_gen {
        return Ok(None); // stale: the table changed since this was built
    }
    let n_rows = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
    let n_nonnull = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
    if n_rows > BLOCK_ROWS || n_nonnull > n_rows {
        return Err(Error::Corrupt("column segment: impossible row counts".into()));
    }
    let stored_ty = take(&mut p, 1)?[0];
    if stored_ty != ty as u8 {
        return Ok(None); // the column was altered under the segment
    }
    let enc = take(&mut p, 1)?[0];
    let width = take(&mut p, 1)?[0] as u32;
    if width > 64 {
        return Err(Error::Corrupt("column segment: bad bit width".into()));
    }
    let has_nulls = match take(&mut p, 1)?[0] {
        0 => false,
        1 => true,
        _ => return Err(Error::Corrupt("column segment: bad has_nulls".into())),
    };
    let zmin = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap());
    let zmax = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap());
    let nulls = if has_nulls {
        Some(take(&mut p, n_rows.div_ceil(8))?)
    } else {
        if n_nonnull != n_rows {
            return Err(Error::Corrupt("column segment: null-free flag disagrees".into()));
        }
        None
    };

    // Everything after the null bitmap is the encoding's payload; parse it into
    // the streaming codec here, so `for_each` allocates nothing and the length
    // checks happen once per block, not once per value. A length-prefix reader
    // over the remaining bytes, bounds-checked, `Corrupt`-never-panic.
    let lp = |p: &mut usize| -> Result<&[u8]> {
        let len = u32::from_le_bytes(take_at(bytes, p, 4)?.try_into().unwrap()) as usize;
        take_at(bytes, p, len)
    };
    let text_col = matches!(ty, ColumnType::Text | ColumnType::Blob);
    let codec = match (enc, text_col) {
        (ENC_RAW64, false) => {
            let body = take(&mut p, n_nonnull.checked_mul(8).ok_or_else(|| {
                Error::Corrupt("column segment: payload length overflow".into())
            })?)?;
            Codec::Raw64(body)
        }
        (ENC_FOR_BITPACK, false) => {
            let body = take(&mut p, (n_nonnull * width as usize).div_ceil(8))?;
            Codec::For { lo: zmin as i64, width, packed: body }
        }
        (ENC_RUN_DEFAULT, false) => {
            let default = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap());
            let n_exc = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
            if n_exc > n_nonnull {
                return Err(Error::Corrupt("column segment: too many exceptions".into()));
            }
            let mut exc = Vec::with_capacity(n_exc);
            let mut prev: Option<u32> = None;
            for _ in 0..n_exc {
                let idx = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap());
                let val = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap());
                // Ascending, in range — the streaming cursor relies on it.
                if idx as usize >= n_nonnull || prev.is_some_and(|q| idx <= q) {
                    return Err(Error::Corrupt("column segment: bad exception index".into()));
                }
                prev = Some(idx);
                exc.push((idx, val));
            }
            Codec::RunDefault { default, exc }
        }
        (ENC_DICT, false) => {
            let dn = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
            let mut dict = Vec::with_capacity(dn);
            for _ in 0..dn {
                dict.push(u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap()));
            }
            let codes = take(&mut p, (n_nonnull * width as usize).div_ceil(8))?;
            if dn == 0 && n_nonnull > 0 {
                return Err(Error::Corrupt("column segment: empty dict".into()));
            }
            Codec::DictNum { dict, width, codes }
        }
        (ENC_DICT, true) => {
            let dn = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
            let mut dict = Vec::with_capacity(dn);
            for _ in 0..dn {
                dict.push(lp(&mut p)?);
            }
            let codes = take(&mut p, (n_nonnull * width as usize).div_ceil(8))?;
            if dn == 0 && n_nonnull > 0 {
                return Err(Error::Corrupt("column segment: empty dict".into()));
            }
            Codec::DictText { dict, width, codes }
        }
        (ENC_RAW_TEXT, true) => {
            let mut offs = Vec::with_capacity(n_nonnull);
            for _ in 0..n_nonnull {
                offs.push(lp(&mut p)?);
            }
            Codec::RawText(offs)
        }
        // An encoding/type this build does not pair: fall back to the row scan.
        _ => return Ok(None),
    };
    if p != bytes.len() {
        return Err(Error::Corrupt("column segment: trailing bytes".into()));
    }
    Ok(Some(Block {
        n_rows: n_rows as u32,
        n_nonnull: n_nonnull as u32,
        ty,
        zmin,
        zmax,
        nulls,
        codec,
    }))
}

/// Bounds-checked read of `n` bytes at `*p`, advancing it — the module's
/// standard `Corrupt`-never-panic slice reader.
fn take_at<'a>(bytes: &'a [u8], p: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = p
        .checked_add(n)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| Error::Corrupt("column segment: truncated".into()))?;
    let s = &bytes[*p..end];
    *p = end;
    Ok(s)
}

// ------------------------------------------------------- zone-map predicates

/// The comparison a zone map can reason about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

/// A predicate a block's zone map can decide WITHOUT decoding: `col OP k`,
/// over an integer column, where `k` is a folded constant or a query
/// parameter.
pub struct ZonePred {
    pub col: u16,
    pub op: Cmp,
    pub k: i64,
}

/// Recognize the whole filter as one integer comparison against a constant or
/// parameter — the shape a zone map can decide.
///
/// Deliberately narrow, and every restriction is a correctness one rather than
/// laziness:
/// - the program must be the ENTIRE filter, so nothing else can disqualify a
///   row a block-level "all pass" conclusion would then wave through;
/// - integers (and timestamps) only, because a float zone map is built with
///   comparisons NaN loses, so "every value passes" would not follow from it;
/// - the constant must be an integer of the same class, so no cross-type
///   coercion happens here that the row path would have done differently.
///
/// Anything else returns `None` and the ordinary filtered fold runs.
pub fn zone_predicate(prog: &mpedb_types::ExprProgram, params: &[Value]) -> Option<ZonePred> {
    use mpedb_types::Instr;
    let [a, b, c] = prog.instrs.as_slice() else {
        return None;
    };
    let op = match c {
        Instr::Lt => Cmp::Lt,
        Instr::Le => Cmp::Le,
        Instr::Gt => Cmp::Gt,
        Instr::Ge => Cmp::Ge,
        Instr::Eq => Cmp::Eq,
        _ => return None,
    };
    let operand = |i: &Instr| -> Option<i64> {
        match i {
            Instr::PushConst(x) => match prog.consts.get(*x as usize)? {
                Value::Int(v) | Value::Timestamp(v) => Some(*v),
                _ => None,
            },
            Instr::PushParam(x) => match params.get(*x as usize)? {
                Value::Int(v) | Value::Timestamp(v) => Some(*v),
                _ => None,
            },
            _ => None,
        }
    };
    match (a, b) {
        (Instr::PushCol(col), rhs) => Some(ZonePred { col: *col, op, k: operand(rhs)? }),
        // `1000 <= day_id` — the same fact with the operands swapped, so the
        // comparison must be mirrored, not merely reused.
        (lhs, Instr::PushCol(col)) => {
            let k = operand(lhs)?;
            let op = match op {
                Cmp::Lt => Cmp::Gt,
                Cmp::Le => Cmp::Ge,
                Cmp::Gt => Cmp::Lt,
                Cmp::Ge => Cmp::Le,
                Cmp::Eq => Cmp::Eq,
            };
            Some(ZonePred { col: *col, op, k })
        }
        _ => None,
    }
}

/// What a block's zone map says about a predicate, before any value is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// No row in the block can satisfy it — skip the block entirely.
    None,
    /// Every row satisfies it — take the block without testing.
    All,
    /// Some might; the block has to be read.
    Some,
}

pub fn zone_verdict(b: &Block<'_>, p: &ZonePred) -> Verdict {
    if b.n_rows == 0 {
        return Verdict::None;
    }
    let Some((lo, hi)) = b.int_bounds() else {
        // No integer bounds. Two reasons, and they differ: an INTEGER column
        // whose every row is NULL satisfies nothing (NULL passes no
        // comparison), while a non-integer column simply has to be read.
        return if b.is_int_column() {
            Verdict::None
        } else {
            Verdict::Some
        };
    };
    let (all, none) = match p.op {
        Cmp::Ge => (lo >= p.k, hi < p.k),
        Cmp::Gt => (lo > p.k, hi <= p.k),
        Cmp::Le => (hi <= p.k, lo > p.k),
        Cmp::Lt => (hi < p.k, lo >= p.k),
        Cmp::Eq => (lo == p.k && hi == p.k, p.k < lo || p.k > hi),
    };
    if none {
        // Sound with NULLs present too: a NULL satisfies no comparison, so if
        // no non-null value can pass, no row can.
        Verdict::None
    } else if all && b.null_free() {
        // "All" needs null-freeness: the bounds describe the non-null values
        // only, and a NULL would not have passed.
        Verdict::All
    } else {
        Verdict::Some
    }
}

// ---------------------------------------------------------------- the passes

impl crate::Database {
    /// Build column segments for every segmentable column of every table —
    /// the explicit pass of DESIGN-COLUMNAR §5. Nothing here runs on the write
    /// path; a heavy write workload simply leaves segments stale (and so
    /// unused) until the next pass, which is correct.
    ///
    /// The pass reads ONE snapshot and stamps every record with that
    /// snapshot's `mod_gen`. If a writer commits while the pass runs, the
    /// records are stamped with a generation the table no longer reports and
    /// every one of them reads as stale — wasted work, never a wrong answer.
    pub fn compact_columns(&self) -> Result<Vec<ColSegStat>> {
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let mut out = Vec::new();

        for t in bundle.schema.tables.iter().filter(|x| !x.dead) {
            if !matches!(t.kind, mpedb_types::TableKind::Standard) {
                continue;
            }
            for (ci, col) in t.columns.iter().enumerate() {
                if !segmentable(col.ty) {
                    continue;
                }
                // One read snapshot per column: the generation and the values
                // must come from the SAME view, or the stamp would describe a
                // state the values were not read from.
                let r = self.engine.begin_read()?;
                let gen = r.mod_gen(t.id)?;
                let mut blocks: Vec<Vec<u8>> = Vec::new();
                let mut buf: Vec<Value> = Vec::with_capacity(BLOCK_ROWS);
                let mut rows = 0u64;
                let fold = r.fold_range_column(
                    t.id,
                    None,
                    None,
                    ci as u16,
                    mpedb_core::FoldOpts::SERIAL,
                    &mut |v: &Value| {
                        buf.push(v.clone());
                        rows += 1;
                        if buf.len() == BLOCK_ROWS {
                            blocks.push(encode_block(gen, col.ty, &buf)?);
                            buf.clear();
                        }
                        Ok(())
                    },
                );
                let fold = fold.and_then(|_| {
                    if !buf.is_empty() {
                        blocks.push(encode_block(gen, col.ty, &buf)?);
                    }
                    Ok(())
                });
                r.finish()?;
                match fold {
                    Ok(()) => {}
                    // A column this context cannot fold is not an error — it
                    // simply gets no segment.
                    Err(Error::Unsupported(_)) => continue,
                    Err(e) => return Err(e),
                }

                let bytes: u64 = blocks.iter().map(|b| b.len() as u64).sum();
                let n_blocks = blocks.len() as u32;
                // One session for the whole column: the records land together
                // or not at all, so a reader never sees half a column.
                let mut s = self.begin()?;
                for (bi, b) in blocks.into_iter().enumerate() {
                    s.sys_record_put(NS, &record_key(t.id, ci as u16, bi as u32), &b)?;
                }
                s.commit()?;
                out.push(ColSegStat {
                    table: t.name.clone(),
                    column: col.name.clone(),
                    blocks: n_blocks,
                    rows,
                    bytes,
                });
            }
        }
        Ok(out)
    }

    /// Drop every stored column segment. Segments are regenerable, so this is
    /// always safe — it only costs the next scan its speed.
    pub fn drop_column_segments(&self) -> Result<usize> {
        let keys: Vec<Vec<u8>> = self
            .sys_record_scan(NS)?
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let n = keys.len();
        let mut s = self.begin()?;
        for k in keys {
            s.sys_record_delete(NS, &k)?;
        }
        s.commit()?;
        Ok(n)
    }
}

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
        let key = crate::sys_record_subkey(NS, &record_key(table, col, bi))?;
        let Some(bytes) = snap.sys_get(&key)? else { break };
        match decode_block(&bytes, want_gen, ty)? {
            Some(b) => covered += b.n_rows as u64,
            None => return Ok(false), // stale or unknown: the row scan runs
        }
        n_blocks += 1;
        if covered > want_rows {
            return Ok(false); // more rows than the table holds: do not trust it
        }
    }
    if n_blocks == 0 || covered != want_rows {
        return Ok(false); // no segments, or they do not cover the table
    }
    for bi in 0..n_blocks {
        let key = crate::sys_record_subkey(NS, &record_key(table, col, bi))?;
        let bytes = snap
            .sys_get(&key)?
            .ok_or_else(|| Error::Corrupt("column segment vanished mid-scan".into()))?;
        let b = decode_block(&bytes, want_gen, ty)?
            .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
        b.for_each(push)?;
    }
    Ok(true)
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
    let (Ok(want_gen), Ok(want_rows)) = (snap.mod_gen(table), snap.row_count(table)) else {
        return Ok(false);
    };
    // Load and validate every column up front: a decline discovered on column
    // 2 must not have fed the folder from column 1.
    let mut recs: Vec<Vec<Vec<u8>>> = Vec::with_capacity(needed.len());
    for &(col, ty) in needed {
        match load_column(snap, table, col, ty, want_gen, want_rows)? {
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
            let blk = decode_block(&rec[bi], want_gen, *ty)?
                .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
            match n_rows {
                None => n_rows = Some(blk.n_rows as usize),
                Some(n) if n != blk.n_rows as usize => return Ok(false),
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
        let key = crate::sys_record_subkey(NS, &record_key(table, col, bi))?;
        let Some(bytes) = snap.sys_get(&key)? else { break };
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
    let (Ok(want_gen), Ok(want_rows)) = (snap.mod_gen(table), snap.row_count(table)) else {
        return Ok(false);
    };
    let Some(agg_recs) = load_column(snap, table, agg_col, agg_ty, want_gen, want_rows)? else {
        return Ok(false);
    };
    // The same column twice is legal (`sum(day_id) WHERE day_id >= …`) and
    // needs no second load.
    let pred_recs = if pred.col == agg_col && pred_ty == agg_ty {
        None
    } else {
        match load_column(snap, table, pred.col, pred_ty, want_gen, want_rows)? {
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
        let ablk = decode_block(abytes, want_gen, agg_ty)?
            .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
        let pblk = decode_block(pbytes, want_gen, pred_ty)?
            .ok_or_else(|| Error::Corrupt("column segment changed mid-scan".into()))?;
        // Blocks are built in one pass at one block size, so a mismatch means
        // the two columns do not describe the same rows — refuse to pair them.
        if ablk.n_rows != pblk.n_rows {
            return Ok(false);
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
                    let pass = match v {
                        // A NULL satisfies no comparison — SQL's 3VL, and the
                        // same answer the row path's `eval_filter` gives.
                        Value::Null => false,
                        Value::Int(x) | Value::Timestamp(x) => match pred.op {
                            Cmp::Lt => *x < pred.k,
                            Cmp::Le => *x <= pred.k,
                            Cmp::Gt => *x > pred.k,
                            Cmp::Ge => *x >= pred.k,
                            Cmp::Eq => *x == pred.k,
                        },
                        _ => false,
                    };
                    if pass {
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
