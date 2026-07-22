//! `wasm32` OS emulation for the process-PRIVATE (`:memory:`) engine path.
//!
//! A browser tab is the degenerate case of mpedb's operational model: **one
//! process, one thread, no filesystem, no durability**. Every OS primitive the
//! shared-memory engine needs is therefore either meaningless (there is no
//! second process to exclude, no owner that can die behind our back) or
//! trivial (an `mmap` of an anonymous file is "hand me a zeroed byte range",
//! which is a `Box<[u8]>`).
//!
//! So this module does NOT reimplement the engine. It reimplements the *system
//! calls*, and `shm.rs` — meta double-buffering, the reader-pin protocol, the
//! COW B+tree, the freelist fixpoint — is compiled and run **unmodified**. What
//! answers a query in the browser is the real engine; only the kernel is fake.
//!
//! ## Why each stub is sound HERE, and only here
//!
//! Each entry says what the call protects against and why that hazard cannot
//! occur in a `wasm32-unknown-unknown` tab. Nothing in this file is compiled
//! for a native target — see the `#![cfg(target_arch = "wasm32")]` below — so
//! the Linux/macOS paths keep their real system calls byte for byte.
//!
//! | call | native job | why the stub is safe |
//! |---|---|---|
//! | `mmap`/`munmap` | share one page cache between processes | only ONE process maps this file; a private mapping IS the file. Returns the fd's own buffer, so `pread`/`pwrite` and the mapping stay trivially coherent (native gets that from `MAP_SHARED`). |
//! | `msync` | push dirty pages to stable storage | `open_memory` REFUSES any durability but `Durability::None` (`shm.rs`), so no commit ever promises to survive. There is nothing to make stable. |
//! | `ftruncate` | size the backing file | resizes the buffer. Only ever called BEFORE `mmap` on this path, so no live pointer is invalidated (asserted below). |
//! | `flock` | cross-process attach/init exclusion | no second process can open this database: the buffer is unreachable outside the module instance. |
//! | `kill(pid, 0)` | liveness probe for reader/ring slot reclaim | reports ESRCH for every pid but our own, which is the truth: no other process exists. |
//! | `fchmod`/`fchown` | enforce a configured isolation boundary | unreachable — perms apply to a real file, and the private path has none. Returns failure rather than pretending success. |
//! | boot id / pid ns / proc start | detect reboot & PID reuse across attaches | a tab's memory dies with the tab; nothing outlives an "attach" to be stale against. Fixed constants. |
//! | futex | block a thread until another wakes it | there is no other thread that could ever post the wake. |
//! | writer lock | mutual exclusion + owner-death recovery | one thread: exclusion is a flag, and the only "owner death" is a re-entrant call, which is reported as the same `EDEADLK`-class error a native ERRORCHECK mutex gives. |
//!
//! The two places this file can still say "no" are the ones that would be a
//! LIE if it said yes: opening a real path, and resolving a user/group name.
//! Both return errors, so a browser build refuses rather than differs.

#![cfg(target_arch = "wasm32")]

use std::collections::BTreeMap;
use std::io;
use std::sync::{Mutex, OnceLock};

/// Matches the native `std::os::unix::io::RawFd`, which does not exist here.
pub type RawFd = i32;

// ---------------------------------------------------------------------------
// The virtual file table
// ---------------------------------------------------------------------------
//
// One entry per anonymous backing "file". The payload is a `Box<[u8]>`: its
// heap allocation does not move when the map rehashes or another entry is
// inserted, so a pointer handed out by `mmap` stays valid for the life of the
// entry. That is the whole reason the mapping and `pread`/`pwrite` agree.

struct Files {
    open: BTreeMap<RawFd, Box<[u8]>>,
    /// `dup` refcount per fd. `Shm` keeps a `try_clone`d handle and the opener
    /// drops its own, so the buffer must outlive the first `File` to die —
    /// exactly what a real `dup`'d fd does.
    refs: BTreeMap<RawFd, u32>,
    /// Fds handed to `mmap`. `ftruncate` on a mapped fd would reallocate and
    /// dangle the mapping, so it is refused rather than allowed to corrupt.
    mapped: BTreeMap<RawFd, usize>,
    next_fd: RawFd,
}

