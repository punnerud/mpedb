//! Extent allocation — DESIGN-BLOBEXTENT §3.3/§4/§6, the write half.
//!
//! A writer's runs live in a PRIVATE pool with per-run PROVENANCE: which
//! drawn freelist entry a run came from, or `None` for runs this txn created
//! (a high-water allocation freed back, or a drawn allocation freed back —
//! the ATTRIBUTION RULE: *consumed = ever allocated out of the entry*, so a
//! same-txn free never returns to the drawn entry's write-back, which is what
//! keeps write-back values a shrinking subset and the commit fixpoint's
//! termination argument intact).
//!
//! Payload is `pwrite`n through the FILE, never through the mapping — no
//! fault-and-zero — and always BEFORE the 20-byte reference reaches the tree
//! (payload-before-reference, the crash argument). The ranges are remembered
//! in `extent_dirty` so commit can range-sync them: on Linux the pre-flip
//! barrier is a no-op and these msyncs ARE the durability (review finding 1).

use super::*;

/// One run in the writer's private pool, with provenance for the fixpoint.
#[derive(Debug, Clone, Copy)]
pub(super) struct PoolRun {
    pub start: u64,
    pub npages: u32,
    /// `Some(i)` = still attributed to `taken_runs[i]` (never allocated out
    /// of it); `None` = this txn's own creation.
    pub from: Option<usize>,
}

/// One deferred extent-map edit, replayed chronologically at commit.
#[derive(Debug, Clone, Copy)]
pub(super) enum MapEdit {
    Insert(u64, u32),
    Delete(u64),
}

/// A drawn kind-1 freelist entry: left in the tree (draw is read-only, the
/// #37 rule), struck out at commit for exactly what was consumed.
#[derive(Debug, Clone)]
pub(super) struct TakenRunEntry {
    pub key: [u8; 11],
    /// The entry's runs as drawn — write-back is the pool's remaining runs
    /// with this provenance, and equality against this decides whether the
    /// entry was touched at all (untouched entries are OMITTED, the #37 rule).
    pub runs: Vec<(u64, u32)>,
}

/// Decode + validate a kind-1 entry's `(start ‖ npages)` pairs: strictly
/// ascending, non-overlapping, inside the data region. A corrupt run is how a
/// pwrite would land on the lock pages — same rule `refill_reusable` enforces
/// for bare page ids.
pub(super) fn decode_run_entry(
    val: &[u8],
    data_start: u64,
    page_count: u64,
) -> Result<Vec<(u64, u32)>> {
    if val.is_empty() || !val.len().is_multiple_of(12) {
        return Err(Error::Corrupt("bad run freelist entry".into()));
    }
    let mut runs = Vec::with_capacity(val.len() / 12);
    let mut prev_end = 0u64;
    for ch in val.chunks_exact(12) {
        let start = u64::from_le_bytes(ch[0..8].try_into().unwrap());
        let npages = u32::from_le_bytes(ch[8..12].try_into().unwrap());
        if npages == 0 {
            return Err(Error::Corrupt("empty run in freelist entry".into()));
        }
        let end = start
            .checked_add(u64::from(npages))
            .ok_or_else(|| Error::Corrupt("run overflows the page space".into()))?;
        if start < data_start || end > page_count {
            return Err(Error::Corrupt(format!(
                "freelist run {start}+{npages} outside the data region"
            )));
        }
        if start < prev_end || (prev_end != 0 && start == 0) {
            return Err(Error::Corrupt(
                "freelist runs not strictly ascending/disjoint".into(),
            ));
        }
        // Strictly ascending also rules out duplicates.
        if start == prev_end && prev_end != 0 {
            // adjacent is legal (different commits may not have coalesced);
            // overlap is not — start >= prev_end holds here by the check above
        }
        prev_end = end;
        runs.push((start, npages));
    }
    Ok(runs)
}

