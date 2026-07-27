//! sqlite's own advisory byte-range locks, spoken natively — DESIGN-
//! SQLITE-BACKED §2/§5. Everything here targets the SAME bytes sqlite's unix
//! VFS locks (lockingv3), which is the entire point: a foreign sqlite writer
//! experiences mpedb's presence as a perfectly normal `SQLITE_BUSY`, and
//! mpedb sees theirs.
//!
//! Offsets (sqlite os_unix.c, frozen with the format):
//! `PENDING = 0x4000_0000`, `RESERVED = PENDING+1`, `SHARED = PENDING+2`
//! for `SHARED_SIZE = 510` bytes.
//!
//! Lock flavor: **OFD locks (`F_OFD_SETLK`) where the platform has them**
//! (Linux; macOS gains them recentishly — probed at runtime, not assumed).
//! OFD locks belong to the open file DESCRIPTION, so the review's [R#5]
//! trap — sqlite's own `close()`/unlock inside this process cancelling our
//! lock — cannot reach them, while they still conflict with foreign
//! processes' classic POSIX locks exactly like sqlite's own. Where OFD is
//! unavailable we fall back to classic `F_SETLK` and the guard says so
//! ([`SharedLock::ofd`]) — callers doing in-process sqlite work must then
//! run the drop/re-take dance the design specifies.

//! ## wasm32
//!
//! There is no filesystem in a browser, so every entry point below fails at
//! its opening `File::open(base)` before any lock primitive runs — a sqlite
//! base file cannot exist to be locked. The primitives are therefore stubbed
//! as *unreachable but honest*: if one were ever reached it reports that the
//! lock could not be taken, never that it was. Silently "succeeding" at a
//! lock against a foreign sqlite writer is the one answer that would be
//! dangerous, and it is the one answer this cannot give.
//!
//! ## Windows
//!
//! Windows speaks the protocol for real (#159), and the paragraph that used to
//! stand here — "sqlite's Windows VFS uses its own locking protocol, with
//! different byte offsets and a different shared/pending/reserved scheme" —
//! was **wrong**, which is why the feature sat gated behind it. The offsets are
//! not a VFS's business: `PENDING_BYTE`/`RESERVED_BYTE`/`SHARED_FIRST`/
//! `SHARED_SIZE` are defined once in sqlite's core, and its own comment says
//! the range is shared across platforms deliberately —
//!
//! > *"clients on win95, winNT, and unix all talking to the same shared file
//! > and all locking correctly … by using the same locking range we are at
//! > least open to the possibility."*
//!
//! So this is not a translation by analogy; it is the same bytes with the
//! platform's own call. Verified against the amalgamation this crate already
//! pins as its oracle, not from memory:
//!
//! * `winGetReadLock` → `LockFileEx(SHARED_FIRST, SHARED_SIZE, shared,
//!   FAIL_IMMEDIATELY)` — the same range and the same shared type as `F_RDLCK`
//!   here. A foreign writer's `EXCLUSIVE` covers that whole range, so it
//!   conflicts with ours and gets its normal `SQLITE_BUSY`; a foreign reader's
//!   shared lock coexists, untouched.
//! * `winCheckReservedLock` → a **shared try-lock on `RESERVED_BYTE` that is
//!   released immediately**, exactly what [`getlk_free`] means. Windows has no
//!   `F_GETLK`, and try-then-release is not our workaround for that: it is
//!   sqlite's own answer, so our probe and its probe are the same operation.
//!
//! Two places where Windows is STRONGER than the POSIX path, both load-bearing:
//! its locks belong to the HANDLE, so an in-process sqlite `CloseHandle` on its
//! own handle cannot cancel ours — the [R#5] trap that classic POSIX locks have
//! and OFD locks do not. [`SharedLock::ofd`] therefore reports `true` here: the
//! caller does not need the drop/re-take dance. And its locks are mandatory
//! rather than advisory, which is harmless precisely because sqlite's pager
//! never allocates the page these bytes live in.

