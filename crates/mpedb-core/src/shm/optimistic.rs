//! Shared-memory optimistic operations; ordering and file layout are unchanged.
use super::*;

impl Shm {
    // ---------- committed-footprint ring (optimistic concurrency) ----------

    #[inline]
    pub(super) fn opt_field(&self, txn_id: u64, field: usize) -> &AtomicU64 {
        let slot = (txn_id % OPT_RING_SLOTS) as usize;
        self.atomic_u64(
            opt_ring_start_page(self.max_readers) as usize * PAGE_SIZE
                + slot * OPT_RING_ENTRY
                + field,
        )
    }

    /// *Writer-lock holder.* Record this commit's footprint BEFORE the meta
    /// flip. `kind`/`table_bits`/`key_hash` describe what txn `txn_id` wrote.
    /// The txn_id field is stored LAST (Release) so a reader that sees it can
    /// trust the payload; in practice every read is also serialized by the
    /// writer mutex, which is a full barrier.
    pub fn opt_record(&self, txn_id: u64, fp: &CommitFootprint<'_>) {
        let CommitFootprint { kind, table_bits, key_hash, cols, keys, range, delta } = *fp;
        self.opt_field(txn_id, OFP_COLS).store(cols, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_KIND).store(kind, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_TBITS).store(table_bits, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_KHASH).store(key_hash, Ordering::Relaxed);
        // The exact set, when this commit could name it. `nkeys = 0` is the
        // fail-safe reading: no exact set, use the summary. It is therefore
        // also what a commit with too many keys, or with none it could name,
        // publishes — and what a torn or unwritten slot reads as.
        let n = match keys {
            Some(k) if !k.is_empty() && k.len() <= OFP_MAX_KEYS => k.len(),
            _ => 0,
        };
        for (i, h) in keys.unwrap_or(&[]).iter().take(n).enumerate() {
            self.opt_field(txn_id, OFP_KEYS + i * 8).store(*h, Ordering::Relaxed);
        }
        self.opt_field(txn_id, OFP_NKEYS).store(n as u64, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_RANGE).store(range, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_DELTA).store(delta as u64, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_TXN).store(txn_id, Ordering::Release);
    }

    /// The exact key set commit `t` published, or `None` when it had none.
    /// A count outside `1..=OFP_MAX_KEYS` reads as "none" rather than being
    /// trusted — the same bounds-checking every other decoder here does.
    pub(super) fn opt_keys(&self, t: u64) -> Option<[u64; OFP_MAX_KEYS]> {
        let n = self.opt_field(t, OFP_NKEYS).load(Ordering::Relaxed) as usize;
        if n == 0 || n > OFP_MAX_KEYS {
            return None;
        }
        let mut out = [0u64; OFP_MAX_KEYS];
        for (i, slot) in out.iter_mut().enumerate().take(n) {
            *slot = self.opt_field(t, OFP_KEYS + i * 8).load(Ordering::Relaxed);
        }
        // Unused tail stays 0, which `key_hash` never produces (`h | 1`), so a
        // padded array can be compared without carrying the length.
        Some(out)
    }

