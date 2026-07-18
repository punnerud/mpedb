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

use mpedb_types::{Error, Result, MAX_TABLES};

/// Sys subkey of the CDC control record.
pub const CDC_TABS_KEY: &[u8] = b"cdc\0tabs";
/// Sys subkey prefix of a CDC dirty entry (followed by table_id BE4 ‖ hash).
pub const CDC_DIRTY_PREFIX: &[u8] = b"cdc\0d/";
/// Exclusive upper bound for a `sys_scan_range` over the whole dirty family:
/// `/` is 0x2f, so 0x30 is the first subkey past every `cdc\0d/…` entry.
pub const CDC_DIRTY_PREFIX_END: &[u8] = b"cdc\0d0";

/// The CDC control record (`cdc\0tabs`). Table ids are 0..[`MAX_TABLES`](mpedb_types::MAX_TABLES)
/// (< 128), so membership is a `u128` bitmap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Bit `i` set → table id `i` has dirty-set capture enabled.
    pub captured: u128,
    /// Bit `i` set → writes to table id `i` are refused with [`Error::Frozen`].
    pub blocked: u128,
    /// Bumped on every change so per-process caches can detect staleness.
    pub generation: u64,
}

impl CaptureConfig {
    pub const ENCODED_LEN: usize = 40;

    #[inline]
    pub fn is_captured(&self, table_id: u32) -> bool {
        (table_id as usize) < MAX_TABLES && (self.captured >> table_id) & 1 == 1
    }

    #[inline]
    pub fn is_blocked(&self, table_id: u32) -> bool {
        (table_id as usize) < MAX_TABLES && (self.blocked >> table_id) & 1 == 1
    }

    /// Whether anything at all is enabled — the engine's cheap "skip capture"
    /// fast path when no table is mirrored.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.captured == 0 && self.blocked == 0
    }

    pub fn set_captured(&mut self, table_id: u32, on: bool) {
        debug_assert!((table_id as usize) < MAX_TABLES);
        let bit = 1u128 << (table_id & (MAX_TABLES as u32 - 1));
        if on {
            self.captured |= bit;
        } else {
            self.captured &= !bit;
        }
    }

    pub fn set_blocked(&mut self, table_id: u32, on: bool) {
        debug_assert!((table_id as usize) < MAX_TABLES);
        let bit = 1u128 << (table_id & (MAX_TABLES as u32 - 1));
        if on {
            self.blocked |= bit;
        } else {
            self.blocked &= !bit;
        }
    }

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut b = [0u8; Self::ENCODED_LEN];
        b[0..16].copy_from_slice(&self.captured.to_le_bytes());
        b[16..32].copy_from_slice(&self.blocked.to_le_bytes());
        b[32..40].copy_from_slice(&self.generation.to_le_bytes());
        b
    }

    pub fn decode(bytes: &[u8]) -> Result<CaptureConfig> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(Error::Corrupt(format!(
                "cdc control record is {} bytes (expected {})",
                bytes.len(),
                Self::ENCODED_LEN
            )));
        }
        Ok(CaptureConfig {
            captured: u128::from_le_bytes(bytes[0..16].try_into().unwrap()),
            blocked: u128::from_le_bytes(bytes[16..32].try_into().unwrap()),
            generation: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
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
    fn capture_config_roundtrip_and_bitmaps() {
        let mut c = CaptureConfig::default();
        assert!(c.is_empty());
        c.set_captured(3, true);
        c.set_captured(55, true);
        c.set_blocked(3, true);
        c.generation = 7;
        assert!(c.is_captured(3) && c.is_captured(55) && !c.is_captured(4));
        assert!(c.is_blocked(3) && !c.is_blocked(55));
        assert!(!c.is_empty());
        // out-of-range ids never match
        assert!(!c.is_captured(64) && !c.is_blocked(200));
        let round = CaptureConfig::decode(&c.encode()).unwrap();
        assert_eq!(round, c);
        c.set_captured(3, false);
        assert!(!c.is_captured(3));
    }

    #[test]
    fn capture_config_rejects_wrong_length() {
        // truncation at every offset must yield Corrupt, never a panic
        let full = CaptureConfig {
            captured: 1,
            blocked: 2,
            generation: 3,
        }
        .encode();
        for n in 0..full.len() {
            assert!(CaptureConfig::decode(&full[..n]).is_err());
        }
        assert!(CaptureConfig::decode(&[0u8; CaptureConfig::ENCODED_LEN + 1]).is_err());
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
