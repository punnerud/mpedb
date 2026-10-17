//! Write transaction savepoint operations.
use super::*;

impl<'e> WriteTxn<'e> {
    // ---------- savepoints ----------

    /// Capture a statement-level savepoint. COW makes this cheap: a rollback
    /// only restores root pointers and returns pages allocated after the
    /// savepoint to the reusable pool (originals were never touched).
    /// Used by the batch leader so one failing intent aborts only itself.
    pub fn savepoint(&self) -> TxnSavepoint {
        TxnSavepoint {
            allocs: self.allocs,
            adopts: self.adopts,
            pristine: self.is_pristine(),
            catalog_root: self.catalog_root,
            freelist_root: self.freelist_root,
            table_roots: self.table_roots.clone(),
            dirty: self.dirty.clone(),
            freed: self.freed.clone(),
            reusable: self.reusable.clone(),
            taken: self.taken.clone(),
            refill_cursor: self.refill_cursor,
            high_water: self.high_water,
        }
    }

    /// Roll back to a savepoint taken in this transaction. `high_water` is
    /// deliberately NOT restored: pages physically allocated from it since the
    /// savepoint (ids in `[sp.high_water, high_water)`) belong to no committed
    /// freelist entry, so they are returned to `reusable` and the commit
    /// fixpoint records them as freed — page accounting stays exact.
    ///
    /// `reusable`, `taken` and `refill_cursor` MUST be restored together —
    /// they are the whole of what a post-savepoint `refill_reusable` changed.
    /// Refill is READ-ONLY (design/DESIGN.md §4.5): it draws an entry's pages
    /// into `reusable`, records the provenance in `taken`, advances
    /// `refill_cursor` past the key — and LEAVES the entry in the tree. So
    /// there is no tree edit to undo here; what must be undone is the private
    /// bookkeeping. The entry still lists the drawn pages, so keeping them in
    /// `reusable` would offer at commit pages the fixpoint also finds listed,
    /// and a stale `taken` would have it strike out a consumption that the
    /// rollback erased. Restoring the three to the snapshot drops exactly what
    /// the refill added while re-offering the pages that were reusable before
    /// the savepoint. (`freelist_root` is restored too, but defensively: only
    /// the commit fixpoint moves it, and `rollback_to` refuses to run inside
    /// one.)
    pub fn rollback_to(&mut self, sp: TxnSavepoint) {
        debug_assert!(
            self.undo_is_exact(&sp),
            "#160: rollback_to would re-offer a page that is still linked into \
             a tree — the statement allocated from a non-pristine transaction. \
             Check undo_is_exact() first and take the caller's escape hatch."
        );
        self.rollback_to_inner(sp);
    }

    /// [`rollback_to`](Self::rollback_to) without the exactness tripwire — for
    /// [`rollback_to_full`](Self::rollback_to_full), which restores the bytes of
    /// every page that was dirty at the savepoint and is therefore exact even
    /// when pages were allocated: the parent's restored bytes no longer link
    /// them, so re-offering them is correct.
    fn rollback_to_inner(&mut self, sp: TxnSavepoint) {
        debug_assert!(!self.in_freelist_op);
        if std::env::var_os("SP_TRACE").is_some() {
            let mut roots: Vec<_> = sp.table_roots.iter().collect();
            roots.sort();
            eprintln!(
                "[sp] rollback_to_inner: restore catalog_root={} table_roots={roots:?} \
                 dirty={} (was catalog_root={} table_roots.len={} dirty.len={}) \
                 inplace_undo.len={} (NOT restored)",
                sp.catalog_root,
                sp.dirty.len(),
                self.catalog_root,
                self.table_roots.len(),
                self.dirty.len(),
                self.inplace_undo.len(),
            );
        }
        self.catalog_root = sp.catalog_root;
        self.freelist_root = sp.freelist_root;
        self.table_roots = sp.table_roots;
        self.dirty = sp.dirty;
        self.freed = sp.freed;
        self.reusable = sp.reusable;
        self.taken = sp.taken;
        self.refill_cursor = sp.refill_cursor;
        for id in sp.high_water..self.high_water {
            self.reusable.push(id);
        }
    }

