//! Platform abstraction for the OS primitives the shared-memory engine needs
//! (task #18). **Linux is the reference platform.**
//!
//! ## macOS is crash-safe (FLD-2)
//!
//! macOS lacks robust process-shared mutexes and Linux futexes, so the writer
//! lock is NOT the shared pthread mutex there. Instead `WriterLock` gives
//! equivalent owner-death recovery via a sidecar `flock` (the kernel releases it
//! when the holder dies) plus a private ERRORCHECK mutex and a shared-memory
//! tri-state DIRTY word (design/DESIGN-MACOS-LOCK.md). Durability uses
//! `fcntl(F_FULLFSYNC)` (real platter flush) and `msync` bases rounded to the
//! 16 KiB Apple-Silicon page. Futex waits degrade to a polling "park" (correct,
//! just busier). Verified: SIGKILL waves recover with `eowner_recovery=true`
//! across none/commit/wal modes, all invariants held, no wedge.
//!
//! Not yet ported: a mid-life sidecar (dev,ino) identity check (DESIGN step 5) —
//! guards only a live DB file unlink+recreate, which the Linux path also leaves
//! unguarded, so it is deferred to keep the platforms symmetric.

//! ## wasm32 is the single-process degenerate case
//!
//! A browser tab has no filesystem, no second process and no durability
//! (`Shm::open_memory` refuses anything but `Durability::None`). Every
//! primitive below therefore has a third arm that is a no-op or a constant,
//! and `crate::wasmcompat` documents why each is sound. The `unix` arms are
//! narrowed to `all(unix, …)` purely so wasm can take that third arm — the
//! Linux and macOS code paths are unchanged.

#[cfg(all(unix, not(target_arch = "wasm32")))]
use std::os::unix::io::RawFd;
// Windows: the handle type and a `libc`-shaped module over real Win32 calls,
// so the functions below gain a fourth arm rather than a fourth signature.
#[cfg(windows)]
use crate::wincompat::{libc, RawFd};

// Large-file support on 32-bit glibc Linux (armv7): the plain libc wrappers
// take a 32-bit `off_t` there, and the engine addresses files past 2 GiB —
// the explicit `*64` variants are the LFS interface. Everywhere else the
// plain names are already 64-bit, so the aliases are the plain names.
#[cfg(all(target_os = "linux", target_env = "gnu", target_pointer_width = "32"))]
use libc::{
    fallocate64 as sys_fallocate, fstat64 as sys_fstat, ftruncate64 as sys_ftruncate,
    pwrite64 as sys_pwrite, stat64 as sys_stat,
};
#[cfg(all(
    target_os = "linux",
    not(all(target_env = "gnu", target_pointer_width = "32"))
))]
use libc::{
    fallocate as sys_fallocate, fstat as sys_fstat, ftruncate as sys_ftruncate,
    pwrite as sys_pwrite, stat as sys_stat,
};

/// `ftruncate` with a 64-bit length on every platform (LFS on 32-bit glibc).
/// The shm layer calls this instead of `libc::ftruncate` directly — including
/// on wasm32, where it lands on the `wasmcompat` buffer-resize shim.
/// Windows: `SetFilePointerEx` + `SetEndOfFile` — there is no single call that
/// sets a length, which is why the shim pairs them.
#[cfg(windows)]
pub fn ftruncate_len(fd: RawFd, len: u64) -> libc::c_int {
    unsafe { libc::ftruncate(fd, len as i64) }
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
pub fn ftruncate_len(fd: RawFd, len: u64) -> libc::c_int {
    #[cfg(target_os = "linux")]
    {
        unsafe { sys_ftruncate(fd, len as i64) }
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsafe { libc::ftruncate(fd, len as libc::off_t) }
    }
}
#[cfg(target_arch = "wasm32")]
use crate::wasmcompat::{libc, RawFd};

/// The wasm32 arm of [`ftruncate_len`]: `off_t` is already 64-bit in the
/// buffer shim, so this is the plain resize.
#[cfg(target_arch = "wasm32")]
pub fn ftruncate_len(fd: RawFd, len: u64) -> libc::c_int {
    unsafe { libc::ftruncate(fd, len as libc::off_t) }
}
use std::sync::atomic::AtomicU32;
use std::time::Duration;

/// Flush file data to storage. Linux: `fdatasync`. macOS: `fcntl(F_FULLFSYNC)`
/// — the only macOS call that forces the drive to flush its write cache to the
/// platter (plain `fsync` returns before that, so a power loss can still lose an
/// acked commit). Slower than `fsync`, but that is the price of real durability.
/// Falls back to `fsync` only when the filesystem rejects F_FULLFSYNC (ENOTSUP).
pub fn fdatasync(fd: RawFd) -> libc::c_int {
    // wasm32: nothing has been promised durable, and there is no device.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = fd;
        0
    }
    // Windows: `FlushFileBuffers` IS the platter barrier. `FlushViewOfFile`
    // (the `msync` position) reaches only the filesystem cache, so the two
    // compose exactly as macOS's `msync` + `F_FULLFSYNC` do.
    #[cfg(windows)]
    {
        unsafe { libc::fsync(fd) }
    }
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::fdatasync(fd) }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if rc == -1 {
            let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if e == libc::ENOTSUP || e == libc::EINVAL || e == libc::ENOTTY {
                return unsafe { libc::fsync(fd) };
            }
        }
        rc
    }
}

/// Base-address alignment that `msync`/`mmap` require: the OS page size.
/// Linux: 4096 (== the engine's logical `PAGE_SIZE`). macOS on Apple Silicon:
/// 16384 — larger than a logical page, so an `msync` whose base is a logical
/// page that is not also a 16 KiB boundary returns `EINVAL`. Callers round the
/// base down to this granularity. Cached after the first `sysconf`.
pub fn sync_granularity() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHE: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    // wasm32 has no `sysconf`; a wasm page is 64 KiB but nothing here is a real
    // mapping, and `msync` is a no-op, so the alignment only has to be a
    // multiple of the engine's logical PAGE_SIZE.
    #[cfg(target_arch = "wasm32")]
    let g: isize = 4096;
    // Windows wants the ALLOCATION granularity (64 KiB), not the page size:
    // `MapViewOfFile` rejects an offset that is page- but not 64-KiB-aligned.
    // Same class of trap as macOS's 16 KiB `msync` base.
    #[cfg(windows)]
    let g = crate::wincompat::sync_granularity() as isize;
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    let g = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let g = if g > 0 { g as usize } else { 4096 };
    CACHE.store(g, Ordering::Relaxed);
    g
}

