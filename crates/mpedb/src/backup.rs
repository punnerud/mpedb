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
use mpedb_sqlitefmt as fmtx;
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
    /// The source's commit counter at capture (`Database::snapshot_txn`, one
    /// meta read). The C-API's backup_step compares it per step: a moved
    /// counter means the source committed mid-backup, and sqlite's contract
    /// there is invalidate-and-restart (plan §11).
    source_txn: u64,
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
        // high_water: pages that actually hold content. File length is the
        // fallocated pre-reserve (often MiB of empty pages); pacing progress
        // over that makes a 2-row `:memory:` DB report thousands of steps
        // (CPython `test_progress` expects ~2). Progress is paced over content
        // pages; install still copies the whole file (empty pages included).
        let content_pages = {
            let _writer = self.begin()?;
            // `leak_counters` is the public (txn_id, high_water, …) probe.
            let hw = self.engine.leak_counters().map(|c| c.1).unwrap_or(1).max(1);
            std::fs::copy(&src, &tmp).map_err(Error::Io)?;
            hw
        };
        // Void the volatile control state (module docs): the first attach to
        // the copy re-initializes the writer mutex and the reader table.
        void_boot_id(&tmp)?;
        let file_pages = std::fs::metadata(&tmp).map_err(Error::Io)?.len() / PAGE_SIZE as u64;
        Ok(BackupImage {
            tmp,
            dest: dest.to_path_buf(),
            page_count: content_pages.min(file_pages.max(1)).max(1),
            done: 0,
            source_txn: self.snapshot_txn(),
        })
    }

    /// The logical content as a REAL sqlite image (plan §11) — geometry no
    /// consumer can call fabricated: `page_count` over THIS is the number of
    /// 4096-byte pages sqlite itself would use for the content (the CPython
    /// progress pair's whole complaint). `None` when any live table falls
    /// outside the writer's v1 scope — hidden-rowid Standard tables (the
    /// shim's `CREATE TABLE` shape, which IS a sqlite rowid table), no
    /// secondary indexes, scalar values only — in which case backup keeps
    /// mpedb's own honest geometry and serialize refuses by name.
    ///
    /// `skip` names tables that are the CALLER's bookkeeping (the C-API's
    /// seed table), not content.
    pub fn sqlite_image(&self, skip: &[&str]) -> Result<Option<Vec<u8>>> {
        let bundle = self.schema();
        let r = self.engine.begin_read()?;
        let mut tables: Vec<fmtx::ImageTable> = Vec::new();
        for t in bundle.tables.iter().filter(|t| !t.dead) {
            if skip.iter().any(|s| mpedb_types::ident_eq(s, &t.name)) {
                continue;
            }
            let Some(hid) = t.hidden_rowid_col() else { return Ok(None) };
            if !matches!(t.kind, mpedb_types::TableKind::Standard) || !t.indexes.is_empty() {
                return Ok(None);
            }
            let cols: Vec<String> = t
                .visible_columns()
                .iter()
                .map(|c| {
                    let ty = match c.ty {
                        mpedb_types::ColumnType::Int64 => " INTEGER",
                        mpedb_types::ColumnType::Float64 => " REAL",
                        mpedb_types::ColumnType::Text => " TEXT",
                        mpedb_types::ColumnType::Blob => " BLOB",
                        mpedb_types::ColumnType::Bool => " BOOLEAN",
                        _ => "",
                    };
                    format!("\"{}\"{}",  c.name.replace('"', "\"\""), ty)
                })
                .collect();
            let sql = format!(
                "CREATE TABLE \"{}\" ({})",
                t.name.replace('"', "\"\""),
                cols.join(", ")
            );
            let mut rows: Vec<Vec<fmtx::Value>> = Vec::new();
            let mut cur = r.scan(t.id, None, None)?;
            while let Some(row) = cur.next()? {
                let mut out = Vec::with_capacity(row.len().saturating_sub(1));
                for (i, v) in row.into_iter().enumerate() {
                    if i == hid as usize {
                        continue; // the hidden rowid is the tree key, not content
                    }
                    out.push(match v {
                        mpedb_types::Value::Null => fmtx::Value::Null,
                        mpedb_types::Value::Int(x) => fmtx::Value::Int(x),
                        mpedb_types::Value::Float(f) => fmtx::Value::Float(f),
                        mpedb_types::Value::Text(s) => fmtx::Value::Text(s),
                        mpedb_types::Value::Blob(b) => fmtx::Value::Blob(b),
                        mpedb_types::Value::Bool(b) => fmtx::Value::Int(b as i64),
                        _ => return Ok(None),
                    });
                }
                rows.push(out);
            }
            tables.push(fmtx::ImageTable { name: t.name.clone(), sql, rows });
        }
        r.finish()?;
        match fmtx::write_image(&tables, 4096) {
            Ok(img) => Ok(Some(img)),
            // The writer names what it cannot represent; for backup that is
            // the honest fall-back-to-mpedb-geometry signal, not an error.
            Err(fmtx::Error::Unsupported(_)) => Ok(None),
            Err(e) => Err(Error::Internal(format!("sqlite image writer: {e}"))),
        }
    }
}

