//! Precomputed plan footprints: which tables/keys a compiled plan will touch,
//! known *before* execution. Read-only plans route past all write
//! coordination; write plans expose their write set for batch scheduling and
//! (Phase 2) conflict grouping — the queue of prepared requests doubles as an
//! index over imminent data access.

use crate::error::{Error, Result};
use std::fmt;

/// Content hash of a compiled plan: blake3(canonical plan bytes ‖ schema hash
/// ‖ format version). Identifies a plan across all attached processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlanHash(pub [u8; 32]);

impl fmt::Display for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for PlanHash {
    type Err = Error;
    fn from_str(s: &str) -> Result<PlanHash> {
        let s = s.trim();
        if s.len() != 64 || !s.is_ascii() {
            return Err(Error::Config("plan hash must be 64 hex chars".into()));
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hex = std::str::from_utf8(chunk).unwrap();
            out[i] = u8::from_str_radix(hex, 16)
                .map_err(|_| Error::Config("invalid hex in plan hash".into()))?;
        }
        Ok(PlanHash(out))
    }
}

/// One component of a primary-key value, resolved when parameters are bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPart {
    /// Statement parameter index ($1 = 0).
    Param(u16),
    /// Index into the plan's constant pool.
    Const(u16),
    /// Slot in the ACCUMULATED OUTER tuple of a join — the index nested-loop
    /// parametrization (`ON inner.col = outer.col` pushed into the inner
    /// fetch). Only legal inside a `Join`'s access path, where the outer row
    /// exists; a statement-level access path carrying one is corrupt.
    OuterCol(u16),
}

/// A composite bound for a range access over the primary key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBound {
    /// Values for a prefix of the PK columns.
    pub parts: Vec<KeyPart>,
    pub inclusive: bool,
}

/// How a plan touches a table's keyspace. `Point`/`Range` are exact — the
/// affected keys are computable from (plan, params) alone without executing.
/// Plans whose predicates do not pin the primary key degrade honestly to
/// `Full` (table-level footprint) rather than overclaiming precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAccess {
    /// Exactly one PK: one part per PK column.
    Point(Vec<KeyPart>),
    Range {
        lo: Option<KeyBound>,
        hi: Option<KeyBound>,
    },
    Full,
}

/// A SET of table ids, held **strictly ascending** (sorted, no duplicates).
///
/// This is the sparse replacement for the old `u128` per-table bitmap
/// (design/DESIGN-TABLE-CAP.md). A bitmap's size is the id SPACE; a `TableSet`'s
/// size is what a plan actually touches — which is 1 for the overwhelming
/// majority of plans, and ≤ 5 for every plan in the corpus. Because plans are
/// persisted in the catalog and content-hashed, both properties matter:
///
/// - **cost**: encoded as `u16 count ‖ count × u32 LE`, so a one-table read set
///   is 6 bytes where two `u128`s cost a flat 32 — and the representation
///   imposes NO ceiling on the table id, which is the whole point.
/// - **canonicity**: strictly ascending means one set has exactly one encoding,
///   so the plan hash is stable, and [`TableSet::decode`] *enforces* it — a
///   non-ascending or duplicated list is `Corrupt`, never a silent alias.
///
/// The ascending invariant is maintained by construction ([`TableSet::insert`],
/// [`TableSet::union_with`]) and re-checked on decode; nothing else may build
/// one from a raw vec except [`TableSet::from_sorted`], which validates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableSet(Vec<u32>);

impl TableSet {
    pub const fn new() -> TableSet {
        TableSet(Vec::new())
    }

    /// Build from an already-sorted, duplicate-free list of ids, validating
    /// both properties (and the `< MAX_TABLES` range). Used by decode and by
    /// tests; every other construction path goes through `insert`.
    pub fn from_sorted(ids: Vec<u32>) -> Result<TableSet> {
        for (i, &id) in ids.iter().enumerate() {
            if id as usize >= crate::MAX_TABLES {
                return Err(Error::Corrupt(format!("table id {id} out of range")));
            }
            if i > 0 && id <= ids[i - 1] {
                return Err(Error::Corrupt(
                    "table set ids must be strictly ascending".into(),
                ));
            }
        }
        Ok(TableSet(ids))
    }