/// Ensure `[offset, offset+len)` is backed by real blocks (Linux: `fallocate`,
/// so a mid-commit touch never hits a lazy hole → no SIGBUS). macOS
/// (bench-grade): grow the file with `ftruncate` (may leave a sparse hole; fine
/// while disk space is available). Never shrinks.
/// Windows: grow the file. `SetFileValidData` would mark the range valid
/// without writing it, but it requires SE_MANAGE_VOLUME_NAME — a privilege a
/// library must not assume it has — and going without costs only the same
/// lazy-hole behaviour the macOS arm already accepts.
#[cfg(windows)]
pub fn preallocate(fd: RawFd, offset: i64, len: i64) -> libc::c_int {
    let want = offset + len;
    let cur = unsafe { libc::file_size(fd) };
    if want > cur {
        unsafe { libc::ftruncate(fd, want) }
    } else {
        0
    }
}

#[cfg(not(windows))]
pub fn preallocate(fd: RawFd, offset: i64, len: i64) -> libc::c_int {
    // wasm32: `ftruncate` already zero-fills the whole reserve up front, so
    // every byte is backed. There is no lazy hole and no SIGBUS to avoid.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (fd, offset, len);
        0
    }
    #[cfg(target_os = "linux")]
    {
        let rc = unsafe { sys_fallocate(fd, 0, offset, len) };
        if rc == 0 {
            return 0;
        }
        // Some filesystems (FAT/exFAT, many network FS) do not implement
        // fallocate and return EOPNOTSUPP/ENOSYS. Fall back to ftruncate. On
        // those filesystems this is still SIGBUS-safe: they cannot represent a
        // sparse hole, so growing the file physically allocates the blocks
        // rather than leaving a lazy hole (unlike ext4/xfs, where we never take
        // this path). Never shrinks.
        let e = unsafe { *libc::__errno_location() };
        if e == libc::EOPNOTSUPP || e == libc::ENOSYS {
            let want = offset + len;
            let mut st: sys_stat = unsafe { std::mem::zeroed() };
            let cur = if unsafe { sys_fstat(fd, &mut st) } == 0 { st.st_size } else { 0 };
            return if want > cur { unsafe { sys_ftruncate(fd, want) } } else { 0 };
        }
        rc
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let want = offset + len;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let cur = if unsafe { libc::fstat(fd, &mut st) } == 0 { st.st_size } else { 0 };
        if want > cur {
            unsafe { libc::ftruncate(fd, want) }
        } else {
            0
        }
    }
}

/// Force `[0, len)` from UNWRITTEN to WRITTEN extents by writing zeros over it.
///
/// `preallocate` (fallocate) reserves blocks but leaves them *unwritten*; the
/// FIRST write to an unwritten extent triggers a filesystem extent-state
/// conversion that `fdatasync` must then journal. Because mpedb is copy-on-write
/// — every commit allocates FRESH pages from the reserve — that conversion lands
/// on nearly every commit, a measured ~7× stall on xfs and ~2× on ext4. Doing it
/// ONCE up front turns every later commit into a plain overwrite. The caller
/// gates this on file size: a multi-hundred-GiB reserve is left unwritten, since
/// zeroing it at create would dwarf any per-commit saving. Returns 0 on success.
/// No-op on non-Linux (macOS reserves via sparse `ftruncate`; it is bench-grade).
/// Windows likewise has nothing to convert: the unwritten-extent stall is an
/// ext4/xfs behaviour and NTFS has no equivalent state for a `SetEndOfFile`
/// grow, so zeroing the reserve would be pure cost.
#[cfg(target_os = "linux")]
pub fn prewrite_zeros(fd: RawFd, len: u64) -> libc::c_int {
    const CHUNK: usize = 1 << 20; // 1 MiB write buffer
    let zeros = vec![0u8; CHUNK];
    let mut off: u64 = 0;
    while off < len {
        let n = CHUNK.min((len - off) as usize);
        let w = unsafe { sys_pwrite(fd, zeros.as_ptr() as *const libc::c_void, n, off as i64) };
        if w < 0 || w as usize != n {
            return -1;
        }
        off += n as u64;
    }
    0
}

// Also the wasm32 arm: the buffer is born zeroed, so there is no
// unwritten→written extent conversion to pay down.
#[cfg(not(target_os = "linux"))]
pub fn prewrite_zeros(_fd: RawFd, _len: u64) -> libc::c_int {
    0
}

/// Reclaim `[offset, offset+len)` as a hole (WAL checkpoint). Best-effort;
/// failure only wastes space. macOS: no-op (space is not reclaimed).
/// Windows: `FSCTL_SET_ZERO_DATA` on a sparse file would do this. Not wired in
/// stage 1 — punching returns space to the filesystem and is never a
/// correctness requirement, which is the same call the macOS arm makes.
#[cfg(windows)]
pub fn punch_hole(fd: RawFd, offset: i64, len: i64) {
    let _ = (fd, offset, len);
}

#[cfg(not(windows))]
pub fn punch_hole(fd: RawFd, offset: i64, len: i64) {
    #[cfg(target_os = "linux")]
    unsafe {
        sys_fallocate(
            fd,
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset,
            len,
        );
    }
    // Non-linux (macOS and wasm32): no hole punching; space is not reclaimed.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fd, offset, len);
    }
}

/// Advise transparent huge pages over the mapping. Opportunistic; macOS: no-op.
/// Windows: large pages need SE_LOCK_MEMORY_NAME and a non-pageable
/// allocation, which does not fit a file-backed shared mapping. This is a
/// throughput hint, not a requirement.
#[cfg(windows)]
pub fn madvise_hugepage(ptr: *mut libc::c_void, len: usize) {
    let _ = (ptr, len);
}

#[cfg(not(windows))]
pub fn madvise_hugepage(ptr: *mut libc::c_void, len: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::madvise(ptr, len, libc::MADV_HUGEPAGE);
    }
    // Non-linux (macOS and wasm32): no huge-page advice.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ptr, len);
    }
}

