//! mpedb-side mirror state: the `mir\0` sys-keyspace namespace codec
//! (DESIGN-MIRROR §2). Records commit atomically with row writes in one meta
//! flip. Every decoder is bounds-checked — corrupt bytes yield
//! [`Error::Corrupt`], never a panic (repo invariant).

use mpedb_types::{Error, Result};

/// Facade sys-record namespace for all mirror state (`mir\0<subkey>`).
pub const MIR_NS: &str = "mir";

// ---- fixed subkeys ----
/// Immutable-ish mirror configuration (source identity, mode, scope).
pub const KEY_CFG: &[u8] = b"cfg";
/// The authority state machine record.
pub const KEY_EPOCH: &[u8] = b"epoch";
/// Adapter-opaque pull cursor.
pub const KEY_CUR: &[u8] = b"cur";
/// Local echo of the source's applied high-water (status only).
pub const KEY_HW: &[u8] = b"hw";

// ---- keyed families ----
const KEY_MAP_PREFIX: &[u8] = b"map/";
const KEY_IMP_PREFIX: &[u8] = b"imp/";
const KEY_PARK_PREFIX: &[u8] = b"park/";
const KEY_SKIP_PREFIX: &[u8] = b"skip/";

/// `park/<table_id BE4><xxh3_128(pk keycode) BE16>` — a parked conflict, keyed
/// by PK (idempotent: a PK re-conflicting updates its record). Scan the whole
/// family over `[park/, park0)`.
pub fn park_key(table_id: u32, pk_keycode: &[u8]) -> Vec<u8> {
    keyed(KEY_PARK_PREFIX, table_id, pk_keycode)
}
pub const KEY_PARK_END: &[u8] = b"park0"; // '0' = byte after '/'

/// `skip/<table_id BE4><xxh3_128(pk keycode)>` — a manual-policy apply-skip
/// marker: while present, the pull applier leaves this PK at its local value.
pub fn skip_key(table_id: u32, pk_keycode: &[u8]) -> Vec<u8> {
    keyed(KEY_SKIP_PREFIX, table_id, pk_keycode)
}

fn keyed(prefix: &[u8], table_id: u32, pk_keycode: &[u8]) -> Vec<u8> {
    let hash = xxhash_rust::xxh3::xxh3_128(pk_keycode);
    let mut k = Vec::with_capacity(prefix.len() + 4 + 16);
    k.extend_from_slice(prefix);
    k.extend_from_slice(&table_id.to_be_bytes());
    k.extend_from_slice(&hash.to_be_bytes());
    k
}

/// Why a row was parked (DESIGN-MIRROR §8 conflict taxonomy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ConflictKind {
    /// A non-PK unique value is already held by a row outside the batch.
    UniqueBlocked = 1,
    /// A stricter mpedb CHECK / NOT NULL / type rule rejected the row.
    Validation = 2,
    /// A source value could not be mapped to mpedb (quarantine).
    TypeViolation = 3,
    /// The source rejected a pushed row.
    PushRejected = 4,
    /// Both sides changed the same PK since the last sync (divergence).
    Divergence = 5,
}

impl ConflictKind {
    fn from_tag(t: u8) -> Result<ConflictKind> {
        Ok(match t {
            1 => ConflictKind::UniqueBlocked,
            2 => ConflictKind::Validation,
            3 => ConflictKind::TypeViolation,
            4 => ConflictKind::PushRejected,
            5 => ConflictKind::Divergence,
            other => return Err(Error::Corrupt(format!("bad conflict kind {other}"))),
        })
    }
}

/// A parked-conflict record. PK-only for v1 (row images can be re-read); the
/// operator sees which PK conflicted, why, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkRecord {
    pub kind: ConflictKind,
    pub wall_us: i64,
    pub table_id: u32,
    pub pk_keycode: Vec<u8>,
}

