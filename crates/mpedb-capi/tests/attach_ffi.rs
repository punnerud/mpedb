//! #51 FFI: `ATTACH DATABASE` through the sqlite3 C-API, with the real wire
//! shapes Python/Django use — `sqlite3_open` on a second file, `ATTACH` via
//! exec AND via prepare/step, cross-file SELECT through prepare/bind/step,
//! `PRAGMA database_list`, and the v1 refusals surfacing as clean SQLITE
//! errors (never a differing answer).

use mpedb_sqlite3::*;
use std::ffi::{c_char, c_void, CStr, CString};
use std::os::raw::c_int;
use std::ptr;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn exec(db: *mut Sqlite3, sql: &str) -> c_int {
    let s = cs(sql);
    sqlite3_exec(db, s.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn errmsg(db: *mut Sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy().into_owned()
}

unsafe fn col_text(st: *mut Stmt, i: c_int) -> String {
    let p = sqlite3_column_text(st, i);
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p as *const c_char).to_string_lossy().into_owned()
    }
}

fn sqlite_transient() -> *mut c_void {
    -1isize as *mut c_void
}

struct Files(Vec<String>);
impl Drop for Files {
    fn drop(&mut self) {
        for f in &self.0 {
            let _ = std::fs::remove_file(f);
            let _ = std::fs::remove_file(format!("{f}-wal"));
        }
    }
}

fn paths(tag: &str) -> (Files, String, String) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let main = format!("{dir}/mpedb-capi-att-{tag}-{}-main.db", std::process::id());
    let other = format!("{dir}/mpedb-capi-att-{tag}-{}-other.db", std::process::id());
    let _ = std::fs::remove_file(&main);
    let _ = std::fs::remove_file(&other);
    (Files(vec![main.clone(), other.clone()]), main, other)
}

unsafe fn open_file(path: &str) -> *mut Sqlite3 {
    let mut db: *mut Sqlite3 = ptr::null_mut();
    let name = cs(path);
    assert_eq!(sqlite3_open(name.as_ptr(), &mut db), SQLITE_OK, "open {path}");
    db
}

/// The whole Python/Django wire flow, end to end.
#[test]
fn attach_cross_select_through_the_c_abi() {
    unsafe {
        let (_guard, main_path, other_path) = paths("flow");

        // Build the second database with its own sqlite3_open connection.
        let other = open_file(&other_path);
        assert_eq!(
            exec(other, "CREATE TABLE u (x INTEGER PRIMARY KEY, y INT)"),
            SQLITE_OK
        );
        assert_eq!(exec(other, "INSERT INTO u (x, y) VALUES (1, 10)"), SQLITE_OK);
        assert_eq!(exec(other, "INSERT INTO u (x, y) VALUES (2, 20)"), SQLITE_OK);
        assert_eq!(sqlite3_close(other), SQLITE_OK);

        // Main connection + ATTACH via plain exec.
        let db = open_file(&main_path);
        assert_eq!(
            exec(db, "CREATE TABLE t (a INTEGER PRIMARY KEY, tag TEXT)"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "INSERT INTO t (a, tag) VALUES (1, 'one')"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (a, tag) VALUES (2, 'two')"), SQLITE_OK);
        let attach = format!("ATTACH DATABASE '{other_path}' AS other");
        assert_eq!(exec(db, &attach), SQLITE_OK, "{}", errmsg(db));

        // Cross-file JOIN through prepare/step, with a bound parameter.
        let mut st: *mut Stmt = ptr::null_mut();
        let sql = cs("SELECT t.tag, u.y FROM t JOIN other.u ON u.x = t.a WHERE u.y > ? ORDER BY t.a");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );
        assert_eq!(sqlite3_bind_int(st, 1, 5), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(col_text(st, 0), "one");
        assert_eq!(sqlite3_column_int(st, 1), 10);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(col_text(st, 0), "two");
        assert_eq!(sqlite3_column_int(st, 1), 20);
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // Reset + re-step re-executes against a FRESH member snapshot.
        let mut agg: *mut Stmt = ptr::null_mut();
        let sql = cs("SELECT count(*) FROM other.u");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut agg, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(agg), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(agg, 0), 2);
        assert_eq!(sqlite3_step(agg), SQLITE_DONE);
        assert_eq!(sqlite3_reset(agg), SQLITE_OK);

        // A separate connection writes the attached FILE directly (the v1 way
        // to write it) — the re-stepped statement must see the new row.
        let side = open_file(&other_path);
        assert_eq!(exec(side, "INSERT INTO u (x, y) VALUES (3, 30)"), SQLITE_OK);
        assert_eq!(sqlite3_close(side), SQLITE_OK);
        assert_eq!(sqlite3_step(agg), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(agg, 0), 3);
        assert_eq!(sqlite3_finalize(agg), SQLITE_OK);

        // PRAGMA database_list: seq 0 = main + its path; attached at seq 2.
        let mut pl: *mut Stmt = ptr::null_mut();
        let sql = cs("PRAGMA database_list");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut pl, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(pl), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(pl, 0), 0);
        assert_eq!(col_text(pl, 1), "main");
        assert_eq!(col_text(pl, 2), main_path);
        assert_eq!(sqlite3_step(pl), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(pl, 0), 2);
        assert_eq!(col_text(pl, 1), "other");
        assert_eq!(col_text(pl, 2), other_path);
        assert_eq!(sqlite3_step(pl), SQLITE_DONE);
        assert_eq!(sqlite3_finalize(pl), SQLITE_OK);

        // The v1 refusal, on the wire: a write to the attached db errors
        // with a clean message and writes nothing.
        assert_eq!(exec(db, "INSERT INTO other.u (x, y) VALUES (9, 9)"), SQLITE_ERROR);
        assert!(
            errmsg(db).contains("cross-file writes"),
            "refusal message: {}",
            errmsg(db)
        );

        // ATTACH through PREPARE/STEP (CPython's cursor.execute path).
        let (_g2, _m2, third_path) = paths("third");
        let third = open_file(&third_path);
        assert_eq!(exec(third, "CREATE TABLE w (k INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(sqlite3_close(third), SQLITE_OK);
        let mut at: *mut Stmt = ptr::null_mut();
        let sql = cs(&format!("ATTACH DATABASE '{third_path}' AS third"));
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut at, ptr::null_mut()),
            SQLITE_OK,
            "ATTACH must prepare (deferred validation): {}",
            errmsg(db)
        );
        assert_eq!(sqlite3_step(at), SQLITE_DONE);
        assert_eq!(sqlite3_finalize(at), SQLITE_OK);
        assert_eq!(exec(db, "SELECT count(*) FROM third.w"), SQLITE_OK);

        // DETACH, and the name is gone.
        assert_eq!(exec(db, "DETACH DATABASE third"), SQLITE_OK);
        assert_eq!(exec(db, "SELECT count(*) FROM third.w"), SQLITE_ERROR);
        assert!(errmsg(db).contains("no such table"), "{}", errmsg(db));

        // Bind a TEXT param through a cross plan for good measure.
        let mut tp: *mut Stmt = ptr::null_mut();
        let sql = cs("SELECT a FROM main.t WHERE tag = ?");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut tp, ptr::null_mut()),
            SQLITE_OK
        );
        let v = cs("two");
        assert_eq!(sqlite3_bind_text(tp, 1, v.as_ptr(), -1, sqlite_transient()), SQLITE_OK);
        assert_eq!(sqlite3_step(tp), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(tp, 0), 2);
        assert_eq!(sqlite3_step(tp), SQLITE_DONE);
        assert_eq!(sqlite3_finalize(tp), SQLITE_OK);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