/// Make a process-shared mutex robust so it survives owner death (`EOWNERDEAD`).
/// Linux-only: macOS lacks robust mutexes and instead gets its owner-death
/// recovery from the FLD-2 sidecar `flock` writer lock ([`WriterLock`]), so the
/// shared pthread mutex is never used there.
///
/// # Safety
/// `attr` must point to an initialized `pthread_mutexattr_t`.
#[cfg(target_os = "linux")]
pub unsafe fn mutexattr_set_robust(attr: *mut libc::pthread_mutexattr_t) {
    libc::pthread_mutexattr_setrobust(attr, libc::PTHREAD_MUTEX_ROBUST);
}

/// Mark a mutex consistent after `EOWNERDEAD` recovery. Linux-only (see
/// [`mutexattr_set_robust`]).
///
/// # Safety
/// `m` must point to a locked mutex recovered from `EOWNERDEAD`.
#[cfg(target_os = "linux")]
pub unsafe fn mutex_make_consistent(m: *mut libc::pthread_mutex_t) -> libc::c_int {
    libc::pthread_mutex_consistent(m)
}

/// Cross-process futex wait: return after a wake, a value change, or the
/// timeout. Callers always re-check state, so an early/spurious return is fine.
/// macOS has no cross-process futex: **park briefly and return** ⇒ the caller
/// polls (correct, just busier).
/// Windows: `WaitOnAddress` is a DIRECT futex equivalent, not a degradation —
/// macOS has to fall back to polling here and Windows does not.
#[cfg(windows)]
pub fn futex_wait(word: &AtomicU32, expected: u32, timeout: Duration) {
    crate::wincompat::futex_wait(word, expected, timeout);
}

#[cfg(not(windows))]
pub fn futex_wait(word: &AtomicU32, expected: u32, timeout: Duration) {
    #[cfg(target_os = "linux")]
    unsafe {
        let ts = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        libc::syscall(
            libc::SYS_futex,
            word.as_ptr(),
            libc::FUTEX_WAIT, // shared (no PRIVATE flag): cross-process
            expected,
            &ts,
        );
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = (word, expected);
        std::thread::sleep(timeout.min(Duration::from_micros(200)));
    }
    // wasm32: single-threaded. No other thread can ever post the wake this
    // would wait for, so a wait that returned late would be a pure hang;
    // returning immediately makes the caller re-check state, which is the
    // documented contract.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (word, expected, timeout);
    }
}

/// Wake all waiters on `word`. macOS: no-op (waiters poll).
/// Windows: `WakeByAddressAll`.
#[cfg(windows)]
pub fn futex_wake_all(word: &AtomicU32) {
    crate::wincompat::futex_wake_all(word);
}

#[cfg(not(windows))]
pub fn futex_wake_all(word: &AtomicU32) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAKE, i32::MAX);
    }
    // Non-linux (macOS: waiters poll; wasm32: there are no waiters).
    #[cfg(not(target_os = "linux"))]
    {
        let _ = word;
    }
}

// ---- macOS crash-safe writer lock (design/DESIGN-MACOS-LOCK.md, FLD-2) -------------
//
// Linux uses the robust pthread mutex directly (in shm.rs). macOS has none, so
// the writer lock is: a sidecar-inode `flock` (the KERNEL releases it when the
// holder dies → free death oracle + rendezvous) + a process-private ERRORCHECK
// mutex (intra-process exclusion + re-entrancy → EDEADLK). shm.rs layers the
// tri-state DIRTY word (the "recovered" signal) on top. This struct provides
// ONLY the exclusion primitives.

#[cfg(all(unix, not(target_os = "linux")))]
pub use macos_lock::WriterLock;

#[cfg(target_arch = "wasm32")]
pub use wasm_lock::WriterLock;

/// The wasm32 writer lock. `shm.rs` takes the same non-Linux route macOS does
/// (a `WriterLock` object plus the shared tri-state DIRTY word) — but with one
/// thread and one process, exclusion collapses to a flag.
///
/// The flag is not a formality: it preserves the ERRORCHECK behaviour the
/// native lock has. A nested write transaction is a re-entrant acquire, and
/// native answers that with `EDEADLK` rather than deadlocking. So does this —
/// the SAME `Error::Internal("writer lock re-entered …")` the macOS path
/// returns — which is what the task means by "assert non-reentrancy rather
/// than lock". Owner death needs no recovery here: the only owner is us, and
/// if we are gone so is the entire heap the database lived in.
#[cfg(target_arch = "wasm32")]
mod wasm_lock {
    use mpedb_types::{Error, Result};
    use std::cell::Cell;
    use std::rc::Rc;

    thread_local! {
        /// One flag per process. There is exactly one database mapping in a
        /// module instance, and no path that opens a second concurrently.
        static HELD: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    }

    pub struct WriterLock {
        held: Rc<Cell<bool>>,
    }

    // Single-threaded by construction: `wasm32-unknown-unknown` has no
    // threads, so nothing can observe this from another thread. `Shm` is
    // declared Send+Sync for the native multi-process case and that
    // declaration must keep compiling here.
    unsafe impl Send for WriterLock {}
    unsafe impl Sync for WriterLock {}

    impl WriterLock {
        /// No sidecar file exists (or could); the path is accepted and ignored.
        pub fn open(_path: &std::path::Path) -> Result<WriterLock> {
            Ok(WriterLock {
                held: HELD.with(|h| h.clone()),
            })
        }

        /// Every wasm32 lock is already the private one — this exists so the
        /// caller can be written once for both file-less platforms.
        pub fn private() -> WriterLock {
            WriterLock {
                held: HELD.with(|h| h.clone()),
            }
        }

        pub fn lock(&self) -> Result<()> {
            if self.held.replace(true) {
                return Err(Error::Internal(
                    "writer lock re-entered by its owner (nested write transaction)".into(),
                ));
            }
            Ok(())
        }

        pub fn trylock(&self) -> Result<Option<()>> {
            if self.held.replace(true) {
                // Single-threaded: "already held" can only be US, so this is
                // re-entrancy, not contention. Reporting EBUSY-style `None`
                // would send the caller into a retry loop that can never win.
                self.held.set(true);
                return Err(Error::Internal(
                    "writer lock re-entered by its owner (nested write transaction)".into(),
                ));
            }
            Ok(Some(()))
        }

        pub fn release_exclusion(&self) {
            self.held.set(false);
        }
    }
}

