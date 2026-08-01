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

mod passes;
mod scan;
mod zone;

pub use zone::{zone_predicate, zone_verdict, Cmp, Verdict, ZonePred};
pub(crate) use scan::{
    feed_filtered_from_segments, feed_from_segments, feed_group_from_segments,
    sum_f64_whole_table,
};

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

    /// Add every value of a null-free `Float64` `Raw64` block to `acc`, IN THE
    /// SAME ORDER `for_each` would, and return the count — the vectorized `sum`
    /// path (DESIGN-COLUMNAR §7.2). It stays bit-identical to the row scan
    /// because it accumulates into the caller's single running total per value
    /// in sequence (float addition is not associative, so order is the
    /// contract), and it is fast because it never materializes a `Value` or
    /// crosses a `dyn` closure — an f64 read and add in a tight loop LLVM keeps
    /// scalar (it may not reorder float adds) but strips of all the per-row tax.
    ///
    /// `None` = not specializable (has NULLs, or an encoding other than raw
    /// f64) → the caller uses the generic per-value fold. Timestamps/integers
    /// are out: their `sum` needs the i128 overflow-exact monoid, not an f64.
    pub fn add_f64_into(&self, acc: &mut f64) -> Option<u64> {
        if self.ty != ColumnType::Float64 || !self.null_free() {
            return None;
        }
        let n = self.n_nonnull as usize;
        match &self.codec {
            // Raw f64s: one contiguous scan.
            Codec::Raw64(p) => {
                let bytes = p.get(..n * 8)?;
                for chunk in bytes.chunks_exact(8) {
                    *acc += f64::from_le_bytes(chunk.try_into().unwrap());
                }
            }
            // Low-cardinality (the common price/measure column): decode the
            // dictionary to f64 ONCE, then add `dict[code_k]` in k-order — the
            // same order `for_each` visits, so the sum is bit-identical, minus
            // the per-row `Value` box and closure. `codes` is width-bit-packed.
            Codec::DictNum { dict, width, codes } => {
                let vals: Vec<f64> = dict.iter().map(|&b| f64::from_bits(b)).collect();
                for k in 0..n {
                    let c = packed_at(codes, k, *width) as usize;
                    *acc += *vals.get(c)?;
                }
            }
            // Sparse: one default plus an ascending exception list, added in
            // k-order so the float rounding matches `for_each` exactly.
            Codec::RunDefault { default, exc } => {
                let d = f64::from_bits(*default);
                let mut ei = 0usize;
                for k in 0..n {
                    if ei < exc.len() && exc[ei].0 as usize == k {
                        *acc += f64::from_bits(exc[ei].1);
                        ei += 1;
                    } else {
                        *acc += d;
                    }
                }
            }
            // FOR/bitpack is integer-only; text codecs never hold a float.
            _ => return None,
        }
        Some(n as u64)
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

/// What one `sync_columnar` pass decided (DESIGN-COLUMNAR §2).
#[derive(Debug, Clone, Default)]
pub struct ColumnarSync {
    /// `(table, columns built)` — the scan-heavy tables that got segments.
    pub columnarized: Vec<(String, u32)>,
    /// Tables the model marks point-oriented, whose segments were dropped.
    pub dropped: Vec<String>,
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

/// What the stage-B live check found a table needs. `Fresh` tables are omitted
/// from a plan, so this only ever names a table with work to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintainAction {
    /// Eligible for segments but has none live — build.
    Build,
    /// Eligible and segments are live, but `tail` rows have accreted above the
    /// stage-5 watermark (which covers `covered`) past the rebuild fraction —
    /// rebuild to absorb them.
    Rebuild { covered: u64, tail: u64 },
    /// The model no longer wants segments here, but a watermark is still live —
    /// drop them.
    Drop,
    /// Nothing to do (never stored in a plan; the default when nothing matches).
    Fresh,
}

/// One table's maintenance verdict from the live check.
#[derive(Debug, Clone)]
pub struct ColumnarMaintenance {
    pub table: String,
    pub action: MaintainAction,
}

/// The model's columnar-eligibility rule, shared by `sync_columnar` and the
/// maintenance pass: a `fact` is columnar, a `dimension` is not, and anything
/// unroled follows the archetype (a `star-olap` database columnarizes by
/// default). One place so the two passes can never disagree on what is eligible.
fn columnar_eligible(
    model: &mpedb_types::model::WorkloadModel,
    scan_archetype: bool,
    table_name: &str,
) -> bool {
    let role = model
        .tables
        .iter()
        .find(|m| mpedb_types::ident_eq(&m.name, table_name))
        .and_then(|m| m.role);
    match role {
        Some(mpedb_types::model::TableRole::Fact) => true,
        Some(mpedb_types::model::TableRole::Dimension) => false,
        _ => scan_archetype,
    }
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

/// Read block `bi`'s bytes via its CONTIGUOUS extent record. The block's sys
/// record IS an [`ExtentRef`](mpedb_core) catalog cell (DESIGN-COLUMNAR §7.3):
/// its bytes live in the extent, read straight from the mapping, never as a
/// 512 KiB overflow chain in the catalog tree. `Ok(None)` when the record is
/// absent (the column ends there).
///
/// It copies the run out and then REVALIDATES the pin, exactly as
/// [`ReadTxn::blob_read`] does per chunk: `extent_slice` borrows the mapping,
/// and while this reader stays pinned the freed-extent MVCC rule keeps the run
/// un-reused — but the moment live-pin eviction (max pin age) lands, a writer
/// could reuse the pages mid-`memcpy`. A torn image that still passed the
/// block's MAGIC+format+`mod_gen` would be a WRONG aggregate, not a decline, so
/// the `still_pinned` recheck turns any such eviction into `SnapshotEvicted`
/// instead. Today no live pin is ever evicted, so it never fires — it is the
/// assertion that keeps that invariant honest.
fn block_bytes(
    snap: &mpedb_core::engine::ReadTxn<'_>,
    table: u32,
    col: u16,
    bi: u32,
) -> Result<Option<Vec<u8>>> {
    let key = crate::sys_record_subkey(NS, &record_key(table, col, bi))?;
    let Some((start_page, len)) = snap.sys_extent_ref(&key)? else {
        return Ok(None);
    };
    let bytes = snap.extent_slice(start_page, len)?.to_vec();
    if !snap.still_pinned() {
        return Err(Error::SnapshotEvicted);
    }
    Ok(Some(bytes))
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

