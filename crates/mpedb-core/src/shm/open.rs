//! Shared-memory open operations; ordering and file layout are unchanged.
use super::*;

impl Shm {
    // ---------- open / create ----------

    /// True for the process-private in-memory database path (`":memory:"`).
    /// Not multi-process attachable (no filesystem name).
    pub fn is_memory_path(path: &Path) -> bool {
        path.as_os_str() == ":memory:"
    }

    /// Process-private in-memory database: backing is a memfd (Linux) or an
    /// unlinked temp file (elsewhere), **`ftruncate` only** — no `fallocate`,
    /// no pre-zero. Pages are demand-faulted on first write (true lazy RAM).
    ///
    /// Durability must be [`Durability::None`]: there is no durable path and no
    /// WAL companion. Multi-process attach by path is impossible (and not a
    /// goal); each open is a fresh empty DB.
    pub fn open_memory(
        size_bytes: u64,
        max_readers: u32,
        durability: Durability,
        schema_hash: &[u8; 32],
    ) -> Result<Shm> {
        if durability != Durability::None {
            return Err(Error::Config(
                "in-memory database (path = \":memory:\") requires durability = none".into(),
            ));
        }
        let size = (size_bytes / PAGE_SIZE as u64) * PAGE_SIZE as u64;
        let page_count = size / PAGE_SIZE as u64;
        let min_pages = data_start_page(max_readers) + 8;
        if page_count < min_pages {
            return Err(Error::Config(format!(
                "size_mb too small for in-memory db: need at least {min_pages} pages"
            )));
        }
        // MPEDB_OPEN_TRACE=1: per-stage µs (#N2 0.2.9 — the connect cell's
        // attribution reaches its floor here).
        // Clock only when tracing: `Instant::now()` panics on wasm32, and
        // wasm's database IS a private in-memory one — this exact function.
        let mut t0 = std::env::var_os("MPEDB_OPEN_TRACE")
            .is_some()
            .then(std::time::Instant::now);
        let mut stage = |name: &str| {
            if let Some(prev) = t0 {
                eprintln!("[open_memory] {name}: {}µs", prev.elapsed().as_micros());
                t0 = Some(std::time::Instant::now());
            }
        };
        let file = memory_backing_file()?;
        // `crate::os::ftruncate_len` routes through the LFS variant on 32-bit
        // glibc (armv7's plain `ftruncate` takes a 32-bit off_t) and through
        // the buffer-resize shim on wasm32.
        if crate::os::ftruncate_len(file.as_raw_fd(), size) != 0 {
            return Err(io_err("ftruncate (in-memory size)"));
        }
        stage("memfd+truncate");
        // Synthetic path for macOS .wlock sidecar only; never used as a real
        // multi-process attach name. Linux ignores it in `map`.
        let pseudo = Path::new(":memory:");
        let mut shm = Self::map(&file, size, max_readers, durability, pseudo, true)?;
        shm.is_private = true;
        stage("map");
        shm.format(page_count, max_readers, durability, schema_hash)?;
        stage("format");
        shm.post_attach(schema_hash)?;
        stage("post_attach");
        Ok(shm)
    }