/// Windows writer lock: a sidecar `LockFileEx` plus a process-private
/// re-entrancy guard — the same two-level shape as the macOS FLD-2 lock, for
/// the same reason.
///
/// **Owner death is handled by the kernel**, exactly as `flock` does on macOS:
/// Windows releases every lock a handle holds when that handle closes, and it
/// closes them all when the process dies, however it dies. That is the property
/// the whole FLD-2 design exists to reconstruct, and here it comes for free.
///
/// The local guard is a plain `Mutex<Option<ThreadId>>` rather than a
/// pthread ERRORCHECK mutex: its only job is to turn a nested write transaction
/// into a named error instead of a self-deadlock, and the owning thread id is
/// enough to see that. A second `LockFileEx` from the SAME handle would
/// succeed silently on Windows, so without this a re-entrant writer would
/// quietly get two locks and release one.
#[cfg(windows)]
mod win_lock {
    use crate::wincompat::{libc, AsRawFd};
    use mpedb_types::{Error, Result};
    use std::collections::HashMap;
    use std::fs::File;
    use std::sync::{Arc, LazyLock, Mutex, Weak};
    use std::thread::ThreadId;

    fn reentered() -> Error {
        Error::Internal("writer lock re-entered by its owner (nested write transaction)".into())
    }
    fn ioerr(ctx: &str) -> Error {
        Error::Io(std::io::Error::new(
            std::io::Error::last_os_error().kind(),
            format!("{ctx}: {}", std::io::Error::last_os_error()),
        ))
    }

    struct Inner {
        /// `None` for a PRIVATE (`:memory:`) mapping: there is no file to
        /// exclude on and no second process that could see it, so the local
        /// `owner` guard is the whole lock. Some(f) owns the handle; drop →
        /// close → lock auto-release.
        file: Option<File>,
        /// Which thread is INSIDE the section — accurate because only the
        /// SRWLOCK holder writes it. Used for re-entrancy only.
        owner: Mutex<Option<ThreadId>>,
        /// The intra-process half of the lock (#164). Process-PRIVATE and
        /// never in the mapping: an SRWLOCK has no owner-death property, and
        /// the cross-process half (`LockFileEx`) is what supplies that.
        srw: crate::wincompat::SrwLock,
    }

    /// One shared `Inner` per canonical path per process. Windows byte-range
    /// locks are per HANDLE, so two opens of the same file in one process would
    /// not exclude each other — the second would take the lock while the first
    /// still held it, and both would believe they were the only writer. The
    /// registry hands every in-process handle the same one, which turns that
    /// into the re-entrancy error it actually is.
    type LockRegistry = HashMap<std::path::PathBuf, Weak<Inner>>;
    static REGISTRY: LazyLock<Mutex<LockRegistry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

    pub struct WriterLock {
        inner: Arc<Inner>,
    }

    impl WriterLock {
        pub fn open(path: &std::path::Path) -> Result<WriterLock> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                // Explicit: the sidecar's CONTENT is never read — the lock
                // lives in the kernel, not in the bytes — but truncating an
                // existing one would still be wrong, because a second process
                // may hold a lock on it right now.
                .truncate(false)
                .open(path)?;
            // Canonicalize so two spellings of one path share an Inner. It can
            // only fail if the file vanished between create and here, in which
            // case the un-canonical path is still a correct key for this run.
            let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(inner) = reg.get(&key).and_then(Weak::upgrade) {
                drop(file);
                return Ok(WriterLock { inner });
            }
            let inner = Arc::new(Inner {
                file: Some(file),
                owner: Mutex::new(None),
                srw: crate::wincompat::SrwLock::new(),
            });
            reg.insert(key, Arc::downgrade(&inner));
            Ok(WriterLock { inner })
        }

        /// A lock for a PRIVATE (`:memory:`) mapping — no sidecar file, and
        /// deliberately not in the registry, because two private mappings are
        /// two unrelated databases that happen to share a name.
        ///
        /// The cross-process half of the lock has nothing to exclude: the
        /// backing file is a process-private temp handle no other process can
        /// name. Building it anyway is not merely wasteful on Windows, it is
        /// broken — the sentinel path is `:memory:`, and `:` cannot appear in
        /// a Windows filename, so the create fails and an in-memory database
        /// cannot open at all.
        pub fn private() -> WriterLock {
            WriterLock {
                inner: Arc::new(Inner {
                    file: None,
                    owner: Mutex::new(None),
                    // A `:memory:` database has no sidecar, so before #164 it
                    // had NO thread exclusion at all here — only re-entrancy
                    // detection. The SRWLOCK gives a private mapping the same
                    // intra-process guarantee a shared one gets, which is what
                    // the macOS arm already had via its pthread mutex.
                    srw: crate::wincompat::SrwLock::new(),
                }),
            }
        }

        /// Re-entrancy, asked BEFORE the SRWLOCK is touched.
        ///
        /// `SRWLOCK` is not recursive: a nested exclusive acquire from the
        /// owning thread deadlocks rather than failing. The whole stack above
        /// depends on getting a NAMED error instead — `begin_write_deadline`
        /// folds it to `Busy`, and the C-API maps it to `SQLITE_BUSY`.
        ///
        /// Reading `owner` without holding the section is exact for this one
        /// question: if it names the caller, only the caller could have put it
        /// there while holding, so it is a true re-entry. Any other value means
        /// somebody else holds it or nobody does, and both lead to the acquire
        /// below.
        fn check_not_reentered(&self) -> Result<()> {
            let g = self.inner.owner.lock().unwrap_or_else(|e| e.into_inner());
            if *g == Some(std::thread::current().id()) {
                return Err(reentered());
            }
            Ok(())
        }

        /// Record (or clear) the owning thread. Only ever called by the thread
        /// that holds the SRWLOCK, which is what makes it accurate — the old
        /// `claim()` wrote this field BEFORE winning anything, so under
        /// contention every arriving thread overwrote it and the re-entrancy
        /// detector named the wrong thread. Nothing caught that: every test of
        /// it is single-threaded.
        fn set_owner(&self, who: Option<std::thread::ThreadId>) {
            *self.inner.owner.lock().unwrap_or_else(|e| e.into_inner()) = who;
        }

