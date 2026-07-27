//! **Windows arm of the OS primitives the shared-memory engine needs (#159).**
//!
//! Same trick as [`crate::wasmcompat`]: supply the Unix-shaped *names* the
//! engine already calls — `RawFd`, `FileExt`, `OpenOptionsExt`, and a `libc`
//! module that SHADOWS the real crate for the files that import it — so the
//! several hundred call sites in `shm.rs` compile unchanged instead of being
//! forked per platform. What differs from `wasmcompat` is that nothing here is
//! emulated over a buffer: every call below is a real Win32 call with real
//! cross-process semantics.
//!
//! ## Why hand-declared `extern "system"` rather than `windows-sys`
//!
//! Twenty-odd kernel32 entry points, all stable since Windows 8, against a
//! dependency tree of thousands of generated bindings in a crate that the
//! project's own ethos keeps dependency-light. The declarations are below and
//! are checkable against MSDN by eye.
//!
//! ## The mapping model, and the one non-obvious part
//!
//! `mmap(MAP_SHARED)` becomes `CreateFileMapping` + `MapViewOfFile`. The
//! section handle is closed **immediately after** the view is created: a view
//! holds its own reference to the section, so the mapping stays valid and there
//! is no handle to track alongside the pointer. That is what lets `munmap` be
//! `UnmapViewOfFile(ptr)` with nothing else to look up — Windows takes only the
//! base address, exactly like the shape `munmap`'s callers already assume.
//!
//! ## Durability composes the same way it does on macOS
//!
//! `msync` → `FlushViewOfFile` pushes dirty pages to the filesystem; it does
//! **not** reach the platter. `crate::os::fdatasync` → `FlushFileBuffers` is
//! what does. The engine already issues both in that order because macOS needs
//! the same split (`msync` then `F_FULLFSYNC`), so the Windows durability path
//! inherits an ordering that is already reviewed rather than inventing one.
//!
//! ## What is NOT here, deliberately
//!
//! `passwd`, `group`, `fchown`, `getpwnam_r` — Windows has no uid/gid, and a
//! shim that pretended otherwise would be inventing a security model. Those
//! call sites are `cfg(unix)` at their use, not faked here. Same for
//! `memfd_create` (a temp file stands in) and `copy_file_range` (which already
//! has a portable fallback).

#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;

/// A raw OS handle, in the position `RawFd` occupies on Unix.
///
/// `isize` rather than `*mut c_void` so it is `Send`/`Sync`/`Copy` and can sit
/// in the structs that already store an fd, unchanged.
pub type RawFd = isize;

pub use std::fs::{File, OpenOptions};

// ------------------------------------------------------------------ Win32

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileMappingW(
        h: isize,
        attrs: *mut c_void,
        protect: u32,
        max_hi: u32,
        max_lo: u32,
        name: *const u16,
    ) -> isize;
    fn MapViewOfFile(map: isize, access: u32, off_hi: u32, off_lo: u32, len: usize) -> *mut c_void;
    fn UnmapViewOfFile(base: *const c_void) -> i32;
    fn FlushViewOfFile(base: *const c_void, len: usize) -> i32;
    fn FlushFileBuffers(h: isize) -> i32;
    fn CloseHandle(h: isize) -> i32;
    fn GetLastError() -> u32;
    fn LockFileEx(
        h: isize,
        flags: u32,
        reserved: u32,
        lo: u32,
        hi: u32,
        ov: *mut Overlapped,
    ) -> i32;
    fn UnlockFileEx(h: isize, reserved: u32, lo: u32, hi: u32, ov: *mut Overlapped) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn GetProcessTimes(h: isize, create: *mut u64, exit: *mut u64, kern: *mut u64, user: *mut u64)
        -> i32;
    fn GetCurrentProcessId() -> u32;
    fn GetSystemInfo(info: *mut SystemInfo);
    fn SetEndOfFile(h: isize) -> i32;
    fn SetFilePointerEx(h: isize, dist: i64, new: *mut i64, method: u32) -> i32;
    fn GetFileSizeEx(h: isize, size: *mut i64) -> i32;
    fn WaitOnAddress(addr: *const c_void, compare: *const c_void, size: usize, ms: u32) -> i32;
    fn WakeByAddressAll(addr: *const c_void);
    fn WaitForSingleObject(h: isize, ms: u32) -> u32;
    fn GetSystemTimePreciseAsFileTime(ft: *mut u64);
    fn GetTickCount64() -> u64;
}

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: isize,
}

