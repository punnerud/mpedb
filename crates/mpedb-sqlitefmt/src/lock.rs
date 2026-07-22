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

use std::fs::File;
#[cfg(not(target_arch = "wasm32"))]
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// `F_RDLCK`/`F_UNLCK` as the `flock.l_type` field's type. The cast is REQUIRED
/// on Linux (the libc consts are `c_int`, the field is `c_short`) and a no-op
/// on macOS (the consts are already `c_short`) — which is why clippy on macOS
/// flags the inline spelling as `unnecessary_cast`. One allowed cast here, and
/// every use site stays cast-free on both platforms.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::unnecessary_cast)]
const RDLCK: i16 = libc::F_RDLCK as i16;
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::unnecessary_cast)]
const UNLCK: i16 = libc::F_UNLCK as i16;

#[cfg(target_arch = "wasm32")]
const RDLCK: i16 = 0;
#[cfg(target_arch = "wasm32")]
const UNLCK: i16 = 2;

/// The lock-command triple's type, `libc::c_int` natively.
#[cfg(target_arch = "wasm32")]
type LockCmd = i32;
#[cfg(not(target_arch = "wasm32"))]
type LockCmd = libc::c_int;

/// The fd a lock op targets. wasm32 has no fds; unreachable (see module doc).
#[cfg(not(target_arch = "wasm32"))]
fn fd_of(f: &File) -> i32 {
    f.as_raw_fd()
}
#[cfg(target_arch = "wasm32")]
fn fd_of(_f: &File) -> i32 {
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

#[cfg(not(target_arch = "wasm32"))]
fn flock(ty: i16, start: i64, len: i64) -> libc::flock {
    // Zeroed base: l_whence = SEEK_SET (0), l_pid filled by the kernel.
    let mut f: libc::flock = unsafe { std::mem::zeroed() };
    f.l_type = ty as libc::c_short;
    f.l_start = start;
    f.l_len = len;
    f
}

/// Try a non-blocking lock op; `Ok(true)` = acquired, `Ok(false)` = someone
/// conflicting holds it.
#[cfg(target_arch = "wasm32")]
fn setlk(_fd: i32, _cmd: LockCmd, _ty: i16, _start: i64, _len: i64) -> Result<bool> {
    no_locks()
}

#[cfg(not(target_arch = "wasm32"))]
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
fn getlk_free(_fd: i32, _cmd: LockCmd, _ty: i16, _start: i64, _len: i64) -> Result<bool> {
    no_locks()
}

#[cfg(not(target_arch = "wasm32"))]
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