use std::fs::File;
#[cfg(all(unix, not(target_arch = "wasm32")))]
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// `F_RDLCK`/`F_UNLCK` as the `flock.l_type` field's type. The cast is REQUIRED
/// on Linux (the libc consts are `c_int`, the field is `c_short`) and a no-op
/// on macOS (the consts are already `c_short`) — which is why clippy on macOS
/// flags the inline spelling as `unnecessary_cast`. One allowed cast here, and
/// every use site stays cast-free on both platforms.
#[cfg(all(unix, not(target_arch = "wasm32")))]
#[allow(clippy::unnecessary_cast)]
const RDLCK: i16 = libc::F_RDLCK as i16;
#[cfg(all(unix, not(target_arch = "wasm32")))]
#[allow(clippy::unnecessary_cast)]
const UNLCK: i16 = libc::F_UNLCK as i16;

#[cfg(any(target_arch = "wasm32", windows))]
const RDLCK: i16 = 0;
#[cfg(any(target_arch = "wasm32", windows))]
const UNLCK: i16 = 2;

/// The lock-command triple's type, `libc::c_int` natively.
#[cfg(any(target_arch = "wasm32", windows))]
type LockCmd = i32;
#[cfg(all(unix, not(target_arch = "wasm32")))]
type LockCmd = libc::c_int;

/// What a lock op names its target by: an fd on unix, a `HANDLE` on Windows
/// (locks there belong to the handle, which is the whole reason `ofd` is
/// `true` on that platform), nothing on wasm32.
#[cfg(all(unix, not(target_arch = "wasm32")))]
type LockFd = i32;
#[cfg(windows)]
type LockFd = std::os::windows::io::RawHandle;
#[cfg(target_arch = "wasm32")]
type LockFd = i32;

#[cfg(all(unix, not(target_arch = "wasm32")))]
fn fd_of(f: &File) -> LockFd {
    f.as_raw_fd()
}
#[cfg(windows)]
fn fd_of(f: &File) -> LockFd {
    use std::os::windows::io::AsRawHandle as _;
    f.as_raw_handle()
}
#[cfg(target_arch = "wasm32")]
fn fd_of(_f: &File) -> LockFd {
    -1
}

#[cfg(target_arch = "wasm32")]
fn no_locks<T>() -> Result<T> {
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no byte-range locks in the wasm32 build (there is no sqlite base file to lock)",
    )))
}

use crate::{Error, Result};

const PENDING_BYTE: i64 = 0x4000_0000;
const RESERVED_BYTE: i64 = PENDING_BYTE + 1;
const SHARED_FIRST: i64 = PENDING_BYTE + 2;
const SHARED_SIZE: i64 = 510;

#[cfg(all(unix, not(target_arch = "wasm32")))]
fn flock(ty: i16, start: i64, len: i64) -> libc::flock {
    // Zeroed base: l_whence = SEEK_SET (0), l_pid filled by the kernel.
    let mut f: libc::flock = unsafe { std::mem::zeroed() };
    f.l_type = ty as libc::c_short;
    // `as _`: 32-bit glibc (armv7) has a 32-bit off_t here. Every byte this
    // module locks is a sqlite LOCK BYTE at a fixed position below 2^31
    // (PENDING_BYTE = 0x4000_0000, spans <= 512 bytes), never a
    // file-size-dependent offset — the narrowing cannot truncate.
    f.l_start = start as _;
    f.l_len = len as _;
    f
}

/// Try a non-blocking lock op; `Ok(true)` = acquired, `Ok(false)` = someone
/// conflicting holds it.
#[cfg(target_arch = "wasm32")]
fn setlk(_fd: LockFd, _cmd: LockCmd, _ty: i16, _start: i64, _len: i64) -> Result<bool> {
    no_locks()
}