        /// Blocking acquire: the process-private SRWLOCK first, then the
        /// cross-process `LockFileEx`.
        ///
        /// The order is the point (#164). Threads queue on a ~25 ns userspace
        /// lock, and the file lock is reached by exactly one thread per
        /// process — so it only ever sees genuine cross-process contention,
        /// exactly as `flock` does on the macOS arm. Before this, four threads
        /// all blocked in `LockFileEx` on one shared handle: correct (#165
        /// proves it excludes) but 4.4x slower than one thread on the same
        /// machine.
        ///
        /// `LockFileEx` stays the cross-process layer and is not up for
        /// negotiation: the kernel releases it on process death with no
        /// abandoned state, which is what the DIRTY word's owner-death
        /// protocol is built on. An SRWLOCK has no such property, which is
        /// precisely why it is process-PRIVATE here and never in the mapping.
        pub fn lock(&self) -> Result<()> {
            self.check_not_reentered()?;
            self.inner.srw.lock();
            self.set_owner(Some(std::thread::current().id()));
            let Some(f) = self.inner.file.as_ref() else { return Ok(()) };
            let fd = f.as_raw_fd();
            if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
                return Ok(());
            }
            self.set_owner(None);
            // SAFETY: acquired three lines up and not released since.
            unsafe { self.inner.srw.unlock() };
            Err(ioerr("LockFileEx(exclusive)"))
        }

        pub fn trylock(&self) -> Result<Option<()>> {
            self.check_not_reentered()?;
            if !self.inner.srw.try_lock() {
                // Another THREAD of this process holds it. Contention, not
                // failure — and it no longer costs a kernel round trip to
                // discover.
                return Ok(None);
            }
            self.set_owner(Some(std::thread::current().id()));
            let Some(f) = self.inner.file.as_ref() else { return Ok(Some(())) };
            let fd = f.as_raw_fd();
            if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(()));
            }
            let errno = libc::last_errno();
            self.set_owner(None);
            // SAFETY: acquired above and not released since.
            unsafe { self.inner.srw.unlock() };
            if errno == libc::EAGAIN {
                // ERROR_LOCK_VIOLATION: another PROCESS holds it.
                Ok(None)
            } else {
                Err(ioerr("LockFileEx(exclusive, nonblocking)"))
            }
        }

        /// Release, innermost first: the file lock, then the owner record,
        /// then the SRWLOCK. `shm::writer_unlock` CASes the DIRTY word 1 to 0
        /// BEFORE calling this and while still holding exclusion, so that
        /// ordering is preserved with the file lock dropped first here.
        pub fn release_exclusion(&self) {
            if let Some(f) = self.inner.file.as_ref() {
                unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
            }
            self.set_owner(None);
            // SAFETY: `lock`/`trylock` returned success, so this thread holds it.
            unsafe { self.inner.srw.unlock() };
        }
    }
}

#[cfg(windows)]
pub use win_lock::WriterLock;

#[cfg(all(unix, not(target_os = "linux")))]
mod macos_lock {
    use mpedb_types::{Error, Result};
    use std::collections::HashMap;
    use std::fs::File;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::sync::{Arc, LazyLock, Mutex, Weak};

    fn reentered() -> Error {
        Error::Internal("writer lock re-entered by its owner (nested write transaction)".into())
    }
    fn ioerr(ctx: &str) -> Error {
        Error::Io(std::io::Error::new(
            std::io::Error::last_os_error().kind(),
            format!("{ctx}: {}", std::io::Error::last_os_error()),
        ))
    }

