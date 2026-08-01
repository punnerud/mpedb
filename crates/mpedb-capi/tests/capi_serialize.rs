//! `sqlite3_serialize` / `sqlite3_deserialize` / the backup progress meter
//! (plan §5 + §11), driven through the exported C API. Since §11 the image
//! is a REAL sqlite file of the logical content (the writer's v1 scope —
//! the shim's ordinary rowid-table shape); out-of-scope bases refuse by
//! name, and deserialize adopts BOTH formats (sqlite via the native
//! reader, mpedb bytes as before). CPython's SerializeTests/BackupTests
//! are the governing spec; this pins what their harness cannot show, plus
//! the restart semantics repo-side (CPython runs by hand).

use mpedb_sqlite3::*;
use std::ffi::{c_char, c_longlong, c_uint, c_void, CStr, CString};
use std::os::raw::c_int;
use std::ptr;

mod common;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn open_memory() -> *mut Sqlite3 {
    let mut db: *mut Sqlite3 = ptr::null_mut();
    let name = cs(":memory:");
    assert_eq!(sqlite3_open(name.as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec_rc(db: *mut Sqlite3, sql: &str) -> c_int {
    let s = cs(sql);
    sqlite3_exec(db, s.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn errmsg(db: *mut Sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db) as *const c_char).to_str().unwrap().to_string()
}

const NOCOPY: c_uint = 0x001;
const FREEONCLOSE: c_uint = 1;
const RESIZEABLE: c_uint = 2;

#[test]
fn serialize_roundtrips_and_the_missing_table_speaks_sqlite() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec_rc(db, "CREATE TABLE t (a INTEGER, v TEXT)"), SQLITE_OK);
        assert_eq!(exec_rc(db, "INSERT INTO t VALUES (1, 'x')"), SQLITE_OK);

        // §11: serialize now emits a REAL sqlite image (the writer's v1
        // scope: the shim's ordinary rowid-table shape). A DECLARED primary
        // key is outside it — NULL, the named refusal, never a foreign
        // format under sqlite's name.
        assert_eq!(exec_rc(db, "CREATE TABLE pked (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        let mut sz: c_longlong = 0;
        assert!(sqlite3_serialize(db, ptr::null(), &mut sz, 0).is_null());
        assert_eq!(exec_rc(db, "DROP TABLE pked"), SQLITE_OK);

        // NOCOPY: the documented "no contiguous in-memory image" answer is
        // NULL — CPython then retries without the flag.
        let mut size: c_longlong = -1;
        let p = sqlite3_serialize(db, ptr::null(), &mut size, NOCOPY);
        assert!(p.is_null());

        let mut size: c_longlong = 0;
        let main = cs("main");
        let p = sqlite3_serialize(db, main.as_ptr(), &mut size, 0);
        assert!(!p.is_null(), "serialize main must produce an image");
        assert!(size > 0);

        // Wreck the live database, prove it is gone — with sqlite's words.
        assert_eq!(exec_rc(db, "DROP TABLE t"), SQLITE_OK);
        assert_ne!(exec_rc(db, "SELECT v FROM t"), SQLITE_OK);
        assert!(errmsg(db).contains("no such table"), "{}", errmsg(db));

        // Deserialize hands the buffer BACK under FREEONCLOSE — sqlite's
        // ownership contract, success or failure — so it must come from OUR
        // malloc. Copy the serialized image into one.
        let buf = sqlite3_malloc64(size as u64) as *mut u8;
        assert!(!buf.is_null());
        std::ptr::copy_nonoverlapping(p, buf, size as usize);
        sqlite3_free(p as *mut c_void);
        let rc = sqlite3_deserialize(db, main.as_ptr(), buf, size, size, FREEONCLOSE | RESIZEABLE);
        assert_eq!(rc, SQLITE_OK, "{}", errmsg(db));

        // The table is back, content included.
        let s = cs("SELECT v FROM t WHERE a = 1");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        let v = CStr::from_ptr(sqlite3_column_text(st, 0) as *const c_char).to_str().unwrap();
        assert_eq!(v, "x");
        sqlite3_finalize(st);

        // Bytes that are no database: sqlite's exact words, and (under
        // FREEONCLOSE) the buffer is consumed even on failure.
        let junk = sqlite3_malloc64(3) as *mut u8;
        std::ptr::copy_nonoverlapping([0u8, 1, 3].as_ptr(), junk, 3);
        let rc = sqlite3_deserialize(db, main.as_ptr(), junk, 3, 3, FREEONCLOSE);
        assert_eq!(rc, SQLITE_NOTADB);
        assert!(errmsg(db).contains("file is not a database"), "{}", errmsg(db));

        sqlite3_close(db);
    }
}

#[test]
fn deserialize_detaches_from_a_real_file_without_touching_it() {
    unsafe {
        let dir = common::scratch_base_str();
        let path = format!("{}/mpedb-deser-file-{}.db", dir.trim_end_matches('/'), std::process::id());
        let _ = std::fs::remove_file(&path);
        let cpath = cs(&path);
        let mut db: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(cpath.as_ptr(), &mut db), SQLITE_OK);
        assert_eq!(exec_rc(db, "CREATE TABLE keepme (k INTEGER)"), SQLITE_OK);
        assert_eq!(exec_rc(db, "INSERT INTO keepme VALUES (42)"), SQLITE_OK);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        // Serialize an EMPTY image from a second connection and adopt it.
        let other = open_memory();
        assert_eq!(exec_rc(other, "CREATE TABLE fresh (f INTEGER)"), SQLITE_OK);
        let mut size: c_longlong = 0;
        let img = sqlite3_serialize(other, ptr::null(), &mut size, 0);
        assert!(!img.is_null());
        let rc = sqlite3_deserialize(db, ptr::null(), img, size, size, FREEONCLOSE);
        assert_eq!(rc, SQLITE_OK, "{}", errmsg(db));
        sqlite3_close(other);

        // The connection now serves the IMAGE (fresh exists, keepme is gone)…
        assert_eq!(exec_rc(db, "INSERT INTO fresh VALUES (1)"), SQLITE_OK);
        assert_ne!(exec_rc(db, "SELECT k FROM keepme"), SQLITE_OK);
        sqlite3_close(db);

        // …and the user's FILE was never written: reopening it finds keepme
        // exactly as left. (mtime is a hint; the content read is the proof.)
        let mut db2: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(cpath.as_ptr(), &mut db2), SQLITE_OK);
        let s = cs("SELECT k FROM keepme");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db2, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(st, 0), 42);
        sqlite3_finalize(st);
        sqlite3_close(db2);
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        let _ = before <= after; // informational only — content above decides
        let _ = std::fs::remove_file(&path);
    }
}