#[cfg(windows)]
fn setlk(fd: LockFd, _cmd: LockCmd, ty: i16, start: i64, len: i64) -> Result<bool> {
    if ty == UNLCK {
        return win::unlock(fd, start, len).map(|()| true);
    }
    debug_assert_eq!(ty, RDLCK, "this module only ever takes SHARED locks");
    win::try_lock_shared(fd, start, len)
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
fn setlk(fd: i32, cmd: libc::c_int, ty: i16, start: i64, len: i64) -> Result<bool> {
    let mut f = flock(ty, start, len);
    let r = unsafe { libc::fcntl(fd, cmd, &mut f) };
    if r == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EAGAIN) => Ok(false),
        _ => Err(Error::Io(err)),
    }
}

/// Would a `ty` lock on `[start, start+len)` be granted right now? (F_GETLK
/// probe — takes nothing.)
#[cfg(target_arch = "wasm32")]
fn getlk_free(_fd: LockFd, _cmd: LockCmd, _ty: i16, _start: i64, _len: i64) -> Result<bool> {
    no_locks()
}

/// Windows has no `F_GETLK`, so the probe is a shared try-lock released at
/// once — which is not a workaround but sqlite's own `winCheckReservedLock`,
/// so its probe and ours are the same operation against the same bytes. The
/// hold is microscopic and SHARED, so it cannot exclude a foreign reader, and
/// a foreign writer that collides with it sees the ordinary retry it already
/// handles.
#[cfg(windows)]
fn getlk_free(fd: LockFd, _cmd: LockCmd, ty: i16, start: i64, len: i64) -> Result<bool> {
    debug_assert_eq!(ty, RDLCK, "this module only ever probes with a read lock");
    if win::try_lock_shared(fd, start, len)? {
        win::unlock(fd, start, len)?;
        return Ok(true);
    }
    Ok(false)
}

/// `LockFileEx`/`UnlockFileEx`, hand-declared — the crate is dependency-light
/// by design (DESIGN-SQLITE-BACKED §4) and two `extern "system"` lines do not
/// justify a windows-sys dependency in the one crate that must not drag one.
#[cfg(windows)]
mod win {
    use super::{Error, Result};
    use std::os::windows::io::RawHandle;