fn files() -> &'static Mutex<Files> {
    static FILES: OnceLock<Mutex<Files>> = OnceLock::new();
    FILES.get_or_init(|| {
        Mutex::new(Files {
            open: BTreeMap::new(),
            refs: BTreeMap::new(),
            mapped: BTreeMap::new(),
            // 0/1/2 are stdio by convention even where they do nothing; start
            // above them so a stray 0 is recognisably "no fd" as it is natively.
            next_fd: 3,
        })
    })
}

fn with_files<R>(f: impl FnOnce(&mut Files) -> R) -> R {
    let mut g = files().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

// ---------------------------------------------------------------------------
// `std::fs` stand-ins
// ---------------------------------------------------------------------------

/// An anonymous, process-private byte buffer with a file-shaped API — the
/// `wasm32` stand-in for Linux's `memfd_create` fd.
///
/// There is deliberately no way to construct one from a path: see
/// [`OpenOptions::open`].
#[derive(Debug)]
pub struct File {
    fd: RawFd,
}

impl File {
    /// The `memfd_create` replacement: a nameless, zero-length buffer.
    pub fn anonymous() -> io::Result<File> {
        Ok(File {
            fd: with_files(|f| {
                let fd = f.next_fd;
                f.next_fd += 1;
                f.open.insert(fd, Box::default());
                f.refs.insert(fd, 1);
                fd
            }),
        })
    }

    /// `dup`: a second handle on the SAME buffer, refcounted. `Shm` holds one
    /// of these while the opener drops its own, so this must keep the buffer
    /// alive rather than hand out an independent copy.
    pub fn try_clone(&self) -> io::Result<File> {
        with_files(|f| match f.refs.get_mut(&self.fd) {
            Some(n) => {
                *n += 1;
                Ok(File { fd: self.fd })
            }
            None => Err(ebadf()),
        })
    }

    /// No-op: the private path promises no durability, so there is nothing
    /// outstanding to make stable. See the module header.
    pub fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }

    /// The `AsRawFd` stand-in, inherent so `shm.rs` needs no trait in scope.
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        with_files(|f| match f.open.get(&self.fd) {
            Some(b) => Ok(Metadata { len: b.len() as u64 }),
            None => Err(ebadf()),
        })
    }

    /// `pread`. Short only at EOF, exactly like the real thing.
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        with_files(|f| {
            let b = f.open.get(&self.fd).ok_or_else(ebadf)?;
            let start = (offset as usize).min(b.len());
            let n = (b.len() - start).min(buf.len());
            buf[..n].copy_from_slice(&b[start..start + n]);
            Ok(n)
        })
    }

    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let n = self.read_at(buf, offset)?;
        if n == buf.len() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read_exact_at past end of anonymous buffer",
            ))
        }
    }

    /// `pwrite`. Never grows the buffer: on this path every byte written is
    /// inside the reserve `ftruncate` already sized, and a silent grow would
    /// reallocate under a live mapping.
    pub fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        with_files(|f| {
            let b = f.open.get_mut(&self.fd).ok_or_else(ebadf)?;
            let start = offset as usize;
            let end = start.checked_add(buf.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "write offset overflow")
            })?;
            if end > b.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write past the end of an anonymous buffer (grow it with ftruncate first)",
                ));
            }
            b[start..end].copy_from_slice(buf);
            Ok(())
        })
    }
}

impl Drop for File {
    fn drop(&mut self) {
        with_files(|f| {
            let gone = match f.refs.get_mut(&self.fd) {
                Some(n) => {
                    *n -= 1;
                    *n == 0
                }
                None => true,
            };
            if gone {
                f.refs.remove(&self.fd);
                f.open.remove(&self.fd);
                f.mapped.remove(&self.fd);
            }
        });
    }
}

