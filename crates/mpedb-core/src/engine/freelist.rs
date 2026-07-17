use super::*;

impl PageStore for WriteTxn<'_> {
    fn page(&self, id: u64) -> Result<&[u8]> {
        self.eng.shm.page(id)
    }

    fn page_mut(&mut self, id: u64) -> Result<&mut [u8]> {
        // COW discipline enforced in production, not just tests: committed
        // pages are immutable while readers may hold snapshots.
        if !self.dirty.contains(&id) {
            return Err(Error::Internal(format!(
                "page_mut on non-dirty page {id} (COW violation)"
            )));
        }
        self.eng.shm.page_mut_unchecked(id)
    }

    fn alloc(&mut self) -> Result<u64> {
        let id = self.alloc_id()?;
        self.dirty.insert(id);
        self.eng.shm.page_mut_unchecked(id)?.fill(0);
        Ok(id)
    }

    fn alloc_raw(&mut self) -> Result<u64> {
        // Identical to `alloc` minus the full-page fill(0). Split rather than
        // making `alloc` lazy: every other caller (btree nodes) relies on the
        // zeroed contract, and quietly weakening it for all of them to speed up
        // one path is how a subtle corruption gets introduced.
        let id = self.alloc_id()?;
        self.dirty.insert(id);
        Ok(id)
    }

    fn free(&mut self, id: u64) -> Result<()> {
        if self.dirty.remove(&id) {
            // allocated this txn: immediately reusable, invisible to readers.
            // Sorted insert — `reusable` is kept ordered so `freelist_plan` can
            // test membership by binary search instead of building a HashSet
            // per fixpoint pass (#37: that set was 14% of the write path).
            if let Err(at) = self.reusable.binary_search(&id) {
                self.reusable.insert(at, id);
            }
            return Ok(());
        }
        if !self.freed.insert(id) {
            return Err(Error::Internal(format!("double free of page {id}")));
        }
        Ok(())
    }

    fn is_dirty(&self, id: u64) -> bool {
        self.dirty.contains(&id)
    }
}

impl WriteTxn<'_> {
    /// Pick the next page id (freelist-reuse first, then high-water), without
    /// touching its contents. Shared by `alloc` and `alloc_raw`.
    fn alloc_id(&mut self) -> Result<u64> {
        // Draw a POOL, not one page's worth. Drawing is read-only and costs
        // nothing at commit unless the pages get consumed (see `taken`), so a
        // deep pool is free — and it is what keeps the fixpoint below from
        // running dry and minting high-water pages (#37).
        //
        // Still not during a freelist op: refill reads the tree with a cursor,
        // and mid-`btree::insert` `freelist_root` still points at the old,
        // intact tree — consistent to read, but the entry it would draw from
        // may be one the in-progress mutation is about to rewrite.
        if self.reusable.is_empty() && !self.in_freelist_op {
            // Only when DRY: `alloc_id` runs several times per txn, and topping
            // the pool up on each one would cost a tree descent per allocation.
            for _ in 0..FREELIST_POOL_DRAWS {
                if self.reusable.len() >= FREELIST_POOL_TARGET || !self.refill_reusable()? {
                    break;
                }
            }
        }
        let id = match self.reusable.pop() {
            Some(id) => {
                leakstat::bump(&leakstat::ALLOC_REUSABLE);
                id
            }
            None => {
                leakstat::bump(&leakstat::ALLOC_HW);
                if self.in_freelist_op {
                    leakstat::bump(&leakstat::ALLOC_HW_IN_FL);
                }
                // The top RESERVED_CONTROL_PAGES of the file are dispensed only
                // to `reserved_alloc` txns, so the mirror's control-plane writes
                // (HALTED/frozen/cursor/park markers) can still commit when the
                // data region is otherwise full (DESIGN-MIRROR §3.10). Data and
                // CDC capture use the normal ceiling and hit DbFull first.
                let ceiling = if self.reserved_alloc {
                    self.eng.shm.page_count
                } else {
                    self.eng
                        .shm
                        .page_count
                        .saturating_sub(RESERVED_CONTROL_PAGES)
                };
                if self.high_water >= ceiling {
                    return Err(Error::DbFull);
                }
                let id = self.high_water;
                self.high_water += 1;
                id
            }
        };
        Ok(id)
    }
}

