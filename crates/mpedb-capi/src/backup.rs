//! `sqlite3_backup_*` — the online backup API, mapped honestly onto mpedb.
//!
//! # Why this can be built at all
//!
//! An mpedb database is ONE self-describing file, and mpedb has exactly one
//! writer at a time. So "a consistent copy of the source" is not an
//! approximation of anything: it is the file's bytes taken while the writer
//! lock is held. `mpedb::backup` does that half (including voiding the copy's
//! volatile control state so the new file is a fresh incarnation); this module
//! is the C-API shape on top of it, plus the one thing the facade cannot do —
//! swapping the DESTINATION connection's open database for the copy.
//!
//! # The contract, verified against sqlite 3.45 rather than assumed
//!
//! * `sqlite3_backup_init(dst, dstName, src, srcName)` returns a handle, or
//!   NULL with the error left **on the destination connection** (that is where
//!   CPython reads it).
//! * `sqlite3_backup_step(b, n)` copies up to `n` pages, `n < 0` meaning all;
//!   `SQLITE_OK` while pages remain, `SQLITE_DONE` when the copy is complete.
//! * `sqlite3_backup_remaining` / `_pagecount` report the copy's progress in
//!   pages, and are read AFTER each step by CPython's progress callback.
//! * `sqlite3_backup_finish` releases the handle and returns the backup's
//!   final status; finishing before `SQLITE_DONE` abandons the backup and the
//!   destination is left untouched.
//!
//! # The ONE deliberate difference, stated plainly
//!
//! sqlite copies pages incrementally under a read lock and **restarts the
//! whole backup** if the source is written mid-copy. mpedb captures the image
//! in one instant under the writer lock (`mpedb::backup`), so nothing can
//! invalidate it and there is nothing to restart. Two consequences a caller can
//! observe:
//!
//! 1. A write to the source between `backup_init` and the last `backup_step` is
//!    NOT in the copy — where sqlite would restart and include it. Both answers
//!    are a consistent database; ours is the state at `init`.
//! 2. `step(n)` paces the progress REPORT, not the capture. The number of steps
//!    a backup takes is therefore `ceil(pagecount / n)` over **mpedb's** page
//!    count — an mpedb file pre-reserves its pages, so that count is the file
//!    geometry and bears no relation to how many pages sqlite would have used
//!    for the same data.
//!
//! Everything else — when the destination becomes visible, what a partial
//! backup leaves behind, where errors are reported — matches.

use crate::consts::*;
use crate::{conn, register_shim_builtins, Sqlite3};
use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::time::Duration;

/// A live backup: the captured image plus the destination it will be installed
/// over. Handed to the caller as an opaque `sqlite3_backup *`.
pub struct Sqlite3Backup {
    /// The destination connection. Kept alive by `Sqlite3::backups`, which
    /// makes `sqlite3_close` refuse while a backup is outstanding — so this
    /// pointer cannot dangle.
    dst: *mut Sqlite3,
    image: Option<mpedb::backup::BackupImage>,
    /// Set once the image has been installed over the destination: further
    /// steps are `SQLITE_DONE` no-ops, and `finish` has nothing to undo.
    installed: bool,
    page_count: u64,
    remaining: u64,
}

/// Resolve a schema name argument. sqlite names the source/destination schema
/// (`"main"`, `"temp"`, an ATTACHed name); mpedb has exactly one — the file —
/// so `main` (and the empty/NULL spelling CPython never sends) is the only name
/// that resolves, and every other one is refused BY NAME rather than silently
/// backing up the wrong database.
unsafe fn is_main(name: *const c_char) -> bool {
    match crate::c_str_opt(name) {
        None => true,
        Some(s) => s.is_empty() || s.eq_ignore_ascii_case("main"),
    }
}

unsafe fn schema_name(name: *const c_char) -> String {
    crate::c_str_opt(name).unwrap_or_default().to_string()
}

