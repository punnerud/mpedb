//! Shared plan registry (DESIGN.md §7.2): plans live as system records inside
//! the database so ANY attached process can `execute(hash, params)` without
//! parsing SQL.
//!
//! Record layout under subkey `b"plan/" ++ hash bytes (32)`, all little-endian:
//!
//! ```text
//! u32 sql_len ‖ sql ‖ u32 blob_len ‖ blob ‖ u64 last_used_txn
//! ```
//!
//! where `blob = CompiledPlan::encode()`.
//!
//! Trust model (reviewed invariant): these records live in shared memory and
//! must be treated as hostile. A loaded blob is only accepted after
//! `CompiledPlan::decode` fully re-validates it against the schema (including
//! footprint recomputation) AND the decoded plan's recomputed content hash
//! equals the requested hash. Any failure degrades to `UnknownPlan` — the
//! caller re-prepares from SQL, which overwrites the bad entry.

use mpedb_core::WriteTxn;
use mpedb_sql::CompiledPlan;
use mpedb_types::{Error, PlanHash, Result, Schema};

pub(crate) const PLAN_PREFIX: &[u8] = b"plan/";

/// Registry capacity: at most this many plans are kept in the database.
pub(crate) const MAX_REGISTRY_PLANS: usize = 4096;

/// How many of the oldest plans (by `last_used_txn`) one insert evicts when
/// the registry is full.
pub(crate) const EVICT_BATCH: usize = 256;

pub(crate) fn plan_subkey(hash: &PlanHash) -> Vec<u8> {
    let mut k = Vec::with_capacity(PLAN_PREFIX.len() + 32);
    k.extend_from_slice(PLAN_PREFIX);
    k.extend_from_slice(&hash.0);
    k
}

pub(crate) struct Record<'a> {
    /// Original SQL text, kept for tooling (`mpedb-cli` registry listing) and
    /// as the re-prepare fallback documented in DESIGN.md §7.2.
    #[allow(dead_code)]
    pub sql: &'a str,
    pub blob: &'a [u8],
    pub last_used_txn: u64,
}

pub(crate) fn encode_record(sql: &str, blob: &[u8], last_used_txn: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + sql.len() + 4 + blob.len() + 8);
    out.extend_from_slice(&(sql.len() as u32).to_le_bytes());
    out.extend_from_slice(sql.as_bytes());
    out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    out.extend_from_slice(blob);
    out.extend_from_slice(&last_used_txn.to_le_bytes());
    out
}

/// Bounds-checked parse of a registry record. `None` = malformed (shared
/// memory is untrusted; a malformed record is simply not a plan).
pub(crate) fn parse_record(bytes: &[u8]) -> Option<Record<'_>> {
    fn take<'a>(b: &'a [u8], pos: &mut usize, n: usize) -> Option<&'a [u8]> {
        let end = pos.checked_add(n).filter(|&e| e <= b.len())?;
        let s = &b[*pos..end];
        *pos = end;
        Some(s)
    }
    let mut pos = 0usize;
    let sql_len = u32::from_le_bytes(take(bytes, &mut pos, 4)?.try_into().ok()?) as usize;
    let sql = std::str::from_utf8(take(bytes, &mut pos, sql_len)?).ok()?;
    let blob_len = u32::from_le_bytes(take(bytes, &mut pos, 4)?.try_into().ok()?) as usize;
    let blob = take(bytes, &mut pos, blob_len)?;
    let last_used_txn = u64::from_le_bytes(take(bytes, &mut pos, 8)?.try_into().ok()?);
    if pos != bytes.len() {
        return None;
    }
    Some(Record {
        sql,
        blob,
        last_used_txn,
    })
}

/// Copy of `bytes` with `last_used_txn` (the trailing u64) replaced, or `None`
/// if the record does not parse.
pub(crate) fn patched_last_used(bytes: &[u8], last_used_txn: u64) -> Option<Vec<u8>> {
    parse_record(bytes)?;
    let mut out = bytes.to_vec();
    let n = out.len();
    out[n - 8..].copy_from_slice(&last_used_txn.to_le_bytes());
    Some(out)
}