pub struct Metadata {
    len: u64,
}

impl Metadata {
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Present so `shm.rs` compiles unchanged; every `open` fails, because there is
/// no filesystem to open from. A browser build reaches this only if a caller
/// asks for a file-backed database, and answering that with anything but an
/// error would be a lie.
#[derive(Default)]
pub struct OpenOptions;

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions
    }
    pub fn read(&mut self, _: bool) -> &mut Self {
        self
    }
    pub fn write(&mut self, _: bool) -> &mut Self {
        self
    }
    pub fn create(&mut self, _: bool) -> &mut Self {
        self
    }
    pub fn create_new(&mut self, _: bool) -> &mut Self {
        self
    }
    pub fn append(&mut self, _: bool) -> &mut Self {
        self
    }
    pub fn truncate(&mut self, _: bool) -> &mut Self {
        self
    }
    pub fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
    pub fn custom_flags(&mut self, _: i32) -> &mut Self {
        self
    }
    pub fn open<P: AsRef<std::path::Path>>(&mut self, path: P) -> io::Result<File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "no filesystem in the wasm32 build: cannot open `{}` \
                 (only the in-memory database, path \":memory:\", exists here)",
                path.as_ref().display()
            ),
        ))
    }
}

fn ebadf() -> io::Error {
    io::Error::new(io::ErrorKind::Other, "bad file descriptor")
}

// ---------------------------------------------------------------------------
// The `libc` stand-in
// ---------------------------------------------------------------------------
//
// `shm.rs` does `use crate::wasmcompat::libc;` under cfg(wasm32), which shadows
// the real `libc` crate for that module. Every `libc::…` path there then
// resolves here, so the call sites themselves are untouched.

pub mod libc {
    // libc's own spelling — these exist to be drop-in replacements for
    // `libc::mode_t` and friends, so they must match ITS naming, not Rust's.
    #![allow(non_camel_case_types)]

    use super::{with_files, RawFd};

    pub use core::ffi::{c_char, c_int, c_long, c_void};

    pub type mode_t = u32;
    pub type uid_t = u32;
    pub type gid_t = u32;
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
    pub const EINVAL: c_int = 22;
    pub const ENOSYS: c_int = 38;
    pub const ERANGE: c_int = 34;
    pub const EDEADLK: c_int = 35;
    pub const EBUSY: c_int = 16;
    pub const EOWNERDEAD: c_int = 130;

    /// Hand back the fd's own buffer. There is no second address space to
    /// share with, so "shared mapping of a file" and "the file" are the same
    /// bytes — which is exactly the coherence `MAP_SHARED` buys natively.
    ///
    /// # Safety
    /// Mirrors the real `mmap` contract; `fd` must name a live [`super::File`].
    pub unsafe fn mmap(
        _addr: *mut c_void,
        length: usize,
        _prot: c_int,
        _flags: c_int,
        fd: RawFd,
        offset: i64,
    ) -> *mut c_void {
        if offset != 0 {
            return MAP_FAILED;
        }
        with_files(|f| {
            let Some(b) = f.open.get_mut(&fd) else {
                return MAP_FAILED;
            };
            if b.len() < length {
                return MAP_FAILED;
            }
            f.mapped.insert(fd, length);
            b.as_mut_ptr() as *mut c_void
        })
    }

    /// The buffer is owned by the [`super::File`] and freed when it drops, so
    /// unmapping is just forgetting the pointer. Freeing here instead would
    /// double-free against that `Drop`.
    ///
    /// # Safety
    /// Mirrors the real `munmap` contract.
    pub unsafe fn munmap(_addr: *mut c_void, _length: usize) -> c_int {
        0
    }

    /// No-op: the private path forces `Durability::None`, so nothing has been
    /// promised to be stable and there is no device to flush to.
    ///
    /// # Safety
    /// Mirrors the real `msync` contract.
    pub unsafe fn msync(_addr: *mut c_void, _length: usize, _flags: c_int) -> c_int {
        0
    }

