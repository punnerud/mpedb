//! Shared-memory notify operations; ordering and file layout are unchanged.
use super::*;

impl Shm {
    // ---------- change notification (#139) ----------

    /// Byte offset of `field` within the notification page (#147). The region
    /// is page-relative now, so every accessor goes through here rather than
    /// through `lock_area_off`.
    #[inline]
    pub(super) fn notify_off(&self, field: usize) -> usize {
        notify_start_page(self.max_readers) as usize * PAGE_SIZE + field
    }

    #[inline]
    pub(super) fn notify_slot_off(&self, table_id: u32) -> usize {
        self.notify_off(NF_SLOTS_OFF + (table_id % NOTIFY_SLOTS) as usize * NOTIFY_ENTRY)
    }

    /// Parked-listener count. Read by every commit, so it is the one thing on
    /// this path that must stay cheap.
    ///
    /// **Known leak (review finding R1), stated rather than discovered.** This
    /// is a bare counter in shared memory with no liveness sweep. A listener
    /// **SIGKILLed while parked** never decrements it, and nothing else will:
    /// the only reset is [`Self::notify_reset`], at format or boot-epoch
    /// change. Every later commit then issues wake syscalls for a listener that
    /// does not exist, forever, and repeated kills accumulate.
    ///
    /// It is a performance leak and never a wrong answer — the count may only
    /// ever be too HIGH, and too high costs syscalls while too low would cost a
    /// missed wakeup. That asymmetry is why the fix must not be a naive pid
    /// sweep: misjudging liveness in the other direction (the zombie case in
    /// #136) would turn a cost into a lost notification.
    ///
    /// The real fix is a slot registry with the reader table's pid +
    /// start-time identity, swept the way reader slots are. It does not fit in
    /// the ~230 bytes left on the lock page, so it needs the notification
    /// region moved off page 2 — a format change, and therefore a decision
    /// rather than something to slip into a review.
    pub fn notify_waiters(&self) -> &AtomicU32 {
        self.atomic_u32(self.notify_off(NF_WAITERS))
    }

    /// The futex word for `table_id`'s slot. Listeners wait on it; publishers
    /// change it. Shared by every table that hashes here — a false wakeup.
    pub fn notify_seq(&self, table_id: u32) -> &AtomicU32 {
        self.atomic_u32(self.notify_slot_off(table_id) + NF_SEQ)
    }

    /// The futex word every publish bumps, whatever it wrote. A listener
    /// watching more than one table parks here (see [`LA_NOTIFY_ANY`]).
    pub fn notify_any_seq(&self) -> &AtomicU32 {
        self.atomic_u32(self.notify_off(NF_ANY))
    }

    /// This region's incarnation stamp (#140). A persisted generation is only
    /// comparable against a generation read under the SAME epoch.
    pub fn notify_epoch(&self) -> u64 {
        self.atomic_u64(self.notify_off(NF_EPOCH)).load(Ordering::Acquire)
    }

    /// A value that differs from every previous incarnation's. Wall time gives
    /// the across-reboot and across-recreate spread; the pid and a
    /// process-local counter cover two stamps landing in the same clock tick,
    /// which is not hypothetical — the `:memory:` temp-file namer had exactly
    /// that collision on macOS. Never 0: 0 is free to mean "no epoch known".
    pub(super) fn fresh_epoch() -> u64 {
        use std::sync::atomic::AtomicU64;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        // via os: std::time AND std::process::id both panic on wasm32.
        let nanos = (crate::os::wall_clock_micros() as u64).wrapping_mul(1000);
        let mix = nanos
            ^ ((crate::os::process_id() as u64) << 32)
            ^ SEQ.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        mix | 1
    }

    /// This table's change generation, and the key digest of the last change
    /// (0 = unknown / whole table). Returns `None` when the slot currently
    /// belongs to a DIFFERENT table — the caller then knows only that
    /// something hashing here changed, which is a false wakeup, not a lie.
    pub fn notify_read(&self, table_id: u32) -> Option<(u64, u64)> {
        let off = self.notify_slot_off(table_id);
        // gen last, Acquire: pairs with the publisher's Release store, so a
        // reader that sees a bumped generation also sees the id and key that
        // belong to it.
        let owner = self.atomic_u32(off + NF_TABLE).load(Ordering::Acquire);
        let key = self.atomic_u64(off + NF_KEY).load(Ordering::Acquire);
        let gen = self.atomic_u64(off + NF_GEN).load(Ordering::Acquire);
        (owner == table_id).then_some((gen, key))
    }