/// Decode and validate a registry record into a plan (the load path).
///
/// Reviewed invariants:
/// - `CompiledPlan::decode` fully re-validates the blob against the schema
///   and recomputes the footprint (never trusts stored flags).
/// - The decoded plan's recomputed hash must equal the requested hash.
/// - `PlanInvalidated` (schema changed) propagates so the caller re-prepares;
///   every other failure is reported as `UnknownPlan` — nothing is deleted,
///   a later `prepare` overwrites the entry.
pub(crate) fn decode_registry_plan(
    record_bytes: &[u8],
    hash: &PlanHash,
    schema: &Schema,
) -> Result<CompiledPlan> {
    let Some(rec) = parse_record(record_bytes) else {
        return Err(Error::UnknownPlan(*hash));
    };
    let plan = match CompiledPlan::decode(rec.blob, schema) {
        Ok(p) => p,
        Err(Error::PlanInvalidated) => return Err(Error::PlanInvalidated),
        Err(_) => return Err(Error::UnknownPlan(*hash)),
    };
    if plan.hash() != *hash {
        return Err(Error::UnknownPlan(*hash));
    }
    Ok(plan)
}

/// Insert (or overwrite) a plan record inside an open write transaction.
/// Returns whether anything was written (false = an identical record is
/// already published, so the caller can abort instead of committing).
pub(crate) fn insert_plan(
    txn: &mut WriteTxn<'_>,
    hash: &PlanHash,
    sql: &str,
    blob: &[u8],
) -> Result<bool> {
    let subkey = plan_subkey(hash);
    let existing = txn.sys_get(&subkey)?;
    if let Some(cur) = &existing {
        if parse_record(cur).is_some_and(|r| r.blob == blob) {
            return Ok(false); // already published with identical content
        }
    }
    if existing.is_none() {
        evict_if_full(txn)?;
    }
    let record = encode_record(sql, blob, txn.meta.txn_id + 1);
    txn.sys_put(&subkey, &record)?;
    Ok(true)
}

/// Registry hygiene: when the registry holds `MAX_REGISTRY_PLANS` or more
/// entries, drop the `EVICT_BATCH` oldest by `last_used_txn` (malformed
/// records sort first).
///
/// Recency tradeoff (reviewed): `last_used_txn` starts at insert time and is
/// refreshed ONLY when a `WriteSession` loads the plan from the registry on
/// a local-cache miss (a ride-along write on its already-open transaction).
/// Read-only loads via `Database::execute` deliberately do NOT bump it —
/// doing so would take the global writer lock on a read path, making
/// cold-cache SELECTs block behind live writers (see
/// `Database::cached_or_load`). Consequently a plan used exclusively by
/// readers keeps its insert-time stamp forever and may be evicted despite
/// recent use; that is accepted, because eviction is only hygiene and the
/// caller heals with a re-`prepare`. Eviction must therefore tolerate
/// entries whose `last_used_txn` was never bumped after insertion (they sort
/// by insert time, which is always valid).
fn evict_if_full(txn: &mut WriteTxn<'_>) -> Result<()> {
    let mut plans: Vec<(Vec<u8>, u64)> = txn
        .sys_scan()?
        .into_iter()
        .filter(|(k, _)| k.starts_with(PLAN_PREFIX))
        .map(|(k, v)| {
            let last_used = parse_record(&v).map_or(0, |r| r.last_used_txn);
            (k, last_used)
        })
        .collect();
    if plans.len() < MAX_REGISTRY_PLANS {
        return Ok(());
    }
    plans.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    for (subkey, _) in plans.into_iter().take(EVICT_BATCH) {
        txn.sys_delete(&subkey)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip() {
        let rec = encode_record("SELECT 1", b"blobby", 42);
        let parsed = parse_record(&rec).unwrap();
        assert_eq!(parsed.sql, "SELECT 1");
        assert_eq!(parsed.blob, b"blobby");
        assert_eq!(parsed.last_used_txn, 42);

        let patched = patched_last_used(&rec, 99).unwrap();
        assert_eq!(parse_record(&patched).unwrap().last_used_txn, 99);
        assert_eq!(parse_record(&patched).unwrap().blob, b"blobby");
    }

    #[test]
    fn malformed_records_are_rejected_not_panicked() {
        assert!(parse_record(b"").is_none());
        assert!(parse_record(&[0xff; 3]).is_none());
        // huge sql_len must not read out of bounds
        let mut evil = Vec::new();
        evil.extend_from_slice(&u32::MAX.to_le_bytes());
        evil.extend_from_slice(b"x");
        assert!(parse_record(&evil).is_none());
        // trailing garbage
        let mut rec = encode_record("s", b"b", 1);
        rec.push(0);
        assert!(parse_record(&rec).is_none());
        // every truncation fails cleanly
        let rec = encode_record("SELECT 1", b"blob", 7);
        for cut in 0..rec.len() {
            assert!(parse_record(&rec[..cut]).is_none(), "cut at {cut}");
        }
    }
}
