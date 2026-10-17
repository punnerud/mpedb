//! Shared-memory readers operations; ordering and file layout are unchanged.
use super::*;

impl Shm {
    // ---------- reader table ----------

    pub(super) fn slot_off(&self, idx: u32) -> usize {
        READER_TABLE_PAGE as usize * PAGE_SIZE + idx as usize * READER_SLOT_SIZE
    }

    pub(super) fn slot_word(&self, idx: u32) -> &AtomicU64 {
        self.atomic_u64(self.slot_off(idx) + RS_WORD)
    }

    pub(super) fn slot_txn(&self, idx: u32) -> &AtomicU64 {
        self.atomic_u64(self.slot_off(idx) + RS_TXN)
    }

    pub(super) fn slot_pid_start(&self, idx: u32) -> &AtomicU64 {
        self.atomic_u64(self.slot_off(idx) + RS_PID_START)
    }

    /// Set in the pid half of a slot word while its claimer is still
    /// initializing txn/pid_start. Safe: Linux PID_MAX_LIMIT is 2^22, so
    /// bit 31 is never part of a real pid.
    pub(super) const CLAIMING: u32 = 1 << 31;

    #[inline]
    pub(super) fn pack(pid: u32, seq: u32) -> u64 {
        ((pid as u64) << 32) | seq as u64
    }

    #[inline]
    pub(super) fn unpack(word: u64) -> (u32, u32) {
        ((word >> 32) as u32, word as u32)
    }

    /// Claim a reader slot and pin the current snapshot (design/DESIGN.md §4.3 pin
    /// protocol, with the paired-SeqCst-fence fix). Returns (slot index,
    /// owned word value, pinned meta).
    ///
    /// Private in-memory DBs use a process-local pin table (no multi-process
    /// claim CAS / SeqCst recheck): no other process can attach, so the
    /// cross-process store-buffering race cannot arise. Same-process
    /// multi-thread is still covered by the private pin array.
    pub fn claim_and_pin(&self) -> Result<(u32, u64, MetaSnapshot)> {
        if self.is_private {
            return self.claim_and_pin_private();
        }
        let pid = crate::os::process_id();
        let (idx, word) = match self.claim_slot(pid) {
            Some(x) => x,
            None => {
                // slot exhaustion: sweep dead slots ourselves, then retry once
                self.sweep_dead_readers();
                self.claim_slot(pid).ok_or(Error::ReadersFull)?
            }
        };
        // pin loop; on any error the slot must not leak (a leaked slot owned
        // by a live process is unsweepable)
        loop {
            let meta = match self.newest_meta() {
                Ok(m) => m,
                Err(e) => {
                    self.release_slot(idx, word);
                    return Err(e);
                }
            };
            self.slot_txn(idx).store(meta.txn_id, Ordering::Release);
            // SC fence pairs with the writer's SC fence after lock acquisition;
            // forbids the store-buffering outcome where our pin store and the
            // writer's reader-table scan both pass each other.
            fence(Ordering::SeqCst);
            match self.newest_meta() {
                Ok(recheck) if recheck.txn_id == meta.txn_id => {
                    return Ok((idx, word, meta));
                }
                Ok(_) => continue, // a commit landed in the window: re-pin
                Err(e) => {
                    self.release_slot(idx, word);
                    return Err(e);
                }
            }
        }
    }

