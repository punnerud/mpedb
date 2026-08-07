//! The table-id rewrite itself — one transaction, every id-bearing record.
//!
//! The policy (which records carry an id, and where in the key) lives in
//! [`crate::compact`]; this is the mechanism. They are apart because the policy
//! is a statement about the FILE FORMAT that a test can check against a live
//! database, and the mechanism is engine internals that cannot be checked that
//! way. Keeping them together would have made the checkable part unreachable.
//!
//! It is its own module rather than more of `write.rs` (3 300 lines) for the
//! ordinary reason, and because a reader looking for "what does compaction
//! touch" should find one file rather than a diff.
//!
//! # The two key families
//!
//! Both live in the same catalog B-tree, told apart by their first byte:
//!
//! ```text
//!   0x01 ‖ table_id BE4 ‖ index_no BE4   → (tree root, row count, mod_gen)
//!   0x02 ‖ <sys subkey>                  → policies, CDC, stats, plans, …
//! ```
//!
//! So a compaction rewrites keys in ONE tree, and the whole operation is a
//! sequence of `delete`+`insert` pairs against `self.catalog_root` — no page
//! is moved, no tree is rebuilt, and the commit path is the ordinary one.
//!
//! # Why the collect-then-write shape
//!
//! Every rewrite reads the WHOLE old key set first, then writes. Doing it
//! streaming would mean inserting a new key into a tree still being scanned,
//! and — worse — a new id can COLLIDE with an old one not yet moved: table 7
//! becoming table 3 while the real table 3 is still sitting there. Collecting
//! first makes the collision impossible instead of making it depend on
//! iteration order.

use super::{cat_tree_key, sys_key, WriteTxn, SYS_PREFIX};
use crate::btree::{self, InsertMode};
use crate::compact::{id_offset, read_id, rewrite_key};
use mpedb_types::{Error, Result};

/// What one rewrite actually moved.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rewritten {
    /// Catalog directory entries — one per `(table, index)` pair.
    pub catalog: usize,
    /// Sys records whose key carried a table id.
    pub sys: usize,
    /// Published plans deleted (they carry ids inside their bytes).
    pub plans: usize,
}

