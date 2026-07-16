use super::*;

impl<'e> WriteTxn<'e> {
    // ---------- commit / abort ----------

    pub fn commit(self) -> Result<()> {
        self.commit_with(|| {})
    }

    /// Commit, running `after_flip` after the meta publish (and durability
    /// steps) but BEFORE the writer lock is released. The intent-ring leader
    /// posts batch results there: with posting serialized under the lock, a
    /// slot can never be picked up, released, and re-used while a stale
    /// poster still holds a reference to its previous incarnation.
    pub fn commit_with<F: FnOnce()>(self, after_flip: F) -> Result<()> {
        let __commit_t = std::time::Instant::now();
        let __r = self.commit_inner(after_flip);
        leakstat::add(&leakstat::INS_NS_COMMIT, __commit_t.elapsed().as_nanos() as u64);
        __r
    }

    fn commit_inner<F: FnOnce()>(mut self, after_flip: F) -> Result<()> {
        let new_txn = self.meta.txn_id + 1;

        // 1. write back catalog entries (may COW catalog pages → more frees)
        let entries: Vec<((u32, u32), (u64, u64))> =
            self.table_roots.iter().map(|(&k, &v)| (k, v)).collect();
        for ((tid, ino), (root, cnt)) in entries {
            let mut val = [0u8; 16];
            val[0..8].copy_from_slice(&root.to_le_bytes());
            val[8..16].copy_from_slice(&cnt.to_le_bytes());
            let cat_root = self.catalog_root;
            let out = btree::insert(
                &mut self,
                cat_root,
                &cat_tree_key(tid, ino),
                &mut btree::Payload::Flat(&val),
                InsertMode::Upsert,
            )?;
            self.catalog_root = out.new_root;
        }

        // 2. freelist fixpoint (DESIGN.md §4.5). Two things get written:
        //
        //   - each drawn entry, minus whatever we consumed out of it (deleted
        //     if we consumed all of it, left completely alone if we consumed
        //     none — the common case, and the reason drawing is free);
        //   - this commit's own free set, under this commit's txn id.
        //
        // "Own free set" = pages COWed away, plus any page still sitting in
        // `reusable` that no drawn entry lists. Those are pages this txn
        // allocated (from the high-water mark, or from an entry it then fully
        // consumed) and freed again; nothing else records them, so dropping
        // them here would leak them outright.
        //
        // The circularity is LMDB's: these writes themselves allocate and free
        // pages, changing what should have been written. So iterate to a
        // fixpoint. Termination (unchanged by the read-only refill, which frees
        // nothing): each pass can only add pages freed by COWing the
        // height-bounded freelist path, the sets grow monotonically, and once
        // `reusable` is consumed allocation falls back to `high_water`, which
        // frees nothing — so the loop is bounded by O(tree height).
        let mut written: Vec<([u8; 10], Vec<u64>)> = Vec::new();
        let mut iterations = 0;
        // The whole fixpoint mutates the freelist tree: block refill so no
        // cursor read can draw from an entry these writes are rewriting (see
        // `in_freelist_op`). Allocations fall back to reusable/high-water.
        self.in_freelist_op = true;
        leakstat::add(&leakstat::COMMIT_FREED, self.freed.len() as u64);
        leakstat::add(&leakstat::COMMIT_LEFTOVER, self.reusable.len() as u64);
        leakstat::bump(&leakstat::COMMITS);
        loop {
            iterations += 1;
            if iterations > 64 {
                return Err(Error::Internal("freelist fixpoint did not converge".into()));
            }
            let plan = self.freelist_plan(new_txn, &written);
            if plan == written {
                break;
            }
            // Apply the DIFF against the previous pass: a key whose value did
            // not move must not be rewritten, or its COW would dirty the tree
            // again every pass and the loop would never settle.
            for (k, _) in &written {
                if !plan.iter().any(|(pk, _)| pk == k) {
                    let fl_root = self.freelist_root;
                    let out = btree::delete(&mut self, fl_root, k)?;
                    self.freelist_root = out.new_root;
                }
            }
            for (k, ids) in &plan {
                if written.iter().any(|(wk, wv)| wk == k && wv == ids) {
                    continue;
                }
                let fl_root = self.freelist_root;
                self.freelist_root = if ids.is_empty() {
                    // drawn dry — the entry goes away
                    btree::delete(&mut self, fl_root, k)?.new_root
                } else {
                    let mut val = Vec::with_capacity(ids.len() * 8);
                    for &id in ids {
                        val.extend_from_slice(&id.to_le_bytes());
                    }
                    btree::insert(&mut self, fl_root, k, &mut btree::Payload::Flat(&val), InsertMode::Upsert)?.new_root
                };
            }
            leakstat::add(&leakstat::COMMIT_ENTRIES, plan.len() as u64);
            leakstat::add(
                &leakstat::COMMIT_PAGES,
                plan.iter().map(|(_, v)| v.len() as u64).sum::<u64>(),
            );
            written = plan;
        }
        self.taken.clear();
        self.in_freelist_op = false;

        // 3. durability: data must be durable before the meta references it
        let snapshot = MetaSnapshot {
            slot: self.meta.slot,
            txn_id: new_txn,
            catalog_root: self.catalog_root,
            freelist_root: self.freelist_root,
            high_water: self.high_water,
        };
        match self.eng.shm.durability {
            Durability::Commit => {
                let mut ids: Vec<u64> = self.dirty.iter().copied().collect();
                ids.sort_unstable();
                let mut i = 0;
                while i < ids.len() {
                    let start = ids[i];
                    let mut end = start;
                    while i + 1 < ids.len() && ids[i + 1] == end + 1 {
                        i += 1;
                        end = ids[i];
                    }
                    // NO barrier per run: `F_FULLFSYNC` is per-fd, and every
                    // run here is data, i.e. the same ordering class. One
                    // barrier below covers them all. Barriering each run cost a
                    // platter flush per CONTIGUOUS RUN — the sequential-insert
                    // benchmark has one run and never showed it, but a random
                    // update scatters the btree path and the freelist path, so
                    // N=3-5 and this was 4-6 flushes per commit (#41).
                    self.eng.shm.msync_range_nobarrier(
                        start as usize * PAGE_SIZE,
                        (end - start + 1) as usize * PAGE_SIZE,
                    )?;
                    i += 1;
                }
                // ⚠ ORDERING: this barrier is what makes the data durable BEFORE
                // the meta that will reference it (DESIGN.md §4.1). It cannot be
                // merged with the meta's barrier below — a single barrier over
                // both would let a power loss land meta on the platter and not
                // its data, and meta_T would then be checksum-valid pointing at
                // COW pages that were never written. Two flushes is the FLOOR
                // here; `wal` gets away with one because its record is a single
                // self-describing checksummed object with no ordering to keep.
                self.eng.shm.sync_barrier()?;
            }
            // WAL-class (§5.4): ONE sequential record replaces the scattered
            // COW-page msyncs above and the meta-page msync below. `wal`
            // fdatasyncs it before ack (durable-on-ack); `async` only appends
            // and lets the background flusher coalesce the fdatasync
            // (crash-consistent, deferred — §5.4.2). Either way, on error the
            // commit aborts cleanly: nothing was published, Drop unlocks.
            Durability::Wal | Durability::Async => {
                let mut ids: Vec<u64> = self.dirty.iter().copied().collect();
                ids.sort_unstable();
                if self.eng.shm.durability == Durability::Async {
                    self.eng.shm.wal_append_async(&ids, &snapshot)?;
                } else {
                    self.eng.shm.wal_commit(&ids, &snapshot)?;
                }
            }
            Durability::None => {}
        }

        // 3b. Optimistic mode: record this commit's footprint into the
        // committed-footprint ring BEFORE the flip, so any successor that reads
        // the flipped meta is guaranteed to already see this entry (both run
        // under the writer lock). Every commit records — data writes as POINT
        // or TABLE, catalog/sys-only commits as EMPTY — so an optimistic
        // validator never sees a spurious gap for a same-mode committer.
        if self.eng.concurrency == Concurrency::Optimistic {
            use crate::shm::{OFP_KIND_EMPTY, OFP_KIND_POINT, OFP_KIND_TABLE};
            let (kind, tbits, khash) = match self.commit_point {
                Some((table, khash)) => (OFP_KIND_POINT, 1u64 << (table & 63), khash),
                None if self.written_tables != 0 => {
                    (OFP_KIND_TABLE, self.written_tables, 0)
                }
                None => (OFP_KIND_EMPTY, 0, 0),
            };
            self.eng.shm.opt_record(new_txn, kind, tbits, khash);
        }

        // 4. publish (fence(Release) + atomic stores inside)
        let new_slot = self.eng.shm.write_meta_slot(self.meta.slot, &snapshot);

        match self.eng.shm.durability {
            Durability::Commit => {
                self.eng.shm.msync_page(new_slot)?;
                self.eng
                    .shm
                    .durable_txn()
                    .fetch_max(new_txn, std::sync::atomic::Ordering::AcqRel);
            }
            // WAL: the record (pages + meta fields) is already durable, so
            // the commit may be acknowledged right after the flip — no
            // meta-page msync (recovery replays the log, never trusts the
            // mapping metas; see shm::recover_after_owner_death).
            Durability::Wal => {
                self.eng
                    .shm
                    .durable_txn()
                    .fetch_max(new_txn, std::sync::atomic::Ordering::AcqRel);
            }
            Durability::None | Durability::Async => {}
        }

        after_flip();
        if self.eng.shm.durability.uses_wal() {
            // Amortized checkpoint (wal AND async), still under the writer
            // lock, after the results are posted (the full-mapping msync can
            // take a while and must not delay waiter wakeups). An msync failure
            // here is swallowed deliberately: the checkpoint simply does not
            // advance, recovery still replays from the old wal_ckpt, and the
            // next commit retries — whereas failing a commit that is already
            // durable and acknowledged would be a lie.
            let _ = self.eng.shm.wal_maybe_checkpoint();
        }
        self.finished = true;
        self.eng.shm.writer_unlock();
        Ok(())
    }

    /// Discard everything. COW means committed state was never touched; pages
    /// taken from high_water simply stay above the committed watermark, and
    /// freelist consumption was only recorded in dirty pages now dropped.
    pub fn abort(mut self) {
        self.finished = true;
        self.eng.shm.writer_unlock();
    }
}

impl Drop for WriteTxn<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.eng.shm.writer_unlock();
        }
    }
}