impl ParkRecord {
    /// Layout: kind u8 ‖ wall_us i64 BE ‖ table_id u32 BE ‖ pk keycode.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(1 + 8 + 4 + self.pk_keycode.len());
        v.push(self.kind as u8);
        v.extend_from_slice(&self.wall_us.to_be_bytes());
        v.extend_from_slice(&self.table_id.to_be_bytes());
        v.extend_from_slice(&self.pk_keycode);
        v
    }
    pub fn decode(bytes: &[u8]) -> Result<ParkRecord> {
        if bytes.len() < 13 {
            return Err(Error::Corrupt(format!(
                "park record is {} bytes (need >= 13)",
                bytes.len()
            )));
        }
        Ok(ParkRecord {
            kind: ConflictKind::from_tag(bytes[0])?,
            wall_us: i64::from_be_bytes(bytes[1..9].try_into().unwrap()),
            table_id: u32::from_be_bytes(bytes[9..13].try_into().unwrap()),
            pk_keycode: bytes[13..].to_vec(),
        })
    }
}

/// `map/<table_id BE4>` — per-table source mapping (added in M2.2).
pub fn map_key(table_id: u32) -> Vec<u8> {
    prefixed(KEY_MAP_PREFIX, table_id)
}

/// `imp/<table_id BE4>` — import resume watermark (last imported PK keycode).
pub fn imp_key(table_id: u32) -> Vec<u8> {
    prefixed(KEY_IMP_PREFIX, table_id)
}

fn prefixed(prefix: &[u8], table_id: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + 4);
    k.extend_from_slice(prefix);
    k.extend_from_slice(&table_id.to_be_bytes());
    k
}

/// The raw engine sys subkey for a `mir` record: `mir\0<key>`. Matches the
/// facade sys-record convention ([`mpedb`] `WriteSession::sys_record_put`), so
/// records written through the facade are read back with this key against a
/// config-free [`mpedb_core::Engine`] read txn.
pub fn sys_subkey(key: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(MIR_NS.len() + 1 + key.len());
    k.extend_from_slice(MIR_NS.as_bytes());
    k.push(0);
    k.extend_from_slice(key);
    k
}

/// Which external engine a mirror's source is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SourceKind {
    Sqlite = 1,
    Postgres = 2,
}

impl SourceKind {
    fn from_tag(t: u8) -> Result<SourceKind> {
        match t {
            1 => Ok(SourceKind::Sqlite),
            2 => Ok(SourceKind::Postgres),
            other => Err(Error::Corrupt(format!("bad mirror source_kind {other}"))),
        }
    }
}

/// How the source's changes are captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureMode {
    /// Trigger-maintained changelog on the source (primary).
    Tracked = 1,
    /// No source modification; full-table checksum merge-diff.
    NoTouch = 2,
}

impl CaptureMode {
    fn from_tag(t: u8) -> Result<CaptureMode> {
        match t {
            1 => Ok(CaptureMode::Tracked),
            2 => Ok(CaptureMode::NoTouch),
            other => Err(Error::Corrupt(format!("bad mirror capture mode {other}"))),
        }
    }
}

/// Which side is authoritative (default conflict winner + legal switch arrow).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Authority {
    Source = 0,
    Mpedb = 1,
}

impl Authority {
    fn from_tag(t: u8) -> Result<Authority> {
        match t {
            0 => Ok(Authority::Source),
            1 => Ok(Authority::Mpedb),
            other => Err(Error::Corrupt(format!("bad mirror authority {other}"))),
        }
    }
}

/// Position in the authority state machine (DESIGN-MIRROR §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MirrorState {
    Importing = 1,
    SrcAuth = 2,
    DrainToMpedb = 3,
    CutoverToMpedb = 4,
    MAuth = 5,
    DrainToSrc = 6,
    CutoverToSrc = 7,
    Halted = 8,
}

impl MirrorState {
    fn from_tag(t: u8) -> Result<MirrorState> {
        Ok(match t {
            1 => MirrorState::Importing,
            2 => MirrorState::SrcAuth,
            3 => MirrorState::DrainToMpedb,
            4 => MirrorState::CutoverToMpedb,
            5 => MirrorState::MAuth,
            6 => MirrorState::DrainToSrc,
            7 => MirrorState::CutoverToSrc,
            8 => MirrorState::Halted,
            other => return Err(Error::Corrupt(format!("bad mirror state {other}"))),
        })
    }
}

/// The `mir\0epoch` record: the fenced authority state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Epoch {
    pub epoch: u64,
    pub authority: Authority,
    pub state: MirrorState,
    pub frozen: bool,
}

