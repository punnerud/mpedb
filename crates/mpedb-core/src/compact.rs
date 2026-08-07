//! Offline table-id compaction — the escape hatch DESIGN-DROP-TABLE §0 named
//! and deferred.
//!
//! Table ids are never reused, so `schema.tables` keeps a dead slot per
//! LIFETIME create and `MAX_TABLES` is a lifetime budget rather than a live
//! one. A workload that creates and drops tables — a per-tenant table, a
//! nightly staging table — spends that budget on tombstones and eventually
//! hits a wall with almost nothing live.
//!
//! §0 chose no-reuse over reuse deliberately, and this module is the other half
//! of that decision rather than a reversal of it. Reuse would need every DROP
//! to purge every persisted `table_id` — crash-atomically, forever, including
//! from subsystems not yet written. Compaction does the same rewrite ONCE, as a
//! batch, with the database exclusive: "a single batch, strictly easier to get
//! right than online per-DROP purge".
//!
//! # Why exclusive is the whole safety argument
//!
//! Between renumbering the schema and renumbering the records keyed by the old
//! ids there is an instant where a reader would see one against the other. That
//! is the aliasing window reuse could never close, and the only reason it can
//! be closed here is that nothing else is attached. An exclusivity check that
//! is merely advisory would give this operation exactly the failure mode the
//! design rejected.
//!
//! # The registry is the point
//!
//! [`ID_KEYS`] lists every sys-keyspace prefix whose KEY embeds a table id.
//! Without one list, the next module that persists a `table_id` becomes a
//! silent compaction bug — which is precisely the "permanent maintenance tax"
//! §0 held against reuse. With it, the tax is a test: `every_sys_namespace_is_
//! classified` fails when a prefix appears that nobody has classified as either
//! id-bearing or id-free.

use mpedb_types::Error;

/// One sys-keyspace prefix whose key embeds a `table_id` as 4 big-endian bytes.
#[derive(Debug, Clone, Copy)]
pub struct IdKey {
    /// The key's leading bytes, exactly as written on disk.
    pub prefix: &'static [u8],
    /// Where the id starts, counted from the END of `prefix`.
    ///
    /// Zero for every record that puts the id first. The sync cursors do not:
    /// their key is `<tag><link BE8>/<table_id BE4>`, so the id sits nine bytes
    /// in. Carrying the offset rather than assuming zero is what let that one
    /// be listed at all.
    pub id_at: usize,
    /// Who owns the record, for the dry run's report and for the reader trying
    /// to find out why a prefix is here.
    pub owner: &'static str,
}

/// Every sys-keyspace prefix whose key embeds a table id.
///
/// The prefixes are LITERALS rather than references to each module's constant,
/// and that is forced: they live in three crates (`mpedb-core`, `mpedb`,
/// `mpedb-mirror`) and this module sits at the bottom of that stack. They are
/// on-disk format, not implementation detail, so a literal here is a statement
/// about the file — and the classification test below is what keeps it true.
pub const ID_KEYS: &[IdKey] = &[
    // mpedb/src/policy_store.rs — `pol/<id BE4>/<name>`, `rlsen/`, `polep/`
    IdKey { prefix: b"pol/", id_at: 0, owner: "row-level-security policy" },
    IdKey { prefix: b"rlsen/", id_at: 0, owner: "row-level-security enable flag" },
    IdKey { prefix: b"polep/", id_at: 0, owner: "policy epoch" },
    // mpedb-mirror/src/state.rs
    IdKey { prefix: b"park/", id_at: 0, owner: "mirror parked conflict" },
    IdKey { prefix: b"skip/", id_at: 0, owner: "mirror apply-skip" },
    IdKey { prefix: b"map/", id_at: 0, owner: "mirror source mapping" },
    IdKey { prefix: b"imp/", id_at: 0, owner: "mirror import watermark" },
    // mpedb-core/src/cdc.rs — the dirty set. `cdc\0tabs` holds TableSets in its
    // VALUE and is handled separately (see `rewrite_capture_config`).
    IdKey { prefix: b"cdc\0d/", id_at: 0, owner: "CDC dirty entry" },
    // mpedb/src/stats.rs — `stats\0<id BE4><index_no BE4>`
    IdKey { prefix: b"stats\0", id_at: 0, owner: "index statistics" },
    // mpedb/src/sync.rs — `pull/<link BE8>/<id BE4>`
    IdKey { prefix: b"pull/", id_at: 9, owner: "sync pull cursor" },
    IdKey { prefix: b"push/", id_at: 9, owner: "sync push cursor" },
];

