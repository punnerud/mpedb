//! Platform abstraction for the OS primitives the shared-memory engine needs
//! (task #18). **Linux is the reference, crash-safe platform.**
//!
//! ## macOS is BENCHMARK-GRADE ONLY
//!
//! macOS lacks robust process-shared mutexes and Linux futexes. This module
//! degrades those to a plain process-shared mutex (no `EOWNERDEAD` recovery),
//! a polling "park" instead of a futex, and `fsync` instead of `fdatasync`.
//! That is enough to run **multi-process throughput benchmarks** on many-core
//! hardware, but on macOS a process that dies holding the writer lock WEDGES
//! the database, and durability is not platter-guaranteed. Do not treat the
//! macOS build as crash-safe or durable.

use std::os::unix::io::RawFd;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

/// Flush file data to storage. Linux: `fdatasync`. macOS: `fcntl(F_FULLFSYNC)`
/// — the only macOS call that forces the drive to flush its write cache to the
/// platter (plain `fsync` returns before that, so a power loss can still lose an
/// acked commit). Slower than `fsync`, but that is the price of real durability.
/// Falls back to `fsync` only when the filesystem rejects F_FULLFSYNC (ENOTSUP).
pub fn fdatasync(fd: RawFd) -> libc::c_int {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::fdatasync(fd) }
    }
    #[cfg(not(target_os = "linux"))]
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
    let g = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let g = if g > 0 { g as usize } else { 4096 };
    CACHE.store(g, Ordering::Relaxed);
    g
}

/// Ensure `[offset, offset+len)` is backed by real blocks (Linux: `fallocate`,
/// so a mid-commit touch never hits a lazy hole → no SIGBUS). macOS
/// (bench-grade): grow the file with `ftruncate` (may leave a sparse hole; fine
/// while disk space is available). Never shrinks.
pub fn preallocate(fd: RawFd, offset: i64, len: i64) -> libc::c_int {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::fallocate(fd, 0, offset, len) }
    }
    #[cfg(not(target_os = "linux"))]
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

/// Reclaim `[offset, offset+len)` as a hole (WAL checkpoint). Best-effort;
/// failure only wastes space. macOS: no-op (space is not reclaimed).
pub fn punch_hole(fd: RawFd, offset: i64, len: i64) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::fallocate(
            fd,
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset,
            len,
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fd, offset, len);
    }
}

/// Advise transparent huge pages over the mapping. Opportunistic; macOS: no-op.
pub fn madvise_hugepage(ptr: *mut libc::c_void, len: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::madvise(ptr, len, libc::MADV_HUGEPAGE);
    }
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (word, expected);
        std::thread::sleep(timeout.min(Duration::from_micros(200)));
    }
}

/// Wake all waiters on `word`. macOS: no-op (waiters poll).
pub fn futex_wake_all(word: &AtomicU32) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr(), libc::FUTEX_WAKE, i32::MAX);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = word;
    }
}

// ---- macOS crash-safe writer lock (DESIGN-MACOS-LOCK.md, FLD-2) -------------
//
// Linux uses the robust pthread mutex directly (in shm.rs). macOS has none, so
// the writer lock is: a sidecar-inode `flock` (the KERNEL releases it when the
// holder dies → free death oracle + rendezvous) + a process-private ERRORCHECK
// mutex (intra-process exclusion + re-entrancy → EDEADLK). shm.rs layers the
// tri-state DIRTY word (the "recovered" signal) on top. This struct provides
// ONLY the exclusion primitives.

#[cfg(not(target_os = "linux"))]
pub use macos_lock::WriterLock;

#[cfg(not(target_os = "linux"))]
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
        file: File,                        // OWNS the wl_fd; drop → close → flock auto-release
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
    static REGISTRY: LazyLock<Mutex<HashMap<(u64, u64), Weak<Inner>>>> =
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
                file,
                local_mtx: make_errorcheck_mutex(),
            });
            reg.insert(devino, Arc::downgrade(&inner));
            Ok(WriterLock { inner })
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
            let fd = self.inner.file.as_raw_fd();
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
            let fd = self.inner.file.as_raw_fd();
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
            let fd = self.inner.file.as_raw_fd();
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
pub fn proc_start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // comm may contain spaces/parens: fields resume after the LAST ')'
        let rest = &stat[stat.rfind(')')? + 2..];
        rest.split_ascii_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
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

/// PID-namespace identity (Linux: `/proc/self/ns/pid` inode). macOS has no PID
/// namespaces → a fixed constant (boot recovery relies on [`boot_id`] instead).
pub fn pid_namespace_id() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let l = std::fs::read_link("/proc/self/ns/pid").ok()?;
        let s = l.to_string_lossy().into_owned();
        let inner = s.strip_prefix("pid:[")?.strip_suffix(']')?.to_owned();
        inner.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(1)
    }
}

/// Boot identity: changes across reboots, so a post-reboot attach triggers
/// robust-mutex/reader-table recovery. Linux: `/proc/sys/kernel/random/boot_id`.
/// macOS: `sysctl(KERN_BOOTTIME)` (the boot instant).
pub fn boot_id() -> Option<[u8; 16]> {
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
    #[cfg(not(target_os = "linux"))]
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
