//! Shared-memory pages operations; ordering and file layout are unchanged.
use super::*;

impl Shm {
    // ---------- external payloads (`vkind=4`) ----------

    /// Where this database's external payloads live.
    ///
    /// Beside the file rather than inside it, and named after it, so a
    /// database and its payloads travel together and two databases in one
    /// directory cannot claim each other's objects.
    pub fn external_dir(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".extents");
        PathBuf::from(p)
    }

    /// The object's own name. Lower-case hex of the content hash, fixed width,
    /// so a directory listing sorts and a name never needs escaping.
    pub fn external_path(&self, hash: u128) -> PathBuf {
        self.external_dir().join(format!("{hash:032x}.ext"))
    }

    /// Begin writing one payload. See [`ExternalWriter`] for the crash argument.
    pub fn external_begin(&self) -> Result<ExternalWriter> {
        let dir = self.external_dir();
        std::fs::create_dir_all(&dir).map_err(|_| io_err("mkdir(external payloads)"))?;
        let seq = EXTERNAL_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!(".writing-{}-{}", crate::os::process_id(), seq));
        let file = std::fs::File::create(&temp).map_err(|_| io_err("create(external payload)"))?;
        Ok(ExternalWriter {
            file,
            temp,
            dir,
            hasher: xxhash_rust::xxh3::Xxh3::new(),
            len: 0,
        })
    }

    /// Read one back. The caller checks the hash — this only fetches bytes.
    pub fn read_external_payload(
        &self,
        hash: u128,
        total_len: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let path = self.external_path(hash);
        let bytes = std::fs::read(&path).map_err(|e| match e.kind() {
            // Not corruption and not a bug: on a device that syncs these
            // objects, a reference can arrive before the bytes it names. The
            // caller can wait and ask again; a Corrupt would tell it to give up.
            std::io::ErrorKind::NotFound => {
                Error::Unsupported(format!("external payload {hash:032x} is not here yet"))
            }
            _ => io_err("read(external payload)"),
        })?;
        if bytes.len() as u64 != total_len {
            return Err(Error::Corrupt("external payload length mismatch".into()));
        }
        out.extend_from_slice(&bytes);
        Ok(())
    }

    // ---------- raw access helpers ----------

    #[inline]
    pub(super) fn base(&self) -> *mut u8 {
        self.map
    }

    #[inline]
    pub(super) fn at(&self, offset: usize) -> *mut u8 {
        debug_assert!(offset < self.len);
        unsafe { self.base().add(offset) }
    }

    #[inline]
    pub(super) fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        debug_assert!(offset.is_multiple_of(8));
        unsafe { &*(self.at(offset) as *const AtomicU64) }
    }

    #[inline]
    pub(super) fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        debug_assert!(offset.is_multiple_of(4));
        unsafe { &*(self.at(offset) as *const AtomicU32) }
    }

    /// Read-only view of a page. Caller contract (MVCC): the page must be
    /// immutable for the borrow's duration — either committed and pinned, or
    /// dirty and owned by the calling writer.
    /// A bounds-checked byte slice of the mapping spanning pages — the extent
    /// read path (DESIGN-BLOBEXTENT §5). The caller proved the run's geometry;
    /// this only guards the mapping's edge.
    pub fn bytes(&self, offset: usize, len: usize) -> Result<&[u8]> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::Corrupt("byte range overflow".into()))?;
        if end > self.page_count as usize * PAGE_SIZE {
            return Err(Error::Corrupt("byte range beyond the mapping".into()));
        }
        Ok(unsafe { std::slice::from_raw_parts(self.at(offset), len) })
    }

    pub fn page(&self, id: u64) -> Result<&[u8]> {
        if id >= self.page_count {
            return Err(Error::Corrupt(format!("page id {id} out of bounds")));
        }
        Ok(unsafe {
            std::slice::from_raw_parts(self.at(id as usize * PAGE_SIZE), PAGE_SIZE)
        })
    }

    /// Mutable view of a page. Caller contract: writer lock held AND the page
    /// is unreachable from any committed meta (freshly allocated this txn).
    #[allow(clippy::mut_from_ref)]
    pub fn page_mut_unchecked(&self, id: u64) -> Result<&mut [u8]> {
        if id >= self.page_count {
            return Err(Error::Corrupt(format!("page id {id} out of bounds")));
        }
        Ok(unsafe {
            std::slice::from_raw_parts_mut(self.at(id as usize * PAGE_SIZE), PAGE_SIZE)
        })
    }

    // ---------- meta pages ----------

    pub(super) fn meta_off(slot: u64) -> usize {
        slot as usize * PAGE_SIZE
    }

    /// Assemble the checksum preimage from explicitly loaded field values
    /// (never a raw memcpy of the page, which could mix torn writes).
    pub(super) fn meta_checksum_input(&self, slot: u64, s: &MetaSnapshot) -> [u8; META_LOGICAL_LEN] {
        let base = Self::meta_off(slot);
        let mut buf = [0u8; META_LOGICAL_LEN];
        // init-frozen fields are stable after the init handshake: plain copy
        unsafe {
            std::ptr::copy_nonoverlapping(self.at(base), buf.as_mut_ptr(), M_TXN_ID);
        }
        buf[M_TXN_ID..M_TXN_ID + 8].copy_from_slice(&s.txn_id.to_le_bytes());
        buf[M_CATALOG_ROOT..M_CATALOG_ROOT + 8].copy_from_slice(&s.catalog_root.to_le_bytes());
        buf[M_FREELIST_ROOT..M_FREELIST_ROOT + 8]
            .copy_from_slice(&s.freelist_root.to_le_bytes());
        buf[M_HIGH_WATER..M_HIGH_WATER + 8].copy_from_slice(&s.high_water.to_le_bytes());
        buf[M_EXTENT_MAP_ROOT..M_EXTENT_MAP_ROOT + 8]
            .copy_from_slice(&s.extent_map_root.to_le_bytes());
        buf[M_SCHEMA_GEN..M_SCHEMA_GEN + 8].copy_from_slice(&s.schema_gen.to_le_bytes());
        buf
    }

    /// Try to read one meta slot; None if its checksum does not validate.
    pub(super) fn read_meta_slot(&self, slot: u64) -> Option<MetaSnapshot> {
        let base = Self::meta_off(slot);
        // Acquire on the checksum pairs with the Release store in
        // write_meta_slot, making all data pages of that commit visible.
        let checksum = self.atomic_u64(base + M_CHECKSUM).load(Ordering::Acquire);
        let snap = MetaSnapshot {
            slot,
            txn_id: self.atomic_u64(base + M_TXN_ID).load(Ordering::Relaxed),
            catalog_root: self.atomic_u64(base + M_CATALOG_ROOT).load(Ordering::Relaxed),
            freelist_root: self
                .atomic_u64(base + M_FREELIST_ROOT)
                .load(Ordering::Relaxed),
            high_water: self.atomic_u64(base + M_HIGH_WATER).load(Ordering::Relaxed),
            extent_map_root: self
                .atomic_u64(base + M_EXTENT_MAP_ROOT)
                .load(Ordering::Relaxed),
            schema_gen: self.atomic_u64(base + M_SCHEMA_GEN).load(Ordering::Relaxed),
        };
        let expect = xxhash_rust::xxh3::xxh3_64(&self.meta_checksum_input(slot, &snap));
        // A torn read (fields from two different commits) fails the checksum
        // and the slot is skipped; the double buffer guarantees the other
        // slot is a complete older commit.
        if expect == checksum {
            Some(snap)
        } else {
            None
        }
    }

    /// The newest valid committed meta. In durability=commit and wal modes,
    /// gated by the durable_txn watermark so no process observes a commit
    /// that a power failure could still erase (identical gate semantics in
    /// both durable modes; wal advances the watermark after its fdatasync).
    pub fn newest_meta(&self) -> Result<MetaSnapshot> {
        // Private in-memory: process-local double buffer (no mapped dual-slot
        // xxh3). Seeded at format and updated in every write_meta_slot.
        if self.is_private {
            return Ok(self.load_priv_meta());
        }
        if !matches!(self.durability, Durability::Commit | Durability::Wal) {
            // Ungated modes read whatever is newest — but the READ itself can
            // tear twice over: `read_meta_slot` loads the checksum, then the
            // body, and a reader descheduled inside one call can straddle two
            // consecutive commits (slots alternate, and at `none` a commit is
            // microseconds — no msync). Slot A then tears against commit N+1
            // and slot B against N+2, and a single pass reports a HEALTHY
            // file as "both checksums invalid" — measured at ~4 % per
            // map-collide run on a loaded box, no SIGKILL required; the
            // shadow-paging induction (§4.1) rules out both slots being
            // GENUINELY torn while a valid commit chain exists. Same cure and
            // same termination argument as the gated loop below: retry while
            // the observed slot state MOVES; a state unchanged between two
            // passes admits nothing new, so the failure is genuine.
            let mut last: Option<(u64, u64, u64, u64)> = None;
            for _ in 0..64 {
                match self.newest_meta_gated(u64::MAX) {
                    Ok(m) => return Ok(m),
                    Err(e) => {
                        let a = Self::meta_off(META_PAGE_A);
                        let b = Self::meta_off(META_PAGE_B);
                        let state = (
                            self.atomic_u64(a + M_TXN_ID).load(Ordering::Acquire),
                            self.atomic_u64(a + M_CHECKSUM).load(Ordering::Acquire),
                            self.atomic_u64(b + M_TXN_ID).load(Ordering::Acquire),
                            self.atomic_u64(b + M_CHECKSUM).load(Ordering::Acquire),
                        );
                        if last == Some(state) {
                            return Err(e); // stable: genuine corruption
                        }
                        last = Some(state);
                    }
                }
                std::hint::spin_loop();
            }
            return Err(Error::Corrupt("no stable meta page after retries".into()));
        }
        // The durable gate is monotone, but a reader racing two consecutive
        // durable commits can load a STALE gate and then find both slots
        // newer than it — a spurious "no valid meta". Reload the gate and
        // retry: a fresh gate only ever admits more, and the newest slot is
        // always <= the gate its committer advanced.
        //
        // ⚠ This loop is LOAD-BEARING, and it is cheap to prove: replace it with
        // a single `newest_meta_gated(self.durable_txn().load(Acquire))` and
        // `cargo run -p mpedb-bench -- --only mpedb` reports spurious reader
        // retries within seconds (measured 2026-07-15: 3 with it disabled, 0 as
        // shipped, same flags both arms). The commit path publishes the meta slot
        // BEFORE it advances the gate — `write_meta_slot`, then `msync_page`
        // (milliseconds on real disk), then `durable_txn.fetch_max` — so the
        // window is wide and a reader falls in it under load. mpedb-bench's
        // SPURIOUS_CORRUPT_RETRIES counter is the tripwire for that regression.
        let mut last_gate = 0;
        for _ in 0..64 {
            let gate = self.durable_txn().load(Ordering::Acquire);
            match self.newest_meta_gated(gate) {
                Ok(m) => return Ok(m),
                Err(e) => {
                    if gate == last_gate {
                        return Err(e); // stable: genuine corruption
                    }
                    last_gate = gate;
                }
            }
            std::hint::spin_loop();
        }
        Err(Error::Corrupt(
            "no durably-gated meta page after retries".into(),
        ))
    }

    pub(super) fn newest_meta_gated(&self, gate: u64) -> Result<MetaSnapshot> {
        let a = self.read_meta_slot(META_PAGE_A).filter(|m| m.txn_id <= gate);
        let b = self.read_meta_slot(META_PAGE_B).filter(|m| m.txn_id <= gate);
        match (a, b) {
            (Some(a), Some(b)) => Ok(if a.txn_id >= b.txn_id { a } else { b }),
            (Some(m), None) | (None, Some(m)) => Ok(m),
            (None, None) => Err(Error::Corrupt(
                "no valid meta page (both checksums invalid)".into(),
            )),
        }
    }

    /// Ignore the durability gate (used by recovery under the writer lock).
    pub(super) fn newest_meta_ungated(&self) -> Result<MetaSnapshot> {
        let a = self.read_meta_slot(META_PAGE_A);
        let b = self.read_meta_slot(META_PAGE_B);
        match (a, b) {
            (Some(a), Some(b)) => Ok(if a.txn_id >= b.txn_id { a } else { b }),
            (Some(m), None) | (None, Some(m)) => Ok(m),
            (None, None) => Err(Error::Corrupt(
                "no valid meta page (both checksums invalid)".into(),
            )),
        }
    }

    /// Publish a commit into the alternate meta slot. Caller: writer lock
    /// held; all COW data pages already written via plain stores.
    /// Ordering per design/DESIGN.md §4.1: fence(Release) → body fields (Relaxed) →
    /// checksum (Release).
    pub fn write_meta_slot(&self, prev_slot: u64, s: &MetaSnapshot) -> u64 {
        let slot = 1 - prev_slot;
        let base = Self::meta_off(slot);
        fence(Ordering::Release); // orders the plain data-page stores
        self.atomic_u64(base + M_TXN_ID).store(s.txn_id, Ordering::Relaxed);
        self.atomic_u64(base + M_CATALOG_ROOT)
            .store(s.catalog_root, Ordering::Relaxed);
        self.atomic_u64(base + M_FREELIST_ROOT)
            .store(s.freelist_root, Ordering::Relaxed);
        self.atomic_u64(base + M_HIGH_WATER)
            .store(s.high_water, Ordering::Relaxed);
        self.atomic_u64(base + M_EXTENT_MAP_ROOT)
            .store(s.extent_map_root, Ordering::Relaxed);
        self.atomic_u64(base + M_SCHEMA_GEN)
            .store(s.schema_gen, Ordering::Relaxed);
        let mut stamped = *s;
        stamped.slot = slot;
        let sum = xxhash_rust::xxh3::xxh3_64(&self.meta_checksum_input(slot, &stamped));
        self.atomic_u64(base + M_CHECKSUM).store(sum, Ordering::Release);
        // Private path: also publish into the process-local double buffer so
        // `newest_meta` / pin skips dual-slot xxh3. Same stamped body.
        if self.is_private {
            self.publish_priv_meta(&stamped);
        }
        // #121 instrumentation: the commit boundary in the durability trace.
        // The store, not a flush — every FLUSH before this one belongs to this
        // commit's data class, which is exactly what §4.1 orders.
        if crate::plsim::active() {
            let _ = crate::plsim::record_publish(s.txn_id, slot);
        }
        slot
    }

    /// Copy the process-local published meta (`is_private` only).
    #[inline]
    pub(super) fn load_priv_meta(&self) -> MetaSnapshot {
        let i = self.priv_meta_active.load(Ordering::Acquire) as usize & 1;
        // SAFETY: writer only mutates the inactive buffer, then Release-stores
        // the active index. We loaded the index with Acquire, so this buffer
        // is immutable until the next flip (and that flip targets the other).
        unsafe { *self.priv_meta[i].get() }
    }

    /// Publish under the writer lock into the inactive private meta buffer.
    #[inline]
    pub(super) fn publish_priv_meta(&self, s: &MetaSnapshot) {
        let cur = self.priv_meta_active.load(Ordering::Relaxed) as usize & 1;
        let next = 1 - cur;
        // SAFETY: exclusive writer; readers only observe the active buffer.
        unsafe {
            *self.priv_meta[next].get() = *s;
        }
        self.priv_meta_active.store(next as u64, Ordering::Release);
    }

    // ---------- lock area ----------

    pub(super) fn lock_area_off(field: usize) -> usize {
        LOCK_PAGE as usize * PAGE_SIZE + field
    }

    // Linux drives the shared PROCESS_SHARED robust mutex directly; macOS routes
    // writer exclusion through the FLD-2 sidecar flock (crate::os::WriterLock),
    // so this shared-memory mutex pointer is unused there.
    #[cfg(target_os = "linux")]
    pub(super) fn mutex_ptr(&self) -> *mut libc::pthread_mutex_t {
        self.at(Self::lock_area_off(LA_MUTEX)) as *mut libc::pthread_mutex_t
    }

    pub fn durable_txn(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_DURABLE_TXN))
    }

    pub fn oldest_pinned_cache(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_OLDEST_PINNED))
    }

    /// Bytes of durable log (advanced only after fdatasync; §5.4 wal).
    pub fn wal_len(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_WAL_LEN))
    }

    /// Log offset below which records are checkpointed into the main file.
    pub fn wal_ckpt(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_WAL_CKPT))
    }

    /// Append cursor: bytes written to the log, synced or not (§5.4.2 async).
    /// Advanced under the writer lock by `wal_append_async`; read by the
    /// deferred flusher. Unused (stays 0) outside `async`.
    pub fn wal_appended(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_WAL_APPENDED))
    }

    pub(super) fn init_state(&self) -> &AtomicU32 {
        self.atomic_u32(Self::lock_area_off(LA_INIT_STATE))
    }

    /// Acquire the global writer mutex. Returns `true` if EOWNERDEAD recovery
    /// ran (a previous writer died holding the lock). Recovery per §5.2:
    /// msync both meta pages, refresh durable_txn — nothing else, by COW
    /// construction.
    #[cfg(target_os = "linux")]
    pub fn writer_lock(&self) -> Result<bool> {
        // Spin briefly before blocking.
        //
        // At exactly TWO writers the blocking path degenerates into a
        // ping-pong: each release wakes the other, which finds the lock taken
        // again and sleeps. Measured, 2 processes inserting into one file:
        // **0.28 voluntary context switches per insert** and 8.3 µs of CPU per
        // insert, against 0.018 and 4.1 µs at eight writers — total throughput
        // 160k/s at two, 274k/s at eight. More contention, more throughput,
        // because a writer that releases and re-acquires wins the race and gets
        // a burst of work per wake-up. Two is the worst case there is.
        //
        // This changes how we WAIT, not the protocol: the attributes
        // (PROCESS_SHARED + ROBUST + ERRORCHECK, design/DESIGN.md §4.2) are untouched
        // and `trylock` reports `EOWNERDEAD` exactly as `lock` does, so owner-
        // death recovery is unaffected.
        //
        // Bounded, and deliberately small: spinning while the holder is
        // descheduled is pure waste, and a writer holds this lock for a whole
        // commit — not a few instructions. This buys the handoff, not the wait.
        for _ in 0..writer_spins() {
            match unsafe { libc::pthread_mutex_trylock(self.mutex_ptr()) } {
                0 => return Ok(false),
                libc::EOWNERDEAD => return self.adopt_after_owner_death(),
                // EBUSY: someone holds it. NOTE (measured, #109 review): on
                // glibc a ROBUST+ERRORCHECK mutex answers the owner's own
                // trylock with EDEADLK, not EBUSY — try_writer_lock maps that
                // to the reentry error and the deadline path folds it to an
                // immediate Busy. Apple's libpthread deliberately answers
                // EBUSY for the same relock (rdar://16261552), so on macOS
                // the owner falls through to the blocking call below, which
                // reports the reentry properly (§7.2) — meaning the
                // fold-to-immediate-Busy shortcut is glibc-only and a macOS
                // same-thread sibling waits out its deadline instead.
                libc::EBUSY => std::hint::spin_loop(),
                _ => break,
            }
        }
        let rc = unsafe { libc::pthread_mutex_lock(self.mutex_ptr()) };
        match rc {
            0 => Ok(false),
            libc::EOWNERDEAD => self.adopt_after_owner_death(),
            libc::EDEADLK => Err(Error::Internal(
                "writer lock re-entered by its owner (nested write transaction)".into(),
            )),
            _ => Err(Error::Internal(format!("pthread_mutex_lock failed: {rc}"))),
        }
    }

    /// The `EOWNERDEAD` path, shared by the spin and the blocking acquire: a
    /// previous writer died holding the lock, so mark it consistent and run the
    /// §5.2 recovery. We hold the (now-consistent) mutex either way — an error
    /// here must not leak it.
    #[cfg(target_os = "linux")]
    pub(super) fn adopt_after_owner_death(&self) -> Result<bool> {
        unsafe { crate::os::mutex_make_consistent(self.mutex_ptr()) };
        if let Err(e) = self.recover_after_owner_death() {
            self.writer_unlock();
            return Err(e);
        }
        Ok(true)
    }

    /// macOS: acquire exclusion via the sidecar flock + private ERRORCHECK
    /// mutex, then read the tri-state DIRTY word (design/DESIGN-MACOS-LOCK.md §2).
    /// NO ACQUIRE SPIN HERE, and that is a measured decision rather than an
    /// oversight (#164). Linux spins 64 `trylock`s above because #109 measured
    /// a park/wake ping-pong worth buying out. The obvious move was to mirror
    /// it here; the obvious move was wrong. Same-process A/B
    /// (`runner_noise --spin-ab`, 15 reps x 3 s), each platform judged against
    /// its own 1-writer null control — a column where an uncontended `trylock`
    /// succeeds first try, so the spin provably cannot act:
    ///
    /// | writers | Linux (has the spin) | Windows | macOS |
    /// |---|---|---|---|
    /// | 1 (null control) | OK 0.1 % | FAILED 1.3 % | OK 0.1 % |
    /// | 2 | **+11 %** | no effect | no effect |
    /// | 4 | **+14 %** | no effect | **-0.6 %** |
    /// | 8 | **+10 %** | no effect | **-0.4 %** |
    ///
    /// Linux is the positive control: the instrument recovers the known #109
    /// effect, so it can see one when there is one. On the FLD-2 platforms
    /// there is none — nothing on Windows at any width, and a small resolvable
    /// COST on macOS at 4 and 8. So the residual Windows depth gap is not the
    /// handoff; that hypothesis is dead, and what #164 step 2 left standing —
    /// the per-transaction `LockFileEx` syscall pair — is what remains.
    #[cfg(not(target_os = "linux"))]
    pub fn writer_lock(&self) -> Result<bool> {
        self.wl.lock()?; // on Err both levels are already released
        self.post_acquire()
    }

    /// macOS: the tri-state DIRTY signal (the flock-based analogue of
    /// EOWNERDEAD). Runs holding both exclusion levels, before any engine write.
    #[cfg(not(target_os = "linux"))]
    pub(super) fn writer_dirty(&self) -> &AtomicU32 {
        self.atomic_u32(Self::lock_area_off(LA_WRITER_DIRTY))
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn post_acquire(&self) -> Result<bool> {
        match self.writer_dirty().load(Ordering::Acquire) {
            0 => {
                self.writer_dirty().store(1, Ordering::Release);
                Ok(false)
            }
            1 => match self.recover_after_owner_death() {
                Ok(()) => {
                    self.writer_dirty().store(1, Ordering::Release);
                    Ok(true)
                }
                Err(e) => {
                    // POISON (fail-closed): do NOT clear; release exclusion so a
                    // later acquirer hard-errors rather than proceed unrecovered.
                    self.writer_dirty().store(2, Ordering::Release);
                    self.wl.release_exclusion();
                    Err(e)
                }
            },
            _ => {
                self.wl.release_exclusion();
                Err(Error::Internal(
                    "writer lock unrecoverable (ENOTRECOVERABLE): a prior owner-death \
                     recovery failed; database wedged pending operator recovery"
                        .into(),
                ))
            }
        }
    }

    /// Non-blocking writer-lock attempt: Ok(Some(recovered)) on success,
    /// Ok(None) if another process holds it.
    #[cfg(target_os = "linux")]
    pub fn try_writer_lock(&self) -> Result<Option<bool>> {
        let rc = unsafe { libc::pthread_mutex_trylock(self.mutex_ptr()) };
        match rc {
            0 => Ok(Some(false)),
            libc::EBUSY => Ok(None),
            libc::EOWNERDEAD => {
                unsafe { crate::os::mutex_make_consistent(self.mutex_ptr()) };
                if let Err(e) = self.recover_after_owner_death() {
                    self.writer_unlock();
                    return Err(e);
                }
                Ok(Some(true))
            }
            libc::EDEADLK => Err(Error::Internal(
                "writer lock re-entered by its owner (nested write transaction)".into(),
            )),
            _ => Err(Error::Internal(format!("pthread_mutex_trylock failed: {rc}"))),
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn try_writer_lock(&self) -> Result<Option<bool>> {
        match self.wl.trylock()? {
            None => Ok(None),
            Some(()) => self.post_acquire().map(Some),
        }
    }

    /// Base byte offset of intent-ring slot `i` (see `crate::ring`).
    pub fn ring_slot_off(&self, i: u32) -> usize {
        debug_assert!(i < RING_SLOTS);
        ring_start_page(self.max_readers) as usize * PAGE_SIZE + i as usize * RING_SLOT_SIZE
    }

    /// Atomic accessor into the mapping at an absolute byte offset (used by
    /// the ring; offset must be 8-aligned and in-bounds).
    pub fn atomic_u64_at(&self, offset: usize) -> &AtomicU64 {
        self.atomic_u64(offset)
    }

    pub fn atomic_u32_at(&self, offset: usize) -> &AtomicU32 {
        self.atomic_u32(offset)
    }

    /// Plain byte access for owner-exclusive ring payload areas.
    /// Caller contract: the region is written only by the slot owner between
    /// RESERVED and READY, and read only by the lock-holding leader.
    #[allow(clippy::mut_from_ref)]
    pub fn bytes_at_unchecked(&self, offset: usize, len: usize) -> &mut [u8] {
        debug_assert!(offset + len <= self.len);
        unsafe { std::slice::from_raw_parts_mut(self.at(offset), len) }
    }

    pub fn my_pid_start(&self) -> u64 {
        self.my_pid_start
    }

    #[cfg(target_os = "linux")]
    pub fn writer_unlock(&self) {
        unsafe {
            libc::pthread_mutex_unlock(self.mutex_ptr());
        }
    }

    /// macOS: clear MY clean-section flag (CAS 1→0, never resurrecting a POISON)
    /// while still holding flock, then release exclusion.
    #[cfg(not(target_os = "linux"))]
    pub fn writer_unlock(&self) {
        let _ = self
            .writer_dirty()
            .compare_exchange(1, 0, Ordering::Release, Ordering::Relaxed);
        self.wl.release_exclusion();
    }

    pub(super) fn recover_after_owner_death(&self) -> Result<()> {
        match self.durability {
            // Re-establish the double-buffer durability invariant: the dead
            // writer may have published a meta it never msynced. Without this
            // msync, the NEXT commit overwrites the other slot — the only one
            // holding a durable copy of the last acknowledged commit — and a
            // torn write to it during power loss would regress the database
            // below the durable watermark (§5.2).
            Durability::Commit => self.msync_range(0, 2 * PAGE_SIZE)?,
            // WAL-class modes (wal AND async) do NOT need the meta-page msync,
            // checked against the same §5.2 invariant: power-loss recovery
            // replays the log, not the mapping metas. An acknowledged (wal) or
            // appended (async) commit's pages AND meta fields live in a log
            // record at ≥ wal_ckpt BEFORE its meta slot is written in the
            // mapping, and wal_ckpt only advances after a full-mapping MS_SYNC
            // that makes both meta slots durable (wal_checkpoint_if). So
            // overwriting a never-msynced meta slot cannot lose a recorded
            // commit — wal_recover() reconstructs it from the log. (For async
            // the "acknowledged" set is only the deferred-flushed prefix; a
            // dead writer's un-flushed tail is the mode's declared loss window,
            // §5.4.2 — not something the double buffer ever protected.)
            Durability::Wal | Durability::Async | Durability::None => {}
        }
        // Refresh durable_txn from the newest valid meta. Sound in wal mode
        // for the same reason: a meta exists in the mapping only after its
        // record's fdatasync returned, so the newest mapping meta is always
        // durable-by-log and may be acknowledged to readers.
        let newest = self.newest_meta_ungated()?;
        self.durable_txn().fetch_max(newest.txn_id, Ordering::Release);
        Ok(())
    }

    /// msync a range AND barrier it to the platter. The safe default: one call,
    /// one durable range. Every caller but the commit path wants this.
    pub fn msync_range(&self, offset: usize, len: usize) -> Result<()> {
        self.msync_range_nobarrier(offset, len)?;
        self.sync_barrier()
    }

    /// msync a range WITHOUT the platter barrier.
    ///
    /// ⚠ **You must call [`Shm::sync_barrier`] before acking anything as
    /// durable.** On macOS `msync(MS_SYNC)` only hands the pages to the
    /// filesystem; the drive's write cache still holds them, so skipping the
    /// barrier makes `durability=commit` faster and a lie — the exact failure
    /// this codebase has caught all three engines committing (benchmarks/head-to-head.md).
    ///
    /// It exists because `F_FULLFSYNC` is per-FD, not per-range: a caller with
    /// several ranges to make durable can msync them all and barrier ONCE. The
    /// commit path is that caller and the only one — a commit's dirty pages are
    /// scattered (a btree path, a freelist path), and barriering each contiguous
    /// run cost one platter flush per run.
    /// `pwrite` into the main file — the extent payload path
    /// (DESIGN-BLOBEXTENT commitment 6): bytes reach the unified page cache
    /// without faulting mapping pages, and a later `msync` of the same range
    /// makes them durable exactly like mapping stores.
    pub fn file_write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        // wasm32: `write_all_at` is inherent on the wasmcompat File.
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        use std::os::unix::fs::FileExt;
        #[cfg(windows)]
        use crate::wincompat::FileExt;
        self.file
            .write_all_at(buf, offset)
            .map_err(|_| io_err("pwrite(extent)"))
    }

    /// Kernel-side bulk copy from `src` into this database file — the extent
    /// import fast path (#50: measured +70% over the read/pwrite loop on
    /// ext4, because the destination's pages are never faulted through
    /// userspace). Returns `Ok(false)` when the kernel/filesystem cannot
    /// (non-Linux, EXDEV, EOPNOTSUPP, or any partial-progress error after
    /// which the CALLER falls back to pwrite FROM `src_off + copied` — the
    /// returned count makes that resumable). The copied range dirties the
    /// shared page cache exactly as pwrite does, so the existing
    /// `extent_dirty` msync discipline covers its durability (the
    /// powerloss-simulation-proven path); a pure-metadata FICLONERANGE clone
    /// would NOT be covered, which is one of the two reasons it waits for
    /// the vkind=3 format window (the other is body alignment).
    pub fn file_copy_range_from(
        &self,
        src: &File,
        src_off: u64,
        dst_off: u64,
        len: u64,
    ) -> Result<u64> {
        #[cfg(target_os = "linux")]
        {
            #[cfg(unix)]
            use std::os::unix::io::AsRawFd;
            // The kernel updates these in place; a cast-temporary here would
            // silently discard the progress and loop on the same offsets.
            let mut so = src_off as i64;
            let mut dofs = dst_off as i64;
            let mut left = len;
            while left > 0 {
                let n = unsafe {
                    libc::copy_file_range(
                        src.as_raw_fd(),
                        &mut so,
                        self.file.as_raw_fd(),
                        &mut dofs,
                        left as usize,
                        0,
                    )
                };
                // ≤ 0 = error (EXDEV/EOPNOTSUPP/…) or source EOF before
                // `len` (caller declared more bytes than the file holds —
                // its short-read error path owns the message). Either way:
                // report progress, the caller resumes with pwrite.
                if n <= 0 {
                    return Ok(len - left);
                }
                left -= n as u64;
            }
            let _ = (src_off, dst_off);
            Ok(len)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (src, src_off, dst_off, len);
            Ok(0)
        }
    }

    pub fn msync_range_nobarrier(&self, offset: usize, len: usize) -> Result<()> {
        // msync requires the base address aligned to the OS page size. On Linux
        // that is PAGE_SIZE (4096); on Apple Silicon it is 16 KiB — larger than a
        // logical page — so a base at e.g. meta B (page 1) or the lock page
        // (page 2) would give EINVAL. Round the base down to that granularity and
        // grow len by the same delta so the *end* stays fixed (never exceeds the
        // mapping). The extra adjacent pages swept in are harmless: they are
        // immutable COW data or runtime lock/reader state that is reset on attach.
        let gran = crate::os::sync_granularity();
        let start = offset & !(gran - 1);
        let len = len + (offset - start);
        let rc = unsafe { libc::msync(self.at(start) as *mut libc::c_void, len, libc::MS_SYNC) };
        if rc != 0 {
            return Err(io_err("msync"));
        }
        // #121 instrumentation, AFTER the syscall returned — "msync returned"
        // is the durability edge the commit power-loss simulator's fault model
        // is built on. Off unless MPEDB_COMMIT_SYNC_LOG is set (crate::plsim).
        if crate::plsim::active() {
            let end = (start + len).min(self.len);
            let bytes = unsafe { std::slice::from_raw_parts(self.at(start), end - start) };
            crate::plsim::record_flush(start as u64, bytes)?;
        }
        Ok(())
    }

    /// Flush everything already msync'd on this fd to the PLATTER.
    ///
    /// macOS: `msync(MS_SYNC)` hands the pages to the filesystem but does NOT
    /// flush the drive's write cache, so an acked `durability=commit` would still
    /// be sitting in volatile cache — the exact promise the commit class makes.
    /// `F_FULLFSYNC` is the real barrier, and it is per-fd, so one covers every
    /// range msync'd since the last one.
    ///
    /// Linux: nothing to do. `msync(MS_SYNC)` already ran `vfs_fsync` for the
    /// range, and this compiles away.
    ///
    /// Measured on an M3 Pro: without the barrier at all, single-client durable
    /// insert reads 7,583 ops/s (127 µs) — 26x "faster" than a genuinely durable
    /// SQLite, because it is not durable. With it, mpedb `wal` and SQLite+
    /// fullfsync both land at ~290 ops/s, which is what an Apple SSD platter
    /// flush costs.
    #[allow(clippy::unnecessary_wraps)] // Linux: nothing can fail; macOS: plenty
    pub fn sync_barrier(&self) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        {
            // wasm32: `as_raw_fd` is inherent on the wasmcompat File, and
            // `fdatasync` there is the documented no-op (nothing is durable).
            #[cfg(not(target_arch = "wasm32"))]
            #[cfg(unix)]
            use std::os::unix::io::AsRawFd;
            if crate::os::fdatasync(self.file.as_raw_fd()) != 0 {
                return Err(io_err("F_FULLFSYNC after msync"));
            }
        }
        // #121 instrumentation: mark where the engine placed its ordering
        // class break, on every platform (on Linux the call above compiles
        // away, and the FLUSH events themselves are the durability edges).
        if crate::plsim::active() {
            crate::plsim::record_barrier()?;
        }
        Ok(())
    }

    pub fn msync_page(&self, id: u64) -> Result<()> {
        self.msync_range(id as usize * PAGE_SIZE, PAGE_SIZE)
    }


}