    struct Inner {
        /// `None` for a PRIVATE (`:memory:`) mapping — see
        /// [`WriterLock::private`]. Some(f) OWNS the wl_fd; drop → close →
        /// flock auto-release.
        file: Option<File>,
        local_mtx: *mut libc::pthread_mutex_t, // process-private ERRORCHECK
    }
    // The pthread mutex is thread-safe; the File is Send+Sync. One Inner per
    // (dev,ino) per process, shared behind Arc.
    unsafe impl Send for Inner {}
    unsafe impl Sync for Inner {}

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe {
                libc::pthread_mutex_destroy(self.local_mtx);
                drop(Box::from_raw(self.local_mtx));
            }
        }
    }

    // One shared Inner per (dev,ino) per process: a second open() of the SAME
    // file would otherwise be a distinct OFD whose flock self-BLOCKS the first
    // (flock treats separate fds independently), deadlocking the process. The
    // registry hands every in-process handle the SAME OFD + mutex, so a double
    // open is caught as EDEADLK re-entrancy, not a self-deadlock.
    /// `(dev, ino)` → the process-shared lock state for that file.
    type LockRegistry = HashMap<(u64, u64), Weak<Inner>>;
    static REGISTRY: LazyLock<Mutex<LockRegistry>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn make_errorcheck_mutex() -> *mut libc::pthread_mutex_t {
        let m = Box::into_raw(Box::new(unsafe { std::mem::zeroed::<libc::pthread_mutex_t>() }));
        unsafe {
            let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
            libc::pthread_mutexattr_init(&mut attr);
            libc::pthread_mutexattr_settype(&mut attr, libc::PTHREAD_MUTEX_ERRORCHECK);
            libc::pthread_mutex_init(m, &attr);
            libc::pthread_mutexattr_destroy(&mut attr);
        }
        m
    }

    /// The sidecar-`flock` writer lock. Cheap to clone (Arc).
    pub struct WriterLock {
        inner: Arc<Inner>,
    }

    impl WriterLock {
        /// Open (creating if absent) the sidecar `<db>.wlock`. Processes that
        /// open the same inode share one OFD (and one local mutex) via the
        /// per-(dev,ino) registry, so `flock` exclusion is cross-process.
        pub fn open(path: &std::path::Path) -> Result<WriterLock> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC) // never inherit across exec → no wedge
                .open(path)?;
            let fd = file.as_raw_fd();
            // belt-and-braces (some fork paths clear O_CLOEXEC creation intent).
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut st) } != 0 {
                return Err(ioerr("fstat(wlock)"));
            }
            let devino = (st.st_dev as u64, st.st_ino as u64);

            let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(inner) = reg.get(&devino).and_then(Weak::upgrade) {
                drop(file); // reuse the registered OFD; close this duplicate fd
                return Ok(WriterLock { inner });
            }
            let inner = Arc::new(Inner {
                file: Some(file),
                local_mtx: make_errorcheck_mutex(),
            });
            reg.insert(devino, Arc::downgrade(&inner));
            Ok(WriterLock { inner })
        }

        /// A lock for a PRIVATE (`:memory:`) mapping — no sidecar file, and
        /// deliberately not in the registry, since two private mappings are two
        /// unrelated databases that happen to share a sentinel name.
        ///
        /// The `flock` half has nothing to exclude: the backing store is a
        /// process-private temp handle no other process can name. Building it
        /// anyway wrote a file called `:memory:.wlock` into the caller's
        /// working directory — litter here, and fatal on Windows where `:` is
        /// not a legal filename character (#159). The ERRORCHECK mutex is kept,
        /// because nested write transactions must still be caught.
        pub fn private() -> WriterLock {
            WriterLock {
                inner: Arc::new(Inner {
                    file: None,
                    local_mtx: make_errorcheck_mutex(),
                }),
            }
        }

        /// Blocking acquire of exclusion: local mutex (re-entrancy → Err), then
        /// the cross-process `flock(LOCK_EX)` (the kernel wait; wakes on release
        /// or holder death). On Err, both levels are already released.
        pub fn lock(&self) -> Result<()> {
            let m = self.inner.local_mtx;
            match unsafe { libc::pthread_mutex_lock(m) } {
                0 => {}
                libc::EDEADLK => return Err(reentered()),
                rc => return Err(Error::Internal(format!("local writer mutex lock: {rc}"))),
            }
            let Some(f) = self.inner.file.as_ref() else { return Ok(()) };
            let fd = f.as_raw_fd();
            loop {
                if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
                    return Ok(());
                }
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                unsafe { libc::pthread_mutex_unlock(m) };
                return Err(ioerr("flock(LOCK_EX)"));
            }
        }

        /// Non-blocking acquire: Ok(Some(())) held, Ok(None) if another process
        /// or thread holds it.
        pub fn trylock(&self) -> Result<Option<()>> {
            let m = self.inner.local_mtx;
            match unsafe { libc::pthread_mutex_trylock(m) } {
                0 => {}
                libc::EDEADLK => return Err(reentered()),
                libc::EBUSY => return Ok(None),
                rc => return Err(Error::Internal(format!("local writer mutex trylock: {rc}"))),
            }
            let Some(f) = self.inner.file.as_ref() else { return Ok(Some(())) };
            let fd = f.as_raw_fd();
            if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let e = std::io::Error::last_os_error().raw_os_error();
                unsafe { libc::pthread_mutex_unlock(m) };
                if e == Some(libc::EWOULDBLOCK) {
                    return Ok(None);
                }
                return Err(ioerr("flock(LOCK_EX|NB)"));
            }
            Ok(Some(()))
        }

        /// Release both levels (infallible; `flock(UN)` retried on EINTR).
        pub fn release_exclusion(&self) {
            let Some(f) = self.inner.file.as_ref() else {
                unsafe { libc::pthread_mutex_unlock(self.inner.local_mtx) };
                return;
            };
            let fd = f.as_raw_fd();
            loop {
                if unsafe { libc::flock(fd, libc::LOCK_UN) } == 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    break;
                }
            }
            unsafe { libc::pthread_mutex_unlock(self.inner.local_mtx) };
        }
    }
}

// ---- process / boot identity (reader-slot pid-reuse + boot recovery) --------

/// A per-process start time; `(pid, start_time)` survives PID reuse. Linux:
/// `/proc/<pid>/stat` field 22. macOS: `proc_pidinfo(PROC_PIDTBSDINFO)` start
/// instant. Returns `None` if the pid is gone (caller treats that as dead).
/// Windows: the process CREATION TIME, which plays exactly the role
/// `/proc/<pid>/stat`'s start time plays on Linux — a pid can be recycled, a
/// (pid, creation-time) pair cannot, and that pair is what the reader table
/// needs to tell a live holder from a dead one whose pid was reused.
#[cfg(windows)]
pub fn proc_start_time(pid: u32) -> Option<u64> {
    crate::wincompat::proc_start_time(pid)
}

#[cfg(not(windows))]
pub fn proc_start_time(pid: u32) -> Option<u64> {
    // wasm32: one process, born with the module instance. Any pid other than
    // ours is debris that cannot exist, and ours has a single fixed
    // incarnation — there is no `exec` to start a second one.
    #[cfg(target_arch = "wasm32")]
    {
        (pid == crate::wasmcompat::MY_PID).then_some(1)
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // comm may contain spaces/parens: fields resume after the LAST ')'
        let rest = &stat[stat.rfind(')')? + 2..];
        rest.split_ascii_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // Real per-process start stamp via libproc — `kinfo_proc`/`sysctl` is not
        // exposed by libc here, but `proc_pidinfo` is. PROC_PIDTBSDINFO fills
        // `proc_bsdinfo` with the process start `timeval`; fold it into a stable
        // u64 microsecond stamp so `(pid, start)` distinguishes a reused pid from
        // the original reader. A dead/absent pid returns 0 bytes → None.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let sz = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let rc = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                sz,
            )
        };
        if rc != sz {
            return None;
        }
        Some(
            (info.pbi_start_tvsec as u64)
                .wrapping_mul(1_000_000)
                .wrapping_add(info.pbi_start_tvusec as u64),
        )
    }
}

/// Does the OS state, definitely, that no process holds this pid?
///
/// The reader-slot sweep may only reclaim a slot on a DEFINITE answer, so this
/// is deliberately one-sided: `false` means "alive, or we cannot tell", and a
/// caller that cannot tell must leave the slot alone. Reclaiming a slot whose
/// owner is merely unreadable would drop a live reader's pin and let the writer
/// reuse pages out from under it.
///
/// This is the seam. `shm.rs` used to ask `kill(pid, 0)` and compare
/// `last_os_error()` to `ESRCH` — which on Windows reads the raw Win32 code
/// (`ERROR_INVALID_PARAMETER`, 87) and never equals any errno, so every dead
/// pid answered "alive" and the sweep reclaimed nothing. Two shm tests caught
/// exactly that.
#[cfg(windows)]
pub fn pid_definitely_dead(pid: u32) -> bool {
    crate::wincompat::pid_definitely_dead(pid)
}