    /// Process-private pin: one `newest_meta`, CAS into `priv_pins`. `word` is
    /// returned as 0 so [`Self::release_slot`] / [`Self::slot_still_owned`]
    /// branch on `is_private`.
    ///
    /// When [`Self::exclusive_write`] is set (in-place private writer), *other*
    /// threads spin until it clears. The owning thread nests freely (TLS depth)
    /// so `write_rules` / catalog reloads under the write lock do not deadlock.
    pub(super) fn claim_and_pin_private(&self) -> Result<(u32, u64, MetaSnapshot)> {
        let mut spins = 0u32;
        loop {
            // Foreign exclusive writer: wait. Nested owner: pass through.
            while self.exclusive_write.load(Ordering::Acquire) != 0
                && EXCLUSIVE_DEPTH.with(|d| d.get()) == 0
            {
                spins = spins.wrapping_add(1);
                if spins > 4_000_000 {
                    std::thread::yield_now();
                    spins = 0;
                } else {
                    std::hint::spin_loop();
                }
            }
            let meta = self.newest_meta()?;
            for i in 0..PRIV_PIN_SLOTS {
                if self.priv_pins[i]
                    .compare_exchange(
                        PRIV_PIN_FREE,
                        meta.txn_id,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    // Foreign exclusive started after our CAS — drop and retry.
                    if self.exclusive_write.load(Ordering::Acquire) != 0
                        && EXCLUSIVE_DEPTH.with(|d| d.get()) == 0
                    {
                        self.priv_pins[i].store(PRIV_PIN_FREE, Ordering::Release);
                        break;
                    }
                    return Ok((i as u32, 0, meta));
                }
            }
            if spins > 256 {
                return Err(Error::ReadersFull);
            }
            spins = spins.wrapping_add(1);
            std::hint::spin_loop();
        }
    }

    /// True if any process-local private pin is held.
    #[inline]
    pub fn any_private_pin(&self) -> bool {
        for i in 0..PRIV_PIN_SLOTS {
            if self.priv_pins[i].load(Ordering::Acquire) != PRIV_PIN_FREE {
                return true;
            }
        }
        false
    }

    /// Try to enter exclusive in-place write mode (`is_private` only).
    ///
    /// Protocol: set the flag first (blocks foreign pins), then scan. If any
    /// pin already exists, clear the flag and return false (caller uses COW).
    /// Nested `begin_read` on this thread is allowed via TLS depth.
    pub fn try_begin_exclusive_write(&self) -> bool {
        if !self.is_private {
            return false;
        }
        self.exclusive_write.store(1, Ordering::Release);
        fence(Ordering::SeqCst);
        if self.any_private_pin() {
            self.exclusive_write.store(0, Ordering::Release);
            return false;
        }
        EXCLUSIVE_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
        true
    }

    /// End exclusive in-place mode (always safe; no-op if not exclusive).
    #[inline]
    pub fn end_exclusive_write(&self) {
        EXCLUSIVE_DEPTH.with(|d| {
            let n = d.get();
            if n > 0 {
                d.set(n - 1);
            }
        });
        // Only clear the global flag when this thread has no nested exclusive.
        if EXCLUSIVE_DEPTH.with(|d| d.get()) == 0 {
            self.exclusive_write.store(0, Ordering::Release);
        }
    }

    /// Claim protocol (reviewed): every store into a slot must be owner-only.
    /// 1. CAS {0, s} → {pid|CLAIMING, s+1}: reservation and identity
    ///    publication are one atomic step.
    /// 2. As owner, initialize txn = u64::MAX and pid_start.
    /// 3. CAS {pid|CLAIMING, s+1} → {pid, s+1} to go live; if that fails the
    ///    slot was reclaimed from us — walk away without touching it.
    ///
    /// A stale txn value visible during step 1-2 is safe: it is ≤ the newest
    /// committed txn, so a concurrent oldest-pinned scan only becomes more
    /// conservative (delays reclaim, never corrupts).
    pub(super) fn claim_slot(&self, pid: u32) -> Option<(u32, u64)> {
        static CLAIM_SALT: AtomicU32 = AtomicU32::new(0);
        let n = self.max_readers;
        // randomized start offset decorrelates claim scans across processes
        // and across threads within one process
        let salt = CLAIM_SALT.fetch_add(1, Ordering::Relaxed);
        let start = crate::os::process_id()
            .wrapping_mul(2654435761)
            .wrapping_add(salt.wrapping_mul(40503))
            % n;
        for i in 0..n {
            let idx = (start + i) % n;
            // a benign CAS failure (racing claimer, sweep seq-bump) must not
            // abandon a still-free slot: retry a few times before moving on
            for _ in 0..4 {
                let w = self.slot_word(idx).load(Ordering::Acquire);
                let (pid_half, seq) = Self::unpack(w);
                if pid_half != 0 {
                    break; // genuinely occupied (or mid-claim): next slot
                }
                let claiming = Self::pack(pid | Self::CLAIMING, seq.wrapping_add(1));
                if self
                    .slot_word(idx)
                    .compare_exchange(w, claiming, Ordering::AcqRel, Ordering::Relaxed)
                    .is_err()
                {
                    continue; // lost the race; re-read, slot may still be free
                }
                // owner-only initialization while marked CLAIMING
                self.slot_txn(idx).store(u64::MAX, Ordering::Release);
                self.slot_pid_start(idx)
                    .store(self.my_pid_start, Ordering::Release);
                let owned = Self::pack(pid, seq.wrapping_add(1));
                if self
                    .slot_word(idx)
                    .compare_exchange(claiming, owned, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some((idx, owned));
                }
                // reclaimed from us mid-claim: never touch this slot again
                break;
            }
        }
        None
    }