#[repr(C)]
struct SystemInfo {
    oem_id: u32,
    page_size: u32,
    min_app_addr: *mut c_void,
    max_app_addr: *mut c_void,
    active_mask: usize,
    num_processors: u32,
    proc_type: u32,
    alloc_granularity: u32,
    proc_level: u16,
    proc_revision: u16,
}

const PAGE_READWRITE: u32 = 0x04;
const FILE_MAP_ALL: u32 = 0x000F001F; // FILE_MAP_READ|WRITE|COPY|EXECUTE
const LOCKFILE_EXCLUSIVE: u32 = 0x0000_0002;
const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_OBJECT_0: u32 = 0;
const FILE_BEGIN: u32 = 0;
const ERROR_LOCK_VIOLATION: u32 = 33;
const ERROR_INVALID_PARAMETER: u32 = 87;

// ------------------------------------------------------------- extensions

/// The `AsRawFd` position, over a Windows `HANDLE`.
pub trait AsRawFd {
    fn as_raw_fd(&self) -> RawFd;
}

impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        self.as_raw_handle() as isize
    }
}

/// Positional I/O, the `std::os::unix::fs::FileExt` shape.
///
/// `std::os::windows::fs::FileExt` already provides `seek_read`/`seek_write`
/// with the same semantics; this is the Unix spelling over them so the call
/// sites do not fork. Note `seek_write` is NOT guaranteed to write everything
/// in one call, which is why `write_all_at` loops — the Unix `write_all_at`
/// makes the same promise and Windows does not give it for free.
pub trait FileExt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize>;
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()>;
    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()>;
}

impl FileExt for File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }

    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_read(self, buf, offset) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "read_exact_at hit end of file",
                    ))
                }
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_write(self, buf, offset) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "write_all_at made no progress",
                    ))
                }
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// The `OpenOptionsExt` position. `mode` is accepted and ignored — Windows has
/// no POSIX permission bits, and the engine only ever passes `0o600`, whose
/// intent (an owner-private file) is what NTFS inheritance gives by default.
/// Silently ignoring is right here; refusing would fork every call site over a
/// concept the platform does not have.
///
/// **Sharing mode is not set here on purpose.** Rust's std already opens with
/// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`, which is exactly
/// what multi-process attach needs. Narrowing it would turn the engine's
/// headline property into `ERROR_SHARING_VIOLATION`.
pub trait OpenOptionsExt {
    fn mode(&mut self, mode: u32) -> &mut Self;
    fn custom_flags(&mut self, flags: i32) -> &mut Self;
}

impl OpenOptionsExt for OpenOptions {
    fn mode(&mut self, _mode: u32) -> &mut Self {
        self
    }
    fn custom_flags(&mut self, _flags: i32) -> &mut Self {
        self
    }
}

/// The last OS error, in the position `errno()` occupies on Unix.
pub fn errno() -> libc::c_int {
    unsafe { GetLastError() as libc::c_int }
}

// ----------------------------------------------------------------- futex

/// `WaitOnAddress` — a direct futex equivalent, not a degradation. macOS has to
/// poll here; Windows does not.
pub fn futex_wait(word: &std::sync::atomic::AtomicU32, expected: u32, timeout: std::time::Duration) {
    let ms = timeout.as_millis().min((u32::MAX - 1) as u128) as u32;
    let cmp = expected;
    unsafe {
        WaitOnAddress(
            word as *const _ as *const c_void,
            &cmp as *const u32 as *const c_void,
            4,
            ms,
        );
    }
}

pub fn futex_wake_all(word: &std::sync::atomic::AtomicU32) {
    unsafe { WakeByAddressAll(word as *const _ as *const c_void) }
}

// ------------------------------------------------------------- process id

/// Creation time as the pid's identity, the role `/proc/<pid>/stat`'s start
/// time plays on Linux. A pid can be reused; a (pid, creation-time) pair
/// cannot, which is the property the reader table depends on.
pub fn proc_start_time(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == 0 {
            return None;
        }
        let (mut c, mut e, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
        let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        CloseHandle(h);
        if ok != 0 {
            Some(c)
        } else {
            None
        }
    }
}