/// Encode pairs for a kind-1 entry value.
pub(super) fn encode_run_entry(runs: &[(u64, u32)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(runs.len() * 12);
    for &(start, npages) in runs {
        v.extend_from_slice(&start.to_le_bytes());
        v.extend_from_slice(&npages.to_le_bytes());
    }
    v
}

/// ≤ 960 B inline cap ⇒ at most 80 pairs per entry (the freelist house rule).
pub(super) const RUNS_PER_CHUNK: usize = 80;

impl WriteTxn<'_> {
    /// Allocate a run of `npages`. First-fit over the private pool — allowed
    /// to SPAN adjacent pool runs (draw-time contiguity across entries, the
    /// grow-by-append mitigation) — then refill from drawn kind-1 entries,
    /// then bump the logical high water. The provenance of every consumed
    /// piece is recorded by REMOVING it from the pool: the fixpoint's
    /// write-back for a drawn entry is exactly the pool's remaining runs with
    /// that provenance.
    pub(super) fn alloc_run(&mut self, npages: u32) -> Result<u64> {
        debug_assert!(npages > 0);
        loop {
            if let Some(start) = self.pool_alloc(npages) {
                self.allocated_runs.insert(start, npages);
                return Ok(start);
            }
            // Draw one more freelist entry (pages OR runs — the shared refill
            // walk dispatches on kind); when it yields nothing more, fall
            // back to high water.
            if self.in_freelist_op || !self.refill_reusable()? {
                break;
            }
        }
        // High-water fallback: page-aligned bump inside the format-time
        // preallocated file. The fragmented-full error says WHY (Q4).
        let start = self.high_water;
        let end = start
            .checked_add(u64::from(npages))
            .ok_or_else(|| Error::Internal("extent size overflow".into()))?;
        if end > self.eng.shm.page_count {
            // Q4: the operator learns WHY — fragmentation is actionable
            // (churn pattern), plain fullness is not. DbFull stays the
            // variant callers match on; the detail rides in the source note.
            let frag: u64 = self.run_pool.iter().map(|r| u64::from(r.npages)).sum();
            leakstat::add(&leakstat::EXTENT_FRAG_PAGES, frag);
            return Err(Error::DbFull);
        }
        self.high_water = end;
        self.allocated_runs.insert(start, npages);
        Ok(start)
    }

    /// First-fit over the pool, spanning adjacent runs. Consumes the pieces
    /// it uses (splitting the last one), keeps every piece's provenance.
    fn pool_alloc(&mut self, npages: u32) -> Option<u64> {
        let need = u64::from(npages);
        // Pool is kept sorted by start. Find a maximal contiguous window.
        let mut i = 0;
        while i < self.run_pool.len() {
            let win_start = self.run_pool[i].start;
            let mut covered = 0u64;
            let mut j = i;
            while j < self.run_pool.len()
                && self.run_pool[j].start == win_start + covered
                && covered < need
            {
                covered += u64::from(self.run_pool[j].npages);
                j += 1;
            }
            if covered >= need {
                // Consume [i, j): whole runs, except the last may split.
                let mut remaining = need;
                let mut k = i;
                while remaining > 0 {
                    let run = self.run_pool[k];
                    let take = remaining.min(u64::from(run.npages)) as u32;
                    if take == run.npages {
                        self.run_pool.remove(k);
                    } else {
                        // split: the tail keeps the run's provenance
                        self.run_pool[k].start += u64::from(take);
                        self.run_pool[k].npages -= take;
                        k += 1;
                    }
                    remaining -= u64::from(take);
                }
                return Some(win_start);
            }
            i = j.max(i + 1);
        }
        None
    }

    /// Return a run to the pool with the given provenance, keeping the pool
    /// sorted (insertion by start).
    pub(super) fn pool_insert(&mut self, start: u64, npages: u32, from: Option<usize>) {
        let pos = self
            .run_pool
            .partition_point(|r| r.start < start);
        self.run_pool.insert(pos, PoolRun { start, npages, from });
    }

    /// The kind-1 side of the commit fixpoint, computed ONCE per commit: the
    /// run pool never changes shape inside `in_freelist_op` (refill is
    /// blocked and page allocation does not draw from runs), so unlike the
    /// page plan this needs no iteration — it is applied before the page
    /// loop and stays constant across its passes.
    pub(super) fn apply_run_plan(&mut self, new_txn: u64) -> Result<()> {
        debug_assert!(self.in_freelist_op);
        // Write-back per drawn entry: the pool runs still attributed to it.
        for i in 0..self.taken_runs.len() {
            let kept: Vec<(u64, u32)> = self
                .run_pool
                .iter()
                .filter(|r| r.from == Some(i))
                .map(|r| (r.start, r.npages))
                .collect();
            if kept == self.taken_runs[i].runs {
                continue; // untouched — leave it exactly as it is
            }
            let key = self.taken_runs[i].key;
            let fl_root = self.freelist_root;
            self.freelist_root = if kept.is_empty() {
                btree::delete(self, fl_root, &key)?.new_root
            } else {
                let val = encode_run_entry(&kept);
                btree::insert(
                    self,
                    fl_root,
                    &key,
                    &mut btree::Payload::Flat(&val),
                    InsertMode::Upsert,
                )?
                .new_root
            };
        }
        // This commit's own runs: committed extents freed by this txn, plus
        // pool runs with `from: None` (this txn's creations, freed back) —
        // nothing else records either, so omitting them leaks them outright.
        let mut own: Vec<(u64, u32)> = self.freed_runs.clone();
        own.extend(
            self.run_pool
                .iter()
                .filter(|r| r.from.is_none())
                .map(|r| (r.start, r.npages)),
        );
        own.sort_unstable();
        // Coalesce adjacent runs within the commit's set (§3.3) — cheap here,
        // and it is the only coalescing the ON-DISK format ever gets.
        let mut merged: Vec<(u64, u32)> = Vec::with_capacity(own.len());
        for (start, npages) in own {
            match merged.last_mut() {
                Some((ps, pn)) if *ps + u64::from(*pn) == start => {
                    *pn = pn
                        .checked_add(npages)
                        .ok_or_else(|| Error::Internal("run coalesce overflow".into()))?;
                }
                _ => merged.push((start, npages)),
            }
        }
        for (i, chunk) in merged.chunks(RUNS_PER_CHUNK).enumerate() {
            let key = super::freelist_key(new_txn, super::FK_RUNS, i as u16);
            let val = encode_run_entry(chunk);
            let fl_root = self.freelist_root;
            self.freelist_root = btree::insert(
                self,
                fl_root,
                &key,
                &mut btree::Payload::Flat(&val),
                InsertMode::Upsert,
            )?
            .new_root;
        }
        Ok(())
    }

    /// Apply the deferred extent-map edits — the ONE place the map mutates
    /// (never from inside a data-tree operation).
    pub(super) fn apply_map_edits(&mut self) -> Result<()> {
        for edit in std::mem::take(&mut self.pending_map_edits) {
            match edit {
                MapEdit::Insert(start, npages) => {
                    let key = start.to_be_bytes();
                    let val = npages.to_le_bytes();
                    let root = self.extent_map_root;
                    let out = btree::insert(
                        self,
                        root,
                        &key,
                        &mut btree::Payload::Flat(&val),
                        InsertMode::InsertOnly,
                    )?;
                    if out.existed {
                        return Err(Error::Internal(format!(
                            "extent {start} double-mapped"
                        )));
                    }
                    self.extent_map_root = out.new_root;
                }
                MapEdit::Delete(start) => {
                    let key = start.to_be_bytes();
                    let root = self.extent_map_root;
                    let out = btree::delete(self, root, &key)?;
                    if !out.existed {
                        return Err(Error::Corrupt(format!(
                            "freed extent {start} was not in the extent map"
                        )));
                    }
                    self.extent_map_root = out.new_root;
                }
            }
        }
        Ok(())
    }

    /// Write an extent's payload through the FILE at its run, zero-filling
    /// the tail of the last page (deterministic bytes — commitment 4), and
    /// remember the range for commit's range-sync.
    pub(super) fn write_extent_payload(
        &mut self,
        start_page: u64,
        npages: u32,
        pieces: &[&[u8]],
        total_len: u64,
    ) -> Result<()> {
        let mut off = start_page
            .checked_mul(PAGE_SIZE as u64)
            .ok_or_else(|| Error::Internal("extent offset overflow".into()))?;
        let mut written = 0u64;
        for p in pieces {
            self.eng.shm.file_write_at(p, off)?;
            off += p.len() as u64;
            written += p.len() as u64;
        }
        if written != total_len {
            return Err(Error::Internal(
                "extent payload length disagrees with its pieces".into(),
            ));
        }
        let cap = u64::from(npages) * PAGE_SIZE as u64;
        debug_assert!(total_len <= cap && cap - total_len < PAGE_SIZE as u64);
        if total_len < cap {
            // The tail is small (< one page): a stack buffer of zeros.
            let zeros = [0u8; 4096];
            debug_assert_eq!(PAGE_SIZE, zeros.len());
            self.eng
                .shm
                .file_write_at(&zeros[..(cap - total_len) as usize], off)?;
        }
        self.extent_dirty.push((start_page, npages));
        Ok(())
    }
}