    #[repr(C)]
    #[derive(Default)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        h_event: usize,
    }

    // LOCKFILE_FAIL_IMMEDIATELY. The exclusive bit is deliberately absent:
    // every lock this module takes is SHARED, and taking an exclusive one
    // would exclude foreign sqlite READERS, which the design forbids.
    const FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    // Both spellings a conflicting holder can produce. Anything else is a real
    // error and propagates — the one answer that must never be invented is
    // "acquired", and that is returned only on an actual success.
    const ERROR_LOCK_VIOLATION: i32 = 33;
    const ERROR_IO_PENDING: i32 = 997;

    extern "system" {
        fn LockFileEx(
            file: RawHandle,
            flags: u32,
            reserved: u32,
            len_low: u32,
            len_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn UnlockFileEx(
            file: RawHandle,
            reserved: u32,
            len_low: u32,
            len_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    /// The offset goes in the OVERLAPPED, the length in the two `len_*` args —
    /// a split that is easy to get backwards, and getting it backwards would
    /// lock the wrong bytes silently.
    fn parts(start: i64, len: i64) -> (Overlapped, u32, u32) {
        let s = start as u64;
        let l = len as u64;
        (
            Overlapped {
                offset: s as u32,
                offset_high: (s >> 32) as u32,
                ..Default::default()
            },
            l as u32,
            (l >> 32) as u32,
        )
    }

    pub(super) fn try_lock_shared(fd: RawHandle, start: i64, len: i64) -> Result<bool> {
        let (mut ov, lo, hi) = parts(start, len);
        let ok = unsafe { LockFileEx(fd, FAIL_IMMEDIATELY, 0, lo, hi, &mut ov) };
        if ok != 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(ERROR_LOCK_VIOLATION) | Some(ERROR_IO_PENDING) => Ok(false),
            _ => Err(Error::Io(err)),
        }
    }

    /// Windows requires the released range to match the locked one EXACTLY —
    /// no partial or merged unlocks — which every caller here satisfies by
    /// passing the same `(start, len)` it locked.
    pub(super) fn unlock(fd: RawHandle, start: i64, len: i64) -> Result<()> {
        let (mut ov, lo, hi) = parts(start, len);
        let ok = unsafe { UnlockFileEx(fd, 0, lo, hi, &mut ov) };
        if ok != 0 {
            return Ok(());
        }
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
fn getlk_free(fd: i32, cmd_getlk: libc::c_int, ty: i16, start: i64, len: i64) -> Result<bool> {
    let mut f = flock(ty, start, len);
    let r = unsafe { libc::fcntl(fd, cmd_getlk, &mut f) };
    if r != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(f.l_type == libc::F_UNLCK as libc::c_short)
}

/// (SETLK cmd, GETLK cmd, is_ofd) — OFD probed once per process.
fn lock_cmds() -> (LockCmd, LockCmd, bool) {
    // wasm32: no fcntl commands exist; `false` (not OFD) is the conservative
    // report, and no caller gets this far anyway.
    #[cfg(target_arch = "wasm32")]
    {
        (0, 0, false)
    }
    // Windows locks belong to the HANDLE, so an in-process sqlite closing its
    // own handle cannot cancel ours — the same immunity OFD gives, by a
    // different mechanism. The command pair is unused there.
    #[cfg(windows)]
    {
        (0, 0, true)
    }
    #[cfg(target_os = "linux")]
    {
        (libc::F_OFD_SETLK, libc::F_OFD_GETLK, true)
    }
    #[cfg(target_os = "macos")]
    {
        // Verified functionally on the M3 (design Q1, 2026-07-17):
        // F_OFD_SETLK=90 / F_OFD_GETLK=92 exist and conflict correctly
        // against a second description's write attempt.
        (libc::F_OFD_SETLK, libc::F_OFD_GETLK, true)
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        // Other unixes: classic locks; callers must run the [R#5]
        // drop/re-take dance around in-process sqlite use.
        (libc::F_SETLK, libc::F_GETLK, false)
    }
}

/// A held SHARED lock on a sqlite database — foreign writers get their
/// normal `SQLITE_BUSY`; foreign readers are untouched. Owns its fd, so
/// dropping releases exactly this lock (and, for classic locks, only code
/// closing OTHER fds to the same file in-process can betray it — the [R#5]
/// caveat `ofd` reports).
pub struct SharedLock {
    file: File,
    ofd: bool,
}

impl SharedLock {
    /// Non-blocking acquire, following sqlite's own reader sequence: refuse
    /// if PENDING is held (a writer is draining readers — barging past it
    /// starves them, and sqlite readers would refuse too), then take the
    /// SHARED range. `Ok(None)` = busy right now.
    pub fn acquire(base: &Path) -> Result<Option<SharedLock>> {
        let file = File::options().read(true).write(true).open(base)?;
        let fd = fd_of(&file);
        let (setlk_cmd, getlk_cmd, ofd) = lock_cmds();
        // sqlite's sequence: a reader first proves PENDING is free.
        if !getlk_free(fd, getlk_cmd, RDLCK, PENDING_BYTE, 1)? {
            return Ok(None);
        }
        if !setlk(fd, setlk_cmd, RDLCK, SHARED_FIRST, SHARED_SIZE)? {
            return Ok(None);
        }
        Ok(Some(SharedLock { file, ofd }))
    }

    /// Whether this lock is an OFD lock (immune to in-process sqlite
    /// close()/unlock — the [R#5] trap). `false` means the caller MUST run
    /// the drop/re-take dance around any in-process sqlite library use.
    pub fn ofd(&self) -> bool {
        self.ofd
    }

    /// Is a foreign write TRANSACTION in flight right now? Probes RESERVED
    /// and PENDING with a read-lock test — readers never lock those bytes,
    /// so only a writer conflicts, and a writer holds RESERVED from its
    /// first dirtied page through COMMIT (and PENDING through EXCLUSIVE).
    pub fn writer_active(&self) -> Result<bool> {
        let fd = fd_of(&self.file);
        let (_, getlk_cmd, _) = lock_cmds();
        Ok(
            !getlk_free(fd, getlk_cmd, RDLCK, RESERVED_BYTE, 1)?
                || !getlk_free(fd, getlk_cmd, RDLCK, PENDING_BYTE, 1)?,
        )
    }
}

impl Drop for SharedLock {
    fn drop(&mut self) {
        let (setlk_cmd, _, _) = lock_cmds();
        // Best-effort explicit unlock; closing the fd releases it anyway.
        let _ = setlk(
            fd_of(&self.file),
            setlk_cmd,
            UNLCK,
            SHARED_FIRST,
            SHARED_SIZE,
        );
    }
}

/// Standalone writer probe without holding anything (opens its own fd).
pub fn writer_active(base: &Path) -> Result<bool> {
    let file = File::options().read(true).write(true).open(base)?;
    let fd = fd_of(&file);
    let (_, getlk_cmd, _) = lock_cmds();
    Ok(!getlk_free(fd, getlk_cmd, RDLCK, RESERVED_BYTE, 1)?
        || !getlk_free(fd, getlk_cmd, RDLCK, PENDING_BYTE, 1)?)
}

const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// Is the base's rollback journal HOT — i.e. a crashed writer left state
/// that MUST be rolled back before the main file is believable? lockingv3's
/// definition, checked by fact: the `-journal` exists with a well-formed
/// header (a PERSIST-mode leftover has a ZEROED header and is cold — an
/// existence check alone false-positives on every PERSIST database), and no
/// live writer holds RESERVED (a live writer's journal is just an open
/// transaction, not a corpse). Raw readers must treat `true` as "stop:
/// route through the sqlite library so its recovery runs" — nothing in this
/// crate rolls journals back.
pub fn hot_journal(base: &Path) -> Result<bool> {
    let jpath = {
        let mut s = base.as_os_str().to_owned();
        s.push("-journal");
        std::path::PathBuf::from(s)
    };
    let Ok(mut f) = File::open(&jpath) else {
        return Ok(false);
    };
    use std::io::Read as _;
    let mut magic = [0u8; 8];
    if f.read_exact(&mut magic).is_err() || magic != JOURNAL_MAGIC {
        return Ok(false);
    }
    Ok(!writer_active(base)?)
}

/// The OPTIMISTIC read bracket (design §2): a transient SHARED + the checks
/// that make an unlocked base readable for exactly one statement. The
/// pattern:
///
/// ```ignore
/// match ReadBracket::open(base)? {
///     BracketOutcome::Busy => /* writer active: back off, NOT divergence */
///     BracketOutcome::HotJournal => /* route through the library's recovery */
///     BracketOutcome::Held(b) => {
///         if !b.stamp_matches(&expected)? { /* divergence: reconcile */ }
///         /* read base pages; results buffer until the bracket closes */
///     }
/// }
/// ```
///
/// While held, the SHARED excludes any EXCLUSIVE — commit AND cache-spill
/// alike — which is what makes the pages quiescent for the bracket's
/// lifetime; a RESERVED-only writer has not touched the file yet (mutation
/// requires EXCLUSIVE) and coexists safely.
pub enum BracketOutcome {
    Busy,
    HotJournal,
    Held(ReadBracket),
}

pub struct ReadBracket {
    lock: SharedLock,
    base: std::path::PathBuf,
}

impl ReadBracket {
    pub fn open(base: &Path) -> Result<BracketOutcome> {
        let Some(lock) = SharedLock::acquire(base)? else {
            return Ok(BracketOutcome::Busy);
        };
        // Checked UNDER the SHARED (a writer that could make it hot is now
        // excluded from EXCLUSIVE, so the answer cannot rot mid-bracket).
        if hot_journal(base)? {
            return Ok(BracketOutcome::HotJournal);
        }
        Ok(BracketOutcome::Held(ReadBracket { lock, base: base.to_path_buf() }))
    }

    /// The strong stamp comparison, inside the bracket's quiescence.
    pub fn stamp_matches(&self, expected: &crate::stamp::BaseStamp) -> Result<bool> {
        expected.matches(&self.base)
    }

    pub fn ofd(&self) -> bool {
        self.lock.ofd()
    }
}
