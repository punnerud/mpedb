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

/// Flush file data to storage. Linux: `fdatasync`. macOS (bench-grade): `fsync`
/// — NOT platter-durable (that needs `fcntl(F_FULLFSYNC)`, far slower).
pub fn fdatasync(fd: RawFd) -> libc::c_int {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::fdatasync(fd) }
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsafe { libc::fsync(fd) }
    }
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

/// Make a process-shared mutex robust (Linux: survives owner death →
/// `EOWNERDEAD`). macOS has no robust mutex: **no-op** ⇒ a process that dies
/// holding the writer lock wedges the database (benchmark-grade only).
///
/// # Safety
/// `attr` must point to an initialized `pthread_mutexattr_t`.
pub unsafe fn mutexattr_set_robust(attr: *mut libc::pthread_mutexattr_t) {
    #[cfg(target_os = "linux")]
    {
        libc::pthread_mutexattr_setrobust(attr, libc::PTHREAD_MUTEX_ROBUST);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = attr;
    }
}

/// Mark a mutex consistent after `EOWNERDEAD` recovery. macOS: no-op returning
/// 0 (never reached — the mutex is not robust, so `lock` never yields EOWNERDEAD).
///
/// # Safety
/// `m` must point to a locked mutex recovered from `EOWNERDEAD`.
pub unsafe fn mutex_make_consistent(m: *mut libc::pthread_mutex_t) -> libc::c_int {
    #[cfg(target_os = "linux")]
    {
        libc::pthread_mutex_consistent(m)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = m;
        0
    }
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