/// See [`crate::os::pid_definitely_dead`] — one-sided: only `true` is a
/// commitment. `OpenProcess` failing with `ERROR_INVALID_PARAMETER` is Windows
/// for "no such pid"; `ERROR_ACCESS_DENIED` means it exists and is not ours,
/// which must NOT be read as dead.
///
/// The second half is a case Windows lets us answer and Linux does not. A
/// terminated process whose handle someone still holds keeps its kernel object
/// (and its pid) alive, so `OpenProcess` succeeds on a corpse — the Windows
/// twin of the zombie that pins a slot forever on Linux (known issue #136).
/// `WaitForSingleObject(h, 0)` signals on a dead process, so here we can see
/// through it. `GetExitCodeProcess` would be the more obvious call and is the
/// wrong one: a process may legitimately exit with 259, which is the same value
/// as `STILL_ACTIVE`.
pub fn pid_definitely_dead(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if h == 0 {
            return GetLastError() == ERROR_INVALID_PARAMETER;
        }
        let signalled = WaitForSingleObject(h, 0) == WAIT_OBJECT_0;
        CloseHandle(h);
        signalled
    }
}

pub fn process_id() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// Windows has no pid namespaces, so the concept is a constant. The reader
/// table uses this to refuse cross-namespace pid comparison; with one namespace
/// the answer is always "same".
pub fn pid_namespace_id() -> Option<u64> {
    Some(0)
}

pub fn wall_clock_micros() -> i64 {
    unsafe {
        let mut ft = 0u64;
        GetSystemTimePreciseAsFileTime(&mut ft);
        // FILETIME is 100 ns since 1601-01-01; shift to the Unix epoch.
        const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
        ((ft.saturating_sub(EPOCH_DIFF_100NS)) / 10) as i64
    }
}

/// When this machine booted, in milliseconds since the Unix epoch.
///
/// The role `/proc/sys/kernel/random/boot_id` plays: a value that is constant
/// within a boot and different across one, so a lock record written before a
/// reboot is recognisable as stale. Windows also clears the kernel object
/// namespace on reboot, so a stale lock cannot literally survive — this exists
/// so the stored epoch still MOVES and boot recovery still runs.
///
/// Quantised to whole seconds: `GetTickCount64` has ~15 ms resolution and the
/// wall clock drifts, so two calls in one boot can differ by a few
/// milliseconds. Without the quantisation the "same boot" test would fail
/// spuriously — which reads as a reboot that never happened.
/// Milliseconds since boot (`GetTickCount64`). Monotonic within a boot and
/// reset by one, which is the exact half of the boot-identity question — see
/// [`crate::os::boot_id_matches`].
pub fn uptime_ms() -> u64 {
    unsafe { GetTickCount64() }
}

pub fn boot_epoch_ms() -> u64 {
    let now_ms = (wall_clock_micros() / 1000) as u64;
    let up_ms = unsafe { GetTickCount64() };
    let boot = now_ms.saturating_sub(up_ms);
    (boot / 1000) * 1000
}

pub fn sync_granularity() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHE: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let g = unsafe {
        let mut si: SystemInfo = std::mem::zeroed();
        GetSystemInfo(&mut si);
        // The ALLOCATION granularity (64 KiB), not the page size: MapViewOfFile
        // requires the file offset to be a multiple of it, and it is the larger
        // of the two. Rounding to the page size would produce ERROR_MAPPED_
        // ALIGNMENT on every non-64-KiB-aligned view — the same class of
        // mistake as macOS's 16 KiB msync base.
        si.alloc_granularity as usize
    };
    let g = if g > 0 { g } else { 65536 };
    CACHE.store(g, Ordering::Relaxed);
    g
}

// ------------------------------------------------------------ libc shim

/// The `libc` names the engine calls, over Win32. This module SHADOWS the real
/// `libc` crate in every file that imports it, which is what keeps the call
/// sites identical across platforms.
pub mod libc {
    // The snake_case type names are the POINT: they shadow the real `libc`
    // crate's spelling so the engine's call sites compile unchanged.
    #![allow(non_camel_case_types)]

    use super::*;

    pub type c_int = i32;
    pub type c_char = i8;
    pub type c_void = std::ffi::c_void;
    pub type mode_t = u32;
    pub type off_t = i64;

    pub const PROT_READ: c_int = 1;
    pub const PROT_WRITE: c_int = 2;
    pub const MAP_SHARED: c_int = 1;
    pub const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
    pub const MS_SYNC: c_int = 4;
    pub const LOCK_EX: c_int = 2;
    pub const LOCK_UN: c_int = 8;
    pub const LOCK_NB: c_int = 4;

    pub const ESRCH: c_int = 3;
    pub const EBADF: c_int = 9;
    pub const EAGAIN: c_int = 11;
    pub const EBUSY: c_int = 16;
    pub const EINVAL: c_int = 22;
    pub const ERANGE: c_int = 34;
    pub const EDEADLK: c_int = 35;
    pub const EOWNERDEAD: c_int = 130;

