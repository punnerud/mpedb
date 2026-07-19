//! Generic change-data-capture primitive (DESIGN-MIRROR §2, §3). mpedb-core
//! owns this and knows **nothing about mirroring**: it records which tables have
//! dirty-set capture enabled and which are write-blocked (frozen), and defines
//! the on-disk encoding of a dirty entry. The mirror layer (and any future CDC
//! consumer) sets the control record; the engine reads it at write-txn begin to
//! decide whether to capture writes or refuse them.
//!
//! Storage lives in the sys-keyspace under the `cdc\0` namespace, prefix-
//! disjoint from `plan/`, `pol/`, `mir\0`, etc.:
//! - `cdc\0tabs` → [`CaptureConfig`] (captured + write-blocked bitmaps + gen)
//! - `cdc\0d/` ‖ table_id BE4 ‖ xxh3_128(pk keycode) BE16 → [`DirtyEntry`]
//!
//! The dirty **key** is fixed-size (a 128-bit content hash of the PK keycode)
//! so an arbitrarily long Text/Blob/composite PK can never overflow the btree
//! key limit at the first replicated write; the authoritative PK keycode is
//! carried in the **value** (push needs it to re-read the row). Coalescing is by
//! construction: a second touch of the same PK hashes to the same key and
//! upserts the entry.

use mpedb_types::{Error, Result, TableSet};

/// Sys subkey of the CDC control record.
pub const CDC_TABS_KEY: &[u8] = b"cdc\0tabs";
/// Sys subkey prefix of a CDC dirty entry (followed by table_id BE4 ‖ hash).
pub const CDC_DIRTY_PREFIX: &[u8] = b"cdc\0d/";
/// Exclusive upper bound for a `sys_scan_range` over the whole dirty family:
/// `/` is 0x2f, so 0x30 is the first subkey past every `cdc\0d/…` entry.
pub const CDC_DIRTY_PREFIX_END: &[u8] = b"cdc\0d0";

/// The CDC control record (`cdc\0tabs`): which tables are captured, and which
/// are write-blocked.
///
/// Membership used to be a `u128` bitmap indexed by table id, set through
/// `1u128 << (table_id & (MAX_TABLES - 1))`. That fold was guarded only by a
/// `debug_assert!`, so in release it silently ALIASED any id past the bitmap:
/// enabling capture on table 200 would have set table 72's bit, and the mirror
/// would then replicate the wrong table's rows. Unlike the OFP ring's `& 63`
/// (a conservative conflict signal where aliasing costs a false positive), this
/// is an IDENTITY map — aliasing is a silent cross-table wrong answer. Both
/// sets are therefore sparse [`TableSet`]s, which alias with nothing and put no
/// ceiling on the table id at all (design/DESIGN-TABLE-CAP.md §4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Table ids with dirty-set capture enabled.
    pub captured: TableSet,
    /// Table ids whose writes are refused with [`Error::Frozen`].
    pub blocked: TableSet,
    /// Bumped on every change so per-process caches can detect staleness.
    pub generation: u64,
}

impl CaptureConfig {
    #[inline]
    pub fn is_captured(&self, table_id: u32) -> bool {
        self.captured.contains(table_id)
    }

    #[inline]
    pub fn is_blocked(&self, table_id: u32) -> bool {
        self.blocked.contains(table_id)
    }

