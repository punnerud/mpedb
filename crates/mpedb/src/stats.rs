//! Persisted per-index statistics — stage A of design/DESIGN-MPEE-GENERAL.md.
//!
//! One record per analyzed index in the shared sys-keyspace (namespace
//! `stats`, the same catalog tree the plan registry lives in), written by an
//! explicit [`crate::Database::analyze`] pass and read at prepare time through
//! the planner's `CostSource`. Nothing here runs on the write hot path — the
//! MPEE-COST hard constraint — and nothing here is an estimate: the pass walks
//! the index tree on one snapshot and counts, so the stored value is exact for
//! that snapshot and **deterministic between passes**. A prepare on an
//! unanalyzed database sees `None` everywhere and prices exactly as it did
//! before this module existed.
//!
//! Record: key `table_id u32 BE ‖ index_no u32 BE`, value
//! `version u8 ‖ fingerprint 32B ‖ ndv u64 LE`. The fingerprint —
//! `blake3(table name ‖ index column names)` — guards against DDL renumbering
//! an index out from under a stored record (DROP reuses index numbers): a
//! mismatch reads as "never analyzed", which mis-prices nothing. It is a
//! FINGERPRINT and not the schema generation on purpose: the generation also
//! bumps for every cost-layer change (tunables, policies, stored functions),
//! and gen-guarded stats died on every one of those — the cost_layer test
//! caught exactly that. A stats record is a cost hint; its staleness rule
//! must track the INDEX'S identity, nothing else.

use mpedb_types::{Error, Result, TableKind, Value};

/// Namespace in the sys-record keyspace.
pub const NS: &str = "stats";

const VERSION: u8 = 2;

/// The identity a record is pinned to: the table's NAME and the index's
/// column NAMES (the #118 names-not-ordinals rule, in miniature). Survives
/// unrelated DDL and every cost-layer change; changes exactly when the slot
/// `(table_id, index_no)` could mean a different index.
pub fn index_fingerprint(table: &str, columns: &[&str]) -> [u8; 32] {
    let mut b: Vec<u8> = Vec::with_capacity(table.len() + 16);
    b.extend_from_slice(table.as_bytes());
    b.push(0);
    for c in columns {
        b.extend_from_slice(c.as_bytes());
        b.push(1);
    }
    *blake3::hash(&b).as_bytes()
}

pub fn record_key(table_id: u32, index_no: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[..4].copy_from_slice(&table_id.to_be_bytes());
    k[4..].copy_from_slice(&index_no.to_be_bytes());
    k
}

pub fn encode_record(fingerprint: &[u8; 32], ndv: u64) -> [u8; 41] {
    let mut v = [0u8; 41];
    v[0] = VERSION;
    v[1..33].copy_from_slice(fingerprint);
    v[33..].copy_from_slice(&ndv.to_le_bytes());
    v
}

/// Decode a stats record; `None` for any mismatch (wrong version, wrong
/// fingerprint, truncated) — a stats record is advisory, so corrupt or stale
/// reads degrade to "never analyzed" rather than erroring a prepare.
pub fn decode_record(bytes: &[u8], fingerprint: &[u8; 32]) -> Option<u64> {
    if bytes.len() != 41 || bytes[0] != VERSION {
        return None;
    }
    if &bytes[1..33] != fingerprint {
        return None;
    }
    Some(u64::from_le_bytes(bytes[33..].try_into().ok()?))
}

/// `bucket(n) = 64 − leading_zeros(n)` — the same quantization the solver
/// applies to row counts (`mpee::magnitude`), duplicated here because the
/// planner deliberately does not export its internals. One `debug_assert`
/// mirror test in `mpedb-sql` keeps them from drifting.
pub fn bucket(n: u64) -> u32 {
    64 - n.leading_zeros()
}

/// One analyzed index, for the report `analyze()` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStat {
    pub table: String,
    pub table_id: u32,
    /// Engine numbering: secondary index position + 1.
    pub index_no: u32,
    pub ndv: u64,
}

impl crate::Database {
    /// Walk every analyzable secondary index once and persist its distinct-key
    /// count — the ANALYZE-style pass of DESIGN-MPEE-GENERAL stage A. Explicit
    /// and read-heavy: run it after bulk loads, never automatically.
    ///
    /// v1 analyzes **single-column, non-partial, plain-collation** indexes:
    /// - single-column, because the tree's leading value IS the whole key and
    ///   `fold_index_leading` can stream it without decoding rows;
    /// - non-partial, because a partial index counts members, not rows, and
    ///   the discount rule declines partial indexes anyway;
    /// - plain collation, because a NOCASE/RTRIM tree stores the FOLDED key
    ///   (`fold_index_leading` refuses it) — and folded-key NDV would be the
    ///   wrong statistic to boot.
    ///
    /// A UNIQUE index's NDV is its entry count, read leaf-wholesale. NaN
    /// floats: `Value`'s `!=` calls NaN unequal to itself, so a run of NaNs
    /// counts each as distinct — an OVERcount, which only shrinks the
    /// discount. Conservative in the safe direction, documented here rather
    /// than special-cased.
    pub fn analyze(&self) -> Result<Vec<IndexStat>> {
        self.refresh_schema_if_stale()?;
        let bundle = self.schema();
        let r = self.engine.begin_read()?;

        let mut stats = Vec::new();
        for t in bundle.schema.tables.iter().filter(|t| !t.dead) {
            if !matches!(t.kind, TableKind::Standard) {
                continue;
            }
            for (pos, ix) in t.indexes.iter().enumerate().take(63) {
                if ix.predicate.is_some() || ix.columns.len() != 1 {
                    continue;
                }
                let index_no = pos as u32 + 1;
                let ndv = if ix.unique {
                    r.count_index_entries(t.id, index_no)?
                } else {
                    let mut n = 0u64;
                    let mut prev: Option<Value> = None;
                    let fold = r.fold_index_leading(t.id, index_no, &mut |v| {
                        if prev.as_ref() != Some(&v) {
                            n += 1;
                            prev = Some(v);
                        }
                        Ok(())
                    });
                    match fold {
                        Ok(()) => n,
                        // Collated / typeless key: not analyzable, not an error.
                        Err(Error::Unsupported(_)) => continue,
                        Err(e) => {
                            r.finish()?;
                            return Err(e);
                        }
                    }
                };
                stats.push(IndexStat {
                    table: t.name.clone(),
                    table_id: t.id,
                    index_no,
                    ndv,
                });
            }
        }
        r.finish()?;

        // One commit for the whole batch: the records land together or not at
        // all, so a reader never sees half an analyze.
        let mut s = self.begin()?;
        for st in &stats {
            let t = bundle
                .schema
                .tables
                .iter()
                .find(|t| t.id == st.table_id)
                .expect("just scanned");
            let ix = &t.indexes[(st.index_no - 1) as usize];
            let cols: Vec<&str> = ix
                .columns
                .iter()
                .map(|&c| t.columns[c as usize].name.as_str())
                .collect();
            s.sys_record_put(
                NS,
                &record_key(st.table_id, st.index_no),
                &encode_record(&index_fingerprint(&t.name, &cols), st.ndv),
            )?;
        }
        s.commit()?;
        Ok(stats)
    }
}