impl<'e> WriteTxn<'e> {
    /// Pull one reclaimable freelist entry into `reusable`.
    ///
    /// Reusable iff its freeing txn is **at or below** the oldest-pinned bound.
    /// Not *strictly* below: that off-by-one is the one CLAUDE.md's invariant
    /// list calls out as causing an unbounded high-water leak, and this comment
    /// used to describe it — the code has always been right.
    ///
    /// ⚠ There is a *different* unbounded-high-water bug reachable from here
    /// under sustained concurrent churn; see
    /// `tests/high_water_leak.rs`.
    /// What this commit's freelist writes should be: `(key, ids)` per entry,
    /// where an EMPTY id list means "delete this key". Keys absent from the
    /// result are to be left exactly as they are.
    ///
    /// Two sources:
    ///
    /// 1. Every entry `refill_reusable` drew from, minus the pages that got
    ///    consumed. An entry nothing was allocated out of is **omitted** — it
    ///    still holds precisely what it held, so writing it would be pure churn.
    ///    That omission is what makes drawing a deep pool free, and it is the
    ///    whole fix for #37.
    /// 2. This txn's own free set, under `new_txn`: pages COWed away, plus any
    ///    page left in `reusable` that no drawn entry lists (this txn allocated
    ///    it from the high-water mark, or out of an entry it then drew dry, and
    ///    freed it again — nothing else records it, so omitting it would leak
    ///    it outright).
    pub(super) fn freelist_plan(&self, new_txn: u64, written: &[([u8; 11], Vec<u64>)]) -> Vec<([u8; 11], Vec<u64>)> {
        // `reusable` is sorted (see `free`/`refill_reusable`), so "is this page
        // still free?" is a binary search — no per-pass set to build.
        let mut out: Vec<([u8; 11], Vec<u64>)> = Vec::with_capacity(self.taken.len() + 1);
        for e in &self.taken {
            let kept: Vec<u64> = e
                .ids
                .iter()
                .copied()
                .filter(|id| self.reusable.binary_search(id).is_ok())
                .collect();
            // Once a pass has rewritten an entry it must stay in the plan
            // FOREVER, even if a later pass frees every page back and it looks
            // untouched again. Dropping it here would make the reconcile pass
            // below see a key in `written` that the plan no longer claims, and
            // delete it — with its pages listed nowhere. The page-accounting
            // verifier catches that as "page N leaked: neither reachable nor
            // freelisted", which is exactly how this was found.
            if kept.len() != e.ids.len() || written.iter().any(|(k, _)| *k == e.key) {
                out.push((e.key, kept)); // shrunk, or emptied = delete
            }
        }
        // A reusable page that some drawn entry still lists is accounted for by
        // that entry; everything else in the pool is this txn's own.
        //
        // freed and reusable are disjoint by construction — `free` routes a page
        // to exactly one of them, by whether this txn allocated it.
        let taken = &self.taken;
        let mut own: Vec<u64> = self.freed.iter().copied().collect();
        own.extend(
            self.reusable
                .iter()
                .copied()
                .filter(|id| !taken.iter().any(|e| e.ids.binary_search(id).is_ok())),
        );
        own.sort_unstable();
        debug_assert!(own.windows(2).all(|w| w[0] < w[1]), "own must be strictly ascending");
        for (i, chunk) in own.chunks(FREELIST_CHUNK_PAGES).enumerate() {
            out.push((freelist_key(new_txn, FK_PAGES, i as u16), chunk.to_vec()));
        }
        out
    }

