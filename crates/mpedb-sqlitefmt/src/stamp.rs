//! The settled BaseStamp — DESIGN-SQLITE-BACKED §3. One tuple answers "has
//! anything touched the base since I last looked?" with one `stat()` (plus
//! one 100-byte pread when the caller wants the strong form), after minutes
//! or days unlocked.
//!
//! The load-bearing trick is WHEN the stamp is taken: while the caller still
//! holds a write-excluding lock on the base, [`settle`] spins a scratch file
//! until the filesystem's OWN timestamp clock has advanced strictly past the
//! base's mtime. Because the base was provably quiescent across that
//! boundary, any later mutation lands in a strictly newer tick — plain mtime
//! goes from "unusable" (same-tick writes are invisible) to a trustworthy
//! long-horizon change detector. The header fields make the tuple robust
//! where mtime alone is not: the change counter is monotonic per committing
//! rollback-journal transaction (clock steps cannot rewind it), the `-wal`
//! salts are the WAL-mode witness (a WAL reset reuses the file with NEW
//! salts and UNCHANGED size), and header bytes 18/19 catch a journal-mode
//! flip in an unlocked window.
//!
//! What this module does NOT do: hold locks (the caller's job — the settle
//! contract is meaningless without one) or prove read CONSISTENCY (that is
//! the per-statement SHARED bracket of §2; the stamp only answers
//! *divergence*).

use std::path::Path;
use std::time::SystemTime;

use crate::{Error, Result};

/// Everything that must be equal for "nothing touched the base" to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseStamp {
    pub mtime: SystemTime,
    pub size: u64,
    /// Header offset 24 — incremented when the file is "unlocked after
    /// having been modified" (fileformat2's exact words: NOT per commit in
    /// `locking_mode=EXCLUSIVE` sessions, and possibly not at all in WAL).
    pub change_counter: u32,
    /// Header offset 40 — schema (DDL) generation.
    pub schema_cookie: u32,
    /// Header bytes 18/19 — file-format write/read version; 2 = WAL.
    pub format_versions: [u8; 2],
    /// `-wal` sidecar witness: (salt pair at offset 16, file size), when the
    /// sidecar exists. A checkpoint RESET reuses the wal file from offset 0
    /// with new salts and unchanged size — the salts are what move.
    pub wal: Option<([u8; 8], u64)>,
}

#[cfg(not(target_arch = "wasm32"))]
fn read_at(path: &Path, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    std::fs::File::open(path)?.read_exact_at(buf, off)
}

/// wasm32: `File::open` cannot succeed (no filesystem), so this only ever
/// returns the same failure the native path would on a missing base. Kept as
/// a distinct arm because `read_exact_at` is a unix extension trait.
#[cfg(target_arch = "wasm32")]
fn read_at(path: &Path, _off: u64, _buf: &mut [u8]) -> std::io::Result<()> {
    std::fs::File::open(path)?;
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no positional reads in the wasm32 build",
    ))
}

impl BaseStamp {
    /// Read the stamp as the base is NOW. One stat + one 44-byte pread
    /// (+ one stat and 8-byte pread on the `-wal` when present).
    pub fn read(base: &Path) -> Result<BaseStamp> {
        let md = std::fs::metadata(base)?;
        let mut hdr = [0u8; 44];
        read_at(base, 0, &mut hdr)?;
        let wal_path = {
            let mut s = base.as_os_str().to_owned();
            s.push("-wal");
            std::path::PathBuf::from(s)
        };
        let wal = match std::fs::metadata(&wal_path) {
            Err(_) => None,
            Ok(wmd) => {
                let mut salts = [0u8; 8];
                // A zero-length or torn wal header still stamps (all-zero
                // salts): the tuple compares equal only to itself.
                if wmd.len() >= 24 {
                    read_at(&wal_path, 16, &mut salts)?;
                }
                Some((salts, wmd.len()))
            }
        };
        Ok(BaseStamp {
            mtime: md.modified()?,
            size: md.len(),
            change_counter: u32::from_be_bytes(hdr[24..28].try_into().expect("4")),
            schema_cookie: u32::from_be_bytes(hdr[40..44].try_into().expect("4")),
            format_versions: [hdr[18], hdr[19]],
            wal,
        })
    }

    /// The cheap first-level check: stat-only fields. `false` means
    /// definitely touched; `true` means "run [`Self::matches`] if you need
    /// the strong answer" (a foreign writer CAN leave size unchanged, and a
    /// clock step can lie about mtime — the header fields are the backstop).
    pub fn stat_matches(&self, base: &Path) -> Result<bool> {
        let md = std::fs::metadata(base)?;
        Ok(md.modified()? == self.mtime && md.len() == self.size)
    }

    /// The strong check: every field.
    pub fn matches(&self, base: &Path) -> Result<bool> {
        Ok(*self == BaseStamp::read(base)?)
    }
}

/// Settle in the FILE-clock domain, then stamp — call while (and only while)
/// a write-excluding lock on the base is held. Touches `scratch` (an
/// O_EXCL-created file in the base's directory, caller-provided so its
/// creation rules live with the lock code) until its mtime is STRICTLY
/// greater than the base's, proving the filesystem's timestamp clock has
/// crossed the base's tick; any post-release mutation must then land
/// strictly newer. Loops at most ~4 s (coarse-granularity filesystems cap
/// near 2 s) before refusing — a clock that will not advance is a
/// configuration problem worth a name, not an infinite spin.
pub fn settle_and_read(base: &Path, scratch: &Path) -> Result<BaseStamp> {
    let base_m = std::fs::metadata(base)?.modified()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        // A 1-byte write is what bumps mtime; content is irrelevant.
        std::fs::write(scratch, b"s")?;
        let scratch_m = std::fs::metadata(scratch)?.modified()?;
        if scratch_m > base_m {
            return BaseStamp::read(base);
        }
        if std::time::Instant::now() > deadline {
            return Err(Error::Unsupported(
                "filesystem timestamp clock did not advance within 4s — cannot \
                 settle a trustworthy stamp on this filesystem"
                    .into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
