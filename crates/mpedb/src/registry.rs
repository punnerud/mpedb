//! Shared plan registry (design/DESIGN.md §7.2): plans live as system records inside
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
/// Exclusive upper bound for a `sys_scan_range` over the whole plan family:
/// `/` is 0x2f, so 0x30 is the first subkey past every `plan/…` entry.
pub(crate) const PLAN_PREFIX_END: &[u8] = b"plan0";

/// Sys subkey of the registry's entry counter (u64 LE) — see [`make_room`].
/// Deliberately OUTSIDE `[plan/, plan0)`: `n` (0x6e) sorts above `0` (0x30), so
/// the family walk never sees it and eviction can never delete it.
pub(crate) const PLAN_COUNT_KEY: &[u8] = b"plancount";

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
    /// as the re-prepare fallback documented in design/DESIGN.md §7.2.
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
    let record = encode_record(sql, blob, txn.meta.txn_id + 1);
    if existing.is_some() {
        // Overwrite in place: the entry count does not move, so neither the
        // eviction check nor the counter has anything to do.
        txn.sys_put(&subkey, &record)?;
        return Ok(true);
    }
    let live = make_room(txn)?;
    txn.sys_put(&subkey, &record)?;
    txn.sys_put(PLAN_COUNT_KEY, &(live + 1).to_le_bytes())?;
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
/// Make room for one more entry and return the live `plan/` count the caller's
/// insert will add to.
///
/// **The counter is what keeps `prepare` off the O(registry) path (#124).**
/// Ranking every entry costs one full walk of the plan family — measured at
/// 1.2 MB held and ~540 µs on a full 4096-entry registry — and it used to run
/// on the publication of EVERY statement text never seen before. `plancount`
/// turns the common case into one 8-byte read: below the cap there is nothing
/// to rank, so there is nothing to read but the count.
///
/// **What invalidates it: nothing, because it is not a cache.** It is a derived
/// aggregate written in the SAME COW commit as the insert or the eviction that
/// moves it, so it flips atomically with them and a crash or an abort takes
/// both or neither. `insert_plan` and this function are the only writers of a
/// `plan/…` key in the codebase (`sys_record_put`'s namespace keys are
/// `<ns>\0<key>` and provably cannot collide — there is a regression test).
///
/// **It is still treated as untrusted shared memory.** Absent, malformed, or
/// at/above the cap all fall through to the authoritative walk, which recounts
/// from the tree and returns the truth — so a counter that reads HIGH self-heals
/// on its next use. A counter forced LOW by a hostile write only delays
/// eviction, and eviction is hygiene, not correctness: the cap bounds registry
/// bytes, and anyone able to forge the counter can forge plan records outright.
fn make_room(txn: &mut WriteTxn<'_>) -> Result<u64> {
    if let Some(n) = txn
        .sys_get(PLAN_COUNT_KEY)?
        .and_then(|v| <[u8; 8]>::try_from(v.as_slice()).ok())
        .map(u64::from_le_bytes)
    {
        if n < MAX_REGISTRY_PLANS as u64 {
            return Ok(n);
        }
    }
    evict_if_full(txn)
}

/// Registry hygiene: when the registry holds `MAX_REGISTRY_PLANS` or more
/// entries, drop the `EVICT_BATCH` oldest by `last_used_txn` (malformed
/// records sort first). Returns the live entry count afterwards.
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
///
/// This is the one place that genuinely must enumerate the registry, and it is
/// bounded to the `plan/` family rather than the whole sys keyspace. Reached
/// once per `EVICT_BATCH` inserts in the steady state — see [`make_room`].
fn evict_if_full(txn: &mut WriteTxn<'_>) -> Result<u64> {
    let mut plans: Vec<(Vec<u8>, u64)> = txn
        .sys_scan_range(PLAN_PREFIX, PLAN_PREFIX_END)?
        .into_iter()
        .filter(|(k, _)| k.starts_with(PLAN_PREFIX))
        .map(|(k, v)| {
            let last_used = parse_record(&v).map_or(0, |r| r.last_used_txn);
            (k, last_used)
        })
        .collect();
    if plans.len() < MAX_REGISTRY_PLANS {
        return Ok(plans.len() as u64);
    }
    plans.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let mut live = plans.len() as u64;
    for (subkey, _) in plans.into_iter().take(EVICT_BATCH) {
        if txn.sys_delete(&subkey)? {
            live -= 1;
        }
    }
    Ok(live)
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

    /// #124: the family bound and the counter's placement outside it are the
    /// two byte facts the O(1) publish path rests on — a `plancount` that fell
    /// inside `[plan/, plan0)` would be walked by eviction and then DELETED as
    /// the oldest "malformed record" (they sort first), silently un-capping the
    /// registry.
    #[test]
    fn plan_family_bound_excludes_the_counter_and_covers_every_hash() {
        for b in [0u8, 1, 0x2f, 0x30, 0x7f, 0xfe, 0xff] {
            let k = plan_subkey(&PlanHash([b; 32]));
            assert!(k.as_slice() >= PLAN_PREFIX, "{b:#x} below the family");
            assert!(k.as_slice() < PLAN_PREFIX_END, "{b:#x} above the family");
        }
        assert!(PLAN_COUNT_KEY >= PLAN_PREFIX_END);
        assert!(!PLAN_COUNT_KEY.starts_with(PLAN_PREFIX));
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