    /// Add `id`, keeping the vec strictly ascending. Idempotent.
    ///
    /// Every production construction path range-checks upstream — the planner's
    /// `checked_table` rejects an id that names no table, and CDC ids come from
    /// a validated `Schema` where `validate` enforces `id == position` and
    /// `len() <= MAX_TABLES`. The `debug_assert` states that contract. In
    /// release an out-of-range id is still **fail-closed, never aliased**: it
    /// goes in as itself, and the next `decode` rejects the record with
    /// `Corrupt`. That is the whole point of dropping the bitmap — the old
    /// `1u128 << (id & (MAX_TABLES - 1))` folded instead, and a fold is a
    /// silently WRONG table (cdc.rs, DESIGN-TABLE-CAP §4).
    pub fn insert(&mut self, id: u32) {
        debug_assert!(
            (id as usize) < crate::MAX_TABLES,
            "table id {id} >= MAX_TABLES"
        );
        if let Err(pos) = self.0.binary_search(&id) {
            self.0.insert(pos, id);
        }
    }

    pub fn remove(&mut self, id: u32) {
        if let Ok(pos) = self.0.binary_search(&id) {
            self.0.remove(pos);
        }
    }

    pub fn contains(&self, id: u32) -> bool {
        self.0.binary_search(&id).is_ok()
    }

    pub fn union_with(&mut self, other: &TableSet) {
        for &id in &other.0 {
            self.insert(id);
        }
    }

    /// Do the two sets share any table? Sorted-merge, no allocation.
    pub fn intersects(&self, other: &TableSet) -> bool {
        let (mut i, mut j) = (0, 0);
        while i < self.0.len() && j < other.0.len() {
            match self.0[i].cmp(&other.0[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The lowest id in the set, if any.
    pub fn first(&self) -> Option<u32> {
        self.0.first().copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().copied()
    }

    pub fn as_slice(&self) -> &[u32] {
        &self.0
    }

    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        // `len() ≤ MAX_TABLES` by the ascending + in-range invariant (see
        // `insert`), and MAX_TABLES is far below u16::MAX, so the count can
        // never truncate.
        debug_assert!(self.0.len() <= crate::MAX_TABLES);
        buf.extend_from_slice(&(self.0.len() as u16).to_le_bytes());
        for &id in &self.0 {
            buf.extend_from_slice(&id.to_le_bytes());
        }
    }

    pub fn decode(buf: &[u8], pos: &mut usize) -> Result<TableSet> {
        let err = || Error::Corrupt("truncated table set".into());
        let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
        *pos += 2;
        let n = u16::from_le_bytes(raw.try_into().unwrap()) as usize;
        if n > crate::MAX_TABLES {
            return Err(Error::Corrupt("too many tables in table set".into()));
        }
        let mut ids = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let raw = buf.get(*pos..*pos + 4).ok_or_else(err)?;
            *pos += 4;
            ids.push(u32::from_le_bytes(raw.try_into().unwrap()));
        }
        TableSet::from_sorted(ids)
    }
}

impl FromIterator<u32> for TableSet {
    fn from_iter<I: IntoIterator<Item = u32>>(iter: I) -> TableSet {
        let mut s = TableSet::new();
        for id in iter {
            s.insert(id);
        }
        s
    }
}

impl fmt::Display for TableSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (i, id) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{id}")?;
        }
        f.write_str("]")
    }
}

/// Table sets are SPARSE (see [`TableSet`]) — the table id space imposes no
/// footprint cost and `crate::MAX_TABLES` bounds it only as a resource policy.
/// `indexes_used` is a different thing entirely: a bitmap over the *per-table*
/// index numbering (not table ids), which stays u64 (≤ 64 indexes per table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footprint {
    pub tables_read: TableSet,
    pub tables_written: TableSet,
    /// Indexes probed or maintained, as a bitmap over the per-table index
    /// numbering (bit 0 = PK tree).
    pub indexes_used: u64,
    pub key_access: KeyAccess,
    pub read_only: bool,
}

