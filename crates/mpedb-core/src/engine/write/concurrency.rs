//! Write transaction concurrency operations.
use super::*;

impl<'e> WriteTxn<'e> {
    // ---------- columnar watermark (DESIGN-COLUMNAR §7) ----------

    /// Record that a row with encoded PK `key` was mutated in `table_id`, so the
    /// columnar watermark can be invalidated if that row is COVERED by segments.
    ///
    /// Called from every row mutator (insert/update/delete, typed and blind) at
    /// its success point — `set_tree_root` cannot host this because it does not
    /// have the PK, and the whole discrimination here is PK ≤ watermark. A
    /// mutation at or below the watermark changes one of the segments' first `W`
    /// rows, so the segment/row-tail split scan can no longer trust them: the
    /// watermark record is DELETED (the scan then declines to the row scan). A
    /// mutation ABOVE the watermark is an append — the segments still describe
    /// the first `W` rows exactly, and the row-tail fold re-reads everything past
    /// the watermark fresh — so the record is left intact and the append is free.
    ///
    /// Cost for the common case (no watermark): one point `sys_get` on the first
    /// mutation of the table, cached `None` for the rest of the txn.
    pub(super) fn note_columnar_mutation(&mut self, table_id: u32, key: &[u8]) -> Result<()> {
        if !self.colwm_cache.contains_key(&table_id) {
            let wm = self
                .sys_get(&colwm_subkey(table_id))?
                .and_then(|v| decode_colwm(&v).map(|(_, _, pk)| pk.to_vec()));
            self.colwm_cache.insert(table_id, wm);
        }
        // A malformed empty PK (never written by compaction) is treated as
        // "covers everything" so it is deleted on the first mutation — the
        // conservative direction is always to invalidate.
        let covered = match self.colwm_cache.get(&table_id) {
            Some(Some(wm)) => wm.is_empty() || key <= wm.as_slice(),
            _ => false,
        };
        if covered {
            // Idempotent (see the `colwm_cache` field doc): a savepoint may have
            // restored a record this txn already deleted, so we do NOT drop the
            // cache entry — the next covered mutation re-deletes it.
            self.sys_delete(&colwm_subkey(table_id))?;
        }
        Ok(())
    }

    /// The table's data-modification generation, read on the writer's own view
    /// (the committed catalog plus this txn's writes). Sys writes never bump it,
    /// so during a compaction session it still equals the build snapshot's gen
    /// unless a USER write committed in between — which is exactly the condition
    /// [`set_columnar_watermark_if_gen`](Self::set_columnar_watermark_if_gen)
    /// tests.
    pub fn columnar_mod_gen(&mut self, table_id: u32) -> Result<u64> {
        catalog_entry(self, self.catalog_root, table_id, 0).map(|(_, _, g)| g)
    }

    /// Publish a columnar watermark for `table_id` — but ONLY if the table's
    /// current mod_gen still equals `expect_gen`, i.e. no user write has landed
    /// since the compaction snapshot the segments were built from. Returns
    /// whether it was set. Running under the writer lock makes the check and the
    /// write atomic, which is what closes the build race: a covered mutation that
    /// committed while the segments were being built has already bumped mod_gen
    /// past `expect_gen`, so the CAS fails and no stale watermark is ever born.
    pub fn set_columnar_watermark_if_gen(
        &mut self,
        table_id: u32,
        expect_gen: u64,
        covered_rows: u64,
        wm_pk: &[u8],
    ) -> Result<bool> {
        if covered_rows == 0 || wm_pk.is_empty() {
            return Ok(false);
        }
        if self.columnar_mod_gen(table_id)? != expect_gen {
            return Ok(false);
        }
        let v = encode_colwm(expect_gen, covered_rows, wm_pk);
        self.sys_put(&colwm_subkey(table_id), &v)?;
        Ok(true)
    }