/// Sys-keyspace prefixes that carry NO table id, listed so the completeness
/// test can tell "classified as id-free" from "nobody looked".
///
/// A namespace missing from BOTH lists fails the test. That is the whole
/// mechanism: adding a store is allowed, adding one silently is not.
pub const ID_FREE_KEYS: &[&[u8]] = &[
    b"plan/",      // compiled plans — DELETED by compaction, see `compact`
    // The registry's entry counter, deliberately outside `[plan/, plan0)` so
    // eviction's family walk cannot delete it (registry.rs). Compaction drops
    // the plans, so it must RESET this to zero rather than merely leave it —
    // a counter that outlives what it counts makes the registry believe it is
    // full and evict on the next publish.
    //
    // Found by the classification test on its first real run, against a
    // hand-built list of 27 prefixes. That is the entire argument for having
    // the test.
    b"plancount",
    b"view/",      // CREATE VIEW bodies, keyed by name
    b"trigger/",   // trigger definitions, keyed by name
    b"func\0",     // stored functions, keyed by name
    b"funch\0",    // stored function blobs, keyed by content hash
    b"proc\0",     // stored procedures, keyed by name
    b"proch\0",    // stored procedure blobs, keyed by content hash
    b"op\0",       // `:sym:` operator macros, keyed by symbol
    b"lens\0",     // rRETL lens pairs, keyed by name
    b"rrmap\0",    // rRETL table maps, keyed by name
    b"drvtab\0",   // derived-table definitions, keyed by name
    b"tune\0",     // cost tunables
    b"costpolicy\0", // the stored cost policy
    b"ingest\0",   // ingest source definitions, keyed by name
    b"cdc\0tabs",  // CaptureConfig — ids are in the VALUE, handled separately
    b"mir\0",      // mirror control records, keyed by kind
];

/// What a compaction would do, computed without writing anything.
#[derive(Debug, Default)]
pub struct CompactPlan {
    /// `(old_id, new_id)` for every LIVE table, ascending by old id. A table
    /// whose id does not move is still listed — the caller wants the whole
    /// picture, not the diff.
    pub map: Vec<(u32, u32)>,
    /// Dead slots that would be reclaimed.
    pub dead: usize,
    /// `(owner, records)` per id-bearing prefix that actually has records.
    pub records: Vec<(&'static str, usize)>,
    /// Published plans that would be dropped.
    pub plans: usize,
}

impl CompactPlan {
    /// Is there anything to do? A database with no dead slots is already dense.
    pub fn is_noop(&self) -> bool {
        self.dead == 0
    }

    /// Table ids after compaction, for the report.
    pub fn live(&self) -> usize {
        self.map.len()
    }
}

/// What a compaction WOULD do, computed from a schema and the sys keys,
/// writing nothing.
///
/// Separate from the apply on purpose. The apply needs the database exclusive;
/// this needs only a snapshot, so an operator can ask "what would this cost me"
/// on a live database and decide before taking anything down.
pub fn plan(tables_dead: &[bool], sys_keys: &[Vec<u8>]) -> CompactPlan {
    let mut out = CompactPlan::default();
    // The mapping: live tables keep their ORDER and take dense ids. Preserving
    // order rather than, say, packing by size keeps the diff readable and means
    // a table's id only ever moves DOWN — which is what makes "did it move" a
    // one-line check for anyone auditing a compaction after the fact.
    let mut next = 0u32;
    for (old, dead) in tables_dead.iter().enumerate() {
        if *dead {
            out.dead += 1;
        } else {
            out.map.push((old as u32, next));
            next += 1;
        }
    }
    // Records per owner, so the dry run says what will be touched rather than
    // just how many tables move.
    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for k in sys_keys {
        if k.starts_with(b"plan/") {
            out.plans += 1;
        } else if let Some((_, spec)) = id_offset(k) {
            *counts.entry(spec.owner).or_default() += 1;
        }
    }
    out.records = counts.into_iter().collect();
    out
}

/// Is this key classified — either as id-bearing or as deliberately id-free?
///
/// The completeness check. A key matching NEITHER list is a store somebody
/// added without saying whether compaction has to touch it, and that is the
/// one way this operation goes silently wrong: the record keeps an id that no
/// longer names its table, and now names a DIFFERENT one.
pub fn is_classified(key: &[u8]) -> bool {
    id_offset(key).is_some()
        || ID_KEYS.iter().any(|k| key.starts_with(k.prefix))
        || ID_FREE_KEYS.iter().any(|p| key.starts_with(p))
}

/// Every sys key that nobody has classified. Empty is the passing state.
pub fn unclassified(keys: &[Vec<u8>]) -> Vec<Vec<u8>> {
    keys.iter().filter(|k| !is_classified(k)).cloned().collect()
}

/// Where in a key the table id sits, if this key belongs to an id-bearing
/// prefix. `None` for every other key.
pub fn id_offset(key: &[u8]) -> Option<(usize, &'static IdKey)> {
    for k in ID_KEYS {
        if key.starts_with(k.prefix) {
            let at = k.prefix.len() + k.id_at;
            // A key too short to hold the id at that offset is not one of ours
            // however well the prefix matched — `map/` is also a prefix of a
            // hypothetical `map/x`, and reading four bytes past the end would
            // be the bug this returns None to avoid.
            if key.len() >= at + 4 {
                return Some((at, k));
            }
        }
    }
    None
}

/// Read a big-endian table id out of a key at `at`.
pub fn read_id(key: &[u8], at: usize) -> Result<u32, Error> {
    key.get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| Error::Corrupt("sys key too short for its table id".into()))
}