impl WriteTxn<'_> {
    /// Compact this database's table ids: dense over live tables, tombstones
    /// gone, every id-bearing record moved with them.
    ///
    /// One transaction. A crash part-way leaves the OLD ids — nothing is
    /// published until the commit, so the failure mode is "nothing happened"
    /// rather than "half happened", and half is the one this operation must
    /// never produce.
    ///
    /// The ORDER inside it is not arbitrary: the schema goes last. Until it
    /// does, the file still says what the old ids meant, so a reader that
    /// somehow arrived mid-transaction would read a consistent old world
    /// rather than new records under an old schema.
    pub fn compact_table_ids(&mut self) -> Result<Rewritten> {
        let (dense, map) = self.bundle.schema.compacted()?;
        let done = self.rewrite_table_ids(&map)?;
        let bytes = dense.canonical_bytes();
        let root = self.catalog_root;
        let out = btree::insert(
            self,
            root,
            super::CAT_SCHEMA_KEY,
            &mut btree::Payload::Flat(&bytes),
            InsertMode::Upsert,
        )?;
        self.catalog_root = out.new_root;
        self.bump_schema_gen();
        Ok(done)
    }

    /// Renumber every table id in this transaction, per `map`.
    ///
    /// `map` is `(old, new)` for every LIVE table; a table not listed is
    /// treated as DROPPED and its records are deleted rather than moved. That
    /// is what reclaims a tombstone's leftovers — a dropped table's policies
    /// and parked conflicts are orphans by construction, and compaction is the
    /// only thing that ever collects them.
    ///
    /// The caller must have established that nothing else is attached. This
    /// function cannot check that; it only has a transaction.
    pub fn rewrite_table_ids(&mut self, map: &[(u32, u32)]) -> Result<Rewritten> {
        let lookup: std::collections::HashMap<u32, u32> = map.iter().copied().collect();
        let mut done = Rewritten::default();
        self.rewrite_catalog(&lookup, &mut done)?;
        self.rewrite_sys(&lookup, &mut done)?;
        Ok(done)
    }

    /// `0x01 ‖ table_id ‖ index_no` — the per-tree directory.
    ///
    /// The VALUE is untouched: a tree root is a page number and does not care
    /// which table points at it. Only the key moves.
    fn rewrite_catalog(
        &mut self,
        map: &std::collections::HashMap<u32, u32>,
        done: &mut Rewritten,
    ) -> Result<()> {
        // Collect first — see the module docs on why streaming would alias.
        let lo = [0x01u8];
        let hi = [0x02u8];
        let mut c = btree::cursor(self, self.catalog_root, Some((&lo[..], true)), Some((&hi[..], false)))?;
        let mut entries: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        while let Some((k, v)) = c.next(self)? {
            if k.len() != 9 {
                return Err(Error::Corrupt(format!(
                    "catalog directory key is {} bytes, expected 9",
                    k.len()
                )));
            }
            let tid = u32::from_be_bytes(k[1..5].try_into().expect("checked len"));
            let ino = u32::from_be_bytes(k[5..9].try_into().expect("checked len"));
            entries.push((tid, ino, v));
        }
        for (tid, ino, val) in entries {
            let Some(&new) = map.get(&tid) else {
                // A directory entry for a table not in the map is a dropped
                // table's leftover. DROP already frees the pages; this removes
                // the entry that pointed at them.
                let root = self.catalog_root;
                let out = btree::delete(self, root, &cat_tree_key(tid, ino))?;
                self.catalog_root = out.new_root;
                continue;
            };
            if new == tid {
                continue;
            }
            let root = self.catalog_root;
            let out = btree::delete(self, root, &cat_tree_key(tid, ino))?;
            self.catalog_root = out.new_root;
            let root = self.catalog_root;
            let out = btree::insert(
                self,
                root,
                &cat_tree_key(new, ino),
                &mut btree::Payload::Flat(&val),
                InsertMode::InsertOnly,
            )?;
            if out.existed {
                // Impossible under collect-then-write, and a corruption signal
                // rather than something to overwrite: two tables would be
                // sharing a tree.
                return Err(Error::Corrupt(format!(
                    "catalog entry for table {new} index {ino} already exists"
                )));
            }
            self.catalog_root = out.new_root;
            done.catalog += 1;
        }
        Ok(())
    }

    /// `0x02 ‖ …` — every sys record whose KEY carries a table id, plus the
    /// two families that are handled by deletion rather than by renumbering.
    fn rewrite_sys(
        &mut self,
        map: &std::collections::HashMap<u32, u32>,
        done: &mut Rewritten,
    ) -> Result<()> {
        let lo = [SYS_PREFIX];
        let hi = [SYS_PREFIX + 1];
        let mut c = btree::cursor(self, self.catalog_root, Some((&lo[..], true)), Some((&hi[..], false)))?;
        let mut recs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        while let Some((k, v)) = c.next(self)? {
            recs.push((k[1..].to_vec(), v));
        }

        for (subkey, val) in recs {
            // A published plan carries table ids INSIDE its bytes and inside
            // its footprint's TableSet. Renumbering the file without
            // renumbering those would leave a plan that names a different
            // table and STILL VALIDATES, because the new id is perfectly
            // legal. Deleting is the only answer that cannot be silently
            // wrong; the cost is a re-prepare.
            if subkey.starts_with(b"plan/") {
                let root = self.catalog_root;
                let out = btree::delete(self, root, &sys_key(&subkey))?;
                self.catalog_root = out.new_root;
                done.plans += 1;
                continue;
            }
            // The registry's counter is deliberately outside `[plan/, plan0)`
            // so eviction cannot delete it — which means compaction must, or
            // it outlives what it counts and the registry believes it is full.
            if subkey == b"plancount" {
                let root = self.catalog_root;
                let out = btree::delete(self, root, &sys_key(&subkey))?;
                self.catalog_root = out.new_root;
                continue;
            }
            let Some((at, _)) = id_offset(&subkey) else {
                continue;
            };
            let old = read_id(&subkey, at)?;
            let root = self.catalog_root;
            let out = btree::delete(self, root, &sys_key(&subkey))?;
            self.catalog_root = out.new_root;
            let Some(&new) = map.get(&old) else {
                // A dropped table's leftover — collected, not moved.
                done.sys += 1;
                continue;
            };
            let moved = rewrite_key(&subkey, at, new);
            let root = self.catalog_root;
            let out = btree::insert(
                self,
                root,
                &sys_key(&moved),
                &mut btree::Payload::Flat(&val),
                InsertMode::Upsert,
            )?;
            self.catalog_root = out.new_root;
            done.sys += 1;
        }
        Ok(())
    }
}