    /// Drop a table's columnar watermark. Called by column-layout DDL (ADD/DROP
    /// COLUMN), which rewrites rows OUTSIDE the mutators above and shifts the
    /// ordinals the segments are keyed by — so the tail path, which reads blocks
    /// at their build gen rather than the current one, must be disarmed. The
    /// segment records themselves stay guarded by the whole-table mod_gen check.
    /// Idempotent.
    pub fn clear_columnar_watermark(&mut self, table_id: u32) -> Result<()> {
        self.sys_delete(&colwm_subkey(table_id))?;
        Ok(())
    }

    // ---------- optimistic concurrency (DESIGN-PHASE3) ----------

    /// First-committer-wins validation for an optimistic write of
    /// `(table_id, key_hash)` prepared against snapshot `snap_txn`. Must be
    /// called while holding the writer lock (i.e. on a live `WriteTxn`), before
    /// applying. Returns `Error::WriteConflict` if a conflicting commit landed
    /// on our footprint since the snapshot.
    pub fn optimistic_validate(
        &self,
        snap_txn: u64,
        table_id: u32,
        key_hash: u64,
    ) -> Result<()> {
        if self
            .eng
            .shm
            .opt_conflict(snap_txn, self.meta.txn_id, table_id, key_hash)
        {
            return Err(Error::WriteConflict);
        }
        Ok(())
    }

    /// Record a precise point footprint for this commit (blind-apply path).
    pub fn set_commit_point(&mut self, table_id: u32, key_hash: u64) {
        self.commit_point = Some((table_id, key_hash));
    }

    /// Hint that this transaction's change to `table_id` is confined to keys
    /// under the `keycode` prefix `region`, so a listener watching a key
    /// outside it can skip the wakeup (#139 S2). `None` = unknown.
    ///
    /// A batch commit runs many statements, and the transaction as a whole is
    /// confined to whatever region contains ALL of them — the **common
    /// prefix**. S2 collapsed straight to "unknown" whenever two statements
    /// disagreed; keeping the common prefix instead is strictly sharper and
    /// just as safe, because a key outside the shared prefix is outside every
    /// contributing region by construction. Two INSERTs at `id = 11` and
    /// `id = 22` still share the encoding's leading bytes, so a listener on a
    /// far-away key keeps its filter instead of losing it to the batch.
    ///
    /// Narrowing is never allowed: the region only ever grows (prefix only
    /// ever shortens), because a hint that named just the last writer's key
    /// would let the earlier one's listener sleep through its own change.
    /// Widen the guarded surface by one statement's footprint (#142 G1).
    /// A no-op on an unguarded txn, so the query path can call it blindly.
    ///
    /// Surfaces only ever GROW. A guard that could narrow would let a later
    /// statement drop the table an earlier one read, and the read is exactly
    /// what a lost update would invalidate.
    pub fn guard_widen(&mut self, table_id: u32) {
        if let Some(g) = self.guard.as_mut() {
            g.surface |= 1u64 << (table_id & 63);
        }
    }

    /// Add one exact key to the guard's region summary, or `None` for a
    /// statement whose keys cannot be named — which widens it to everything
    /// and cannot be narrowed again (#143).
    /// Narrow the guard's shard to what this statement proved (#144). The
    /// first statement sets it; every later one must agree or the shard is
    /// lost.
    /// Widen the guarded surface by one column (#146 K1), or by ALL columns
    /// when the statement cannot name them.
    pub fn guard_touch_col(&mut self, col: Option<u16>) {
        if let Some(g) = self.guard.as_mut() {
            g.cols |= match col {
                Some(c) => 1u64 << (c & 63),
                None => crate::shm::COLS_ANY,
            };
        }
    }

    /// Same, for what this txn records as written.
    pub fn record_written_col(&mut self, col: Option<u16>) {
        self.written_cols |= match col {
            Some(c) => 1u64 << (c & 63),
            None => crate::shm::COLS_ANY,
        };
    }

    pub fn guard_shard(&mut self, shard: Option<u64>) {
        if let Some(g) = self.guard.as_mut() {
            if !g.shard_seen {
                g.shard_seen = true;
                g.shard = shard;
            } else if g.shard != shard {
                g.shard = None;
            }
        }
    }

