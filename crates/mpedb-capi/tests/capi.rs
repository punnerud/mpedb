//! Rust FFI tests: drive the exported sqlite3 C-API exactly as a C caller
//! would (raw pointers, 1-based binds, 0-based columns) and assert both the
//! result-code integers and the returned values.

use mpedb_sqlite3::*;
use std::ffi::{c_char, c_void, CStr, CString};
use std::os::raw::c_int;
use std::ptr;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// Open an ephemeral in-memory database (auto-deleted on close).
unsafe fn open_memory() -> *mut Sqlite3 {
    let mut db: *mut Sqlite3 = ptr::null_mut();
    let name = cs(":memory:");
    let rc = sqlite3_open(name.as_ptr(), &mut db);
    assert_eq!(rc, SQLITE_OK, "open :memory:");
    assert!(!db.is_null());
    db
}

unsafe fn exec(db: *mut Sqlite3, sql: &str) -> c_int {
    let s = cs(sql);
    sqlite3_exec(db, s.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

#[test]
fn open_create_insert_select_step() {
    unsafe {
        let db = open_memory();

        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, w REAL)"),
            SQLITE_OK
        );

        // Prepare + bind + step an INSERT.
        let mut st: *mut Stmt = ptr::null_mut();
        let sql = cs("INSERT INTO t (id, name, w) VALUES (?, ?, ?)");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_parameter_count(st), 3);
        assert_eq!(sqlite3_bind_int(st, 1, 7), SQLITE_OK);
        let nm = cs("alice");
        assert_eq!(
            sqlite3_bind_text(st, 2, nm.as_ptr(), -1, sqlite_transient()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_double(st, 3, 2.5), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        assert_eq!(sqlite3_changes(db), 1);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // A second row via exec.
        assert_eq!(exec(db, "INSERT INTO t (id, name) VALUES (3, 'bob')"), SQLITE_OK);

        // SELECT and read the columns.
        let mut q: *mut Stmt = ptr::null_mut();
        let qs = cs("SELECT id, name, w FROM t ORDER BY id");
        assert_eq!(
            sqlite3_prepare_v2(db, qs.as_ptr(), -1, &mut q, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_column_count(q), 3);
        assert_eq!(col_name(q, 0), "id");
        assert_eq!(col_name(q, 1), "name");

        // Rows come back ORDER BY id ascending: id=3 (bob, w NULL) first.
        assert_eq!(sqlite3_step(q), SQLITE_ROW);
        assert_eq!(sqlite3_column_type(q, 0), SQLITE_INTEGER);
        assert_eq!(sqlite3_column_int(q, 0), 3);
        assert_eq!(sqlite3_column_type(q, 1), SQLITE_TEXT);
        assert_eq!(col_text(q, 1), "bob");
        assert_eq!(sqlite3_column_type(q, 2), SQLITE_NULL);
        assert_eq!(sqlite3_column_bytes(q, 2), 0);

        // Second row: id=7, name=alice, w=2.5.
        assert_eq!(sqlite3_step(q), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(q, 0), 7);
        assert_eq!(col_text(q, 1), "alice");
        assert_eq!(sqlite3_column_type(q, 2), SQLITE_FLOAT);
        assert!((sqlite3_column_double(q, 2) - 2.5).abs() < 1e-9);

        assert_eq!(sqlite3_step(q), SQLITE_DONE);

        // Reset + re-step yields the rows again from the top.
        assert_eq!(sqlite3_reset(q), SQLITE_OK);
        assert_eq!(sqlite3_step(q), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(q, 0), 3);

        assert_eq!(sqlite3_finalize(q), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn constraint_error_maps_to_sqlite_constraint() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id) VALUES (1)"), SQLITE_OK);

        // Duplicate PK via exec -> SQLITE_CONSTRAINT with an errmsg.
        let mut errmsg: *mut c_char = ptr::null_mut();
        let s = cs("INSERT INTO t (id) VALUES (1)");
        let rc = sqlite3_exec(db, s.as_ptr(), None, ptr::null_mut(), &mut errmsg);
        assert_eq!(rc, SQLITE_CONSTRAINT, "duplicate PK is SQLITE_CONSTRAINT");
        assert_eq!(sqlite3_errcode(db), SQLITE_CONSTRAINT);
        assert_eq!(sqlite3_extended_errcode(db), SQLITE_CONSTRAINT_PRIMARYKEY);
        assert!(!errmsg.is_null());
        // errmsg is freeable with sqlite3_free.
        sqlite3_free(errmsg as *mut c_void);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn exec_callback_receives_rows() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, name) VALUES (1, 'x'), (2, 'y')"), SQLITE_OK);

        extern "C" fn cb(
            arg: *mut c_void,
            ncol: c_int,
            argv: *mut *mut c_char,
            _names: *mut *mut c_char,
        ) -> c_int {
            unsafe {
                let counter = &mut *(arg as *mut i32);
                assert_eq!(ncol, 2);
                // Second column is the name text.
                let name_ptr = *argv.add(1);
                let name = CStr::from_ptr(name_ptr).to_str().unwrap();
                assert!(name == "x" || name == "y");
                *counter += 1;
            }
            0
        }

        let mut count: i32 = 0;
        let s = cs("SELECT id, name FROM t ORDER BY id");
        let rc = sqlite3_exec(
            db,
            s.as_ptr(),
            Some(cb),
            &mut count as *mut i32 as *mut c_void,
            ptr::null_mut(),
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(count, 2);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn transactions_commit_and_rollback() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY)"), SQLITE_OK);

        // Rolled-back work does not persist.
        assert_eq!(sqlite3_get_autocommit(db), 1);
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(sqlite3_get_autocommit(db), 0);
        assert_eq!(exec(db, "INSERT INTO t (id) VALUES (1)"), SQLITE_OK);
        assert_eq!(exec(db, "ROLLBACK"), SQLITE_OK);
        assert_eq!(sqlite3_get_autocommit(db), 1);
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t"), 0);

        // Committed work persists.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id) VALUES (2)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id) VALUES (3)"), SQLITE_OK);
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t"), 2);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn transaction_control_via_prepare_step() {
    // CPython's sqlite3 drives implicit transactions by preparing+stepping
    // "BEGIN"/"COMMIT" statements (not sqlite3_exec), so that path must work.
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY)"), SQLITE_OK);

        let step_sql = |sql: &str| {
            let s = cs(sql);
            let mut st: *mut Stmt = ptr::null_mut();
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK,
                "prepare {sql}"
            );
            let rc = sqlite3_step(st);
            sqlite3_finalize(st);
            rc
        };

        assert_eq!(step_sql("BEGIN"), SQLITE_DONE);
        assert_eq!(sqlite3_get_autocommit(db), 0);
        assert_eq!(step_sql("INSERT INTO t (id) VALUES (5)"), SQLITE_DONE);
        assert_eq!(step_sql("COMMIT"), SQLITE_DONE);
        assert_eq!(sqlite3_get_autocommit(db), 1);
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t"), 1);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn errors_and_misuse() {
    unsafe {
        let db = open_memory();

        // Syntax error surfaces at prepare.
        let mut st: *mut Stmt = ptr::null_mut();
        let bad = cs("SELCT nonsense");
        let rc = sqlite3_prepare_v2(db, bad.as_ptr(), -1, &mut st, ptr::null_mut());
        assert_eq!(rc, SQLITE_ERROR);
        assert!(st.is_null());
        let msg = CStr::from_ptr(sqlite3_errmsg(db)).to_str().unwrap();
        assert!(!msg.is_empty() && msg != "not an error");

        // NULL statement pointer is misuse.
        assert_eq!(sqlite3_step(ptr::null_mut()), SQLITE_MISUSE);

        // Blank SQL prepares to a NULL stmt with OK.
        let mut st2: *mut Stmt = ptr::null_mut();
        let blank = cs("   -- just a comment\n");
        assert_eq!(
            sqlite3_prepare_v2(db, blank.as_ptr(), -1, &mut st2, ptr::null_mut()),
            SQLITE_OK
        );
        assert!(st2.is_null());

        // Out-of-range bind index.
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        let q = cs("SELECT * FROM t WHERE id = ?");
        let mut qp: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut qp, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_int(qp, 2, 9), SQLITE_RANGE);
        assert_eq!(sqlite3_bind_int(qp, 1, 9), SQLITE_OK);
        sqlite3_finalize(qp);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn prepare_tail_and_multi_exec() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY)"), SQLITE_OK);

        // exec runs multiple ;-separated statements.
        assert_eq!(
            exec(db, "INSERT INTO t (id) VALUES (1); INSERT INTO t (id) VALUES (2);"),
            SQLITE_OK
        );
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t"), 2);

        // prepare_v2 compiles only the first statement and reports the tail.
        let script = cs("SELECT 1; SELECT 2");
        let mut st: *mut Stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        assert_eq!(
            sqlite3_prepare_v2(db, script.as_ptr(), -1, &mut st, &mut tail),
            SQLITE_OK
        );
        assert!(!st.is_null());
        assert!(!tail.is_null());
        let tail_str = CStr::from_ptr(tail).to_str().unwrap();
        assert_eq!(tail_str.trim(), "SELECT 2");
        sqlite3_finalize(st);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn file_backed_persists_across_reopen() {
    unsafe {
        let path = format!("/dev/shm/mpedb-capi-persist-{}.mpedb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let cpath = cs(&path);

        let mut db: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(cpath.as_ptr(), &mut db), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, v) VALUES (42, 'hi')"), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        // Reopen the same file: the row is still there.
        let mut db2: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(cpath.as_ptr(), &mut db2), SQLITE_OK);
        assert_eq!(scalar_count(db2, "SELECT COUNT(*) FROM t"), 1);
        let mut q: *mut Stmt = ptr::null_mut();
        let qs = cs("SELECT v FROM t WHERE id = 42");
        assert_eq!(
            sqlite3_prepare_v2(db2, qs.as_ptr(), -1, &mut q, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(q), SQLITE_ROW);
        assert_eq!(col_text(q, 0), "hi");
        sqlite3_finalize(q);
        assert_eq!(sqlite3_close(db2), SQLITE_OK);

        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn open_v2_no_create_flag_fails_for_missing_file() {
    unsafe {
        let path = format!("/dev/shm/mpedb-capi-missing-{}.mpedb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let cpath = cs(&path);
        let mut db: *mut Sqlite3 = ptr::null_mut();
        let rc = sqlite3_open_v2(
            cpath.as_ptr(),
            &mut db,
            SQLITE_OPEN_READWRITE, // no CREATE
            ptr::null(),
        );
        assert_eq!(rc, SQLITE_CANTOPEN);
        assert!(db.is_null());
    }
}

#[test]
fn version_and_alloc() {
    unsafe {
        let v = CStr::from_ptr(sqlite3_libversion()).to_str().unwrap();
        assert!(v.starts_with("3."));
        assert!(sqlite3_libversion_number() >= 3_000_000);
        let p = sqlite3_malloc(16);
        assert!(!p.is_null());
        sqlite3_free(p);
        sqlite3_free(ptr::null_mut()); // free(NULL) is safe
    }
}

// ---- helpers -------------------------------------------------------------

fn sqlite_transient() -> *mut c_void {
    SQLITE_TRANSIENT as *mut c_void
}

unsafe fn col_name(st: *mut Stmt, i: c_int) -> String {
    CStr::from_ptr(sqlite3_column_name(st, i)).to_str().unwrap().to_string()
}

unsafe fn col_text(st: *mut Stmt, i: c_int) -> String {
    let p = sqlite3_column_text(st, i);
    assert!(!p.is_null());
    CStr::from_ptr(p as *const c_char).to_str().unwrap().to_string()
}

unsafe fn scalar_count(db: *mut Sqlite3, sql: &str) -> i64 {
    let s = cs(sql);
    let mut st: *mut Stmt = ptr::null_mut();
    assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
    assert_eq!(sqlite3_step(st), SQLITE_ROW);
    let n = sqlite3_column_int64(st, 0);
    sqlite3_finalize(st);
    n
}