    /// How many pages this transaction has allocated, ever (#160).
    ///
    /// Monotone and never rewound — not by [`rollback_to`](Self::rollback_to)
    /// and not by [`restart`](Self::restart) — because the only question asked
    /// of it is whether it *differs* from a value captured earlier. Compare it
    /// against [`allocs_at`](Self::allocs_at) of a savepoint to learn whether
    /// anything was allocated since; the difference is not a count of live
    /// pages and must not be read as one.
    pub fn allocs(&self) -> u64 {
        self.allocs
    }

    /// The allocation counter as it stood when `sp` was taken; see
    /// [`allocs`](Self::allocs).
    pub fn allocs_at(sp: &TxnSavepoint) -> u64 {
        sp.allocs
    }

    /// Would [`rollback_to`](Self::rollback_to) undo everything since `sp`
    /// **exactly** — leaving no page both reachable from a live root and back
    /// on offer as reusable? (#160)
    ///
    /// `rollback_to` restores root pointers and the allocator's private
    /// bookkeeping. That undoes a mutation of a page that was COMMITTED at
    /// savepoint time, because COW gave the statement a fresh page and the
    /// root restore drops it. It does NOT undo a mutation of a page that was
    /// already dirty: the page id never changes, so a child linked into it
    /// stays linked while the restore of `reusable` puts that child back on
    /// offer. The next allocation hands it out and two trees share a page —
    /// which the verifier reports as `double free` or `page reachable twice`.
    ///
    /// So the undo is exact iff either nothing was allocated since the
    /// savepoint, or the transaction was pristine when it was taken (every
    /// page the statement wrote it also allocated) and the extent allocator —
    /// which no savepoint covers — is still untouched.
    ///
    /// **This is about page identity, not about rows.** A caller that must
    /// also undo rows a statement already applied needs more than this; see
    /// [`savepoint_full`](Self::savepoint_full), or the ring's own predicate
    /// which pairs this with the executor's `partial` flag (#119).
    pub fn undo_is_exact(&self, sp: &TxnSavepoint) -> bool {
        // An in-place ADOPTION after the savepoint mutates a committed page
        // under its original id — the root restore cannot undo it, so it
        // voids BOTH exactness arguments below (the :memory: savepoint bug's
        // tripwire hole: the repro passed through each arm).
        self.adopts == sp.adopts
            && (self.allocs == sp.allocs || (sp.pristine && self.extents_untouched()))
    }

    pub fn extents_untouched(&self) -> bool {
        self.extent_map_root == self.meta.extent_map_root
            && self.pending_map_edits.is_empty()
            && self.freed_runs.is_empty()
            && self.taken_runs.is_empty()
            && self.extent_dirty.is_empty()
            && self.allocated_runs.is_empty()
            && self.run_pool.is_empty()
            && self.extent_buf.is_empty()
    }

    /// True iff this transaction has not mutated anything yet: no page is
    /// dirty and the extent allocator is untouched.
    ///
    /// The load-bearing consequence (design/DESIGN.md §5.3): a statement started
    /// from a pristine transaction can only ever mutate pages it COW-allocated
    /// itself, so restoring the root pointers with [`rollback_to`](Self::rollback_to)
    /// undoes it EXACTLY — even a statement that failed half-way through. From
    /// a non-pristine transaction that is no longer true (a page dirtied by an
    /// earlier statement is mutated in place, and the page id never changes).
    pub fn is_pristine(&self) -> bool {
        self.dirty.is_empty() && self.extents_untouched()
    }

