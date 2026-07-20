//! Whole-database backup: a consistent image of one `.mpedb` file, installed
//! over another.
//!
//! # Why this is a byte image and not a logical dump
//!
//! An mpedb database is ONE self-describing file — schema, catalog, data,
//! indexes, freelist and geometry all live inside it. So "back this database
//! up" has an exact answer that needs no interpretation: the file's bytes at
//! one instant. A logical dump (re-`CREATE` + re-`INSERT`) would have to
//! reconstruct DDL text mpedb does not keep verbatim, and would silently drop
//! anything the reconstruction does not know about.
//!
//! # The consistency argument
//!
//! The copy runs while this connection holds the **writer lock**
//! ([`Database::begin`]). mpedb has exactly one writer at a time, so for the
//! duration of the copy no commit can publish a new meta and no page can be
//! rewritten: the bytes on the way out are one committed instant. Readers are
//! unaffected — they never mutate a data page — and are not blocked.
//!
//! That is a DIFFERENT (and stronger) contract than sqlite's online backup API,
//! which copies pages incrementally under a read lock and **restarts from the
//! beginning** whenever the source is written mid-copy. Here the image cannot
//! be invalidated by a concurrent writer, because there is no concurrent
//! writer; the cost is that the whole capture happens at
//! [`Database::backup_capture`] rather than being spread over the caller's
//! `step` calls. See [`BackupImage::step`].
//!
//! # A copied file is a fresh incarnation
//!
//! Three regions of the file are RUNTIME state, not data: the writer mutex, the
//! reader table, and the boot id (design/DESIGN.md §4.3, `shm.rs`). Copying
//! them verbatim would hand the new file a mutex recorded as *locked by the
//! process doing the backup* and a reader table full of pins belonging to
//! readers of the SOURCE — a deadlock and an unbounded high-water leak.
//!
//! The image therefore **zeroes the boot id**, which is exactly the signal the
//! engine's own post-attach recovery watches for: the first attach to the copy
//! takes the reboot branch, re-initializes the writer mutex and clears the
//! reader table. No new recovery code, and no new invariant — the copy simply
//! looks to the engine like a file last touched before a reboot, which is the
//! truth about its volatile state.

use crate::Database;
use mpedb_types::{Error, Result, PAGE_SIZE};
use std::path::{Path, PathBuf};

/// A captured, consistent image of a source database, waiting to be installed
/// over a destination file.
///
/// The image lives in a temporary file beside the destination and is removed on
/// drop, so abandoning a backup (dropping this without
/// [`BackupImage::install`]) leaves the destination exactly as it was.
pub struct BackupImage {
    tmp: PathBuf,
    dest: PathBuf,
    page_count: u64,
    done: u64,
}

impl Database {
    /// Capture a consistent image of this database for installation over
    /// `dest`, taken under the writer lock (see the module docs).
    ///
    /// `dest` is the path the image will replace; nothing at that path is
    /// touched until [`BackupImage::install`]. The image is written to a
    /// sibling temporary file, so `dest`'s directory must be writable and hold
    /// room for a second copy of this database.
    pub fn backup_capture(&self, dest: &Path) -> Result<BackupImage> {
        let src = self.path().to_path_buf();
        if same_file(&src, dest) {
            return Err(Error::Config(
                "backup source and destination are the same database".into(),
            ));
        }
        let tmp = tmp_path(dest);
        // Best-effort clean-up of a leftover from an interrupted backup: the
        // name is deterministic, so a crashed run must not block this one.
        let _ = std::fs::remove_file(&tmp);
        {
            // The writer lock, held across the whole read. Dropping the session
            // without committing is a rollback of nothing.
            let _writer = self.begin()?;
            std::fs::copy(&src, &tmp).map_err(Error::Io)?;
        }
        // Void the volatile control state (module docs): the first attach to
        // the copy re-initializes the writer mutex and the reader table.
        void_boot_id(&tmp)?;
        let len = std::fs::metadata(&tmp).map_err(Error::Io)?.len();
        Ok(BackupImage {
            tmp,
            dest: dest.to_path_buf(),
            page_count: len / PAGE_SIZE as u64,
            done: 0,
        })
    }
}

impl BackupImage {
    /// Total pages in the image — the source's file geometry, which is what an
    /// mpedb database's "page count" means (pages are pre-reserved at create).
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Pages not yet accounted for by [`BackupImage::step`].
    pub fn remaining(&self) -> u64 {
        self.page_count - self.done
    }

    /// Account `pages` more pages of the image, or all of them when `pages` is
    /// negative. Returns `true` once the image is fully accounted for.
    ///
    /// **What this does and does not do.** The image was already captured, in
    /// one consistent instant, by [`Database::backup_capture`]; `step` walks a
    /// counter over it so a caller can pace a progress report and abandon the
    /// backup part-way. It is deliberately NOT sqlite's incremental copy: there
    /// the pages are read one batch at a time and the whole backup restarts if
    /// the source is written. Here nothing can invalidate the image, so there
    /// is nothing to restart — and no page is read after the lock is released.
    pub fn step(&mut self, pages: i64) -> bool {
        let n = if pages < 0 {
            self.remaining()
        } else {
            (pages as u64).min(self.remaining())
        };
        self.done += n;
        self.done >= self.page_count
    }

    /// Move the captured image over the destination path, atomically.
    ///
    /// The caller must have CLOSED any handle on the destination first: this
    /// replaces the file, and a live mapping of the old inode would keep
    /// serving the old database.
    pub fn install(self) -> Result<()> {
        std::fs::rename(&self.tmp, &self.dest).map_err(Error::Io)?;
        // Consumed: nothing left to clean up.
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for BackupImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// The temp file an image is captured into: a sibling of the destination, so
/// the final [`BackupImage::install`] is a same-filesystem rename.
fn tmp_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".backup-{}.tmp", std::process::id()));
    dest.with_file_name(name)
}

/// Same file on disk? Compared by (device, inode) when both exist, falling back
/// to the paths — the point is only to refuse a self-backup, which would
/// deadlock on the writer lock or truncate the source.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return ma.dev() == mb.dev() && ma.ino() == mb.ino();
        }
    }
    false
}

/// Zero the copy's boot id, so the engine's post-attach recovery treats it as a
/// file from a previous boot and re-initializes the writer mutex + reader table
/// (module docs). A real boot id is never all-zero.
fn void_boot_id(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(Error::Io)?;
    f.seek(SeekFrom::Start(mpedb_core::shm::BOOT_ID_FILE_OFFSET))
        .map_err(Error::Io)?;
    f.write_all(&[0u8; 16]).map_err(Error::Io)?;
    f.sync_all().map_err(Error::Io)?;
    Ok(())
}