    /// *Writer-lock holder, after the meta flip.* Record that `table_id`
    /// changed, and wake anyone parked on its slot.
    ///
    /// `key` is a digest of the key range the statement touched, or 0 for
    /// "somewhere in this table" — a listener filtering on a key may skip a
    /// nonzero mismatch, and must never skip a 0.
    ///
    /// Ordering: the id and key are stored BEFORE the generation (Release), so
    /// a listener that observes the new generation observes the pair that
    /// produced it. The sequence word moves last, because that is what a
    /// parked futex re-checks.
    pub fn notify_publish(&self, table_id: u32, key: u64) {
        let off = self.notify_slot_off(table_id);
        self.atomic_u32(off + NF_TABLE).store(table_id, Ordering::Release);
        self.atomic_u64(off + NF_KEY).store(key, Ordering::Release);
        self.atomic_u64(off + NF_GEN).fetch_add(1, Ordering::AcqRel);
        self.atomic_u32(off + NF_SEQ).fetch_add(1, Ordering::AcqRel);
        // Last, and after the per-table word: a multi-table listener parked
        // here re-reads every generation it watches, so it must not be released
        // before those generations are visible.
        self.atomic_u32(self.notify_off(NF_ANY)).fetch_add(1, Ordering::AcqRel);
    }

    /// Wake parked listeners for `table_id`. Split from [`Self::notify_publish`]
    /// so a commit can publish every table it wrote and then issue the syscalls
    /// in one pass — and skip them entirely when nobody is parked.
    pub fn notify_wake(&self, table_id: u32) {
        crate::os::futex_wake_all(self.notify_seq(table_id));
    }

    // ---------- waiter registry (#147) ----------

    #[inline]
    pub(super) fn waiter_slot(&self, i: usize, field: usize) -> usize {
        self.notify_off(NF_REG + i * NFW_ENTRY + field)
    }

    /// This process's `/proc` start time — the incarnation half of the identity
    /// a registry slot records. Exposed so a caller registers with exactly the
    /// value the sweep will compare against, rather than reconstructing it.
    pub fn own_start_time(&self) -> Option<u64> {
        proc_start_time(crate::os::process_id())
    }