impl Epoch {
    /// Layout: epoch u64 BE ‖ authority u8 ‖ state u8 ‖ frozen u8.
    pub const ENCODED_LEN: usize = 11;

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut b = [0u8; Self::ENCODED_LEN];
        b[0..8].copy_from_slice(&self.epoch.to_be_bytes());
        b[8] = self.authority as u8;
        b[9] = self.state as u8;
        b[10] = self.frozen as u8;
        b
    }

    pub fn decode(bytes: &[u8]) -> Result<Epoch> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(Error::Corrupt(format!(
                "mirror epoch record is {} bytes (expected {})",
                bytes.len(),
                Self::ENCODED_LEN
            )));
        }
        Ok(Epoch {
            epoch: u64::from_be_bytes(bytes[0..8].try_into().unwrap()),
            authority: Authority::from_tag(bytes[8])?,
            state: MirrorState::from_tag(bytes[9])?,
            frozen: match bytes[10] {
                0 => false,
                1 => true,
                other => return Err(Error::Corrupt(format!("bad mirror frozen flag {other}"))),
            },
        })
    }
}

/// The `mir\0cfg` record: source identity, capture mode, and mirrored-table
/// scope. `mirror_id` is a 128-bit content id derived from the canonical source
/// identity plus an init nonce — it carries no secret (the DSN lives in a 0600
/// config file, §12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorConfig {
    pub mirror_id: [u8; 16],
    pub source_kind: SourceKind,
    pub mode: CaptureMode,
    /// Versions the checksum canonicalization; a bump forces full re-verify.
    pub canonicalization_id: u32,
    /// Included (mirrored) table ids, ascending.
    pub scope: Vec<u32>,
}

impl MirrorConfig {
    /// Layout: mirror_id[16] ‖ source_kind u8 ‖ mode u8 ‖ canon u32 BE ‖
    /// scope_len u16 BE ‖ scope[table_id u32 BE]…
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(16 + 1 + 1 + 4 + 2 + self.scope.len() * 4);
        v.extend_from_slice(&self.mirror_id);
        v.push(self.source_kind as u8);
        v.push(self.mode as u8);
        v.extend_from_slice(&self.canonicalization_id.to_be_bytes());
        v.extend_from_slice(&(self.scope.len() as u16).to_be_bytes());
        for &t in &self.scope {
            v.extend_from_slice(&t.to_be_bytes());
        }
        v
    }

    pub fn decode(bytes: &[u8]) -> Result<MirrorConfig> {
        // fixed header = 16 + 1 + 1 + 4 + 2 = 24 bytes
        if bytes.len() < 24 {
            return Err(Error::Corrupt(format!(
                "mirror cfg record is {} bytes (need >= 24)",
                bytes.len()
            )));
        }
        let mut mirror_id = [0u8; 16];
        mirror_id.copy_from_slice(&bytes[0..16]);
        let source_kind = SourceKind::from_tag(bytes[16])?;
        let mode = CaptureMode::from_tag(bytes[17])?;
        let canonicalization_id = u32::from_be_bytes(bytes[18..22].try_into().unwrap());
        let scope_len = u16::from_be_bytes(bytes[22..24].try_into().unwrap()) as usize;
        let want = 24 + scope_len * 4;
        if bytes.len() != want {
            return Err(Error::Corrupt(format!(
                "mirror cfg scope: expected {want} bytes for {scope_len} tables, got {}",
                bytes.len()
            )));
        }
        let mut scope = Vec::with_capacity(scope_len);
        for i in 0..scope_len {
            let off = 24 + i * 4;
            scope.push(u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap()));
        }
        Ok(MirrorConfig {
            mirror_id,
            source_kind,
            mode,
            canonicalization_id,
            scope,
        })
    }
}

// ---- type provenance: what the SOURCE said, and what we did to it (§2 `map/`)

/// **How a source column was mapped into mpedb** — the field that separates
/// "this can go home again" from "the information is already gone".
///
/// Recording the source *type* alone is a trap: it produces a correct-looking
/// schema over silently wrong data. If PostgreSQL had `numeric(30,10)` and the
/// import chose `Float64`, the digits were discarded at import; remembering the
/// column was numeric does not bring them back. Export would faithfully recreate
/// `numeric(30,10)` and fill it with rounded values, which is worse than an
/// error because it looks right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MapPolicy {
    /// Byte-exact in both directions (`int8`→Int64, `text`→Text, `bytea`→Blob,
    /// `timestamptz`→Timestamp µs). Round-trips unconditionally.
    Exact = 1,
    /// Preserved through a canonical text form (`numeric`→Text, `jsonb`→Text,
    /// `uuid`→Text). Lossless, but the target column must be the same type for
    /// the text to mean the same thing.
    ViaText = 2,
    /// mpedb's type is WIDER than the source's (`int2`/`int4`→Int64,
    /// `varchar(n)`→Text). Import was lossless; a LOCAL WRITE can now hold a
    /// value the source column cannot take — which is exactly the case that
    /// fails at the target's INSERT, and exactly what a pre-flight must catch.
    Widened = 3,
    /// **Information was discarded at import** (`numeric`→Float64 by policy).
    /// The type is recoverable; the values are not.
    LossyAtImport = 4,
}