#[cfg(not(windows))]
pub fn pid_definitely_dead(pid: u32) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        // One process, and it is us: any other pid is debris.
        pid != crate::wasmcompat::MY_PID
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return false; // exists
        }
        // EPERM and anything else mean "exists but not ours to signal" —
        // only ESRCH is the definite answer.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}

/// PID-namespace identity (Linux: `/proc/self/ns/pid` inode). macOS has no PID
/// namespaces → a fixed constant (boot recovery relies on [`boot_id`] instead).
/// Windows has no pid namespaces, so the question the Linux arm asks — "are
/// these two pids even comparable?" — always answers yes.
#[cfg(windows)]
pub fn pid_namespace_id() -> Option<u64> {
    crate::wincompat::pid_namespace_id()
}

#[cfg(not(windows))]
pub fn pid_namespace_id() -> Option<u64> {
    // wasm32: no namespaces. A fixed non-zero id, which is all the check needs
    // (it only ever compares against what a PREVIOUS attach recorded, and the
    // only previous attach is in this same module instance).
    #[cfg(target_arch = "wasm32")]
    {
        Some(1)
    }
    #[cfg(target_os = "linux")]
    {
        let l = std::fs::read_link("/proc/self/ns/pid").ok()?;
        let s = l.to_string_lossy().into_owned();
        let inner = s.strip_prefix("pid:[")?.strip_suffix(']')?.to_owned();
        inner.parse().ok()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        Some(1)
    }
}

/// Kill THIS process right now: no unwinding, no destructors, no exit
/// handlers, nothing flushed. The crash harnesses' primitive (#159 stage 4).
///
/// The child kills ITSELF rather than the parent racing to kill it at the
/// right moment — that is what walks the kill point across the attach /
/// prepare / commit windows deterministically.
///
/// Unix: `kill(getpid(), SIGKILL)`, which is unblockable and uncatchable.
/// Windows: `TerminateProcess` on our own handle, which is the same. What
/// makes them interchangeable for this purpose is not the API but the
/// aftermath: both leave the mapping's dirty pages to the OS, which writes
/// them back regardless. So both platforms are testing PROCESS death, and
/// neither is testing power loss — that distinction is why `powerloss` is a
/// separate harness with a separate mechanism.
pub fn hard_kill_self() -> ! {
    #[cfg(windows)]
    {
        crate::wincompat::hard_kill_self()
    }
    #[cfg(target_arch = "wasm32")]
    {
        // One process and no second one to observe the corpse. A harness that
        // asked for this on wasm32 would be measuring nothing, so say so
        // rather than exit quietly and let a green run mean nothing.
        panic!("hard_kill_self: the crash harnesses need a second process; wasm32 has one")
    }
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        // SIGKILL to self is not guaranteed to have been delivered when
        // `kill` returns, so park instead of falling through.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

/// Kill a CHILD process the same way [`hard_kill_self`] kills this one, so
/// [`died_by_hard_kill`] recognises both.
///
/// `Child::kill` would be the obvious call and is the wrong one on Windows: it
/// is `TerminateProcess(handle, 1)`, and 1 is also the CLI's "runtime error"
/// exit code — so a killed child and a child that failed on its own become
/// indistinguishable, which is precisely the distinction the crash harnesses
/// exist to make. Unix hides this because both directions are SIGKILL.
///
/// Found by `mpedb powerloss` on Windows (#159 stage 4): every worker was
/// reported as "did not die by SIGKILL … hit an error before the kill", with
/// an empty stderr, because the parent's own kill was being read as a failure.
pub fn hard_kill_child(child: &mut std::process::Child) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let h = child.as_raw_handle() as isize;
        if crate::wincompat::terminate_process(h, crate::wincompat::HARD_KILL_CODE) {
            return Ok(());
        }
        Err(std::io::Error::last_os_error())
    }
    #[cfg(not(windows))]
    {
        child.kill()
    }
}

/// Did this child die by [`hard_kill_self`] or [`hard_kill_child`] rather than
/// exiting?
///
/// The harnesses need to tell "killed at the chosen instant" (the point of the
/// test) from "exited on its own" (a child that hit an error first, which is a
/// finding). Unix reads the signal; Windows reads the exit code, which is why
/// [`crate::wincompat::HARD_KILL_CODE`] is a value no ordinary path returns.
pub fn died_by_hard_kill(status: &std::process::ExitStatus) -> bool {
    #[cfg(windows)]
    {
        status.code() == Some(crate::wincompat::HARD_KILL_CODE as i32)
    }
    #[cfg(not(windows))]
    {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = status;
            false
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal() == Some(libc::SIGKILL)
        }
    }
}