    pub fn open(
        path: &Path,
        size_bytes: u64,
        max_readers: u32,
        durability: Durability,
        schema_hash: &[u8; 32],
        perms: &FilePerms,
    ) -> Result<Shm> {
        if Self::is_memory_path(path) {
            return Self::open_memory(size_bytes, max_readers, durability, schema_hash);
        }
        let size = (size_bytes / PAGE_SIZE as u64) * PAGE_SIZE as u64;
        let page_count = size / PAGE_SIZE as u64;
        let min_pages = data_start_page(max_readers) + 8;
        if page_count < min_pages {
            return Err(Error::Config(format!(
                "size_mb too small: need at least {min_pages} pages for geometry"
            )));
        }
        // Born-restrictive: create owner-only (umask can only tighten, never
        // widen 0o600), so there is no world/group-readable instant even under
        // a concurrent open(). Widened to the configured mode by the formatter
        // below (design/DESIGN-MULTIDB.md §1.4). `.mode()` only affects *creation*, so
        // attaching to an existing file leaves its perms untouched.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;

        // Fast path: fully-sized file that reports READY attaches lock-free.
        let st_size = file.metadata()?.len();
        if st_size == size {
            let mut shm = Self::map(&file, size, max_readers, durability, path, false)?;
            if shm.init_state().load(Ordering::Acquire) == INIT_READY {
                shm.validate_frozen(page_count, max_readers, durability, schema_hash)?;
                if durability.uses_wal() {
                    shm.attach_wal(path)?;
                }
                shm.post_attach(schema_hash)?;
                return Ok(shm);
            }
            drop(shm);
        }

        // Slow path: adopt/format under the kernel-cleaned file lock. Any
        // previous creator death (post-create, mid-fallocate, mid-format)
        // lands here and is repaired.
        let _guard = FlockGuard::exclusive(&file)?;
        let st_size = file.metadata()?.len();
        if st_size != size {
            // NEVER resize a READY database (in either direction): a size
            // mismatch on a live db is config drift, and extending/shrinking
            // it would brick every correctly-configured process. Probe first.
            if st_size >= (LOCK_PAGE + 1) * PAGE_SIZE as u64 {
                let probe = Self::map(&file, st_size, max_readers, durability, path, false)?;
                let ready = probe.init_state().load(Ordering::Acquire) == INIT_READY;
                drop(probe);
                if ready {
                    return Err(Error::Config(format!(
                        "database file is {st_size} bytes but config says {size}; \
                         the file is authoritative — fix the config"
                    )));
                }
            }
            // non-READY debris: safe to resize and re-format
            if st_size > size && crate::os::ftruncate_len(file.as_raw_fd(), size) != 0 {
                return Err(io_err("ftruncate (shrinking debris file)"));
            }
            // real preallocation: ENOSPC surfaces here instead of as SIGBUS
            // on first touch of a hole mid-commit
            let rc = crate::os::preallocate(file.as_raw_fd(), 0, size as i64);
            if rc != 0 {
                return Err(io_err("fallocate (preallocating database file)"));
            }
            // Convert the reserve from UNWRITTEN to WRITTEN extents up front, so
            // per-commit COW writes are plain overwrites — not extent-state
            // conversions each `fdatasync` must journal (measured ~7× on xfs, ~2×
            // on ext4). Bounded: a huge reserve (e.g. an 800 GiB `size_mb`) is
            // left unwritten — zeroing it at create would cost more than it saves,
            // and it amortizes as that database fills. One-time, formatter-only.
            if size <= PREZERO_MAX_BYTES {
                let rc = crate::os::prewrite_zeros(file.as_raw_fd(), size);
                if rc != 0 {
                    return Err(io_err("pre-writing reserved region"));
                }
            }
        }
        let mut shm = Self::map(&file, size, max_readers, durability, path, false)?;
        if durability.uses_wal() {
            shm.attach_wal(path)?; // before format: format truncates debris
        }
        let state = shm.init_state().load(Ordering::Acquire);
        if state != INIT_READY {
            // fresh format (state 0) or a dead initializer's debris (state 1)
            shm.format(page_count, max_readers, durability, schema_hash)?;
            // We are the formatter and hold the exclusive flock: widen the
            // born-0o600 file (and its `-wal`, an equal isolation asset holding
            // recent page images) to the configured mode/owner exactly once.
            apply_file_perms(shm.file.as_raw_fd(), perms)?;
            if let Some(w) = shm.wal.as_ref() {
                apply_file_perms(w.file.as_raw_fd(), perms)?;
            }
        } else {
            shm.validate_frozen(page_count, max_readers, durability, schema_hash)?;
        }
        shm.post_attach(schema_hash)?;
        Ok(shm)
    }