    /// Whether anything at all is enabled — the engine's cheap "skip capture"
    /// fast path when no table is mirrored.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.captured.is_empty() && self.blocked.is_empty()
    }

    pub fn set_captured(&mut self, table_id: u32, on: bool) {
        if on {
            self.captured.insert(table_id);
        } else {
            self.captured.remove(table_id);
        }
    }

    pub fn set_blocked(&mut self, table_id: u32, on: bool) {
        if on {
            self.blocked.insert(table_id);
        } else {
            self.blocked.remove(table_id);
        }
    }

    /// `generation LE8 ‖ TableSet(captured) ‖ TableSet(blocked)` — variable
    /// length now that the sets are sparse.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(8 + 4 + 8 * (self.captured.len() + self.blocked.len()));
        b.extend_from_slice(&self.generation.to_le_bytes());
        self.captured.encode_into(&mut b);
        self.blocked.encode_into(&mut b);
        b
    }

    pub fn decode(bytes: &[u8]) -> Result<CaptureConfig> {
        let generation = bytes
            .get(0..8)
            .map(|r| u64::from_le_bytes(r.try_into().unwrap()))
            .ok_or_else(|| Error::Corrupt("truncated cdc control record".into()))?;
        let mut pos = 8usize;
        let captured = TableSet::decode(bytes, &mut pos)?;
        let blocked = TableSet::decode(bytes, &mut pos)?;
        // Exact consumption: trailing bytes mean this is not a control record
        // of this format (e.g. a stale fixed-40-byte one), and must fail loudly
        // rather than decode to a plausible-looking capture set.
        if pos != bytes.len() {
            return Err(Error::Corrupt(format!(
                "cdc control record has {} trailing bytes",
                bytes.len() - pos
            )));
        }
        Ok(CaptureConfig {
            captured,
            blocked,
            generation,
        })
    }
}

/// The kind of change a dirty entry records. State-based, so intermediate ops
/// coalesce: the entry always reflects the PK's latest op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DirtyOp {
    Upsert = 1,
    Delete = 2,
}

impl DirtyOp {
    fn from_tag(tag: u8) -> Result<DirtyOp> {
        match tag {
            1 => Ok(DirtyOp::Upsert),
            2 => Ok(DirtyOp::Delete),
            other => Err(Error::Corrupt(format!("bad cdc dirty op {other}"))),
        }
    }
}

/// Build the fixed-size dirty-entry key for a captured mutation.
pub fn dirty_key(table_id: u32, pk_keycode: &[u8]) -> Vec<u8> {
    let hash = xxhash_rust::xxh3::xxh3_128(pk_keycode);
    let mut k = Vec::with_capacity(CDC_DIRTY_PREFIX.len() + 4 + 16);
    k.extend_from_slice(CDC_DIRTY_PREFIX);
    k.extend_from_slice(&table_id.to_be_bytes());
    k.extend_from_slice(&hash.to_be_bytes());
    k
}

/// A CDC dirty entry: what changed about one PK, plus the authoritative PK
/// keycode (carried here, not in the key, so an unbounded PK never overflows the
/// btree key limit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirtyEntry {
    pub op: DirtyOp,
    /// The committing txn id (`meta.txn_id + 1`) — the push high-water compare.
    pub last_txn: u64,
    /// Wall-clock micros at capture (best-effort; conflict newest-wins only).
    pub wall_us: i64,
    /// The full PK keycode (memcmp-ordered).
    pub pk_keycode: Vec<u8>,
}