impl MapPolicy {
    fn from_tag(t: u8) -> Result<MapPolicy> {
        Ok(match t {
            1 => MapPolicy::Exact,
            2 => MapPolicy::ViaText,
            3 => MapPolicy::Widened,
            4 => MapPolicy::LossyAtImport,
            other => return Err(Error::Corrupt(format!("bad map policy {other}"))),
        })
    }

    /// Whether a value in this column can be handed back to a column of the
    /// recorded source type unchanged.
    pub fn round_trips(self) -> bool {
        !matches!(self, MapPolicy::LossyAtImport)
    }
}

/// One column's provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMap {
    /// The column's name AT THE SOURCE (mpedb may have mangled it — identifiers
    /// are ASCII-restricted, so a quoted/unicode PG name does not survive).
    pub source_name: String,
    /// The source's declared type **including typmod** — `numeric(10,2)`,
    /// `varchar(64)`, `jsonb`, `INTEGER`. This is the string export recreates;
    /// dropping the typmod would silently widen every constrained column.
    pub source_type: String,
    pub not_null: bool,
    /// Mirror the value, never write it back (PG `GENERATED … STORED`, sqlite
    /// STORED/VIRTUAL).
    pub generated: bool,
    /// PG `GENERATED … AS IDENTITY` — needs `OVERRIDING SYSTEM VALUE` on write.
    pub identity: bool,
    pub unique: bool,
    /// The mpedb column type it became.
    pub mapped: mpedb_types::ColumnType,
    pub policy: MapPolicy,
}

/// One mirrored table's provenance: enough to recreate the source's schema and
/// to validate mpedb's data against it BEFORE writing (DESIGN-MIRROR §2 `map/`).
///
/// Without this the `.mpedb` file has no memory of where its data came from: the
/// adapters re-introspect the LIVE source on every attach, which is fine while
/// the source is there (mirroring) and useless once it is not (migration) — the
/// file would only know `Text` and `Blob`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableMap {
    pub source_name: String,
    pub columns: Vec<ColumnMap>,
}

fn put_str(v: &mut Vec<u8>, s: &str) {
    v.extend_from_slice(&(s.len() as u16).to_be_bytes());
    v.extend_from_slice(s.as_bytes());
}

fn take_str(b: &[u8], pos: &mut usize) -> Result<String> {
    let n = take_u16(b, pos)? as usize;
    let end = pos
        .checked_add(n)
        .filter(|&e| e <= b.len())
        .ok_or_else(|| Error::Corrupt("truncated map string".into()))?;
    let s = std::str::from_utf8(&b[*pos..end])
        .map_err(|_| Error::Corrupt("map string is not utf-8".into()))?
        .to_string();
    *pos = end;
    Ok(s)
}

fn take_u16(b: &[u8], pos: &mut usize) -> Result<u16> {
    let end = pos
        .checked_add(2)
        .filter(|&e| e <= b.len())
        .ok_or_else(|| Error::Corrupt("truncated map u16".into()))?;
    let v = u16::from_be_bytes(b[*pos..end].try_into().unwrap());
    *pos = end;
    Ok(v)
}

fn take_u8(b: &[u8], pos: &mut usize) -> Result<u8> {
    let v = *b
        .get(*pos)
        .ok_or_else(|| Error::Corrupt("truncated map byte".into()))?;
    *pos += 1;
    Ok(v)
}