    /// Draw one freelist entry's pages into `reusable` — **without removing the
    /// entry**. Returns whether an entry was drawn.
    ///
    /// Read-only, and that is the whole point (DESIGN.md §4.5). It used to
    /// `btree::delete` the entry, which made every page drawn a page the commit
    /// fixpoint had to write back (it records what is free, and a drawn page is
    /// listed nowhere else). That coupled the fixpoint's own page appetite to
    /// the pool it was handed: feeding it made it hungrier, which is why two
    /// separate attempts to feed it made #37 strictly worse. Leaving the entry
    /// in place decouples them — an entry nobody allocates out of costs nothing
    /// at commit.
    fn refill_reusable(&mut self) -> Result<bool> {
        debug_assert!(!self.in_freelist_op, "refill re-entered a freelist op");
        leakstat::bump(&leakstat::REFILL_CALLS);
        if self.freelist_root == 0 {
            leakstat::bump(&leakstat::REFILL_NO_TREE);
            return Ok(false);
        }
        // Start strictly after the last entry drawn: it is still in the tree,
        // so an inclusive scan would hand out its pages a second time.
        let lo = self.refill_cursor;
        let mut c = btree::cursor(
            self,
            self.freelist_root,
            lo.as_ref().map(|k| (k.as_slice(), false)),
            None,
        )?;
        let Some((key, val)) = c.next(self)? else {
            leakstat::bump(&leakstat::REFILL_NO_TREE);
            return Ok(false);
        };
        if key.len() != 11 || val.len() % 8 != 0 {
            return Err(Error::Corrupt("bad freelist entry".into()));
        }
        // Run entries (FK_RUNS) join the draw path with the extent allocator
        // (DESIGN-BLOBEXTENT §3.3); until then nothing writes them, so one in
        // the tree is corruption, not a version skew.
        if key[8] != FK_RUNS && key[8] != FK_PAGES {
            return Err(Error::Corrupt("bad freelist entry kind".into()));
        }
        if key[8] == FK_RUNS {
            return Err(Error::Corrupt(
                "run freelist entry before the extent allocator exists".into(),
            ));
        }
        let freed_txn = u64::from_be_bytes(key[..8].try_into().unwrap());
        // Pages freed BY commit T are referenced only by snapshots < T (commit
        // T is what replaced them), so they are reusable iff T <= oldest pin.
        let mut bound = self
            .eng
            .shm
            .oldest_pinned_cache()
            .load(std::sync::atomic::Ordering::Acquire);
        if freed_txn > bound && !self.bound_recomputed {
            // the cached bound is stale-conservative; recompute once per txn
            self.bound_recomputed = true;
            leakstat::bump(&leakstat::RECOMPUTES);
            bound = self.eng.shm.compute_oldest_pinned(self.meta.txn_id);
        }
        if freed_txn > bound {
            // Entries are keyed by freeing txn, so this one is the oldest
            // drawable and nothing behind it can be older. Stop.
            leakstat::bump(&leakstat::REFILL_NOT_YET);
            return Ok(false);
        }
        // Validate every id: corrupt freelist bytes must never let alloc
        // zero-fill meta/lock/reader pages (page_mut_unchecked only
        // bounds-checks the upper end).
        let mut ids = Vec::with_capacity(val.len() / 8);
        for chunk in val.chunks_exact(8) {
            let id = u64::from_le_bytes(chunk.try_into().unwrap());
            if id < self.eng.shm.data_start || id >= self.eng.shm.page_count {
                return Err(Error::Corrupt(format!(
                    "freelist lists page {id} outside the data region"
                )));
            }
            // Entries are written sorted, and `freelist_plan` binary-searches
            // them. Enforce it rather than assume it: an unsorted value would
            // silently mis-answer "is this page still listed here?", which
            // double-allocates or leaks instead of failing.
            if ids.last().is_some_and(|&prev| prev >= id) {
                return Err(Error::Corrupt(
                    "freelist entry is not strictly ascending".into(),
                ));
            }
            ids.push(id);
        }
        leakstat::bump(&leakstat::REFILL_OK);
        leakstat::add(&leakstat::REFILL_PAGES, ids.len() as u64);
        let key: [u8; 11] = key.try_into().expect("checked len == 11 above");
        self.reusable.extend(ids.iter().copied());
        self.reusable.sort_unstable();
        self.taken.push(TakenEntry { key, ids });
        self.refill_cursor = Some(key);
        Ok(true)
    }
}

/// One freelist entry `refill_reusable` drew pages from. The entry is still in
/// the tree with `ids` still listed; the commit fixpoint rewrites it with
/// whatever is left unconsumed (or deletes it if nothing is).
#[derive(Clone)]
pub(super) struct TakenEntry {
    /// Freelist keys are always (txn BE, chunk BE) — exactly 10 bytes. Inline,
    /// not a `Vec`: the commit fixpoint rebuilds its plan on every pass, and a
    /// heap allocation per key per pass was a measurable slice of the write path.
    key: [u8; 11],
    ids: Vec<u64>,
}