/// §11's restart rule, repo-side: a source COMMIT mid-backup invalidates
/// the partial copy — the meter rewinds (sqlite's own semantics, measured:
/// journal [1, 1, 0]) and the destination carries the mid-backup row.
#[test]
fn a_mid_backup_source_commit_restarts_the_copy() {
    unsafe {
        let src = open_memory();
        assert_eq!(exec_rc(src, "CREATE TABLE foo (key INTEGER)"), SQLITE_OK);
        assert_eq!(exec_rc(src, "INSERT INTO foo VALUES (3)"), SQLITE_OK);
        assert_eq!(exec_rc(src, "INSERT INTO foo VALUES (4)"), SQLITE_OK);
        let dst = open_memory();
        let main = cs("main");
        let h = sqlite3_backup_init(dst, main.as_ptr(), src, main.as_ptr());
        assert!(!h.is_null());
        // (stock reports pagecount 0 before the first step; the shim knows
        // the geometry from init — a benign difference nothing reads.)
        // Step 1 of 2 (the sqlite-image geometry of foo+2 rows IS 2 pages).
        assert_eq!(sqlite3_backup_step(h, 1), SQLITE_OK);
        assert_eq!(sqlite3_backup_remaining(h), 1);
        assert_eq!(sqlite3_backup_pagecount(h), 2);
        // The source commits mid-backup…
        assert_eq!(exec_rc(src, "INSERT INTO foo VALUES (1001)"), SQLITE_OK);
        // …so the NEXT step restarts: remaining goes back to 1, not 0.
        assert_eq!(sqlite3_backup_step(h, 1), SQLITE_OK);
        assert_eq!(sqlite3_backup_remaining(h), 1);
        // And the copy completes with the new row IN the destination.
        assert_eq!(sqlite3_backup_step(h, 1), SQLITE_DONE);
        assert_eq!(sqlite3_backup_remaining(h), 0);
        assert_eq!(sqlite3_backup_finish(h), SQLITE_OK);
        let s = cs("SELECT key FROM foo WHERE key >= 1000");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(dst, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(st, 0), 1001);
        sqlite3_finalize(st);
        sqlite3_close(dst);
        sqlite3_close(src);
    }
}