impl Footprint {
    pub fn reads_table(&self, table_id: u32) -> bool {
        self.tables_read.contains(table_id)
    }

    pub fn writes_table(&self, table_id: u32) -> bool {
        self.tables_written.contains(table_id)
    }

    /// Two footprints conflict if either writes a table the other touches.
    /// Key-level refinement happens at bind time for `Point` accesses.
    pub fn conflicts_with(&self, other: &Footprint) -> bool {
        self.tables_written.intersects(&other.tables_read)
            || self.tables_written.intersects(&other.tables_written)
            || other.tables_written.intersects(&self.tables_read)
    }

    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        self.tables_read.encode_into(buf);
        self.tables_written.encode_into(buf);
        buf.extend_from_slice(&self.indexes_used.to_le_bytes());
        buf.push(self.read_only as u8);
        match &self.key_access {
            KeyAccess::Full => buf.push(0),
            KeyAccess::Point(parts) => {
                buf.push(1);
                encode_parts(buf, parts);
            }
            KeyAccess::Range { lo, hi } => {
                buf.push(2);
                for bound in [lo, hi] {
                    match bound {
                        None => buf.push(0),
                        Some(b) => {
                            buf.push(1 | ((b.inclusive as u8) << 1));
                            encode_parts(buf, &b.parts);
                        }
                    }
                }
            }
        }
    }

    pub fn decode(buf: &[u8], pos: &mut usize) -> Result<Footprint> {
        let err = || Error::Corrupt("truncated footprint".into());
        let read_u64 = |pos: &mut usize| -> Result<u64> {
            let raw = buf.get(*pos..*pos + 8).ok_or_else(err)?;
            *pos += 8;
            Ok(u64::from_le_bytes(raw.try_into().unwrap()))
        };
        let tables_read = TableSet::decode(buf, pos)?;
        let tables_written = TableSet::decode(buf, pos)?;
        let indexes_used = read_u64(pos)?;
        let read_only = match *buf.get(*pos).ok_or_else(err)? {
            0 => false,
            1 => true,
            _ => return Err(Error::Corrupt("bad read_only flag".into())),
        };
        *pos += 1;
        let key_access = match *buf.get(*pos).ok_or_else(err)? {
            0 => {
                *pos += 1;
                KeyAccess::Full
            }
            1 => {
                *pos += 1;
                KeyAccess::Point(decode_parts(buf, pos)?)
            }
            2 => {
                *pos += 1;
                let mut bounds = [None, None];
                for b in &mut bounds {
                    let tag = *buf.get(*pos).ok_or_else(err)?;
                    *pos += 1;
                    *b = match tag & 1 {
                        0 => None,
                        _ => Some(KeyBound {
                            inclusive: tag & 2 != 0,
                            parts: decode_parts(buf, pos)?,
                        }),
                    };
                }
                let [lo, hi] = bounds;
                KeyAccess::Range { lo, hi }
            }
            t => return Err(Error::Corrupt(format!("bad key access tag {t}"))),
        };
        if read_only && !tables_written.is_empty() {
            return Err(Error::Corrupt("read-only footprint with write set".into()));
        }
        Ok(Footprint {
            tables_read,
            tables_written,
            indexes_used,
            key_access,
            read_only,
        })
    }
}

fn encode_parts(buf: &mut Vec<u8>, parts: &[KeyPart]) {
    buf.extend_from_slice(&(parts.len() as u16).to_le_bytes());
    for p in parts {
        match p {
            KeyPart::Param(i) => {
                buf.push(0);
                buf.extend_from_slice(&i.to_le_bytes());
            }
            KeyPart::Const(i) => {
                buf.push(1);
                buf.extend_from_slice(&i.to_le_bytes());
            }
            // Never present in practice: a join degrades key_access to Full,
            // and OuterCol exists only inside join access paths. Encoded
            // anyway so the match is total; decode's recompute-and-compare
            // guard rejects any footprint that claims otherwise.
            KeyPart::OuterCol(i) => {
                buf.push(2);
                buf.extend_from_slice(&i.to_le_bytes());
            }
        }
    }
}