    /// Same rule, for what THIS txn is recording as its committed shard.
    pub fn record_written_shard(&mut self, shard: Option<u64>) {
        if !self.written_shard_seen {
            self.written_shard_seen = true;
            self.written_shard = shard;
        } else if self.written_shard != shard {
            self.written_shard = None;
        }
    }

    pub fn guard_touch_key(&mut self, key_hash: Option<u64>) {
        if let Some(g) = self.guard.as_mut() {
            g.regions |= match key_hash {
                Some(h) => crate::shm::Shm::region_bit(h),
                None => crate::shm::Shm::REGION_ANY,
            };
            // The exact set is given up permanently the first time a key
            // cannot be named or the set outgrows the ring entry — never
            // re-acquired, because a key already forgotten cannot be added
            // back and a surface that shrank would stop guarding it.
            match (key_hash, g.keys.as_mut()) {
                (Some(h), Some(ks)) => {
                    if !ks.contains(&h) {
                        ks.push(h);
                        if ks.len() > crate::shm::OFP_MAX_KEYS {
                            g.keys = None;
                        }
                    }
                }
                (None, _) => g.keys = None,
                (Some(_), None) => {}
            }
        }
    }

    /// Narrow (or widen) the byte range this action rewrites (#151).
    ///
    /// `None` means "somewhere unnameable", and once said it is permanent —
    /// a range already forgotten cannot be recovered, and a surface that
    /// shrank would stop guarding what it had already claimed.
    pub fn guard_touch_range(&mut self, r: Option<(u32, u32)>) {
        if let Some(g) = self.guard.as_mut() {
            g.range = match (g.range, r) {
                (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
                _ => None,
            };
        }
    }

    /// Carry a pending sub-edit's offset forward across the commits it never
    /// saw (#151), or say it genuinely collides.
    ///
    /// Only a GUARDED session can be rebased, and that is the whole rule rather
    /// than a limitation: the guard's snapshot IS "the version the client
    /// decided against", and without one there is no coordinate system to carry
    /// the offset from. An unguarded splice means whatever it says against the
    /// value as it stands.
    ///
    /// `origin` overrides where the walk starts, and a batch of sub-edits needs
    /// it (#154). One transaction may carry members that read at *different*
    /// versions; the guard is taken against the oldest of them, because that is
    /// what makes the transaction's own refusal honest — but a member whose own
    /// snapshot is newer has ALREADY seen the commits in between, and walking it
    /// from the batch minimum applies their deltas a second time. That is not a
    /// refusal, it is a splice on bytes its author never saw.
    pub fn rebase_splice(
        &self,
        table: u32,
        key_hash: u64,
        col: u16,
        at: u64,
        len: u64,
        origin: Option<u64>,
    ) -> Option<crate::shm::RebaseOutcome> {
        let g = self.guard.as_ref()?;
        // Never earlier than the guard: a member claiming to have read further
        // back than the transaction was guarded against would be walked over
        // commits the guard never validated.
        let from = origin.map_or(g.snap_txn, |o| o.max(g.snap_txn));
        self.eng.shm.opt_rebase(
            from,
            self.meta.txn_id,
            &crate::shm::CellEdit {
                table_bit: 1u64 << (table & 63),
                key: key_hash,
                col_bit: 1u64 << (col & 63),
                at,
                len,
            },
        )
    }

    /// Record the length change a sub-edit made (#151). Additive: a txn with
    /// several splices moves everything after them by the sum.
    pub fn record_written_delta(&mut self, d: i64) {
        self.written_delta = self.written_delta.saturating_add(d);
    }

    /// The guard's range was decided by a rebase walk at execution (#151), so
    /// the commit-time range test must not re-decide it against coordinates
    /// that have since moved. Sound because the writer lock is held throughout:
    /// the window the walk saw is the window the commit sees.
    pub fn mark_rebased(&mut self) {
        if let Some(g) = self.guard.as_mut() {
            g.rebased = true;
        }
    }

    /// The written half of [`Self::guard_touch_range`].
    pub fn record_written_range(&mut self, r: Option<(u32, u32)>) {
        self.written_range = match (self.written_range, r) {
            (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
            _ => None,
        };
    }

    /// Add one exact key to what THIS txn is recording as written, for the
    /// benefit of guards that come later. Same widening rule.
    pub fn record_written_key(&mut self, key_hash: Option<u64>) {
        self.written_regions |= match key_hash {
            Some(h) => crate::shm::Shm::region_bit(h),
            None => crate::shm::Shm::REGION_ANY,
        };
        match (key_hash, self.written_keys.as_mut()) {
            (Some(h), Some(ks)) => {
                if !ks.contains(&h) {
                    ks.push(h);
                    if ks.len() > crate::shm::OFP_MAX_KEYS {
                        self.written_keys = None;
                    }
                }
            }
            (None, _) => self.written_keys = None,
            (Some(_), None) => {}
        }
    }

    /// The guard's snapshot and accumulated surface, for the commit path.
    #[allow(clippy::type_complexity)]
    pub fn guard_state(
        &self,
    ) -> Option<(u64, u64, u64, Option<Vec<u64>>, Option<u64>, u64, bool, Option<(u32, u32)>)> {
        self.guard.as_ref().map(|g| {
            (
                g.snap_txn,
                g.surface,
                g.regions,
                g.keys.clone(),
                g.shard,
                g.cols,
                g.rebased,
                g.range,
            )
        })
    }

    pub fn hint_notify_key(&mut self, table_id: u32, region: Option<&[u8]>) {
        let slot = self.notify_keys.entry(table_id).or_insert_with(|| region.map(|r| r.to_vec()));
        let (Some(have), Some(add)) = (slot.as_mut(), region) else {
            // Either side unknown ⇒ the union is unknown.
            *slot = None;
            return;
        };
        let common = have.iter().zip(add).take_while(|(a, b)| a == b).count();
        if common == 0 {
            *slot = None;
        } else {
            have.truncate(common);
        }
    }

    /// Blind INSERT of a pre-validated, pre-encoded row (optimistic apply).
    /// Caller guarantees `table_id` has no secondary index and the row was
    /// validated during prep; footprint validation guarantees the PK's state
    /// is unchanged since the snapshot. Returns false if the PK already exists.
    pub fn optimistic_insert(
        &mut self,
        table_id: u32,
        key: &[u8],
        payload: &[u8],
    ) -> Result<bool> {
        debug_assert!(self.bundle.sec_indexes[table_id as usize].is_empty());
        self.check_write_blocked(table_id)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let out = btree::insert(self, root, key, &mut btree::Payload::Flat(payload), InsertMode::InsertOnly)?;
        if out.existed {
            return Ok(false);
        }
        self.set_tree_root(table_id, 0, out.new_root, count + 1);
        self.capture_dirty(table_id, key, DirtyOp::Upsert)?;
        self.note_columnar_mutation(table_id, key)?;
        Ok(true)
    }

    /// Blind UPDATE (replace) of an existing PK with a pre-validated row.
    pub fn optimistic_upsert(
        &mut self,
        table_id: u32,
        key: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        debug_assert!(self.bundle.sec_indexes[table_id as usize].is_empty());
        self.check_write_blocked(table_id)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let out = btree::insert(self, root, key, &mut btree::Payload::Flat(payload), InsertMode::Upsert)?;
        self.set_tree_root(table_id, 0, out.new_root, count);
        self.capture_dirty(table_id, key, DirtyOp::Upsert)?;
        self.note_columnar_mutation(table_id, key)?;
        Ok(())
    }

    /// Blind DELETE of a PK. Returns whether the row existed.
    pub fn optimistic_delete(&mut self, table_id: u32, key: &[u8]) -> Result<bool> {
        debug_assert!(self.bundle.sec_indexes[table_id as usize].is_empty());
        self.check_write_blocked(table_id)?;
        let (root, count) = self.tree_root(table_id, 0)?;
        let out = btree::delete(self, root, key)?;
        if out.existed {
            self.set_tree_root(table_id, 0, out.new_root, count - 1);
            self.capture_dirty(table_id, key, DirtyOp::Delete)?;
            self.note_columnar_mutation(table_id, key)?;
        }
        Ok(out.existed)
    }


}