    /// Claim a registry slot for this process, returning its index.
    ///
    /// Sweeps first, so the party that benefits from a clean registry is the
    /// one that pays for it, and a database with no listeners never sweeps at
    /// all. `None` means every slot is taken by a live process: the caller
    /// still parks and still counts, it is simply not reclaimable if killed.
    /// Refusing to listen would be a far worse failure than an unreclaimable
    /// slot.
    pub fn waiter_register(&self, pid: u32, start: u64) -> Option<usize> {
        self.waiter_sweep();
        for i in 0..NOTIFY_WAITER_SLOTS {
            let p = self.atomic_u32(self.waiter_slot(i, NFW_PID));
            if p.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                self.atomic_u64(self.waiter_slot(i, NFW_START)).store(start, Ordering::Release);
                return Some(i);
            }
        }
        None
    }

    /// Release a slot this process owns. Returns true if THIS call cleared it,
    /// which is the caller's licence to decrement the counter — see the
    /// ownership rule on [`NF_REG`].
    pub fn waiter_release(&self, i: usize, pid: u32) -> bool {
        let p = self.atomic_u32(self.waiter_slot(i, NFW_PID));
        p.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }

    /// Reclaim slots whose owner is gone, decrementing the parked count once
    /// per slot actually reclaimed. Returns how many.
    ///
    /// Liveness uses the reader table's identity — pid plus `/proc` start time
    /// — so a recycled pid is not mistaken for the original owner. The
    /// predicate errs toward ALIVE (it never sweeps on `EPERM`), which is the
    /// only safe direction here: sweeping a live waiter would decrement a
    /// count it will decrement again on the way out, and an undercount is a
    /// missed wakeup.
    pub fn waiter_sweep(&self) -> usize {
        let mut freed = 0;
        for i in 0..NOTIFY_WAITER_SLOTS {
            let p = self.atomic_u32(self.waiter_slot(i, NFW_PID));
            let pid = p.load(Ordering::Acquire);
            if pid == 0 {
                continue;
            }
            let start = self.atomic_u64(self.waiter_slot(i, NFW_START)).load(Ordering::Acquire);
            if pid_alive_identity(pid, start) {
                continue;
            }
            // Only the winner of this CAS may decrement: the owner could be
            // releasing concurrently, and two decrements for one increment is
            // an undercount.
            if p.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                self.notify_waiters().fetch_sub(1, Ordering::AcqRel);
                freed += 1;
            }
        }
        freed
    }

    /// Wake listeners parked on the "any table" word (N3). Issued once per commit,
    /// not once per table — the word moved once for each published table, but
    /// a single wake releases everyone parked on it.
    pub fn notify_wake_any(&self) {
        crate::os::futex_wake_all(self.notify_any_seq());
    }

    /// Zero the whole notification region. Called at format and on a boot-epoch
    /// change: generations that outlive a reboot would let a listener conclude
    /// "unchanged" about a file that is no longer the one it read.
    pub fn notify_reset(&self) {
        for slot in 0..NOTIFY_SLOTS {
            let off = self.notify_off(NF_SLOTS_OFF + slot as usize * NOTIFY_ENTRY);
            self.atomic_u64(off + NF_GEN).store(0, Ordering::Relaxed);
            self.atomic_u32(off + NF_SEQ).store(0, Ordering::Relaxed);
            self.atomic_u32(off + NF_TABLE).store(u32::MAX, Ordering::Relaxed);
            self.atomic_u64(off + NF_KEY).store(0, Ordering::Relaxed);
        }
        self.notify_any_seq().store(0, Ordering::Relaxed);
        for i in 0..NOTIFY_WAITER_SLOTS {
            self.atomic_u32(self.waiter_slot(i, NFW_PID)).store(0, Ordering::Relaxed);
            self.atomic_u64(self.waiter_slot(i, NFW_START)).store(0, Ordering::Relaxed);
        }
        // Stamped LAST, with Release: a reader that sees the new epoch sees the
        // zeroed generations that belong to it, never the previous
        // incarnation's still standing behind a fresh stamp.
        self.atomic_u64(self.notify_off(NF_EPOCH))
            .store(Self::fresh_epoch(), Ordering::Release);
        self.notify_waiters().store(0, Ordering::Release);
    }

    // ---------- key fingerprints (#141 N2) ----------

    // The published digest answers "where in this table did the change land",
    // in 8 bytes, for a listener that cares about one key. S2 could only answer
    // it for a single resolvable POINT; every range write fell back to 0
    // ("somewhere") and switched key filtering off — even though the footprint
    // knew the range perfectly well.
    //
    // The generalisation rides on `keycode` being memcmp-ordered: every key in
    // `lo..=hi` shares the common byte prefix of `lo` and `hi`, so that prefix
    // IS the region, and a key outside it cannot be in the range. Points are
    // the degenerate case where the prefix is the whole key.
    //
    // Layout: high byte = prefix length, low 56 bits = its hash. Length is in
    // the fingerprint because a listener must hash the SAME prefix of its own
    // key to compare, and it has no other way to learn how much to hash. A
    // fingerprint is therefore never 0 (length ≥ 1), which keeps 0 free as
    // "unknown" — the value every fail-safe path publishes.

    /// The region bit for one exact key hash. A commit's region summary is the
    /// OR of these over every key it wrote; a guard's is the OR over every key
    /// it touched. Two summaries that share no bit touched no common key.
    ///
    /// This is a 64-bit Bloom filter with k = 1, and its limits are worth being
    /// exact about. Two keys collide with probability 1/64, which costs a false
    /// conflict and therefore a retry. A commit touching many keys saturates the
    /// word and degenerates to "everything" — correct, and the reason a bulk
    /// write should not expect the guard to keep sharding for it.
    #[inline]
    pub fn region_bit(key_hash: u64) -> u64 {
        1u64 << (key_hash % 64)
    }

    /// The summary meaning "could be any key" — what a range write, a
    /// multi-key statement, or anything unresolvable must publish.
    pub const REGION_ANY: u64 = u64::MAX;

    /// Pack a byte range for the ring entry. Offsets past `u32::MAX` (a 4 GiB
    /// cell) are not representable and widen to "anywhere" rather than
    /// truncating into a range that would clear a real conflict.
    pub fn pack_range(lo: u64, hi: u64) -> u64 {
        match (u32::try_from(lo), u32::try_from(hi)) {
            (Ok(l), Ok(h)) if l <= h => ((l as u64) << 32) | h as u64,
            _ => RANGE_ANY,
        }
    }

    /// Fingerprint of a key prefix. `None` (empty prefix) has no fingerprint:
    /// there is nothing to filter on, which is the 0 case.
    pub fn key_fingerprint(prefix: &[u8]) -> u64 {
        if prefix.is_empty() {
            return 0;
        }
        let len = prefix.len().min(255);
        let mut h = 0xcbf29ce484222325u64;
        for &b in &prefix[..len] {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        ((len as u64) << 56) | (h & 0x00ff_ffff_ffff_ffff)
    }

    /// Could a change advertised as `published` have touched `key`?
    ///
    /// Wrong in the "no" direction means a listener sleeps through its own
    /// change, so every case that cannot be decided answers **yes**: an unknown
    /// fingerprint, and a key shorter than the advertised prefix (which cannot
    /// be hashed to compare, and whose length semantics are keycode's business,
    /// not this function's).
    pub fn fingerprint_matches(published: u64, key: &[u8]) -> bool {
        if published == 0 {
            return true;
        }
        let len = (published >> 56) as usize;
        if key.len() < len {
            return true;
        }
        Self::key_fingerprint(&key[..len]) == published
    }


}