    /// Resize the buffer, zero-filling growth as a real `ftruncate` does.
    /// Refused once mapped: a realloc would dangle the live mapping. On this
    /// path the only call is `open_memory`'s, strictly before `mmap`.
    ///
    /// # Safety
    /// Mirrors the real `ftruncate` contract.
    pub unsafe fn ftruncate(fd: RawFd, length: off_t) -> c_int {
        if length < 0 {
            return -1;
        }
        with_files(|f| {
            if f.mapped.contains_key(&fd) {
                return -1; // EBUSY-shaped: never move a buffer out from under a mapping
            }
            let Some(b) = f.open.get_mut(&fd) else {
                return -1;
            };
            let want = length as usize;
            let mut next = vec![0u8; want].into_boxed_slice();
            let keep = b.len().min(want);
            next[..keep].copy_from_slice(&b[..keep]);
            *b = next;
            0
        })
    }

    /// Always succeeds: the lock excludes OTHER processes, and none exist.
    ///
    /// # Safety
    /// Mirrors the real `flock` contract.
    pub unsafe fn flock(_fd: RawFd, _operation: c_int) -> c_int {
        0
    }

    /// Liveness probe. Only this process exists, so every other pid is
    /// genuinely gone — the honest answer is failure, and `shm.rs` reads the
    /// errno to distinguish "dead" (ESRCH) from "not mine" (EPERM).
    ///
    /// # Safety
    /// Mirrors the real `kill` contract.
    pub unsafe fn kill(pid: c_int, _sig: c_int) -> c_int {
        if pid == super::MY_PID as c_int {
            0
        } else {
            super::set_errno(ESRCH);
            -1
        }
    }

    /// Unreachable on the private path (a mode belongs to a real file). Fails
    /// rather than pretending an isolation boundary was applied.
    ///
    /// # Safety
    /// Mirrors the real `fchmod` contract.
    pub unsafe fn fchmod(_fd: RawFd, _mode: mode_t) -> c_int {
        super::set_errno(ENOSYS);
        -1
    }

    /// Unreachable on the private path; see [`fchmod`].
    ///
    /// # Safety
    /// Mirrors the real `fchown` contract.
    pub unsafe fn fchown(_fd: RawFd, _uid: uid_t, _gid: gid_t) -> c_int {
        super::set_errno(ENOSYS);
        -1
    }
}

/// The single pid a wasm module instance has. Any other pid in a reader or ring
/// slot is by construction stale debris, and reported dead.
pub const MY_PID: u32 = 1;

/// `std::io::Error::last_os_error()` has no OS to read on wasm32, so the stubs
/// that must report a specific errno record it here and `errno()` serves it.
/// Only the values `shm.rs` actually branches on ever flow through.
fn errno_cell() -> &'static std::sync::atomic::AtomicI32 {
    static ERRNO: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    &ERRNO
}

pub(crate) fn set_errno(e: i32) {
    errno_cell().store(e, std::sync::atomic::Ordering::Relaxed);
}

/// The errno most recently set by a stub in this module.
pub fn errno() -> i32 {
    errno_cell().load(std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Host imports
// ---------------------------------------------------------------------------

// Imports arrive in the `mpedb` module, so the embedder's import object is
// `{ mpedb: { … } }` — named rather than the default `env`, so a page that
// embeds several wasm modules keeps them apart.
#[link(wasm_import_module = "mpedb")]
extern "C" {
    /// Milliseconds since the Unix epoch, from the embedder (`Date.now()`).
    ///
    /// The ONE thing a browser genuinely has that this module cannot fake: a
    /// clock. See `crate::os::wall_clock_micros` for why it is imported rather
    /// than stubbed. Missing it fails instantiation loudly, which is the right
    /// failure — a silently-1970 database would answer `date('now')` wrongly.
    pub fn mpedb_host_now_ms() -> f64;
}
