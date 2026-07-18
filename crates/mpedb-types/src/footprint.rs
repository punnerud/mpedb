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

/// Table bitmaps are indexed by table id; `crate::MAX_TABLES` (128) bounds them
/// to u128. `indexes_used` is a separate bitmap over the *per-table* index
/// numbering (not table ids), which stays u64 (≤ 64 indexes per table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footprint {
    pub tables_read: u128,
    pub tables_written: u128,
    /// Indexes probed or maintained, as a bitmap over the per-table index
    /// numbering (bit 0 = PK tree).
    pub indexes_used: u64,
    pub key_access: KeyAccess,
    pub read_only: bool,
}

impl Footprint {
    pub fn reads_table(&self, table_id: u32) -> bool {
        self.tables_read & (1u128 << table_id) != 0
    }

    pub fn writes_table(&self, table_id: u32) -> bool {
        self.tables_written & (1u128 << table_id) != 0
    }

    /// Two footprints conflict if either writes a table the other touches.
    /// Key-level refinement happens at bind time for `Point` accesses.
    pub fn conflicts_with(&self, other: &Footprint) -> bool {
        self.tables_written & (other.tables_read | other.tables_written) != 0
            || other.tables_written & (self.tables_read | self.tables_written) != 0
    }

    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.tables_read.to_le_bytes());
        buf.extend_from_slice(&self.tables_written.to_le_bytes());
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
        let read_u128 = |pos: &mut usize| -> Result<u128> {
            let raw = buf.get(*pos..*pos + 16).ok_or_else(err)?;
            *pos += 16;
            Ok(u128::from_le_bytes(raw.try_into().unwrap()))
        };
        let tables_read = read_u128(pos)?;
        let tables_written = read_u128(pos)?;
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
        if read_only && tables_written != 0 {
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

    #[test]
    fn footprint_roundtrip() {
        let fps = vec![
            Footprint {
                tables_read: 0b101,
                tables_written: 0,
                indexes_used: 1,
                key_access: KeyAccess::Point(vec![KeyPart::Param(0), KeyPart::Const(3)]),
                read_only: true,
            },
            Footprint {
                tables_read: 0b1,
                tables_written: 0b1,
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
            Footprint {
                tables_read: u128::MAX,
                tables_written: 1u128 << 127,
                indexes_used: 0,
                key_access: KeyAccess::Full,
                read_only: false,
            },
            // A table id past the old u64 ceiling — the widen's reason to exist.
            Footprint {
                tables_read: 1u128 << 100,
                tables_written: 1u128 << 100,
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
            tables_read: 1,
            tables_written: 0,
            indexes_used: 1,
            key_access: KeyAccess::Full,
            read_only: true,
        };
        let write_t0 = Footprint {
            tables_read: 1,
            tables_written: 1,
            indexes_used: 1,
            key_access: KeyAccess::Full,
            read_only: false,
        };
        let write_t1 = Footprint {
            tables_read: 2,
            tables_written: 2,
            indexes_used: 1,
            key_access: KeyAccess::Full,
            read_only: false,
        };
        assert!(!read_t0.conflicts_with(&read_t0));
        assert!(read_t0.conflicts_with(&write_t0));
        assert!(!write_t0.conflicts_with(&write_t1));
    }

    #[test]
    fn high_table_ids_are_distinct() {
        // Tables 64 and 100 must NOT alias (they would under a u64 bitmap).
        let write_t64 = Footprint {
            tables_read: 1u128 << 64,
            tables_written: 1u128 << 64,
            indexes_used: 1,
            key_access: KeyAccess::Full,
            read_only: false,
        };
        let read_t100 = Footprint {
            tables_read: 1u128 << 100,
            tables_written: 0,
            indexes_used: 1,
            key_access: KeyAccess::Full,
            read_only: true,
        };
        assert!(write_t64.reads_table(64) && write_t64.writes_table(64));
        assert!(read_t100.reads_table(100));
        assert!(!write_t64.conflicts_with(&read_t100));
        assert!(write_t64.conflicts_with(&write_t64));
    }
}