    /// *Writer-lock holder.* First-committer-wins conflict test for an
    /// optimistic write of `(table_id, key_hash)` prepared against snapshot
    /// `snap_txn`, committing under a lock whose newest committed txn is
    /// `current_txn`. Returns true (conflict) if any committed txn in
    /// `(snap_txn, current_txn]` wrote our table at table granularity, wrote
    /// our exact key at point granularity, OR is missing from the ring (an
    /// overwritten/too-old window, or a foreign serial-mode committer's gap —
    /// both handled conservatively). Sound by construction: it never returns
    /// false when a real conflicting commit exists in the window.
    pub fn opt_conflict(
        &self,
        snap_txn: u64,
        current_txn: u64,
        table_id: u32,
        key_hash: u64,
    ) -> bool {
        if current_txn <= snap_txn {
            return false; // nothing committed since our snapshot
        }
        if current_txn - snap_txn > OPT_RING_SLOTS {
            return true; // snapshot older than the ring can witness: conservative
        }
        // `& 63`: the OFP ring is a u64 bitmap even though footprints are sparse
        // and MAX_TABLES is 4096, so ids fold mod 64. Sound (same table → same
        // bit → real conflicts always caught); aliasing only ever adds false
        // positives (see commit.rs and DESIGN-TABLE-CAP §5).
        let my_bit = 1u64 << (table_id & 63);
        for t in (snap_txn + 1)..=current_txn {
            if self.opt_field(t, OFP_TXN).load(Ordering::Acquire) != t {
                return true; // gap / overwritten / foreign writer: conservative
            }
            let kind = self.opt_field(t, OFP_KIND).load(Ordering::Relaxed);
            let tbits = self.opt_field(t, OFP_TBITS).load(Ordering::Relaxed);
            match kind {
                OFP_KIND_EMPTY => {}
                OFP_KIND_TABLE => {
                    if tbits & my_bit != 0 {
                        return true;
                    }
                }
                OFP_KIND_SHARD => {
                    // A shard-scoped commit cannot be compared against one bare
                    // key: conservative.
                    if tbits & my_bit != 0 {
                        return true;
                    }
                }
                OFP_KIND_REGION => {
                    // Region summary (#143): our one key conflicts only if its
                    // bit is in the committed summary.
                    if tbits & my_bit != 0
                        && self.opt_field(t, OFP_KHASH).load(Ordering::Relaxed)
                            & Self::region_bit(key_hash)
                            != 0
                    {
                        return true;
                    }
                }
                OFP_KIND_POINT => {
                    // Conflict only on the same table AND the same key.
                    if tbits & my_bit != 0
                        && self.opt_field(t, OFP_KHASH).load(Ordering::Relaxed) == key_hash
                    {
                        return true;
                    }
                }
                // **Review finding R2.** This arm used to be the POINT arm, so
                // any kind this reader did not know was silently read AS a
                // point — and POINT is the one kind that can answer "no
                // conflict". A future kind would therefore have its `khash`
                // compared for equality against a key hash, almost never
                // match, and produce a MISSED conflict: the wrong-answer
                // direction, from nothing worse than adding a variant.
                //
                // #143 and #144 both added kinds, and both had to remember to
                // add an arm here. The next one must not have to.
                _ => {
                    if tbits & my_bit != 0 {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// *Writer-lock holder.* Carry a pending sub-edit's offset forward across
    /// the commits it never saw, and say whether it genuinely collides (#151).
    ///
    /// The caller decided `at..at+len` against snapshot `snap`. Every commit in
    /// `(snap, current]` that touched the same cell either overlaps that range —
    /// a real collision, the two edits are about the same bytes — or lies
    /// entirely before or after it, in which case it only *moves* the offset.
    /// One walk answers both, because the two questions are the same walk:
    ///
    /// ```text
    /// for t in snap+1 ..= current, touching this cell, with range [lo, hi) and delta d:
    ///     if [at, at+len) overlaps [lo, hi)   -> collision
    ///     else if lo <= at                    -> at += d      (they were before me)
    ///     else                                -> unchanged    (they were after me)
    /// ```
    ///
    /// **The coordinate systems line up because the walk is in order.** `lo` is
    /// in the value's coordinates as of `t`, and `at` has already absorbed every
    /// shift from `t' < t`, so it is in those coordinates too. Comparing them
    /// out of order, or rebasing before comparing, would silently mix two
    /// different rulers.
    ///
    /// `None` means the question is unanswerable — the window is older than the
    /// ring can witness, or a slot is missing — and the caller must refuse
    /// rather than guess, which is the same fail-safe direction every other
    /// limit here takes.
    pub fn opt_rebase(&self, snap: u64, current: u64, cell: &CellEdit) -> Option<RebaseOutcome> {
        let CellEdit { table_bit: my_table_bit, key: my_key, col_bit: my_col_bit, at, len } =
            *cell;
        if current <= snap {
            return Some(RebaseOutcome::At(at));
        }
        if current - snap > OPT_RING_SLOTS {
            return None;
        }
        let mut at = at;
        for t in (snap + 1)..=current {
            if self.opt_field(t, OFP_TXN).load(Ordering::Acquire) != t {
                return None;
            }
            let kind = self.opt_field(t, OFP_KIND).load(Ordering::Relaxed);
            if kind == OFP_KIND_EMPTY {
                continue;
            }
            if self.opt_field(t, OFP_TBITS).load(Ordering::Relaxed) & my_table_bit == 0 {
                continue;
            }
            // A different row of the same table cannot have moved my bytes. Only
            // an EXACT key set can prove that; without one, assume it might have.
            if let Some(theirs) = self.opt_keys(t) {
                if !theirs.contains(&my_key) {
                    continue;
                }
            }
            if self.opt_field(t, OFP_COLS).load(Ordering::Relaxed) & my_col_bit == 0 {
                continue;
            }
            let packed = self.opt_field(t, OFP_RANGE).load(Ordering::Relaxed);
            if packed == RANGE_ANY {
                // They rewrote the whole value; there is no offset to carry
                // forward through that.
                return Some(RebaseOutcome::Collision);
            }
            let (lo, hi) = ((packed >> 32) & 0xffff_ffff, packed & 0xffff_ffff);
            // Half-open overlap. Two zero-width inserts at one point do NOT
            // overlap: both texts land, and the order is the commit order —
            // which is the answer a collaborative editor wants anyway.
            if at < hi && lo < at + len {
                return Some(RebaseOutcome::Collision);
            }
            if lo <= at {
                let d = self.opt_field(t, OFP_DELTA).load(Ordering::Relaxed) as i64;
                at = at.checked_add_signed(d)?;
            }
        }
        Some(RebaseOutcome::At(at))
    }

    /// *Writer-lock holder.* [`Self::opt_conflict`] for a SET of tables at once
    /// (#142 G1): did any commit in `(snap_txn, current_txn]` write a table in
    /// `table_bits`?
    ///
    /// This is the guard's whole engine. It is deliberately a single walk of
    /// the ring for the entire surface rather than one walk per table — a
    /// guarded action names the union of its statements' footprints, so the
    /// per-table loop would re-read the same 64 entries once per table for no
    /// new information.
    ///
    /// No key refinement: a POINT commit is treated as touching its table.
    /// Refining it would need the guard to carry a key per table, and the
    /// surface a multi-statement action names is a set of tables, not a set of
    /// keys. Coarse in the safe direction — a false conflict costs a retry.
    ///
    /// `why` receives the reason when the answer is a conservative yes, so the
    /// caller can count how often each of the ring's three limits actually
    /// bites instead of guessing (`GuardStats`).
    pub fn opt_conflict_set(
        &self,
        snap_txn: u64,
        current_txn: u64,
        surface: &GuardSurface,
        why: &mut GuardVerdict,
    ) -> bool {
        let GuardSurface { tables: table_bits, regions, ref keys, shard, cols, range } = *surface;
        if current_txn <= snap_txn {
            return false; // nothing committed since our snapshot
        }
        if current_txn - snap_txn > OPT_RING_SLOTS {
            // The caller thought for longer than the ring can witness. Not a
            // real conflict — an unanswerable question, answered safely.
            *why = GuardVerdict::SnapshotTooOld;
            return true;
        }
        for t in (snap_txn + 1)..=current_txn {
            if self.opt_field(t, OFP_TXN).load(Ordering::Acquire) != t {
                *why = GuardVerdict::RingGap;
                return true; // gap / overwritten / foreign writer: conservative
            }
            let kind = self.opt_field(t, OFP_KIND).load(Ordering::Relaxed);
            if kind == OFP_KIND_EMPTY {
                continue; // catalog/sys only — touched no user table
            }
            let tbits = self.opt_field(t, OFP_TBITS).load(Ordering::Relaxed);
            if tbits & table_bits == 0 {
                continue; // disjoint tables — nothing further to ask
            }
            // Same table(s). Whether that is a real conflict now depends on the
            // KEYS, and this is the whole of #143: without it, two workers
            // writing different rows of one table conflicted on every commit.
            let overlap = match kind {
                // #144: both sides name a shard ⇒ they conflict only if it is
                // the SAME shard. This is the whole per-user story, and it is a
                // u64 compare, so the user count never enters the cost.
                OFP_KIND_SHARD => match shard {
                    Some(mine) => {
                        self.opt_field(t, OFP_KHASH).load(Ordering::Relaxed) == mine
                    }
                    // We cannot name a shard, so we cannot prove we are outside
                    // theirs.
                    None => true,
                },
                // A committer that named rows and a guard that named a shard
                // are not comparable. Conservative rather than clever: mixing
                // the two modes is the exception, and guessing across them
                // would be guessing about correctness.
                _ if shard.is_some() => true,
                OFP_KIND_REGION | OFP_KIND_POINT => {
                    // #149: exact first. When BOTH sides could name their keys
                    // the answer is a set intersection and there are no false
                    // positives at all — which is the common case, because
                    // most actions touch a handful of rows by primary key.
                    //
                    // The Bloom below is the fallback for when either side gave
                    // up on exactness, and its cost is why this arm exists: at
                    // k = 1 over 64 bits, eight distinct keys collide ~35 % of
                    // the time, and a collision is a permanent conflict between
                    // two rows that have nothing to do with each other.
                    match (keys.as_deref(), self.opt_keys(t)) {
                        (Some(mine), Some(theirs)) => {
                            mine.iter().any(|m| theirs.contains(m))
                        }
                        _ if kind == OFP_KIND_POINT => {
                            Self::region_bit(
                                self.opt_field(t, OFP_KHASH).load(Ordering::Relaxed),
                            ) & regions
                                != 0
                        }
                        _ => self.opt_field(t, OFP_KHASH).load(Ordering::Relaxed) & regions != 0,
                    }
                }
                // TABLE says "any keys": the committer could not name where it
                // landed, so it must be taken to have landed anywhere.
                _ => true,
            };
            if !overlap {
                continue;
            }
            // #146 K1, checked LAST because it is the only test that can clear
            // an otherwise-overlapping pair: same table, same key, same shard,
            // but different COLUMNS. A move writes `ord` while an edit writes
            // `body`, and that is what lets the edit follow the move.
            //
            // `COLS_ANY` on either side means "could be any column", so the
            // masks always intersect and the pair conflicts — which is the
            // pre-#146 behaviour and the direction anything unnameable takes.
            if self.opt_field(t, OFP_COLS).load(Ordering::Relaxed) & cols == 0 {
                continue;
            }
            // #151, checked LAST because it is the finest: same table, same
            // key, same shard, same column — but a different part of the VALUE.
            // Four editors in four paragraphs of one `body` are four writes to
            // one cell to every coarser rule, and `splice()` makes them compose
            // at the storage layer; this is what stops the guard refusing them.
            //
            // **The rule is `theirs.lo >= mine.hi`, not mere disjointness**, and
            // the asymmetry is the whole subtlety. A splice applies to the value
            // as it stands at write time, so a commit that landed BEFORE mine and
            // BEGAN before my range has shifted the bytes my offset was computed
            // against — my `at` no longer means what it meant. Their edit
            // starting at or after my range ends is the one case that can move
            // neither my bytes nor my offsets.
            //
            // So an edit near the start of a cell does invalidate pending edits
            // after it. Removing that costs a per-cell edit history and
            // `subedit::Splice::rebase` at execution; refusing is the fail-safe
            // direction and returns `Lost`, which is exactly the signal a client
            // needs to rebase and resubmit.
            match range {
                // The rebase walk saw the same window under the same writer
                // lock and already cleared it.
                RangeClaim::Settled => continue,
                RangeClaim::Bytes(_, my_hi) => {
                    let theirs = self.opt_field(t, OFP_RANGE).load(Ordering::Relaxed);
                    if theirs != RANGE_ANY && ((theirs >> 32) as u32) >= my_hi {
                        continue;
                    }
                }
                RangeClaim::Whole => {}
            }
            *why = GuardVerdict::Overlap;
            return true;
        }
        false
    }


}