/// A key with its table id replaced.
pub fn rewrite_key(key: &[u8], at: usize, new_id: u32) -> Vec<u8> {
    let mut out = key.to_vec();
    out[at..at + 4].copy_from_slice(&new_id.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id-bearing prefix must be findable, and the offset must be right.
    /// A wrong offset here rewrites four bytes of somebody else's key — which
    /// is silent corruption, so it gets a test per shape rather than a spot
    /// check.
    #[test]
    fn each_id_bearing_key_shape_is_located_correctly() {
        let id = 0x01020304u32;
        let b = id.to_be_bytes();
        // `<prefix><id>` — the common shape.
        for p in [
            &b"pol/"[..],
            b"rlsen/",
            b"polep/",
            b"park/",
            b"skip/",
            b"map/",
            b"imp/",
            b"cdc\0d/",
            b"stats\0",
        ] {
            let mut k = p.to_vec();
            k.extend_from_slice(&b);
            k.extend_from_slice(b"trailing");
            let (at, spec) = id_offset(&k).unwrap_or_else(|| panic!("{p:?} not classified"));
            assert_eq!(at, p.len(), "{p:?}");
            assert_eq!(read_id(&k, at).unwrap(), id, "{p:?}");
            assert_eq!(spec.prefix, p);
        }
        // The sync cursors put the id NINE bytes in: `<tag><link BE8>/<id>`.
        for p in [&b"pull/"[..], b"push/"] {
            let mut k = p.to_vec();
            k.extend_from_slice(&7u64.to_be_bytes());
            k.push(b'/');
            k.extend_from_slice(&b);
            let (at, _) = id_offset(&k).unwrap_or_else(|| panic!("{p:?} not classified"));
            assert_eq!(at, p.len() + 9, "{p:?}");
            assert_eq!(read_id(&k, at).unwrap(), id, "{p:?}");
        }
    }

    /// A key that merely SHARES a prefix but is too short to hold an id must
    /// not be treated as id-bearing — rewriting it would write four bytes past
    /// whatever it actually is.
    #[test]
    fn a_key_too_short_for_its_id_is_not_claimed() {
        assert!(id_offset(b"map/").is_none());
        assert!(id_offset(b"map/ab").is_none());
        assert!(id_offset(b"pull/short").is_none());
        assert!(id_offset(b"plan/whatever").is_none());
        assert!(id_offset(b"view/v").is_none());
    }

    #[test]
    fn rewriting_replaces_exactly_the_id_and_nothing_else() {
        let mut k = b"pol/".to_vec();
        k.extend_from_slice(&5u32.to_be_bytes());
        k.extend_from_slice(b"/mypolicy");
        let out = rewrite_key(&k, 4, 9);
        assert_eq!(&out[..4], b"pol/");
        assert_eq!(read_id(&out, 4).unwrap(), 9);
        assert_eq!(&out[8..], b"/mypolicy");
        assert_eq!(out.len(), k.len());
    }

    /// The mapping is dense over LIVE tables, and dead slots simply disappear.
    #[test]
    fn the_mapping_is_dense_over_live_tables_and_preserves_order() {
        //          0     1      2     3      4      5
        let dead = [false, true, false, true, true, false];
        let p = plan(&dead, &[]);
        assert_eq!(p.map, vec![(0, 0), (2, 1), (5, 2)]);
        assert_eq!(p.dead, 3);
        assert_eq!(p.live(), 3);
        assert!(!p.is_noop());
        // An id never moves UP — that is what makes an audit a one-line check.
        for (old, new) in &p.map {
            assert!(new <= old, "{old} -> {new} moved up");
        }
    }

    /// A database with no dead slots is already dense: the plan must say so
    /// rather than propose a rewrite that changes nothing but drops every
    /// published plan for it.
    #[test]
    fn a_dense_schema_is_a_noop() {
        let p = plan(&[false, false, false], &[]);
        assert!(p.is_noop());
        assert_eq!(p.map, vec![(0, 0), (1, 1), (2, 2)]);
    }

    /// The dry run counts what it would touch, by owner, and counts plans
    /// separately because those are DELETED rather than rewritten.
    #[test]
    fn the_dry_run_counts_records_per_owner_and_plans_apart() {
        let key = |p: &[u8], id: u32| {
            let mut k = p.to_vec();
            k.extend_from_slice(&id.to_be_bytes());
            k
        };
        let keys = vec![
            key(b"pol/", 1),
            key(b"pol/", 2),
            key(b"park/", 1),
            b"plan/aaaa".to_vec(),
            b"plan/bbbb".to_vec(),
            b"view/v".to_vec(), // id-free: counted nowhere
        ];
        let p = plan(&[false, true], &keys);
        assert_eq!(p.plans, 2);
        assert_eq!(
            p.records,
            vec![("mirror parked conflict", 1), ("row-level-security policy", 2)]
        );
    }

    /// No prefix may appear in both lists: a record either carries an id or it
    /// does not, and a prefix in both means somebody guessed twice.
    #[test]
    fn the_two_lists_are_disjoint() {
        for k in ID_KEYS {
            assert!(
                !ID_FREE_KEYS.contains(&k.prefix),
                "{:?} is classified both ways",
                k.prefix
            );
        }
    }
}