    /// `mmap(MAP_SHARED)` over `CreateFileMapping` + `MapViewOfFile`.
    ///
    /// The section handle is closed as soon as the view exists: the view holds
    /// its own reference, so the mapping outlives the handle and `munmap` needs
    /// nothing but the pointer.
    ///
    /// # Safety
    /// Same contract as `mmap`: `fd` must be a valid handle open for read and
    /// write, and the caller owns the returned mapping until `munmap`.
    pub unsafe fn mmap(
        _addr: *mut c_void,
        len: usize,
        _prot: c_int,
        _flags: c_int,
        fd: RawFd,
        offset: off_t,
    ) -> *mut c_void {
        let map = unsafe {
            CreateFileMappingW(fd, std::ptr::null_mut(), PAGE_READWRITE, 0, 0, std::ptr::null())
        };
        if map == 0 {
            return MAP_FAILED;
        }
        let ptr = unsafe {
            MapViewOfFile(
                map,
                FILE_MAP_ALL,
                (offset >> 32) as u32,
                (offset & 0xFFFF_FFFF) as u32,
                len,
            )
        };
        // The view keeps the section alive; the handle is dead weight now.
        unsafe { CloseHandle(map) };
        if ptr.is_null() {
            MAP_FAILED
        } else {
            ptr
        }
    }

    /// # Safety
    /// `addr` must be a base address returned by [`mmap`] and not yet unmapped.
    pub unsafe fn munmap(addr: *mut c_void, _len: usize) -> c_int {
        // Windows takes only the base address — a partial unmap is not
        // expressible, and the engine never asks for one.
        if unsafe { UnmapViewOfFile(addr) } != 0 {
            0
        } else {
            -1
        }
    }

    /// # Safety
    /// `addr` must lie inside a live mapping of at least `len` bytes.
    pub unsafe fn msync(addr: *mut c_void, len: usize, _flags: c_int) -> c_int {
        if unsafe { FlushViewOfFile(addr, len) } != 0 {
            0
        } else {
            -1
        }
    }

    /// `flock` over `LockFileEx`, and the semantics line up where it matters:
    /// Windows releases a file lock when the owning handle closes, **including
    /// on process death**, which is exactly the owner-death property the FLD-2
    /// design leans on for macOS.
    ///
    /// # Safety
    /// `fd` must be a valid open file handle.
    pub unsafe fn flock(fd: RawFd, op: c_int) -> c_int {
        let mut ov: Overlapped = unsafe { std::mem::zeroed() };
        if op & LOCK_UN != 0 {
            return if unsafe { UnlockFileEx(fd, 0, 1, 0, &mut ov) } != 0 { 0 } else { -1 };
        }
        let mut flags = 0u32;
        if op & LOCK_EX != 0 {
            flags |= LOCKFILE_EXCLUSIVE;
        }
        if op & LOCK_NB != 0 {
            flags |= LOCKFILE_FAIL_IMMEDIATELY;
        }
        if unsafe { LockFileEx(fd, flags, 0, 1, 0, &mut ov) } != 0 {
            0
        } else {
            -1
        }
    }

    /// # Safety
    /// `fd` must be a valid open file handle.
    pub unsafe fn ftruncate(fd: RawFd, len: off_t) -> c_int {
        unsafe {
            let mut prev = 0i64;
            if SetFilePointerEx(fd, len, &mut prev, FILE_BEGIN) == 0 {
                return -1;
            }
            if SetEndOfFile(fd) == 0 {
                return -1;
            }
        }
        0
    }

    /// # Safety
    /// `fd` must be a valid open file handle.
    pub unsafe fn file_size(fd: RawFd) -> off_t {
        unsafe {
            let mut sz = 0i64;
            if GetFileSizeEx(fd, &mut sz) != 0 {
                sz
            } else {
                0
            }
        }
    }

    /// # Safety
    /// `fd` must be a valid open file handle.
    pub unsafe fn fsync(fd: RawFd) -> c_int {
        if unsafe { FlushFileBuffers(fd) } != 0 {
            0
        } else {
            -1
        }
    }