    /// Discard EVERYTHING this transaction has done and return it to its
    /// just-begun state, **keeping the writer lock**.
    ///
    /// This is the batch leader's escape hatch (design/DESIGN.md §5.3): when an
    /// intent fails after partially applying itself, no savepoint can undo it
    /// (see [`savepoint_full`](Self::savepoint_full) for why), so the leader
    /// throws the whole round away and replays it with that intent's outcome
    /// pre-decided. Keeping the lock is the point — dropping and re-acquiring
    /// it would let another process lead and execute the very intents this
    /// leader is about to replay.
    ///
    /// Nothing committed is touched: by COW construction every page this txn
    /// wrote was allocated by it, so this is a pure discard. `high_water` is
    /// deliberately NOT rewound, for exactly the reason
    /// [`rollback_to`](Self::rollback_to) does not rewind it: pages minted from
    /// it belong to no committed freelist entry, so they go back to `reusable`
    /// and the commit fixpoint records them as freed, keeping page accounting
    /// exact.
    pub fn restart(&mut self) {
        debug_assert!(!self.finished);
        debug_assert!(!self.in_freelist_op);
        // In-place adoptions mutated committed pages under unchanged ids —
        // resetting the pointers alone would carry the discarded round's
        // mutations into the replay (and a replay re-adoption would then
        // overwrite the committed pre-images). Restore and drain them first.
        self.restore_inplace_undo();
        // A discarded round's journal layers describe pages the reset below
        // orphans; any savepoint token still naming them will fail its epoch
        // check rather than restore stale bytes.
        self.sp_layers.clear();
        self.catalog_root = self.meta.catalog_root;
        self.freelist_root = self.meta.freelist_root;
        self.extent_map_root = self.meta.extent_map_root;
        self.schema_gen_bump = false;
        // A DDL statement in the discarded round rebuilt this txn's captured
        // bundle from its own uncommitted catalog pages
        // (`reload_bundle_from_catalog`); those pages are gone now, so the view
        // goes back to the engine's committed one.
        self.bundle = self.eng.bundle();
        // Roots are a lazily-filled cache over the catalog tree, which the
        // restore above rewound; drop it so every entry reloads.
        self.table_roots.clear();
        self.dirty.clear();
        self.freed.clear();
        self.taken.clear();
        self.refill_cursor = None;
        self.reusable.clear();
        // ascending, so `reusable` stays sorted (binary-searched by the fixpoint)
        for id in self.meta.high_water..self.high_water {
            self.reusable.push(id);
        }
        self.run_pool.clear();
        self.taken_runs.clear();
        self.freed_runs.clear();
        self.allocated_runs.clear();
        self.parked_runs.clear();
        self.pending_map_edits.clear();
        self.extent_dirty.clear();
        self.extent_buf.clear();
        self.extent_buf_off = 0;
        self.bound_recomputed = false;
        self.written_tables = 0;
        self.mutated_tables.clear();
        // Rewound to the committed catalog, which the watermark records live in;
        // drop the cache so it reloads from that restored state.
        self.colwm_cache.clear();
        self.commit_point = None;
        self.work = WorkMeter::new(self.eng.work_budget());
    }