    /// Open an existing, READY database using the file's own frozen
    /// geometry — no config required. For tooling (`mpedb dump`); skips the
    /// schema-hash check (the schema itself is read from the catalog).
    pub fn open_existing(path: &Path) -> Result<Shm> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let st_size = file.metadata()?.len();
        if st_size < 4 * PAGE_SIZE as u64 || st_size % PAGE_SIZE as u64 != 0 {
            return Err(Error::Corrupt(
                "file size is not a valid mpedb database".into(),
            ));
        }
        let mut shm = Self::map(&file, st_size, 1, Durability::None, path, false)?;
        if shm.init_state().load(Ordering::Acquire) != INIT_READY {
            return Err(Error::Corrupt(
                "database file is not initialized (READY marker absent)".into(),
            ));
        }
        let base = Self::meta_off(META_PAGE_A);
        let magic = unsafe { std::slice::from_raw_parts(shm.at(base + M_MAGIC), 8) };
        if magic != MAGIC {
            return Err(Error::Corrupt("bad magic (not an mpedb file)".into()));
        }
        let read_u32 = |off: usize| unsafe { (shm.at(base + off) as *const u32).read() };
        let read_u64 = |off: usize| unsafe { (shm.at(base + off) as *const u64).read() };
        if read_u32(M_FORMAT_VERSION) != FORMAT_VERSION {
            return Err(Error::Schema(format!(
                "file format version {} != engine version {FORMAT_VERSION}",
                read_u32(M_FORMAT_VERSION)
            )));
        }
        if read_u32(M_PAGE_SIZE) as usize != PAGE_SIZE
            || read_u64(M_PAGE_COUNT) != st_size / PAGE_SIZE as u64
        {
            return Err(Error::Corrupt("stored geometry disagrees with file size".into()));
        }
        let max_readers = read_u32(M_MAX_READERS);
        if !(1..=65_536).contains(&max_readers) {
            return Err(Error::Corrupt(format!(
                "stored max_readers {max_readers} out of range"
            )));
        }
        if data_start_page(max_readers) + 1 > st_size / PAGE_SIZE as u64 {
            return Err(Error::Corrupt(
                "stored max_readers implies a reader table beyond the file".into(),
            ));
        }
        let durability = durability_from_tag(read_u32(M_DURABILITY))
            .ok_or_else(|| Error::Corrupt("bad durability tag".into()))?;
        shm.max_readers = max_readers;
        shm.durability = durability;
        shm.data_start = data_start_page(max_readers);
        if durability.uses_wal() {
            shm.attach_wal(path)?;
        }
        let hash_unused = [0u8; 32];
        shm.post_attach(&hash_unused)?;
        Ok(shm)
    }

    /// `private`: this is an in-memory mapping backed by a process-private
    /// temp handle, so no sidecar writer lock is built. See
    /// [`crate::os::WriterLock::private`] — on Windows the sentinel path
    /// `:memory:` is not even a legal filename.
    pub(super) fn map(
        file: &File,
        size: u64,
        max_readers: u32,
        durability: Durability,
        db_path: &Path,
        private: bool,
    ) -> Result<Shm> {
        // Q1 (DESIGN-BLOBEXTENT §14): on a 32-bit target `size as usize`
        // below would silently TRUNCATE a ≥ 4 GiB size — the mapping comes up
        // smaller than `page_count` believes, and the first access past it is
        // a SIGSEGV, not an Error. Refuse here, where it is a config/file
        // problem with a name; every downstream `page * PAGE_SIZE as usize`
        // is then bounded by a size that fits the address space. (A size that
        // fits usize but exceeds what the OS will map still fails cleanly at
        // mmap below.)
        if usize::try_from(size).is_err() {
            return Err(Error::Config(format!(
                "database size {} MiB does not fit this platform's {}-bit address space \
                 — lower size_mb",
                size / (1024 * 1024),
                usize::BITS,
            )));
        }
        #[cfg(target_os = "linux")]
        let _ = db_path; // Linux uses the mapped robust mutex; no sidecar.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io_err("mmap"));
        }
        // opportunistic; harmless where unsupported (macOS: no-op)
        crate::os::madvise_hugepage(ptr, size as usize);
        // macOS/Windows crash-safe writer lock: open (creating if absent)
        // `<db>.wlock`. A private mapping gets the file-less form.
        //
        // Linux takes neither arm — it uses the robust pthread mutex in shared
        // memory and never builds a sidecar at all, so `private` is unread
        // there.
        let _ = private;
        #[cfg(not(target_os = "linux"))]
        let wl = if private {
            crate::os::WriterLock::private()
        } else {
            let mut p = db_path.as_os_str().to_owned();
            p.push(".wlock");
            crate::os::WriterLock::open(Path::new(&p))?
        };
        Ok(Shm {
            map: ptr as *mut u8,
            len: size as usize,
            file: file.try_clone()?,
            path: db_path.to_path_buf(),
            page_count: size / PAGE_SIZE as u64,
            max_readers,
            durability,
            data_start: data_start_page(max_readers),
            my_pid_start: proc_start_time(crate::os::process_id()).ok_or_else(|| {
                Error::Config("cannot read this process's start time for identity".into())
            })?,
            recovered: false,
            wal: None,
            is_private: false,
            priv_pins: std::array::from_fn(|_| AtomicU64::new(PRIV_PIN_FREE)),
            priv_meta_active: AtomicU64::new(0),
            exclusive_write: AtomicU64::new(0),
            priv_meta: [
                UnsafeCell::new(MetaSnapshot {
                    slot: 0,
                    txn_id: 0,
                    catalog_root: 0,
                    freelist_root: 0,
                    high_water: 0,
                    extent_map_root: 0,
                    schema_gen: 0,
                }),
                UnsafeCell::new(MetaSnapshot {
                    slot: 0,
                    txn_id: 0,
                    catalog_root: 0,
                    freelist_root: 0,
                    high_water: 0,
                    extent_map_root: 0,
                    schema_gen: 0,
                }),
            ],
            #[cfg(not(target_os = "linux"))]
            wl,
        })
    }

    /// One-time format. Caller: exclusive flock held. The LAST store, with
    /// Release, is init_state = READY; death at any earlier point leaves a
    /// state the next flock holder re-formats.
    pub(super) fn format(
        &mut self,
        page_count: u64,
        max_readers: u32,
        durability: Durability,
        schema_hash: &[u8; 32],
    ) -> Result<()> {
        self.init_state().store(INIT_FORMATTING, Ordering::Release);

        // zero the control region (metas, lock area, reader table). A PRIVATE
        // backing skips it: `is_private` means a memfd this open just created
        // (open_memory is its only maker), and fresh memfd pages are
        // kernel-zeroed by construction — the memset only faulted all 92 ctl
        // pages in up front, which was 235 of a :memory: connect's ~330 µs
        // (#N2 0.2.9). Every ctl invariant reads identically: untouched bytes
        // stay zero either way, and the stamps below write what they write.
        // Reused SHARED files keep the full wipe — their ctl bytes carry a
        // previous incarnation.
        if !self.is_private {
            let ctl_pages = data_start_page(max_readers) as usize;
            unsafe {
                std::ptr::write_bytes(self.at(0), 0, ctl_pages * PAGE_SIZE);
            }
        }
        // re-set state: the zeroing above cleared it
        self.init_state().store(INIT_FORMATTING, Ordering::Release);
        // Zeroed notification slots would read as "slot owned by table 0", so
        // stamp the no-owner sentinel rather than inheriting that accident.
        self.notify_reset();

        // robust, error-checking, process-shared writer mutex (Linux only).
        // macOS uses the flock-based writer lock: the zeroing above already set
        // the DIRTY word (offset 64) to 0 (CLEAN); a pthread_mutex_init here
        // would stamp a mutex signature over it and corrupt the signal.
        #[cfg(target_os = "linux")]
        unsafe {
            let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
            libc::pthread_mutexattr_init(&mut attr);
            libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED);
            crate::os::mutexattr_set_robust(&mut attr);
            libc::pthread_mutexattr_settype(&mut attr, libc::PTHREAD_MUTEX_ERRORCHECK);
            let rc = libc::pthread_mutex_init(self.mutex_ptr(), &attr);
            libc::pthread_mutexattr_destroy(&mut attr);
            if rc != 0 {
                return Err(Error::Internal(format!("pthread_mutex_init: {rc}")));
            }
        }

        let ns = my_pid_ns_ino()?;
        let bid = boot_id()?;
        unsafe {
            (self.at(Self::lock_area_off(LA_PID_NS_INO)) as *mut u64).write(ns);
            std::ptr::copy_nonoverlapping(
                bid.as_ptr(),
                self.at(Self::lock_area_off(LA_BOOT_ID)),
                16,
            );
        }
        self.durable_txn().store(0, Ordering::Release);
        self.oldest_pinned_cache().store(0, Ordering::Release);

        // frozen fields into both meta pages
        for slot in [META_PAGE_A, META_PAGE_B] {
            let base = Self::meta_off(slot);
            unsafe {
                std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), self.at(base + M_MAGIC), 8);
                (self.at(base + M_FORMAT_VERSION) as *mut u32).write(FORMAT_VERSION);
                (self.at(base + M_PAGE_SIZE) as *mut u32).write(PAGE_SIZE as u32);
                (self.at(base + M_PAGE_COUNT) as *mut u64).write(page_count);
                (self.at(base + M_MAX_READERS) as *mut u32).write(max_readers);
                (self.at(base + M_DURABILITY) as *mut u32).write(durability_tag(durability));
                std::ptr::copy_nonoverlapping(
                    schema_hash.as_ptr(),
                    self.at(base + M_SCHEMA_HASH),
                    32,
                );
            }
        }
        if durability.uses_wal() {
            // any leftover log belongs to a previous incarnation of this
            // file; wal_ckpt = wal_len = 0 in the freshly zeroed lock area
            self.wal_reset_for_format()?;
        }

        // genesis commit: txn 0, empty trees, high_water at data start.
        // Written into slot B so the first real commit goes to slot A.
        let genesis = MetaSnapshot {
            slot: META_PAGE_B,
            txn_id: 0,
            catalog_root: 0,
            freelist_root: 0,
            extent_map_root: 0,
            high_water: data_start_page(max_readers),
            schema_gen: 0,
        };
        self.write_meta_slot(META_PAGE_A, &genesis);
        // slot A gets the same genesis so both slots validate
        self.write_meta_slot(META_PAGE_B, &genesis);

        if durability != Durability::None {
            self.msync_range(0, data_start_page(max_readers) as usize * PAGE_SIZE)?;
        }
        self.init_state().store(INIT_READY, Ordering::Release);
        if durability != Durability::None {
            self.msync_page(LOCK_PAGE)?;
        }
        Ok(())
    }

    pub(super) fn validate_frozen(
        &self,
        page_count: u64,
        max_readers: u32,
        durability: Durability,
        schema_hash: &[u8; 32],
    ) -> Result<()> {
        let base = Self::meta_off(META_PAGE_A);
        let read_u32 = |off: usize| unsafe { (self.at(base + off) as *const u32).read() };
        let read_u64 = |off: usize| unsafe { (self.at(base + off) as *const u64).read() };
        let magic = unsafe { std::slice::from_raw_parts(self.at(base + M_MAGIC), 8) };
        if magic != MAGIC {
            return Err(Error::Corrupt("bad magic (not an mpedb file)".into()));
        }
        if read_u32(M_FORMAT_VERSION) != FORMAT_VERSION {
            return Err(Error::Schema(format!(
                "file format version {} != engine version {FORMAT_VERSION}",
                read_u32(M_FORMAT_VERSION)
            )));
        }
        if read_u32(M_PAGE_SIZE) as usize != PAGE_SIZE {
            return Err(Error::Corrupt("page size mismatch".into()));
        }
        // the file is authoritative for geometry: any drift is a hard error
        if read_u64(M_PAGE_COUNT) != page_count {
            return Err(Error::Config(format!(
                "file has {} pages, config implies {page_count}; fix the config",
                read_u64(M_PAGE_COUNT)
            )));
        }
        if read_u32(M_MAX_READERS) != max_readers {
            return Err(Error::Config(format!(
                "file was created with max_readers={}, config says {max_readers}; \
                 the file is authoritative",
                read_u32(M_MAX_READERS)
            )));
        }
        if read_u32(M_DURABILITY) != durability_tag(durability) {
            return Err(Error::Config(
                "durability mode differs from the one the file was created with".into(),
            ));
        }
        let stored_hash =
            unsafe { std::slice::from_raw_parts(self.at(base + M_SCHEMA_HASH), 32) };
        if stored_hash != schema_hash {
            return Err(Error::Schema(
                "schema hash mismatch: the config schema differs from the database's \
                 (run `mpedb dump --schema` to see the stored schema)"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Identity checks + per-boot recovery. Runs on every attach path.
    pub(super) fn post_attach(&mut self, _schema_hash: &[u8; 32]) -> Result<()> {
        let ns_off = Self::lock_area_off(LA_PID_NS_INO);
        let stored_ns = unsafe { (self.at(ns_off) as *const u64).read() };
        let my_ns = my_pid_ns_ino()?;

        let bid_off = Self::lock_area_off(LA_BOOT_ID);
        let stored_bid: [u8; 16] = unsafe {
            let mut b = [0u8; 16];
            std::ptr::copy_nonoverlapping(self.at(bid_off), b.as_mut_ptr(), 16);
            b
        };
        let my_bid = boot_id()?;

        if !boot_id_matches(&stored_bid)? {
            // First attach since a reboot: pids in the reader table are from a
            // previous boot and the robust mutex's kernel state is gone (a
            // mutex left "locked" by a pre-reboot process would deadlock
            // forever — robust lists do not survive power loss). Reinitialize
            // the volatile control state under the file lock.
            let _guard = FlockGuard::exclusive(&self.file)?;
            let still_stored: [u8; 16] = unsafe {
                let mut b = [0u8; 16];
                std::ptr::copy_nonoverlapping(self.at(bid_off), b.as_mut_ptr(), 16);
                b
            };
            // Windows only: refute a clock-derived mismatch with evidence
            // (see `any_live_reader`). Same boot after all — record the fresh
            // identity so the next attach does not re-derive the mismatch, and
            // do not touch a thing.
            #[cfg(windows)]
            let refuted = self.any_live_reader();
            #[cfg(not(windows))]
            let refuted = false;
            if refuted {
                unsafe {
                    std::ptr::copy_nonoverlapping(my_bid.as_ptr(), self.at(bid_off), 16);
                }
            } else if !boot_id_matches(&still_stored)? {
                // WAL replay FIRST, before any volatile reinit: after power
                // loss the mapping (incl. metas) is whatever the kernel wrote
                // back; the log is the source of truth. Replay is idempotent
                // and the boot id is only updated below, so dying anywhere in
                // here makes the next attacher redo the whole sequence.
                if self.durability.uses_wal() {
                    self.wal_recover()?;
                }
                // Re-init the writer-lock state (Linux: robust mutex; macOS:
                // reset the DIRTY word — a pre-reboot in-section flag is void).
                #[cfg(target_os = "linux")]
                unsafe {
                    let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
                    libc::pthread_mutexattr_init(&mut attr);
                    libc::pthread_mutexattr_setpshared(
                        &mut attr,
                        libc::PTHREAD_PROCESS_SHARED,
                    );
                    crate::os::mutexattr_set_robust(&mut attr);
                    libc::pthread_mutexattr_settype(&mut attr, libc::PTHREAD_MUTEX_ERRORCHECK);
                    libc::pthread_mutex_init(self.mutex_ptr(), &attr);
                    libc::pthread_mutexattr_destroy(&mut attr);
                }
                #[cfg(not(target_os = "linux"))]
                self.writer_dirty().store(0, Ordering::Release);
                unsafe {
                    // clear the reader table: all pre-reboot pins are void
                    std::ptr::write_bytes(
                        self.at(READER_TABLE_PAGE as usize * PAGE_SIZE),
                        0,
                        reader_table_pages(self.max_readers) as usize * PAGE_SIZE,
                    );
                    (self.at(ns_off) as *mut u64).write(my_ns);
                    std::ptr::copy_nonoverlapping(my_bid.as_ptr(), self.at(bid_off), 16);
                }
                // Change generations are pre-reboot facts about a mapping that
                // no longer exists. Left standing, a listener that remembered
                // "table 7 was at generation 900" could conclude "unchanged"
                // about a file it has never actually read.
                self.notify_reset();
                let newest = self.newest_meta_ungated()?;
                self.durable_txn().store(newest.txn_id, Ordering::Release);
                self.oldest_pinned_cache()
                    .store(newest.txn_id, Ordering::Release);
                self.recovered = true;
            }
        } else if stored_ns != my_ns {
            return Err(Error::Config(format!(
                "attach from a different PID namespace (file: {stored_ns}, \
                 process: {my_ns}); kill(2)-based liveness would be unsound — \
                 mpedb requires all processes in one PID namespace"
            )));
        }
        self.sweep_dead_readers();
        Ok(())
    }

}