impl Database {
    /// The whole database as ONE consistent byte image — `backup_capture`'s
    /// snapshot discipline (the copy is taken under the writer lock, and the
    /// boot id is voided so the first attach to the image re-initializes the
    /// writer mutex and reader table) — returned as BYTES instead of staged
    /// beside a destination. The C-API's `sqlite3_serialize` is the consumer;
    /// `sqlite3_deserialize` reopens such an image from a scratch file.
    ///
    /// The image is mpedb's own format, full fallocated length included: a
    /// truncated image would fail the attach-time geometry validation, so
    /// fidelity beats compactness here.
    pub fn serialize_image(&self) -> Result<Vec<u8>> {
        let src = self.path().to_path_buf();
        let tmp = src.with_extension("mpedb-serialize-tmp");
        let _ = std::fs::remove_file(&tmp);
        {
            let _writer = self.begin()?;
            std::fs::copy(&src, &tmp).map_err(Error::Io)?;
        }
        void_boot_id(&tmp)?;
        let bytes = std::fs::read(&tmp).map_err(Error::Io)?;
        let _ = std::fs::remove_file(&tmp);
        Ok(bytes)
    }
}

impl BackupImage {
    /// Total pages reported for progress pacing — content high-water, not the
    /// full fallocated file (see [`Database::backup_capture`]).
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Pages not yet accounted for by [`BackupImage::step`].
    pub fn remaining(&self) -> u64 {
        self.page_count - self.done
    }

    /// The source commit counter this image was captured at (plan §11).
    pub fn source_txn(&self) -> u64 {
        self.source_txn
    }

    /// Repace the progress meter over a REAL sqlite-image geometry: the
    /// content of the captured source measured in sqlite's own pages. The
    /// INSTALL half is untouched — the mpedb copy in `tmp` is what lands on
    /// the destination — this only makes `page_count`/`remaining` numbers no
    /// consumer can call fabricated.
    pub fn set_sqlite_geometry(&mut self, image_len: usize) {
        self.page_count = ((image_len / 4096) as u64).max(1);
        self.done = self.done.min(self.page_count);
    }

    /// Restart after a mid-backup source commit (sqlite's own semantics —
    /// the C-API step calls this when `source_txn` moved): adopt the fresh
    /// capture, rewind the meter.
    pub fn restart_from(&mut self, fresh: BackupImage) {
        let old = std::mem::replace(self, fresh);
        // `tmp_path` is DETERMINISTIC per destination, so the fresh capture
        // already deleted and rewrote the very file the old value's Drop
        // would now remove — letting it run deleted the NEW copy and the
        // install renamed a ghost (measured: journal [1,1,0] perfect, then
        // SQLITE_IOERR and an empty destination). Defuse it.
        std::mem::forget(old);
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