/// `sqlite3_backup_init(pDest, zDestName, pSource, zSourceName)`.
///
/// # Safety
/// Both handles must be connections this shim opened (or NULL); the names must
/// be NUL-terminated or NULL.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_init(
    dst: *mut Sqlite3,
    dst_name: *const c_char,
    src: *mut Sqlite3,
    src_name: *const c_char,
) -> *mut c_void {
    // Every failure below leaves code + message on the DESTINATION connection:
    // that is sqlite's contract, and CPython raises from there. Without it the
    // caller sees a bare NULL and (in CPython) a SystemError.
    let Some(d) = conn(dst) else {
        return std::ptr::null_mut();
    };
    d.clear_error();
    let fail = |d: &mut Sqlite3, code: c_int, msg: String| -> *mut c_void {
        d.set_error(code, code, &msg);
        std::ptr::null_mut()
    };
    if src.is_null() || std::ptr::eq(src, dst) {
        return fail(d, SQLITE_ERROR, "source and destination must be distinct".into());
    }
    if !is_main(dst_name) {
        return fail(d, SQLITE_ERROR, format!("unknown database {}", schema_name(dst_name)));
    }
    if !is_main(src_name) {
        return fail(d, SQLITE_ERROR, format!("unknown database {}", schema_name(src_name)));
    }
    if d.readonly {
        return fail(d, SQLITE_READONLY, "attempt to write a readonly database".into());
    }
    // The destination is REPLACED wholesale, so nothing may still be looking at
    // the old file: sqlite likewise refuses a destination that is mid-write.
    if d.txn.is_some() {
        return fail(d, SQLITE_ERROR, "target is in transaction".into());
    }
    if !d.blobs.is_empty() {
        return fail(d, SQLITE_ERROR, "target has open blob handles".into());
    }
    let dest_path = d.path.clone();
    // Borrow the source only for the capture: `conn` hands out a &mut, and the
    // destination borrow above is still live.
    let capture = {
        let Some(s) = conn(src) else {
            return fail(d, SQLITE_ERROR, "source is not an open database".into());
        };
        if s.txn.is_some() {
            // The source connection itself holds the writer lock; the capture
            // would deadlock against it. sqlite reports exactly this as BUSY.
            return fail(d, SQLITE_BUSY, "source database is locked".into());
        }
        s.db.backup_capture(&dest_path)
    };
    let image = match capture {
        Ok(i) => i,
        Err(e) => return fail(d, SQLITE_ERROR, format!("backup failed: {e}")),
    };
    let b = Box::new(Sqlite3Backup {
        dst,
        page_count: image.page_count(),
        remaining: image.page_count(),
        image: Some(image),
        installed: false,
    });
    let raw = Box::into_raw(b);
    d.backups.push(raw);
    raw as *mut c_void
}

unsafe fn backup<'a>(b: *mut c_void) -> Option<&'a mut Sqlite3Backup> {
    (b as *mut Sqlite3Backup).as_mut()
}

/// `sqlite3_backup_step(p, nPage)`.
///
/// # Safety
/// `b` must be a handle from `sqlite3_backup_init` that has not been finished.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_step(b: *mut c_void, n: c_int) -> c_int {
    let Some(bk) = backup(b) else {
        return SQLITE_MISUSE;
    };
    if bk.installed {
        return SQLITE_DONE;
    }
    let Some(image) = bk.image.as_mut() else {
        return SQLITE_MISUSE;
    };
    let done = image.step(n as i64);
    bk.remaining = image.remaining();
    if !done {
        return SQLITE_OK;
    }
    // Last step: install. The destination's OLD database must be closed before
    // the copy is renamed over it — otherwise the connection keeps serving the
    // unlinked inode — but `Database` has no close, so the order is: rename
    // (the old inode survives, still mapped), open the new file, then ASSIGN,
    // which drops the old one.
    let image = bk.image.take().expect("checked above");
    let Some(d) = conn(bk.dst) else {
        return SQLITE_MISUSE;
    };
    if let Err(e) = image.install() {
        d.set_error(SQLITE_IOERR, SQLITE_IOERR, &format!("backup install failed: {e}"));
        return SQLITE_IOERR;
    }
    match mpedb::Database::open_from_file(&d.path) {
        Ok(newdb) => {
            d.db = newdb;
            // A reopened `Database` starts with an empty function registry:
            // re-install the shim's own builtins and everything this connection
            // registered, so a backup is invisible to the caller's UDFs.
            register_shim_builtins(&d.db);
            for h in &d.host_fns {
                h.reinstall(&d.db);
            }
            for h in &d.host_colls {
                h.reinstall(&d.db);
            }
            if d.busy_timeout_ms > 0 {
                d.db.set_busy_timeout(Some(Duration::from_millis(d.busy_timeout_ms as u64)));
            }
            bk.installed = true;
            SQLITE_DONE
        }
        Err(e) => {
            d.set_error(
                SQLITE_IOERR,
                SQLITE_IOERR,
                &format!("backup installed but the destination could not be reopened: {e}"),
            );
            SQLITE_IOERR
        }
    }
}

/// `sqlite3_backup_finish(p)` — release the handle. Before `SQLITE_DONE` this
/// ABANDONS the backup: the captured image is dropped (its temporary file with
/// it) and the destination is left exactly as it was.
///
/// # Safety
/// `b` must be a handle from `sqlite3_backup_init`, finished at most once.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_finish(b: *mut c_void) -> c_int {
    if b.is_null() {
        return SQLITE_OK;
    }
    let bk = Box::from_raw(b as *mut Sqlite3Backup);
    if let Some(d) = conn(bk.dst) {
        d.backups.retain(|&p| p != b as *mut Sqlite3Backup);
    }
    SQLITE_OK
}

/// `sqlite3_backup_remaining(p)`.
///
/// # Safety
/// As `sqlite3_backup_step`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_remaining(b: *mut c_void) -> c_int {
    backup(b).map_or(0, |bk| bk.remaining.min(c_int::MAX as u64) as c_int)
}

/// `sqlite3_backup_pagecount(p)`.
///
/// # Safety
/// As `sqlite3_backup_step`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_pagecount(b: *mut c_void) -> c_int {
    backup(b).map_or(0, |bk| bk.page_count.min(c_int::MAX as u64) as c_int)
}