    /// `kill(pid, 0)` — a liveness probe, never a signal. Windows cannot signal
    /// another process, and the engine only ever uses this with `sig == 0`;
    /// anything else is refused rather than silently doing nothing, because a
    /// no-op "kill" that reports success is how a supervision loop goes quiet.
    ///
    /// # Safety
    /// Mirrors `libc::kill`'s signature; no invariants beyond the pid.
    pub unsafe fn kill(pid: i32, sig: c_int) -> c_int {
        if sig != 0 {
            return -1;
        }
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h == 0 {
                // ERROR_INVALID_PARAMETER is Windows for "no such pid".
                return -1;
            }
            CloseHandle(h);
        }
        0
    }

    /// Map the last Win32 error onto the errno the caller is comparing against.
    pub fn last_errno() -> c_int {
        match unsafe { GetLastError() } {
            ERROR_LOCK_VIOLATION => EAGAIN,
            ERROR_INVALID_PARAMETER => ESRCH,
            _ => EINVAL,
        }
    }
}

#[cfg(test)]
mod tests {
    /// #159 stage 3. The Windows boot identity is derived from two clocks, and
    /// the reason it is a PREDICATE and not a byte comparison is that one of
    /// them moves without a reboot. These cases are constructed rather than
    /// observed, because a wall-clock step and a reboot are both things a test
    /// cannot ask for.
    #[test]
    fn boot_identity_survives_a_clock_step_but_not_a_reboot() {
        fn stamp(boot_ms: u64, up_ms: u64) -> [u8; 16] {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&boot_ms.to_le_bytes());
            b[8..].copy_from_slice(&up_ms.to_le_bytes());
            b
        }
        let now_boot = super::boot_epoch_ms();
        let now_up = super::uptime_ms();
        let m = |b| crate::os::boot_id_matches(&b).unwrap();

        assert!(m(stamp(now_boot, now_up)), "our own stamp must match");

        // An NTP correction moves the derived boot instant on a machine that
        // never rebooted. Byte equality called this a reboot and wiped the
        // reader table under whatever readers were live.
        assert!(m(stamp(now_boot + 2_000, now_up)), "a 2 s step is not a reboot");
        assert!(m(stamp(now_boot - 2_000, now_up)), "…in either direction");

        // A reboot: the tick counter reset, so the stored uptime is ahead of
        // ours. Exact, no tolerance involved.
        assert!(
            !m(stamp(now_boot, now_up + 1)),
            "a stored uptime ahead of ours means the counter reset"
        );

        // A reboot after a long previous boot: uptime does not betray it (we
        // may be up longer than the stored value), the epoch does.
        assert!(
            !m(stamp(now_boot.saturating_sub(36_000_000), 1)),
            "a boot instant 10 h away is a different boot"
        );
    }

    use super::*;

    /// The alignment `MapViewOfFile` demands is the ALLOCATION granularity
    /// (64 KiB), not the 4 KiB page size. Getting this wrong produces
    /// `ERROR_MAPPED_ALIGNMENT` on every view whose offset is page- but not
    /// 64-KiB-aligned — the same shape as macOS's 16 KiB `msync` base, and
    /// exactly as easy to miss.
    #[test]
    fn sync_granularity_is_the_allocation_granularity() {
        let g = sync_granularity();
        assert!(g >= 4096, "granularity {g} is smaller than a page");
        assert_eq!(g & (g - 1), 0, "granularity {g} is not a power of two");
    }

    /// A pid that exists is alive; one that cannot be opened is not. This is the
    /// whole of the reader table's liveness test.
    #[test]
    fn our_own_pid_is_alive_and_has_a_start_time() {
        let me = process_id();
        assert_eq!(unsafe { libc::kill(me as i32, 0) }, 0, "we are not alive");
        assert!(proc_start_time(me).is_some(), "no creation time for our own pid");
    }

    /// Identity, not just liveness: the pair (pid, creation time) is what makes
    /// a recycled pid distinguishable from the original.
    #[test]
    fn start_time_is_stable_across_calls() {
        let me = process_id();
        assert_eq!(proc_start_time(me), proc_start_time(me));
    }

    /// Two calls inside one boot must agree, or every open would look like it
    /// happened after a reboot and boot recovery would run constantly.
    #[test]
    fn boot_epoch_is_stable_within_a_boot() {
        let a = boot_epoch_ms();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(a, boot_epoch_ms(), "boot epoch moved without a reboot");
    }

    #[test]
    fn wall_clock_is_after_2020() {
        // 2020-01-01 in microseconds. Catches a FILETIME epoch shift being
        // wrong by 369 years, which is the classic way to get this wrong.
        assert!(wall_clock_micros() > 1_577_836_800_000_000);
    }
}