impl TableMap {
    /// Layout: source_name ‖ ncols u16 BE ‖ per column {source_name,
    /// source_type, flags u8, mapped u8, policy u8}. Strings are u16-len
    /// prefixed. Every read is bounds-checked (repo invariant).
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(64 + self.columns.len() * 32);
        put_str(&mut v, &self.source_name);
        v.extend_from_slice(&(self.columns.len() as u16).to_be_bytes());
        for c in &self.columns {
            put_str(&mut v, &c.source_name);
            put_str(&mut v, &c.source_type);
            let flags = u8::from(c.not_null)
                | (u8::from(c.generated) << 1)
                | (u8::from(c.identity) << 2)
                | (u8::from(c.unique) << 3);
            v.push(flags);
            v.push(c.mapped as u8);
            v.push(c.policy as u8);
        }
        v
    }

    pub fn decode(b: &[u8]) -> Result<TableMap> {
        let mut pos = 0usize;
        let source_name = take_str(b, &mut pos)?;
        let ncols = take_u16(b, &mut pos)? as usize;
        // Do NOT pre-allocate on a claimed length: a corrupt ncols must run out
        // of buffer, not out of memory.
        let mut columns = Vec::new();
        for _ in 0..ncols {
            let source_name = take_str(b, &mut pos)?;
            let source_type = take_str(b, &mut pos)?;
            let flags = take_u8(b, &mut pos)?;
            let mapped = mpedb_types::ColumnType::from_tag(take_u8(b, &mut pos)?)
                .ok_or_else(|| Error::Corrupt("bad mapped column type in map record".into()))?;
            let policy = MapPolicy::from_tag(take_u8(b, &mut pos)?)?;
            columns.push(ColumnMap {
                source_name,
                source_type,
                not_null: flags & 1 != 0,
                generated: flags & 2 != 0,
                identity: flags & 4 != 0,
                unique: flags & 8 != 0,
                mapped,
                policy,
            });
        }
        if pos != b.len() {
            return Err(Error::Corrupt(format!(
                "map record has {} trailing bytes",
                b.len() - pos
            )));
        }
        Ok(TableMap {
            source_name,
            columns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_distinct_and_prefixed() {
        assert_eq!(map_key(7), b"map/\x00\x00\x00\x07");
        assert_eq!(imp_key(0x01020304), b"imp/\x01\x02\x03\x04");
        assert_ne!(map_key(1), imp_key(1));
        assert_ne!(map_key(1), map_key(2));
    }

    #[test]
    fn epoch_roundtrip_and_truncation() {
        let e = Epoch {
            epoch: 0x0102_0304_0506_0708,
            authority: Authority::Mpedb,
            state: MirrorState::DrainToSrc,
            frozen: true,
        };
        assert_eq!(Epoch::decode(&e.encode()).unwrap(), e);
        let bytes = e.encode();
        for n in 0..bytes.len() {
            assert!(Epoch::decode(&bytes[..n]).is_err(), "len {n} must be Corrupt");
        }
        assert!(Epoch::decode(&[0u8; Epoch::ENCODED_LEN + 1]).is_err());
        // bad tags
        let mut bad = bytes;
        bad[8] = 9; // authority
        assert!(Epoch::decode(&bad).is_err());
        let mut bad = e.encode();
        bad[10] = 2; // frozen must be 0/1
        assert!(Epoch::decode(&bad).is_err());
    }

    #[test]
    fn park_record_and_keys() {
        let p = ParkRecord {
            kind: ConflictKind::UniqueBlocked,
            wall_us: -7,
            table_id: 3,
            pk_keycode: vec![9, 8, 0, 255],
        };
        assert_eq!(ParkRecord::decode(&p.encode()).unwrap(), p);
        let bytes = p.encode();
        for n in 0..13 {
            assert!(ParkRecord::decode(&bytes[..n]).is_err(), "len {n}");
        }
        let mut bad = bytes.clone();
        bad[0] = 9;
        assert!(ParkRecord::decode(&bad).is_err());
        // keys are fixed-size, family-scannable, PK-idempotent
        let k = park_key(3, b"A");
        assert_eq!(k.len(), KEY_PARK_PREFIX.len() + 4 + 16);
        assert_eq!(park_key(3, b"A"), park_key(3, b"A"));
        assert_ne!(park_key(3, b"A"), park_key(3, b"B"));
        assert_ne!(park_key(3, b"A"), skip_key(3, b"A"));
        assert!(KEY_PARK_END > &k[..KEY_PARK_END.len()]);
    }

    #[test]
    fn config_roundtrip_scope_and_truncation() {
        let c = MirrorConfig {
            mirror_id: [0xAB; 16],
            source_kind: SourceKind::Postgres,
            mode: CaptureMode::NoTouch,
            canonicalization_id: 3,
            scope: vec![0, 5, 55],
        };
        assert_eq!(MirrorConfig::decode(&c.encode()).unwrap(), c);
        // empty scope
        let mut c0 = c.clone();
        c0.scope.clear();
        assert_eq!(MirrorConfig::decode(&c0.encode()).unwrap(), c0);
        // truncation below the header and mid-scope
        let bytes = c.encode();
        for n in 0..bytes.len() {
            assert!(
                MirrorConfig::decode(&bytes[..n]).is_err(),
                "len {n} must be Corrupt"
            );
        }
        // trailing garbage past the declared scope is rejected
        let mut extra = c.encode();
        extra.push(0);
        assert!(MirrorConfig::decode(&extra).is_err());
        // bad enum tags
        let mut bad = c.encode();
        bad[16] = 9;
        assert!(MirrorConfig::decode(&bad).is_err());
    }

    // ---- type provenance (§2 `map/`) ----

    fn sample_map() -> TableMap {
        TableMap {
            source_name: "orders".into(),
            columns: vec![
                ColumnMap {
                    source_name: "id".into(),
                    source_type: "int8".into(),
                    not_null: true,
                    generated: false,
                    identity: true,
                    unique: false,
                    mapped: mpedb_types::ColumnType::Int64,
                    policy: MapPolicy::Exact,
                },
                ColumnMap {
                    source_name: "amount".into(),
                    // the typmod is the whole point: without it export would
                    // recreate a bare `numeric` and silently widen the column
                    source_type: "numeric(10,2)".into(),
                    not_null: false,
                    generated: false,
                    identity: false,
                    unique: false,
                    mapped: mpedb_types::ColumnType::Text,
                    policy: MapPolicy::ViaText,
                },
                ColumnMap {
                    source_name: "qty".into(),
                    source_type: "int4".into(),
                    not_null: false,
                    generated: false,
                    identity: false,
                    unique: false,
                    mapped: mpedb_types::ColumnType::Int64,
                    policy: MapPolicy::Widened,
                },
            ],
        }
    }

    #[test]
    fn table_map_roundtrips() {
        let m = sample_map();
        assert_eq!(TableMap::decode(&m.encode()).unwrap(), m);
    }

    /// Repo invariant: a decoder must yield Corrupt, never panic — at ANY cut.
    #[test]
    fn table_map_truncation_at_every_offset_is_corrupt_not_panic() {
        let buf = sample_map().encode();
        for cut in 0..buf.len() {
            match TableMap::decode(&buf[..cut]) {
                Err(Error::Corrupt(_)) => {}
                other => panic!("cut {cut}: expected Corrupt, got {other:?}"),
            }
        }
        // trailing garbage is rejected too (a short ncols must not leave slack)
        let mut extra = buf.clone();
        extra.push(0);
        assert!(matches!(TableMap::decode(&extra), Err(Error::Corrupt(_))));
    }

    /// A lying length must run out of buffer, not out of memory.
    #[test]
    fn table_map_absurd_column_count_does_not_allocate() {
        let mut b = Vec::new();
        b.extend_from_slice(&2u16.to_be_bytes());
        b.extend_from_slice(b"t1");
        b.extend_from_slice(&u16::MAX.to_be_bytes()); // 65535 columns, no data
        assert!(matches!(TableMap::decode(&b), Err(Error::Corrupt(_))));
    }

    #[test]
    fn map_policy_tags_roundtrip_and_reject_garbage() {
        for p in [MapPolicy::Exact, MapPolicy::ViaText, MapPolicy::Widened, MapPolicy::LossyAtImport] {
            assert_eq!(MapPolicy::from_tag(p as u8).unwrap(), p);
        }
        assert!(MapPolicy::from_tag(0).is_err());
        assert!(MapPolicy::from_tag(99).is_err());
    }

    /// The distinction the record exists for: only LossyAtImport cannot go home.
    #[test]
    fn only_lossy_at_import_fails_to_round_trip() {
        assert!(MapPolicy::Exact.round_trips());
        assert!(MapPolicy::ViaText.round_trips());
        assert!(MapPolicy::Widened.round_trips());
        assert!(!MapPolicy::LossyAtImport.round_trips());
    }
}
