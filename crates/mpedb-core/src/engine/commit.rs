use super::*;

/// A/B escape hatch for #111: restore the historical one-msync-per-contiguous-
/// run data barrier in `durability = commit`. Both arms therefore live in one
/// binary, which is what BENCHMARKS.md's paired-arm method requires (two builds
/// have been the source of at least one false A/B here already).
fn msync_per_run() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("MPEDB_MSYNC_PER_RUN").is_ok_and(|v| v == "1"));
    *ON
}

/// `MPEDB_MSYNC_SPAN=1` forces the span arm on a platform where it is NOT the
/// default (i.e. not Linux), so the pair stays measurable everywhere. Presence
/// alone is not enough — `=0` must mean off, or a benchmark script that spells
/// "disabled" that way silently selects the arm it meant to exclude.
fn msync_span_forced() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("MPEDB_MSYNC_SPAN").is_ok_and(|v| v == "1"));
    *ON
}

/// Is the one-span data barrier the right shape on THIS platform?
///
/// **Linux only, and the asymmetry is the whole point.** `msync(MS_SYNC)` on
/// Linux IS `vfs_fsync_range`, so N runs cost N device flushes and collapsing
/// them to one span is worth 41–63 %. On Darwin `msync` is `vm_object_sync`
/// over the range — it costs RANGE WIDTH, not dirty-page count — and the device
/// flush is the separate `F_FULLFSYNC` in `sync_barrier`. There the per-run loop
/// is ALREADY at §4.1's two-flush floor, and a span is a pure loss that scales
/// with the file: measured on an M3 Pro / APFS, a 1 GiB span costs 2,403 µs
/// against 312 µs for 4 MiB, and end-to-end durable inserts went **+10 % at
/// 300 MiB of live data and +63 % at 1.2 GiB**.
///
/// That number is not a corner case. `strace` on Linux shows the span is
/// typically the WHOLE live data region (329 MiB on a 3 GiB file) on nearly
/// every commit, because the allocator hands out the lowest reusable pages
/// while the hot btree leaf sits near the high-water mark. On Linux that is
/// free; on Darwin it is the bill.
///
/// #41 was the same mistake mirrored: a barrier removed from this loop, correct
/// on macOS, a no-op on Linux, never re-measured on Linux. A durability change
/// must be measured on every platform it compiles for.
fn span_data_barrier() -> bool {
    if msync_per_run() {
        return false;
    }
    cfg!(target_os = "linux") || msync_span_forced()
}

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
        #[cfg(feature = "leakstat")]
        let __commit_t = std::time::Instant::now();
        let __r = self.commit_inner(after_flip);
        #[cfg(feature = "leakstat")]
        leakstat::add(&leakstat::INS_NS_COMMIT, __commit_t.elapsed().as_nanos() as u64);
        __r
    }

    fn commit_inner<F: FnOnce()>(mut self, after_flip: F) -> Result<()> {
        let new_txn = self.meta.txn_id + 1;

        // 1. write back catalog entries (may COW catalog pages → more frees)
        // SORTED, deliberately. Widening a directory value 16 → 24 bytes on
        // its first rewrite can split a catalog leaf, and a split's outcome
        // depends on insertion order — which `HashMap` iteration makes
        // per-process random. Sorting keeps a commit's catalog shape a
        // function of its contents alone, so the byte-identical corpus runs
        // stay byte-identical (review finding F4).
        let mut entries: Vec<((u32, u32), (u64, u64))> =
            self.table_roots.iter().map(|(&k, &v)| (k, v)).collect();
        entries.sort_unstable_by_key(|&(k, _)| k);

        // Every table this txn MUTATED must publish a bumped `mod_gen`
        // (design/DESIGN-COLUMNAR.md §6). The bump lands in the PK-tree entry
        // (`index_no 0`), so make sure that entry is in the writeback set:
        // every row mutation moves the PK tree and therefore already put it
        // there, but loading it here rather than assuming that keeps the
        // invariant local — a missed bump would publish a stale columnar
        // segment as fresh, which is a wrong answer.
        let mutated: Vec<u32> = self.mutated_tables.iter().copied().collect();
        for &tid in &mutated {
            if !self.table_roots.contains_key(&(tid, 0)) {
                let e = self.tree_root(tid, 0)?;
                entries.push(((tid, 0), e));
            }
        }

        // Read each PK entry's CURRENT generation before any of this loop's
        // inserts move the catalog root, so the base is the committed value
        // and not something this loop just wrote. A table with no committed
        // entry yet (CREATE TABLE in this txn) starts at 0.
        let mut next_gen: std::collections::HashMap<u32, u64> =
            std::collections::HashMap::new();
        for &((tid, ino), _) in &entries {
            if ino != 0 {
                continue;
            }
            let base = catalog_entry(&self, self.catalog_root, tid, 0)
                .map(|(_, _, g)| g)
                .unwrap_or(0);
            next_gen.insert(tid, base + u64::from(self.mutated_tables.contains(&tid)));
        }

        for ((tid, ino), (root, cnt)) in entries {
            // The PK entry carries the generation (24 bytes); a secondary
            // index entry has no meaningful one and stays 16.
            let mut val = [0u8; 24];
            val[0..8].copy_from_slice(&root.to_le_bytes());
            val[8..16].copy_from_slice(&cnt.to_le_bytes());
            let val: &[u8] = match next_gen.get(&tid) {
                Some(&g) if ino == 0 => {
                    val[16..24].copy_from_slice(&g.to_le_bytes());
                    &val[..24]
                }
                _ => &val[..16],
            };
            let cat_root = self.catalog_root;
            let out = btree::insert(
                &mut self,
                cat_root,
                &cat_tree_key(tid, ino),
                &mut btree::Payload::Flat(val),
                InsertMode::Upsert,
            )?;
            self.catalog_root = out.new_root;
        }

        // 1.5 extent-map edits — deferred from the data-tree operations that
        // recorded them, applied in ONE place so no tree op ever nests inside
        // another (DESIGN-BLOBEXTENT §4). Runs before the fixpoint so the
        // pages these COW-edits free/allocate are in its working sets.
        if !self.pending_map_edits.is_empty() {
            self.apply_map_edits()?;
        }

        // 2. freelist fixpoint (design/DESIGN.md §4.5). Two things get written:
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
        // fixpoint. Termination (§4.5) is a monotone-lattice argument, and it
        // must hold for BOTH page disciplines — pure COW and the `:memory:`
        // in-place mode (`adopt_inplace`):
        //
        //   - `reusable` only DRAINS: alloc pops it, refill is blocked
        //     (`in_freelist_op`), and — load-bearing — `free()` inters every
        //     fixpoint-time free in `freed` instead of recycling it here (see
        //     the §4.5 comment in `free()`). A page once consumed never comes
        //     back.
        //   - `freed` only GROWS: COWed-away pages (COW mode) and structurally
        //     freed nodes (either mode) are interred and never re-allocated
        //     this txn. A page once freed never leaves.
        //   - Once `reusable` is dry, allocation falls back to `high_water`,
        //     which frees nothing and moves no set — a pass that only draws
        //     high-water leaves the plan identical, and the loop closes.
        //
        // Under in-place adoption the passes free almost nothing (adoption
        // replaces COW's alloc+free with a dirty-bit), which is FINE — fewer
        // set movements, faster settling. What is NOT fine, and what the
        // `free()` routing exists to prevent, is a fixpoint-time free feeding
        // the pool the fixpoint allocates from: that turns the plan into a
        // period-2 oscillation (one leftover page is consumed to record
        // itself, then freed by unrecording itself) and hits the 64-pass cap
        // below. The cap stays as the bug detector: with the lattice intact,
        // real convergence is 2–4 passes.
        let mut written: Vec<([u8; 11], Vec<u64>)> = Vec::new();
        let mut iterations = 0;
        // The whole fixpoint mutates the freelist tree: block refill so no
        // cursor read can draw from an entry these writes are rewriting (see
        // `in_freelist_op`). Allocations fall back to reusable/high-water.
        self.in_freelist_op = true;
        // The kind-1 (extent run) side writes ONCE: the run pool cannot
        // change shape inside `in_freelist_op` (refill is blocked; page
        // allocation never draws from runs), so the page loop below iterates
        // over a run-plan that is already settled — the §4.5 termination
        // argument never meets a growing run entry (attribution rule,
        // DESIGN-BLOBEXTENT §3.3).
        if !self.taken_runs.is_empty()
            || !self.freed_runs.is_empty()
            || self.run_pool.iter().any(|r| r.from.is_none())
        {
            self.apply_run_plan(new_txn)?;
        }
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

        // 3. durability: data must be durable before the meta references it.
        // The extent coalescing buffer flushes FIRST — in every mode the
        // payload bytes must be in the page cache before the range-syncs
        // run and before the flip makes any reference reachable.
        self.flush_extent_buf()?;
        let snapshot = MetaSnapshot {
            slot: self.meta.slot,
            txn_id: new_txn,
            catalog_root: self.catalog_root,
            freelist_root: self.freelist_root,
            high_water: self.high_water,
            extent_map_root: self.extent_map_root,
            // Carried forward unchanged by ordinary writes; a DDL commit
            // (#47 stage 2) bumps it via `self.schema_gen_bump`.
            schema_gen: self.meta.schema_gen + u64::from(self.schema_gen_bump),
        };
        match self.eng.shm.durability {
            Durability::Commit => {
                // ONE range-msync over the whole dirty SPAN, not one per
                // contiguous run (#111). Both make exactly the same pages
                // durable; the difference is the number of syscalls, and on
                // Linux `msync(MS_SYNC)` IS `vfs_fsync_range` — every call ends
                // in a jbd2/XFS-log commit plus a device cache flush. #41 took
                // the *barrier* out of the per-run loop, which fixed macOS
                // (where msync is cheap and `F_FULLFSYNC` is the flush) and did
                // nothing at all for Linux (where `sync_barrier` compiles away
                // and each run-msync is still a full flush). Measured on this
                // host: an autocommit insert batch dirties ~3.7 pages in ~2.8
                // RUNS, so `commit` paid ~3.8 device flushes where §4.1's floor
                // is 2 (data, then meta).
                //
                // Widening the range costs nothing ON LINUX: writeback is
                // driven by the page cache's DIRTY tag, so a span walks dirty
                // pages, not pages. Measured (8 pages scattered over 32 MiB vs
                // over 400 KiB): no difference beyond noise. That is a LINUX
                // statement and nothing more — see `span_data_barrier` for why
                // Darwin gets the per-run loop instead. What the span may sweep
                // in that the run loop would not is (a) COW pages of an ABORTED
                // txn — unreferenced garbage, harmless to write, and (b)
                // nothing else: the writer lock is exclusive, readers never
                // dirty pages, and the meta/lock/reader pages sit BELOW every
                // data page id so the span can never reach them.
                //
                // Extent payload was pwritten, never mapped-stored, so it is
                // NOT in `dirty` — its ranges are folded into the same span,
                // in the same ordering class, covered by the same barrier
                // (DESIGN-BLOBEXTENT §4, review finding 1).
                //
                // `MPEDB_MSYNC_PER_RUN=1` restores the historical per-run loop
                // and `MPEDB_MSYNC_SPAN=1` forces the span where it is not the
                // default, so both arms live in ONE binary on EVERY platform.
                if !span_data_barrier() {
                    for &(start, npages) in &self.extent_dirty {
                        self.eng.shm.msync_range_nobarrier(
                            start as usize * PAGE_SIZE,
                            npages as usize * PAGE_SIZE,
                        )?;
                    }
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
                        self.eng.shm.msync_range_nobarrier(
                            start as usize * PAGE_SIZE,
                            (end - start + 1) as usize * PAGE_SIZE,
                        )?;
                        i += 1;
                    }
                } else {
                    let mut lo = u64::MAX;
                    let mut hi = 0u64;
                    for &(start, npages) in &self.extent_dirty {
                        lo = lo.min(start);
                        hi = hi.max(start + u64::from(npages.saturating_sub(1)));
                    }
                    for &id in &self.dirty {
                        lo = lo.min(id);
                        hi = hi.max(id);
                    }
                    if lo != u64::MAX {
                        // Not `lo >= 2`: that admits the lock page, the reader
                        // table and the whole intent ring, so it would not fire
                        // on the corruption class it exists to catch.
                        debug_assert!(
                            lo >= self.eng.shm.data_start,
                            "dirty span must stay in the data region (>= {}), got {lo}",
                            self.eng.shm.data_start
                        );
                        self.eng.shm.msync_range_nobarrier(
                            lo as usize * PAGE_SIZE,
                            (hi - lo + 1) as usize * PAGE_SIZE,
                        )?;
                    }
                }
                // ⚠ ORDERING: this barrier is what makes the data durable BEFORE
                // the meta that will reference it (design/DESIGN.md §4.1). It cannot be
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
                // Payload BEFORE the record that recovery will replay
                // (DESIGN-BLOBEXTENT §7): extents opted out of page-image
                // logging, so the record's validity must imply payload
                // durability — a range-bounded msync, not a whole-file
                // fdatasync (which would flush every dirty page in the file
                // and turn each large-value commit into a mini-checkpoint).
                // In `async` this is the insert-direction half; the reuse
                // direction is the refill gate on the durable frontier (§6).
                for &(start, npages) in &self.extent_dirty {
                    self.eng.shm.msync_range_nobarrier(
                        start as usize * PAGE_SIZE,
                        npages as usize * PAGE_SIZE,
                    )?;
                }
                // Test instrumentation (powerloss sim): the sim's loss model
                // rolls the main file back to a snapshot — which would erase
                // exactly the ranges the msyncs above made durable. This log
                // tells it which ranges SURVIVE real power loss. No-op unless
                // the env var is set.
                if !self.extent_dirty.is_empty() {
                    crate::shm::extent_sync_log(&self.extent_dirty)?;
                }
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
            // The OFP ring stays a `u64` table bitmap even though footprints are
            // now sparse (DESIGN-TABLE-CAP §5): the `& 63` fold aliases tables
            // mod 64 (e.g. 0, 64 and 4096 share a bit). This
            // is SOUND — two writers of the *same* table always fold to the same
            // bit, so a real conflict is never missed; only distinct aliased
            // tables see a false conflict, costing an extra optimistic
            // re-validation, never correctness. Point kind also compares khash,
            // so aliased tables with different keys don't even false-conflict.
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
        // Commit wins: discard undo images (published meta now owns the dirtied pages).
        self.inplace_undo.clear();
        self.finished = true;
        self.eng.shm.end_exclusive_write();
        self.eng.shm.writer_unlock();
        Ok(())
    }

    /// Discard everything. COW means committed state was never touched; pages
    /// taken from high_water simply stay above the committed watermark, and
    /// freelist consumption was only recorded in dirty pages now dropped.
    ///
    /// With `in_place`, restore pre-mutation images from `inplace_undo` first.
    pub fn abort(mut self) {
        self.restore_inplace_undo();
        self.finished = true;
        self.eng.shm.end_exclusive_write();
        self.eng.shm.writer_unlock();
    }
}

impl WriteTxn<'_> {
    /// Put exclusive in-place pages back to their pre-txn bytes (abort path).
    fn restore_inplace_undo(&mut self) {
        for (id, bytes) in self.inplace_undo.drain() {
            if let Ok(p) = self.eng.shm.page_mut_unchecked(id) {
                p.copy_from_slice(&bytes[..]);
            }
        }
    }
}

impl Drop for WriteTxn<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.restore_inplace_undo();
            self.eng.shm.end_exclusive_write();
            self.eng.shm.writer_unlock();
        }
    }
}