/// Does `stored` name the same boot this process is running in?
///
/// The question boot recovery asks, and the only one — so it is a predicate,
/// not a value comparison. That distinction is load-bearing on Windows.
///
/// Linux (`/proc/sys/kernel/random/boot_id`) and macOS (`kern.boottime`) hand
/// out a genuine identity, so there the answer is byte equality.
///
/// **Windows has neither**, and the obvious substitute is wrong: `now -
/// uptime` reads two clocks that move independently, so an NTP correction
/// changes the derived "boot instant" on a machine that never rebooted. That
/// is not a cosmetic drift. A false mismatch runs boot recovery, which wipes
/// the reader table — and it holds only the file lock, which an
/// already-attached reader does not, so a live reader's pin is dropped and the
/// writer may reclaim pages out from under it. Byte equality here traded a
/// correctness invariant for a clock.
///
/// So Windows answers with two facts instead of one:
///
/// 1. `GetTickCount64` is monotonic within a boot and resets across one. If it
///    has gone BACKWARDS since `stored` was written, that is a reboot, exactly
///    and with no tolerance needed.
/// 2. Otherwise the derived boot instants must agree within [`BOOT_EPOCH_TOL_MS`].
///    Across a real reboot they differ by at least the previous boot's uptime;
///    within one they differ only by clock adjustment.
///
/// Two windows remain. One is closed downstream; the other is stated exactly:
///
/// - **Spurious recovery** — the dangerous one, because it is reachable: a VM
///   resumed from a snapshot, a laptop waking, a first NTP correction on a
///   machine with a dead RTC battery. The wall clock steps more than ten
///   seconds while a database is attached and this returns `false` for a boot
///   that never happened. **Its damage is now refuted downstream rather than
///   merely made unlikely**: `Shm::post_attach` will not act on a mismatch
///   while any reader slot names a live process with matching identity, and on
///   Windows that identity includes an absolute process CREATION FILETIME, so
///   it cannot be a coincidence across a real reboot (`Shm::any_live_reader`).
///   A wrong answer here now costs nothing rather than a live reader's pin.
/// - **Missed reboot.** The two boot instants differ by the previous boot's
///   whole lifetime plus the downtime, so slipping under the tolerance
///   requires that *entire* interval to be under ten seconds — a boot loop,
///   in which nothing was durably committed to miss. Left open deliberately:
///   the fix costs a clock and buys an interval nothing survives.
///
/// The registry route — the per-boot counter at `HKLM\SYSTEM\
/// CurrentControlSet\Control\Session Manager\Memory Management\
/// PrefetchParameters\BootId` — was considered and NOT taken. It is a real
/// identity when it is live, but it is the prefetcher's, and whether it still
/// increments where prefetching is disabled is a fact about Windows that
/// cannot be measured from this project's machines or from a CI runner that
/// never reboots. Replacing a stated window with an unmeasured assumption is
/// not an improvement.
pub fn boot_id_matches(stored: &[u8; 16]) -> Option<bool> {
    #[cfg(windows)]
    {
        /// See [`boot_id_matches`] for how this number was chosen.
        const BOOT_EPOCH_TOL_MS: u64 = 10_000;
        let s_boot = u64::from_le_bytes(stored[..8].try_into().ok()?);
        let s_up = u64::from_le_bytes(stored[8..].try_into().ok()?);
        let now_up = crate::wincompat::uptime_ms();
        if now_up < s_up {
            return Some(false); // the tick counter reset: a reboot, exactly
        }
        let now_boot = crate::wincompat::boot_epoch_ms();
        Some(now_boot.abs_diff(s_boot) <= BOOT_EPOCH_TOL_MS)
    }
    #[cfg(not(windows))]
    {
        Some(boot_id()? == *stored)
    }
}

/// Boot identity: changes across reboots, so a post-reboot attach triggers
/// robust-mutex/reader-table recovery. Linux: `/proc/sys/kernel/random/boot_id`.
/// macOS: `sysctl(KERN_BOOTTIME)` (the boot instant).
///
/// Cached for the process lifetime everywhere EXCEPT Windows: the id is a
/// per-boot, fork-invariant constant (a reboot kills the process), and the
/// Linux `/proc` read was ~a third of a whole `:memory:` connect — three
/// reads per open (#N2 0.2.9, the connect cell's attribution). Windows is
/// deliberately uncached: its pair carries a LIVE uptime clock that
/// [`boot_id_matches`] must read fresh.
pub fn boot_id() -> Option<[u8; 16]> {
    #[cfg(windows)]
    {
        boot_id_uncached()
    }
    #[cfg(not(windows))]
    {
        static CACHED: std::sync::OnceLock<Option<[u8; 16]>> = std::sync::OnceLock::new();
        *CACHED.get_or_init(boot_id_uncached)
    }
}

fn boot_id_uncached() -> Option<[u8; 16]> {
    // wasm32: "boot" is the module instantiation, and the database is created
    // fresh inside it — memory cannot outlive the boot that made it, so the
    // stale-across-reboot hazard this guards is unreachable. A fixed non-zero
    // value (zero would trigger spurious boot recovery, see `shm::boot_id`).
    #[cfg(target_arch = "wasm32")]
    {
        Some(*b"mpedb-wasm-inst\0")
    }
    // Windows has no boot id to read, so this records TWO clocks and
    // [`boot_id_matches`] reads them together. Neither alone is enough — see
    // that function for why byte equality is wrong here.
    #[cfg(windows)]
    {
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&crate::wincompat::boot_epoch_ms().to_le_bytes());
        out[8..].copy_from_slice(&crate::wincompat::uptime_ms().to_le_bytes());
        Some(out)
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() < 32 {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, chunk) in hex.as_bytes().chunks(2).take(16).enumerate() {
            out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        }
        Some(out)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::timeval>();
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                &mut tv as *mut _ as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&(tv.tv_sec as u64).to_le_bytes());
        out[8..16].copy_from_slice(&(tv.tv_usec as u64).to_le_bytes());
        Some(out)
    }
}

/// This process's id.
///
/// Native: `std::process::id()`. **wasm32: a constant, because
/// `std::process::id()` PANICS there** — `wasm32-unknown-unknown` has no
/// process concept, and std's stub aborts rather than inventing one. A tab is
/// a single process, so the constant is not a placeholder: it is the complete
/// truth about how many processes can touch this database.
/// Windows: `GetCurrentProcessId`.
#[cfg(windows)]
pub fn process_id() -> u32 {
    crate::wincompat::process_id()
}

#[cfg(not(windows))]
pub fn process_id() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        crate::wasmcompat::MY_PID
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::process::id()
    }
}

/// Wall-clock microseconds since the Unix epoch — the single clock read behind
/// a literal `'now'` and a `DEFAULT now`.
///
/// Native: `SystemTime::now()`. **wasm32: a HOST import**, because
/// `SystemTime::now()` panics on `wasm32-unknown-unknown` — std has no clock
/// there and aborts rather than inventing one.
///
/// It is imported rather than stubbed to zero on purpose. Returning 0 would
/// make `date('now')` answer `1970-01-01` — a wrong answer, not a refusal, and
/// this engine's rule is *agree or refuse, never differ*. The embedder supplies
/// the browser's own clock (`Date.now()`), so `'now'` is genuinely now.
///
/// A clock before the epoch yields 0 rather than a negative surprise, matching
/// the native helpers this replaces.
/// Windows: `GetSystemTimePreciseAsFileTime`, shifted from the 1601 FILETIME
/// epoch to the Unix one.
#[cfg(windows)]
pub fn wall_clock_micros() -> i64 {
    crate::wincompat::wall_clock_micros()
}

#[cfg(not(windows))]
pub fn wall_clock_micros() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        // Milliseconds as f64 (what `Date.now()` returns) rather than an i64:
        // an i64 across the wasm boundary is a JS BigInt, and this needs no
        // sub-millisecond resolution that would justify the friction.
        let ms = unsafe { crate::wasmcompat::mpedb_host_now_ms() };
        if ms.is_finite() && ms > 0.0 {
            (ms * 1000.0) as i64
        } else {
            0
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}