    /// A **full** savepoint for the SQL `SAVEPOINT` surface. It captures the
    /// accounting snapshot ([`savepoint`](Self::savepoint)) AND the CONTENTS of
    /// every page dirty at this instant, plus the few working-set scalars
    /// `rollback_to` does not restore.
    ///
    /// The plain [`savepoint`](Self::savepoint)/[`rollback_to`](Self::rollback_to)
    /// pair only reverts changes to pages that were COMMITTED at savepoint time
    /// (the COW allocates a fresh page, which the root-pointer restore drops).
    /// It does NOT revert an in-place mutation of a page that was ALREADY dirty
    /// (txn-local) at savepoint time — the page id never changes, so restoring
    /// root pointers leaves the mutated bytes in place. A SQL `ROLLBACK TO`
    /// must undo SUCCESSFUL statements, whose in-place mutations this method
    /// captures and [`rollback_to_full`](Self::rollback_to_full) restores.
    /// Heavier (it copies dirty-page bytes), which is why the cheap pair still
    /// exists — under the precondition [`undo_is_exact`](Self::undo_is_exact)
    /// states.
    ///
    /// This comment used to add that the cheap pair "is fine for the mirror,
    /// which only rolls back a FAILED op — a constraint violation fails before
    /// mutating, so there is nothing in-place to undo". **That is false**, and
    /// it is the same wrong reasoning #160 found in the ring: a failing insert
    /// COWs the leaf it is descending into, and from a dirty transaction that
    /// copy survives the root restore. `insert_row` does undo itself logically
    /// (`undo_partial_insert`), which is what makes the caller's `rollback_to`
    /// redundant there — and, being redundant, purely harmful. See #162.
    /// Since #N2 fase 4 the page contents are captured LAZILY: taking the
    /// savepoint pushes an empty pre-image journal layer, and `page_mut`
    /// records a dirty page's bytes into the innermost layer at its FIRST
    /// mutation after the savepoint (bytes-at-first-mutation ==
    /// bytes-at-savepoint, because `page_mut` is the only mutation road for
    /// dirty pages). The eager copy made Django's fixture shape — a big
    /// dirty set in the outer transaction, then a savepoint per test — pay
    /// O(dirty) per SAVEPOINT and per ROLLBACK TO (the facade's stack clone);
    /// the journal pays O(pages actually touched in the scope).
    pub fn savepoint_full(&mut self) -> Result<TxnSavepointFull> {
        let base = self.savepoint();
        let layer_epoch = self.sp_epoch_next;
        self.sp_epoch_next = self.sp_epoch_next.wrapping_add(1);
        self.sp_layers.push(SpLayer {
            epoch: layer_epoch,
            images: HashMap::new(),
        });
        if std::env::var_os("SP_TRACE").is_some() {
            eprintln!(
                "[sp] savepoint_full: pristine={} dirty={:?} layer_depth={} catalog_root={}",
                self.is_pristine(),
                {
                    let mut d: Vec<_> = self.dirty.iter().copied().collect();
                    d.sort_unstable();
                    d
                },
                self.sp_layers.len() - 1,
                self.catalog_root,
            );
        }
        Ok(TxnSavepointFull {
            base,
            layer_depth: self.sp_layers.len() - 1,
            layer_epoch,
            inplace_keys: self.inplace_undo.keys().copied().collect(),
            schema_gen_bump: self.schema_gen_bump,
            written_tables: self.written_tables,
            commit_point: self.commit_point,
            ext: ExtentSnapshot {
                map_root: self.extent_map_root,
                run_pool: self.run_pool.clone(),
                taken_runs: self.taken_runs.clone(),
                freed_runs: self.freed_runs.clone(),
                allocated_runs: self.allocated_runs.clone(),
                pending_map_edits: self.pending_map_edits.clone(),
                dirty: self.extent_dirty.clone(),
                parked: self.parked_runs.clone(),
                buf: self.extent_buf.clone(),
                buf_off: self.extent_buf_off,
            },
        })
    }

    /// Check a savepoint token against the live layer stack. A mismatch is a
    /// caller-stack/engine-layer desync, and acting on the wrong layer is
    /// silent corruption — the one thing this machinery exists to prevent.
    fn sp_layer_check(&self, sp: &TxnSavepointFull) -> Result<()> {
        if self.sp_layers.get(sp.layer_depth).map(|l| l.epoch) != Some(sp.layer_epoch) {
            return Err(Error::Internal(format!(
                "savepoint layer desync: token depth {} epoch {} vs {} live layers",
                sp.layer_depth,
                sp.layer_epoch,
                self.sp_layers.len(),
            )));
        }
        Ok(())
    }

    /// RELEASE (or plain disposal) of a [`savepoint_full`] marker: fold the
    /// released layers' pre-images into the enclosing layer and drop them.
    /// Everything from the token's depth OUTWARD is released together — the
    /// SQL rule, and the facade truncates its name stack the same way. The
    /// parent keeps its own image where both have one (the parent's is
    /// as-of the parent's savepoint, i.e. older); folding ascending makes
    /// the OLDEST released image win for pages only the children saw. With
    /// no parent the images die: the only remaining undo target is the
    /// whole-transaction abort, which restores COMMITTED state through the
    /// COW/in-place machinery and never needs mid-transaction bytes.
    pub fn release_savepoint_full(&mut self, sp: &TxnSavepointFull) -> Result<()> {
        self.sp_layer_check(sp)?;
        let released = self.sp_layers.split_off(sp.layer_depth);
        if let Some(parent) = self.sp_layers.last_mut() {
            for layer in released {
                for (id, img) in layer.images {
                    parent.images.entry(id).or_insert(img);
                }
            }
        } else {
            // No savepoint can roll back before their frees anymore — the
            // parked extent runs (N5) rejoin the allocatable pool.
            for (start, npages) in std::mem::take(&mut self.parked_runs) {
                self.pool_insert(start, npages, None);
            }
        }
        Ok(())
    }