    /// Release an owned slot. Fails with `SnapshotEvicted` semantics (returns
    /// false) if the generation no longer matches (eviction / theft).
    pub fn release_slot(&self, idx: u32, owned_word: u64) -> bool {
        if self.is_private {
            // Private pin: `owned_word` unused; free if still occupied.
            if (idx as usize) >= PRIV_PIN_SLOTS {
                return false;
            }
            // Any non-FREE value means we still own it (eviction not used for private).
            let prev = self.priv_pins[idx as usize].swap(PRIV_PIN_FREE, Ordering::AcqRel);
            return prev != PRIV_PIN_FREE;
        }
        let (_, seq) = Self::unpack(owned_word);
        self.slot_word(idx)
            .compare_exchange(
                owned_word,
                Self::pack(0, seq.wrapping_add(1)),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Verify an owned slot is still ours (long scans call this periodically).
    pub fn slot_still_owned(&self, idx: u32, owned_word: u64) -> bool {
        if self.is_private {
            // Private pins: no multi-process eviction; free marker means released.
            (idx as usize) < PRIV_PIN_SLOTS
                && self.priv_pins[idx as usize].load(Ordering::Acquire) != PRIV_PIN_FREE
        } else {
            self.slot_word(idx).load(Ordering::Acquire) == owned_word
        }
    }

    /// How many reader slots are occupied right now — an INSTANTANEOUS,
    /// advisory census of concurrent read transactions on this file, across
    /// every attached process.
    ///
    /// Used as the parallel fold's politeness signal
    /// (design/DESIGN-PARALLEL-READ.md §8: "a greedy query must not starve the
    /// OTHER PROCESSES sharing the file"). Deliberately racy and deliberately
    /// unsynchronized — it steers a scheduling choice, never an answer, and a
    /// stale count can only mean one worker more or fewer. One pass of
    /// `max_readers` relaxed loads, paid once per engagement, never per row.
    pub fn live_readers(&self) -> u32 {
        if self.is_private {
            return self
                .priv_pins
                .iter()
                .filter(|p| p.load(Ordering::Relaxed) != PRIV_PIN_FREE)
                .count() as u32;
        }
        (0..self.max_readers)
            .filter(|&idx| Self::unpack(self.slot_word(idx).load(Ordering::Relaxed)).0 != 0)
            .count() as u32
    }

    /// Free slots whose owning process is provably gone. Safe to run from any
    /// process at any time: every free is a generation-CAS of the exact word
    /// observed dead, so racing a re-claim is harmless.
    /// Does any reader slot name a process that is ALIVE right now, with
    /// matching identity?
    ///
    /// Used as EVIDENCE against the boot-identity predicate on Windows (#159).
    /// There is no boot id to read there, so `os::boot_id_matches` derives one
    /// from two clocks — and a wall-clock step larger than its tolerance makes
    /// it say "different boot" on a machine that never rebooted. Acting on that
    /// wipes the reader table, and a wipe is not something a live reader
    /// survives: its pin is dropped and the writer may reclaim pages under it.
    ///
    /// A live, identity-matching pin refutes the clock. Windows records process
    /// CREATION TIME as an absolute FILETIME, so a stale entry surviving a real
    /// reboot would need the same pid created at the same 100-nanosecond
    /// instant. The evidence only ever PREVENTS the destructive action, which
    /// is the safe direction for a predicate that can be wrong.
    ///
    /// Not used on Linux, where the recorded start time is measured in ticks
    /// SINCE BOOT: pid 1234 at tick 500 is a value a fresh boot can hand out
    /// again, so the same evidence would be a coincidence rather than a proof.
    #[cfg(windows)]
    pub(super) fn any_live_reader(&self) -> bool {
        (0..self.max_readers).any(|idx| {
            let w = self.slot_word(idx).load(Ordering::Acquire);
            let (pid_half, _) = Self::unpack(w);
            let pid = pid_half & !Self::CLAIMING;
            if pid == 0 || pid_half & Self::CLAIMING != 0 {
                // A mid-claim slot has no trustworthy identity yet, so it is
                // not evidence of anything.
                return false;
            }
            pid_alive_identity(pid, self.slot_pid_start(idx).load(Ordering::Acquire))
        })
    }

    pub fn sweep_dead_readers(&self) {
        for idx in 0..self.max_readers {
            let w = self.slot_word(idx).load(Ordering::Acquire);
            let (pid_half, seq) = Self::unpack(w);
            if pid_half == 0 {
                continue;
            }
            let pid = pid_half & !Self::CLAIMING;
            let dead = if pid_half & Self::CLAIMING != 0 {
                // mid-claim: pid_start is not yet trustworthy, so only a
                // definite "no such pid" counts as dead (a claimer dying in
                // this µs-window whose pid is instantly recycled leaks one
                // slot until that pid exits — accepted residual, it pins
                // nothing)
                crate::os::pid_definitely_dead(pid)
            } else {
                let recorded_start = self.slot_pid_start(idx).load(Ordering::Acquire);
                !pid_alive_identity(pid, recorded_start)
            };
            if !dead {
                continue;
            }
            let _ = self.slot_word(idx).compare_exchange(
                w,
                Self::pack(0, seq.wrapping_add(1)),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }

    /// Compute the oldest pinned snapshot bound. Caller: writer lock held.
    /// `current_txn` is the newest committed txn (claimed-but-unpinned slots
    /// count as pinning it — they can never pin older). Publishes the result
    /// monotonically into the cache and returns it.
    pub fn compute_oldest_pinned(&self, current_txn: u64) -> u64 {
        let mut oldest = current_txn;
        if self.is_private {
            // Process-private pins: scan the local array. No SeqCst fence —
            // writers and readers share one address space; AcqRel on the pin
            // cells already orders pin stores vs this load.
            for i in 0..PRIV_PIN_SLOTS {
                let t = self.priv_pins[i].load(Ordering::Acquire);
                if t != PRIV_PIN_FREE {
                    oldest = oldest.min(t);
                }
            }
        } else {
            // pairs with the readers' pin-protocol SC fence
            fence(Ordering::SeqCst);
            for idx in 0..self.max_readers {
                let w = self.slot_word(idx).load(Ordering::Acquire);
                let (pid, _) = Self::unpack(w);
                if pid == 0 {
                    continue;
                }
                let t = self.slot_txn(idx).load(Ordering::Acquire);
                let effective = if t == u64::MAX { current_txn } else { t };
                oldest = oldest.min(effective);
            }
        }
        self.oldest_pinned_cache().fetch_max(oldest, Ordering::AcqRel);
        oldest
    }


}