fn decode_parts(buf: &[u8], pos: &mut usize) -> Result<Vec<KeyPart>> {
    let err = || Error::Corrupt("truncated footprint".into());
    let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
    *pos += 2;
    let n = u16::from_le_bytes(raw.try_into().unwrap()) as usize;
    if n > crate::MAX_COLUMNS {
        return Err(Error::Corrupt("too many key parts".into()));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let tag = *buf.get(*pos).ok_or_else(err)?;
        *pos += 1;
        let raw = buf.get(*pos..*pos + 2).ok_or_else(err)?;
        *pos += 2;
        let i = u16::from_le_bytes(raw.try_into().unwrap());
        out.push(match tag {
            0 => KeyPart::Param(i),
            1 => KeyPart::Const(i),
            2 => KeyPart::OuterCol(i),
            _ => return Err(Error::Corrupt("bad key part tag".into())),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_hex_roundtrip() {
        let h = PlanHash([7u8; 32]);
        let s = h.to_string();
        assert_eq!(s.parse::<PlanHash>().unwrap(), h);
        assert!("xyz".parse::<PlanHash>().is_err());
    }

    fn ts(ids: &[u32]) -> TableSet {
        ids.iter().copied().collect()
    }

    #[test]
    fn table_set_is_a_sorted_set() {
        // Insertion order does not matter; duplicates collapse; the vec is
        // strictly ascending — the canonicity the plan hash depends on.
        let a = ts(&[7, 3, 7, 1000, 0]);
        assert_eq!(a.as_slice(), &[0, 3, 7, 1000]);
        assert_eq!(a.len(), 4);
        assert!(a.contains(1000) && a.contains(0) && !a.contains(1));
        let mut b = ts(&[3, 9]);
        b.union_with(&a);
        assert_eq!(b.as_slice(), &[0, 3, 7, 9, 1000]);
        b.remove(9);
        b.remove(9); // idempotent
        assert_eq!(b.as_slice(), &[0, 3, 7, 1000]);
        assert!(a.intersects(&ts(&[1000])));
        assert!(!a.intersects(&ts(&[1, 2, 4, 999, 1001])));
        assert!(!a.intersects(&TableSet::new()));
        assert!(TableSet::new().is_empty() && TableSet::new().first().is_none());
        assert_eq!(a.first(), Some(0));
        assert_eq!(a.to_string(), "[0,3,7,1000]");
    }

    #[test]
    fn table_set_decode_rejects_non_canonical() {
        // Hand-built: count 2, ids [5, 5] — a duplicate is not strictly ascending.
        let mut buf = 2u16.to_le_bytes().to_vec();
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(&5u32.to_le_bytes());
        assert!(TableSet::decode(&buf, &mut 0).is_err());
        // Descending.
        let mut buf = 2u16.to_le_bytes().to_vec();
        buf.extend_from_slice(&9u32.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes());
        assert!(TableSet::decode(&buf, &mut 0).is_err());
        // An id at or past MAX_TABLES.
        let mut buf = 1u16.to_le_bytes().to_vec();
        buf.extend_from_slice(&(crate::MAX_TABLES as u32).to_le_bytes());
        assert!(TableSet::decode(&buf, &mut 0).is_err());
        // A count past MAX_TABLES must be rejected before any id is read, so a
        // corrupt length can never drive a large speculative allocation.
        let buf = u16::MAX.to_le_bytes().to_vec();
        assert!(matches!(
            TableSet::decode(&buf, &mut 0),
            Err(Error::Corrupt(_))
        ));
        // An out-of-range id built on the WRITE side is fail-closed, not
        // aliased: it survives encode as itself and decode refuses the record.
        // (Debug builds trip `insert`'s assert first, so construct directly.)
        let mut buf = 1u16.to_le_bytes().to_vec();
        buf.extend_from_slice(&99_999u32.to_le_bytes());
        match TableSet::decode(&buf, &mut 0) {
            Err(Error::Corrupt(m)) => assert!(m.contains("out of range"), "{m}"),
            other => panic!("expected out-of-range rejection, got {other:?}"),
        }
        // Truncation at every offset of a well-formed set: Corrupt, never panic.
        let good = ts(&[1, 4095]);
        let mut buf = Vec::new();
        good.encode_into(&mut buf);
        for cut in 0..buf.len() {
            assert!(TableSet::decode(&buf[..cut], &mut 0).is_err());
        }
        assert_eq!(TableSet::decode(&buf, &mut 0).unwrap(), good);
    }

    #[test]
    fn footprint_roundtrip() {
        let fps = vec![
            Footprint {
                tables_read: ts(&[0, 2]),
                tables_written: TableSet::new(),
                indexes_used: 1,
                key_access: KeyAccess::Point(vec![KeyPart::Param(0), KeyPart::Const(3)]),
                read_only: true,
            },
            Footprint {
                tables_read: ts(&[0]),
                tables_written: ts(&[0]),
                indexes_used: 0b11,
                key_access: KeyAccess::Range {
                    lo: Some(KeyBound {
                        parts: vec![KeyPart::Param(1)],
                        inclusive: true,
                    }),
                    hi: None,
                },
                read_only: false,
            },
            // Wide set spanning both retired ceilings (64 and 128) and reaching
            // the current one — the sparse form's reason to exist.
            Footprint {
                tables_read: (0..crate::MAX_TABLES as u32).step_by(7).collect(),
                tables_written: ts(&[(crate::MAX_TABLES - 1) as u32]),
                indexes_used: 0,
                key_access: KeyAccess::Full,
                read_only: false,
            },
            // Ids past BOTH old bitmap ceilings.
            Footprint {
                tables_read: ts(&[63, 64, 127, 128, 1000]),
                tables_written: ts(&[100, 4095]),
                indexes_used: 0b11,
                key_access: KeyAccess::Full,
                read_only: false,
            },
        ];
        for fp in &fps {
            let mut buf = Vec::new();
            fp.encode_into(&mut buf);
            let mut pos = 0;
            assert_eq!(&Footprint::decode(&buf, &mut pos).unwrap(), fp);
            assert_eq!(pos, buf.len());
            for cut in 0..buf.len() {
                let _ = Footprint::decode(&buf[..cut], &mut 0);
            }
        }
    }

    #[test]
    fn conflict_semantics() {
        let read_t0 = Footprint {
            tables_read: ts(&[0]),
            tables_written: TableSet::new(),
            indexes_used: 1,
            key_access: KeyAccess::Full,
            read_only: true,
        };
        let write_t0 = Footprint {
            tables_read: ts(&[0]),
            tables_written: ts(&[0]),
            indexes_used: 1,
            key_access: KeyAccess::Full,
            read_only: false,
        };
        let write_t1 = Footprint {
            tables_read: ts(&[1]),
            tables_written: ts(&[1]),
            indexes_used: 1,
            key_access: KeyAccess::Full,
            read_only: false,
        };
        assert!(!read_t0.conflicts_with(&read_t0));
        assert!(read_t0.conflicts_with(&write_t0));
        assert!(write_t0.conflicts_with(&read_t0));
        assert!(!write_t0.conflicts_with(&write_t1));
    }

    #[test]
    fn high_table_ids_are_distinct() {
        // These pairs aliased under a u64 bitmap (0/64), under a u128 bitmap
        // (0/128) and under any mod-N fold. Sparse ids alias with nothing.
        for (a, b) in [(0u32, 64u32), (0, 128), (7, 135), (64, 4032)] {
            let write_a = Footprint {
                tables_read: ts(&[a]),
                tables_written: ts(&[a]),
                indexes_used: 1,
                key_access: KeyAccess::Full,
                read_only: false,
            };
            let read_b = Footprint {
                tables_read: ts(&[b]),
                tables_written: TableSet::new(),
                indexes_used: 1,
                key_access: KeyAccess::Full,
                read_only: true,
            };
            assert!(write_a.reads_table(a) && write_a.writes_table(a));
            assert!(!write_a.reads_table(b) && !write_a.writes_table(b));
            assert!(read_b.reads_table(b) && !read_b.reads_table(a));
            assert!(!write_a.conflicts_with(&read_b), "{a} aliased {b}");
            assert!(write_a.conflicts_with(&write_a));
        }
    }
}