    /// Roll back to a [`savepoint_full`](Self::savepoint_full): restore the
    /// accounting via [`rollback_to`](Self::rollback_to), then the captured
    /// dirty-page contents and working-set scalars.
    ///
    /// A large-value **extent** write in the scope is undone too: the whole
    /// allocator working set was captured at savepoint time and is restored
    /// here. The PAYLOAD BYTES already pwritten to the file are left where they
    /// are — those pages return to the pool or below `high_water` (the base
    /// savepoint's job) and nothing references them, exactly as for a
    /// rolled-back page allocation. This used to be a clean refusal, which was
    /// correct-but-narrow: on Linux `extent_threshold` is 4 KiB, so it fired
    /// for an ordinary text column, not just for "genuinely large blobs".
    /// Takes the savepoint by reference — the caller keeps it on its stack
    /// for a repeat `ROLLBACK TO` the same name (only the small accounting
    /// snapshot and the extent working set are cloned here; page contents
    /// live in the txn's own journal layers).
    pub fn rollback_to_full(&mut self, sp: &TxnSavepointFull) -> Result<()> {
        self.sp_layer_check(sp)?;
        self.rollback_to_inner(sp.base.clone());
        // The in-place third page state (see `TxnSavepointFull::inplace_keys`):
        // restore committed pre-images of pages adopted AFTER the savepoint,
        // and DROP their undo entries — a later re-adoption must snapshot the
        // committed bytes again, or a full abort would restore the leaked
        // state (the review's second-order finding). The journal may hold the
        // same pages (first post-adoption mutation snapshots the
        // still-committed bytes), so the replay below writes the same images
        // again — harmless by construction.
        let adopted_after: Vec<u64> = self
            .inplace_undo
            .keys()
            .copied()
            .filter(|id| !sp.inplace_keys.contains(id))
            .collect();
        for id in adopted_after {
            if let Some(bytes) = self.inplace_undo.remove(&id) {
                self.eng.shm.page_mut_unchecked(id)?.copy_from_slice(&bytes[..]);
            }
        }
        // Restore the extent allocator BEFORE the page images: the map's own
        // pages live in the page store, so the image restore must be the last
        // word on their contents.
        let ext = sp.ext.clone();
        self.extent_map_root = ext.map_root;
        self.run_pool = ext.run_pool;
        self.taken_runs = ext.taken_runs;
        self.freed_runs = ext.freed_runs;
        self.allocated_runs = ext.allocated_runs;
        self.pending_map_edits = ext.pending_map_edits;
        self.extent_dirty = ext.dirty;
        self.parked_runs = ext.parked;
        self.extent_buf = ext.buf;
        self.extent_buf_off = ext.buf_off;
        self.schema_gen_bump = sp.schema_gen_bump;
        self.written_tables = sp.written_tables;
        self.commit_point = sp.commit_point;
        // Replay the pre-image journals: layers ABOVE the target first (their
        // savepoints die with this rollback — the SQL rule, mirrored by the
        // caller's stack truncate), the target's own layer LAST, so for a
        // page journaled in several layers the OLDEST image is the final
        // word. The target's layer survives EMPTIED: a repeat ROLLBACK TO
        // replays only what gets touched after this one.
        while self.sp_layers.len() > sp.layer_depth + 1 {
            let layer = self.sp_layers.pop().expect("length checked");
            for (id, bytes) in layer.images {
                self.eng.shm.page_mut_unchecked(id)?.copy_from_slice(&bytes[..]);
            }
        }
        let images = std::mem::take(&mut self.sp_layers[sp.layer_depth].images);
        for (id, bytes) in images {
            self.eng.shm.page_mut_unchecked(id)?.copy_from_slice(&bytes[..]);
        }
        Ok(())
    }
}
