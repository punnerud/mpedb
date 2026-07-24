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

/// Encodings. Both are directly addressable by arithmetic — no decode pass, no
/// intermediate buffer, and (stage 2) no obstacle to skipping a block whole.
const ENC_FOR_BITPACK: u8 = 1; // integers: value − min, packed to the needed width
const ENC_RAW64: u8 = 2; // floats (and any block FOR cannot shrink): plain 8-byte values

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
    enc: u8,
    width: u32,
    zmin: u64,
    zmax: u64,
    nulls: &'a [u8],
    payload: &'a [u8],
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
        let lo = self.zmin as i64;
        let mask = if self.width == 0 {
            0
        } else {
            u64::MAX >> (64 - self.width)
        };
        let mut k = 0usize; // index among the NON-NULL values
        for i in 0..self.n_rows as usize {
            if self.nulls[i / 8] & (1 << (i % 8)) != 0 {
                f(&Value::Null)?;
                continue;
            }
            if k >= self.n_nonnull as usize {
                return Err(Error::Corrupt("column segment: null bitmap disagrees".into()));
            }
            let bits = match self.enc {
                ENC_RAW64 => {
                    let o = k * 8;
                    u64::from_le_bytes(self.payload[o..o + 8].try_into().unwrap())
                }
                _ => {
                    // Frame of reference: the value is base + a packed delta,
                    // read in place. No intermediate buffer, no allocation.
                    if self.width == 0 {
                        lo as u64
                    } else {
                        let bit = k * self.width as usize;
                        let byte = bit / 8;
                        let off = (bit % 8) as u32;
                        let end = (byte + 9).min(self.payload.len());
                        let mut acc: u128 = 0;
                        for (j, b) in self.payload[byte..end].iter().enumerate() {
                            acc |= (*b as u128) << (8 * j);
                        }
                        lo.wrapping_add((((acc >> off) as u64) & mask) as i64) as u64
                    }
                }
            };
            k += 1;
            let v = match self.ty {
                ColumnType::Float64 => Value::Float(f64::from_bits(bits)),
                ColumnType::Timestamp => Value::Timestamp(bits as i64),
                _ => Value::Int(bits as i64),
            };
            f(&v)?;
        }
        if k != self.n_nonnull as usize {
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
        ColumnType::Int64 | ColumnType::Float64 | ColumnType::Timestamp
    )
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
    let mut raw: Vec<u64> = Vec::with_capacity(n);
    for (i, v) in vals.iter().enumerate() {
        match v {
            Value::Null => nulls[i / 8] |= 1 << (i % 8),
            Value::Int(x) | Value::Timestamp(x) => raw.push(*x as u64),
            Value::Float(f) => raw.push(f.to_bits()),
            other => {
                return Err(Error::Internal(format!(
                    "column segment: unexpected value {}",
                    other.type_name()
                )))
            }
        }
    }

    // Zone map over the NON-NULL values. Stored for stage 2's block skipping;
    // stage 1 does not read it back, which is why it cannot be wrong yet.
    let (zmin, zmax) = match ty {
        ColumnType::Float64 => {
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for &b in &raw {
                let f = f64::from_bits(b);
                if f < lo {
                    lo = f;
                }
                if f > hi {
                    hi = f;
                }
            }
            (lo.to_bits(), hi.to_bits())
        }
        _ => {
            let mut lo = i64::MAX;
            let mut hi = i64::MIN;
            for &b in &raw {
                let x = b as i64;
                if x < lo {
                    lo = x;
                }
                if x > hi {
                    hi = x;
                }
            }
            (lo as u64, hi as u64)
        }
    };

    // Encoding, chosen from THIS block's own data: integers frame-of-reference
    // against the block minimum, floats raw (a FOR over bit patterns would be
    // arithmetic nonsense).
    let (enc, width, payload) = if ty == ColumnType::Float64 || raw.is_empty() {
        let mut p = Vec::with_capacity(raw.len() * 8);
        for &b in &raw {
            p.extend_from_slice(&b.to_le_bytes());
        }
        (ENC_RAW64, 0u32, p)
    } else {
        let lo = zmin as i64;
        let hi = zmax as i64;
        let range = (hi as i128 - lo as i128) as u128;
        if range > u64::MAX as u128 {
            let mut p = Vec::with_capacity(raw.len() * 8);
            for &b in &raw {
                p.extend_from_slice(&b.to_le_bytes());
            }
            (ENC_RAW64, 0u32, p)
        } else {
            let w = bits_for(range as u64);
            let mut p = Vec::with_capacity((raw.len() * w as usize).div_ceil(8));
            let deltas: Vec<u64> = raw.iter().map(|&b| (b as i64).wrapping_sub(lo) as u64).collect();
            pack_bits(&mut p, &deltas, w);
            (ENC_FOR_BITPACK, w, p)
        }
    };

    let mut out = Vec::with_capacity(40 + nulls.len() + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&mod_gen.to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.push(ty as u8);
    out.push(enc);
    out.push(width as u8);
    out.extend_from_slice(&zmin.to_le_bytes());
    out.extend_from_slice(&zmax.to_le_bytes());
    out.extend_from_slice(&nulls);
    out.extend_from_slice(&payload);
    Ok(out)
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
    let zmin = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap());
    let zmax = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap());
    let nulls_start = p;
    let nulls_len = n_rows.div_ceil(8);
    let nulls = take(&mut p, nulls_len)?;
    let _ = nulls_start;

    // Validate the payload's LENGTH here, so `Block::for_each` can index it by
    // arithmetic without a bounds check per value.
    let need = match enc {
        ENC_RAW64 => n_nonnull
            .checked_mul(8)
            .ok_or_else(|| Error::Corrupt("column segment: payload length overflow".into()))?,
        ENC_FOR_BITPACK => (n_nonnull * width as usize).div_ceil(8),
        _ => return Ok(None), // an encoding this build does not know
    };
    let payload = take(&mut p, need)?;
    if p != bytes.len() {
        return Err(Error::Corrupt("column segment: trailing bytes".into()));
    }
    // `for_each` reads up to 9 bytes at a time from the packed payload; the
    // read is clamped to the slice, so a short tail cannot index out of bounds.
    Ok(Some(Block {
        n_rows: n_rows as u32,
        n_nonnull: n_nonnull as u32,
        ty,
        enc,
        width,
        zmin,
        zmax,
        nulls,
        payload,
    }))
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