impl DirtyEntry {
    /// Value layout: op u8 ‖ last_txn u64 BE ‖ wall_us i64 BE ‖ pk keycode.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(1 + 8 + 8 + self.pk_keycode.len());
        v.push(self.op as u8);
        v.extend_from_slice(&self.last_txn.to_be_bytes());
        v.extend_from_slice(&self.wall_us.to_be_bytes());
        v.extend_from_slice(&self.pk_keycode);
        v
    }

    pub fn decode(bytes: &[u8]) -> Result<DirtyEntry> {
        if bytes.len() < 17 {
            return Err(Error::Corrupt(format!(
                "cdc dirty entry is {} bytes (need >= 17)",
                bytes.len()
            )));
        }
        Ok(DirtyEntry {
            op: DirtyOp::from_tag(bytes[0])?,
            last_txn: u64::from_be_bytes(bytes[1..9].try_into().unwrap()),
            wall_us: i64::from_be_bytes(bytes[9..17].try_into().unwrap()),
            pk_keycode: bytes[17..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_config_roundtrip_and_membership() {
        let mut c = CaptureConfig::default();
        assert!(c.is_empty());
        c.set_captured(3, true);
        c.set_captured(55, true);
        c.set_blocked(3, true);
        c.generation = 7;
        assert!(c.is_captured(3) && c.is_captured(55) && !c.is_captured(4));
        assert!(c.is_blocked(3) && !c.is_blocked(55));
        assert!(!c.is_empty());
        // ids never enabled never match, at any magnitude
        assert!(!c.is_captured(64) && !c.is_blocked(200) && !c.is_captured(4095));
        let round = CaptureConfig::decode(&c.encode()).unwrap();
        assert_eq!(round, c);
        c.set_captured(3, false);
        c.set_captured(3, false); // idempotent
        assert!(!c.is_captured(3));
    }

    #[test]
    fn high_table_ids_do_not_alias() {
        // REGRESSION (DESIGN-TABLE-CAP §4): the old bitmap folded ids
        // `& (MAX_TABLES - 1)` behind a debug_assert, so capturing table 200
        // silently captured table 72 in release builds — the mirror would then
        // replicate the wrong table's rows. Sparse ids alias with nothing.
        let mut c = CaptureConfig::default();
        for id in [200u32, 128, 64, 3000] {
            c.set_captured(id, true);
            c.set_blocked(id, true);
        }
        for id in [200u32, 128, 64, 3000] {
            assert!(c.is_captured(id) && c.is_blocked(id));
            for fold in [id % 64, id % 128] {
                if fold != id {
                    assert!(!c.is_captured(fold), "{id} aliased {fold}");
                    assert!(!c.is_blocked(fold), "{id} aliased {fold}");
                }
            }
        }
        assert_eq!(CaptureConfig::decode(&c.encode()).unwrap(), c);
    }

    #[test]
    fn capture_config_rejects_malformed() {
        // truncation at every offset must yield Corrupt, never a panic
        let full = CaptureConfig {
            captured: [0u32, 4095].into_iter().collect(),
            blocked: [2u32].into_iter().collect(),
            generation: 3,
        }
        .encode();
        for n in 0..full.len() {
            assert!(CaptureConfig::decode(&full[..n]).is_err(), "len {n}");
        }
        // trailing bytes (e.g. the retired fixed-40-byte record) fail loudly
        let mut long = full.clone();
        long.push(0);
        assert!(CaptureConfig::decode(&long).is_err());
        assert!(CaptureConfig::decode(&[0u8; 40]).is_err());
        assert!(CaptureConfig::decode(&full).is_ok());
    }

    #[test]
    fn dirty_entry_roundtrip_and_truncation() {
        let e = DirtyEntry {
            op: DirtyOp::Delete,
            last_txn: 0x0102_0304_0506_0708,
            wall_us: -42,
            pk_keycode: vec![9, 8, 7, 0, 255],
        };
        let bytes = e.encode();
        assert_eq!(DirtyEntry::decode(&bytes).unwrap(), e);
        // empty keycode is legal (17-byte minimum)
        let mut e0 = e.clone();
        e0.pk_keycode.clear();
        assert_eq!(DirtyEntry::decode(&e0.encode()).unwrap(), e0);
        // truncation below the fixed header is Corrupt at every offset
        for n in 0..17 {
            assert!(DirtyEntry::decode(&bytes[..n]).is_err());
        }
        // bad op tag
        let mut bad = bytes.clone();
        bad[0] = 9;
        assert!(DirtyEntry::decode(&bad).is_err());
    }

    #[test]
    fn dirty_key_is_fixed_size_and_coalesces() {
        let short = dirty_key(5, b"A");
        let long = dirty_key(5, &vec![0xABu8; 4000]); // PK far past MAX_KEY
        assert_eq!(short.len(), CDC_DIRTY_PREFIX.len() + 4 + 16);
        assert_eq!(short.len(), long.len(), "key size independent of PK length");
        // same (table, pk) → same key (coalescing); different table → different
        assert_eq!(dirty_key(5, b"A"), dirty_key(5, b"A"));
        assert_ne!(dirty_key(5, b"A"), dirty_key(6, b"A"));
        assert_ne!(dirty_key(5, b"A"), dirty_key(5, b"B"));
        // the family scan bound really is exclusive-above every d/ key
        assert!(CDC_DIRTY_PREFIX_END > &short[..CDC_DIRTY_PREFIX_END.len()]);
    }
}
