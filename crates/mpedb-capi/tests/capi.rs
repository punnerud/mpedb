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
fn named_parameters_bind_by_index() {
    // The shim rewrites `:name`/`@name`/`$name` to mpedb's numbered `$K` before
    // the engine parses, and answers `bind_parameter_count`/`_name`/`_index`
    // from the maps — so an unmodified named-param consumer (CPython's sqlite3)
    // works. This drives the exact calls CPython makes.
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "INSERT INTO t VALUES (1, 'alice', 30)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t VALUES (2, 'bob', 40)"), SQLITE_OK);

        // A three-sigil, name-reusing statement. `:lo` appears twice → one param.
        let sql = cs("SELECT id FROM t WHERE age >= :lo AND age <= @hi AND name = $nm AND :lo > 0");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        // Three distinct names → count 3 (the repeat of :lo shares number 1).
        assert_eq!(sqlite3_bind_parameter_count(st), 3);
        assert_eq!(param_name(st, 1), Some(":lo".to_string()));
        assert_eq!(param_name(st, 2), Some("@hi".to_string()));
        assert_eq!(param_name(st, 3), Some("$nm".to_string()));
        // bind_parameter_index round-trips the spelling (sigil included); a wrong
        // spelling or a missing name → 0.
        assert_eq!(param_index(st, ":lo"), 1);
        assert_eq!(param_index(st, "@hi"), 2);
        assert_eq!(param_index(st, "$nm"), 3);
        assert_eq!(param_index(st, "lo"), 0); // no sigil
        assert_eq!(param_index(st, ":missing"), 0);

        // Bind exactly as CPython does: look each name up by index, bind there.
        assert_eq!(sqlite3_bind_int(st, param_index(st, ":lo"), 30), SQLITE_OK);
        assert_eq!(sqlite3_bind_int(st, param_index(st, "@hi"), 35), SQLITE_OK);
        let nm = cs("alice");
        assert_eq!(
            sqlite3_bind_text(st, param_index(st, "$nm"), nm.as_ptr(), -1, sqlite_transient()),
            SQLITE_OK
        );

        // alice (age 30) matches 30<=30<=35 and :lo(30)>0; bob (40) does not.
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(st, 0), 1);
        assert_eq!(sqlite3_step(st), SQLITE_DONE);

        // Rebind the reused name once and it applies at BOTH occurrences: set
        // :lo above every age so the WHERE excludes every row.
        assert_eq!(sqlite3_reset(st), SQLITE_OK);
        assert_eq!(sqlite3_bind_int(st, 1, 999), SQLITE_OK); // :lo
        assert_eq!(sqlite3_step(st), SQLITE_DONE); // no rows: 999 > every age
        sqlite3_finalize(st);

        // Out-of-range index on a named statement is still SQLITE_RANGE.
        let q = cs("SELECT :only");
        let mut qp: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut qp, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_parameter_count(qp), 1);
        assert_eq!(sqlite3_bind_int(qp, 2, 0), SQLITE_RANGE);
        sqlite3_finalize(qp);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn named_and_positional_mixed_numbering() {
    // Named and `?` share one numbering space (sqlite semantics). Bind by number
    // and read the values straight back to prove each slot maps to the right one.
    unsafe {
        let db = open_memory();
        // `?`=1, :a=2, `?`=3, :a reused=2  → count 3, columns echo the binds.
        let sql = cs("SELECT ?, :a, ?, :a");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_parameter_count(st), 3);
        assert_eq!(param_name(st, 1), None); // anonymous `?`
        assert_eq!(param_name(st, 2), Some(":a".to_string()));
        assert_eq!(param_name(st, 3), None);
        assert_eq!(param_index(st, ":a"), 2);

        assert_eq!(sqlite3_bind_int(st, 1, 100), SQLITE_OK);
        assert_eq!(sqlite3_bind_int(st, 2, 200), SQLITE_OK); // :a
        assert_eq!(sqlite3_bind_int(st, 3, 300), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(st, 0), 100); // ?  -> 1
        assert_eq!(sqlite3_column_int(st, 1), 200); // :a -> 2
        assert_eq!(sqlite3_column_int(st, 2), 300); // ?  -> 3
        assert_eq!(sqlite3_column_int(st, 3), 200); // :a reused -> 2
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        sqlite3_finalize(st);
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
fn last_insert_rowid_via_facade() {
    // The rowid an INSERT assigns/uses on a rowid-alias (single-column INTEGER
    // PRIMARY KEY) table is surfaced by the engine's facade hook
    // (`mpedb::take_last_insert_rowid`, drained per statement in `exec_one`) —
    // no RETURNING rewrite in the shim.
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)"), SQLITE_OK);

        // Omitted rowid: auto-assigned; last_insert_rowid reports it.
        assert_eq!(exec(db, "INSERT INTO t (name) VALUES ('a')"), SQLITE_OK);
        assert_eq!(sqlite3_last_insert_rowid(db), 1);
        assert_eq!(sqlite3_changes(db), 1);

        // Explicit id.
        assert_eq!(exec(db, "INSERT INTO t (id, name) VALUES (42, 'b')"), SQLITE_OK);
        assert_eq!(sqlite3_last_insert_rowid(db), 42);

        // Next auto id continues after the max.
        assert_eq!(exec(db, "INSERT INTO t (name) VALUES ('c')"), SQLITE_OK);
        assert_eq!(sqlite3_last_insert_rowid(db), 43);

        // A plain INSERT yields DONE with no result columns (the facade records
        // the rowid out-of-band; nothing leaks into the caller's result set).
        let mut st: *mut Stmt = ptr::null_mut();
        let ins = cs("INSERT INTO t (name) VALUES ('d')");
        assert_eq!(sqlite3_prepare_v2(db, ins.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        assert_eq!(sqlite3_column_count(st), 0);
        sqlite3_finalize(st);
        assert_eq!(sqlite3_last_insert_rowid(db), 44);

        // The table really has 4 rows (no double-insert).
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t"), 4);

        // A composite-PK table is not a rowid alias: rowid stays as it was.
        assert_eq!(
            exec(db, "CREATE TABLE c (a INT, b INT, v TEXT, PRIMARY KEY (a, b))"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "INSERT INTO c (a, b, v) VALUES (1, 2, 'x')"), SQLITE_OK);
        assert_eq!(sqlite3_last_insert_rowid(db), 44); // unchanged

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn pragma_table_info_and_setup_pragmas() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL, w REAL)"),
            SQLITE_OK
        );

        // Setup pragmas consumers issue must not error.
        assert_eq!(exec(db, "PRAGMA foreign_keys = ON"), SQLITE_OK);
        assert_eq!(exec(db, "PRAGMA synchronous = NORMAL"), SQLITE_OK);

        // PRAGMA table_info(t) — column metadata, readable before step.
        let mut st: *mut Stmt = ptr::null_mut();
        let q = cs("PRAGMA table_info(t)");
        assert_eq!(sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_column_count(st), 6);
        assert_eq!(col_name(st, 1), "name");
        assert_eq!(col_name(st, 5), "pk");

        // Row 0: id INTEGER, notnull=1 (pk), pk=1.
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(st, 0), 0);
        assert_eq!(col_text(st, 1), "id");
        assert_eq!(col_text(st, 2), "INTEGER");
        assert_eq!(sqlite3_column_int(st, 3), 1); // notnull (pk implies not null)
        assert_eq!(sqlite3_column_int(st, 5), 1); // pk position
        // Row 1: name TEXT NOT NULL, not a pk.
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(col_text(st, 1), "name");
        assert_eq!(col_text(st, 2), "TEXT");
        assert_eq!(sqlite3_column_int(st, 3), 1);
        assert_eq!(sqlite3_column_int(st, 5), 0);
        // Row 2: w REAL nullable.
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(col_text(st, 1), "w");
        assert_eq!(col_text(st, 2), "REAL");
        assert_eq!(sqlite3_column_int(st, 3), 0);
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        sqlite3_finalize(st);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn sqlite_master_introspection() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE alpha (id INTEGER PRIMARY KEY, v TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE beta (id INTEGER PRIMARY KEY)"), SQLITE_OK);

        // Django/tooling's canonical "list tables" query. The internal bootstrap
        // table must NOT appear.
        let names = collect_text_col(
            db,
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        );
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);

        // type IN (...) form.
        let names2 = collect_text_col(
            db,
            "SELECT name FROM sqlite_master WHERE type in ('table','view') ORDER BY name",
        );
        assert_eq!(names2, vec!["alpha".to_string(), "beta".to_string()]);

        // ORDER BY must actually sort (create out of name order so creation
        // order != sorted order — guards the "ORDER BY name" parse).
        assert_eq!(exec(db, "CREATE TABLE aardvark (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        let asc = collect_text_col(db, "SELECT name FROM sqlite_master ORDER BY name");
        assert_eq!(asc, vec!["aardvark".to_string(), "alpha".to_string(), "beta".to_string()]);
        let desc = collect_text_col(db, "SELECT name FROM sqlite_master ORDER BY name DESC");
        assert_eq!(desc, vec!["beta".to_string(), "alpha".to_string(), "aardvark".to_string()]);

        // Fetch a table's reconstructed DDL.
        let ddl = collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name='alpha'");
        assert_eq!(ddl.len(), 1);
        assert!(ddl[0].contains("CREATE TABLE") && ddl[0].contains("alpha"), "{}", ddl[0]);

        // count(*) form (alpha, beta, aardvark = 3 user tables).
        let mut st: *mut Stmt = ptr::null_mut();
        let q = cs("SELECT count(*) FROM sqlite_master");
        assert_eq!(sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(st, 0), 3);
        sqlite3_finalize(st);

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

// ---- host scalar UDFs (design/DESIGN-UDF.md stage 1) ----------------------

/// `plus1(x) = x + 1` — the integer round trip through `sqlite3_value_int64`
/// and `sqlite3_result_int64`.
unsafe extern "C" fn udf_plus1(ctx: *mut c_void, argc: c_int, argv: *mut *mut c_void) {
    assert_eq!(argc, 1);
    let x = sqlite3_value_int64(*argv);
    sqlite3_result_int64(ctx, x + 1);
}

/// `addk(x, k) = x + k` — a 2-argument function reading both `argv` slots.
unsafe extern "C" fn udf_addk(ctx: *mut c_void, argc: c_int, argv: *mut *mut c_void) {
    assert_eq!(argc, 2);
    let x = sqlite3_value_int64(*argv);
    let k = sqlite3_value_int64(*argv.offset(1));
    sqlite3_result_int64(ctx, x + k);
}

/// `shout(s)` — uppercases text, exercising `sqlite3_value_text`/`_bytes` in
/// and `sqlite3_result_text` out.
unsafe extern "C" fn udf_shout(ctx: *mut c_void, argc: c_int, argv: *mut *mut c_void) {
    assert_eq!(argc, 1);
    assert_eq!(sqlite3_value_type(*argv), SQLITE_TEXT);
    let p = sqlite3_value_text(*argv);
    let n = sqlite3_value_bytes(*argv);
    let bytes = std::slice::from_raw_parts(p, n as usize);
    let up = String::from_utf8_lossy(bytes).to_uppercase();
    let c = cs(&up);
    sqlite3_result_text(ctx, c.as_ptr(), -1, sqlite_transient());
}

/// A UDF that reports `pApp` back through `sqlite3_user_data`, proving the
/// registration's application pointer reaches the callback.
unsafe extern "C" fn udf_appval(ctx: *mut c_void, _argc: c_int, _argv: *mut *mut c_void) {
    let p = sqlite3_user_data(ctx) as usize;
    sqlite3_result_int64(ctx, p as i64);
}

/// A UDF that always raises, to check `sqlite3_result_error` surfaces as a
/// statement error rather than a silent NULL.
unsafe extern "C" fn udf_boom(ctx: *mut c_void, _argc: c_int, _argv: *mut *mut c_void) {
    let m = cs("boom from the host");
    sqlite3_result_error(ctx, m.as_ptr(), -1);
}

fn fnptr(f: unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void)) -> *mut c_void {
    f as *const () as *mut c_void
}

#[test]
fn create_function_scalar_dispatch() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(db, "INSERT INTO t (id, name) VALUES (1,'ann'),(2,'bo'),(3,'cy')"),
            SQLITE_OK
        );

        // An unregistered function is a prepare-time error.
        let mut st: *mut Stmt = ptr::null_mut();
        let bad = cs("SELECT plus1(id) FROM t");
        assert_ne!(
            sqlite3_prepare_v2(db, bad.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK,
            "an unregistered function must not prepare"
        );

        // Register the scalars. `_v2` and the older arity share one impl.
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("plus1").as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                fnptr(udf_plus1),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_create_function(
                db,
                cs("addk").as_ptr(),
                2,
                SQLITE_UTF8,
                ptr::null_mut(),
                fnptr(udf_addk),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("shout").as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                fnptr(udf_shout),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );

        // 1. In the SELECT list.
        let mut q: *mut Stmt = ptr::null_mut();
        let qs = cs("SELECT plus1(id) FROM t ORDER BY id");
        assert_eq!(
            sqlite3_prepare_v2(db, qs.as_ptr(), -1, &mut q, ptr::null_mut()),
            SQLITE_OK
        );
        let mut got = Vec::new();
        while sqlite3_step(q) == SQLITE_ROW {
            got.push(sqlite3_column_int64(q, 0));
        }
        assert_eq!(got, vec![2, 3, 4]);
        sqlite3_finalize(q);

        // 2. In the WHERE clause.
        assert_eq!(scalar_count(db, "SELECT id FROM t WHERE plus1(id) = 3"), 2);

        // 3. With a bound parameter.
        let mut p: *mut Stmt = ptr::null_mut();
        let ps = cs("SELECT addk(id, ?) FROM t WHERE id = 2");
        assert_eq!(
            sqlite3_prepare_v2(db, ps.as_ptr(), -1, &mut p, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_int(p, 1, 40), SQLITE_OK);
        assert_eq!(sqlite3_step(p), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(p, 0), 42);
        sqlite3_finalize(p);

        // 4. A text function, round-tripping value_text -> result_text.
        let mut s: *mut Stmt = ptr::null_mut();
        let ss = cs("SELECT shout(name) FROM t ORDER BY id");
        assert_eq!(
            sqlite3_prepare_v2(db, ss.as_ptr(), -1, &mut s, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(s), SQLITE_ROW);
        assert_eq!(col_text(s, 0), "ANN");
        sqlite3_finalize(s);

        // 5. `sqlite3_user_data` hands the registration's pApp to the callback.
        let app = 0x5eed_usize as *mut c_void;
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("appval").as_ptr(),
                0,
                SQLITE_UTF8,
                app,
                fnptr(udf_appval),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(scalar_count(db, "SELECT appval()"), 0x5eed);

        // 6. `sqlite3_result_error` surfaces as a statement error.
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("boom").as_ptr(),
                0,
                SQLITE_UTF8,
                ptr::null_mut(),
                fnptr(udf_boom),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        let mut b: *mut Stmt = ptr::null_mut();
        let bs = cs("SELECT boom()");
        assert_eq!(
            sqlite3_prepare_v2(db, bs.as_ptr(), -1, &mut b, ptr::null_mut()),
            SQLITE_OK
        );
        assert_ne!(sqlite3_step(b), SQLITE_ROW, "a raising UDF must not yield a row");
        sqlite3_finalize(b);

        // 7. HALF an aggregate (xStep without xFinal) is a misuse, not a
        //    registration — sqlite requires the pair.
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("halfagg").as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                ptr::null_mut(),
                fnptr(udf_plus1), // xStep present
                ptr::null_mut(),  // xFinal missing
                ptr::null_mut(),
            ),
            SQLITE_MISUSE,
            "an aggregate needs both xStep and xFinal"
        );

        // 8. xFunc == NULL deletes the registration: the name is unknown again.
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("plus1").as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        let mut g: *mut Stmt = ptr::null_mut();
        let gs = cs("SELECT plus1(id) FROM t");
        assert_ne!(
            sqlite3_prepare_v2(db, gs.as_ptr(), -1, &mut g, ptr::null_mut()),
            SQLITE_OK,
            "a deleted function must be unknown again"
        );

        sqlite3_close(db);
    }
}

// ---- host AGGREGATE UDFs (design/DESIGN-UDF.md stage 2) --------------------

/// The struct `mysum`'s `xStep`/`xFinal` keep in the aggregate context. Zeroed
/// by `sqlite3_aggregate_context` on first use, which is exactly what makes
/// `{0, 0}` a valid empty accumulator — sqlite's contract, and what Django's
/// aggregates assume.
#[repr(C)]
#[derive(Debug)]
struct MySumCtx {
    total: i64,
    rows: i64,
}

/// The `pApp` every `mysum` registration carries: a per-row bump the callbacks
/// read back through `sqlite3_user_data`, so a broken `user_data` shows up as a
/// wrong SUM rather than passing silently.
static MYSUM_BUMP: i64 = 100;

fn mysum_app() -> *mut c_void {
    &MYSUM_BUMP as *const i64 as *mut c_void
}

unsafe extern "C" fn agg_step(ctx: *mut c_void, argc: c_int, argv: *mut *mut c_void) {
    assert_eq!(argc, 1);
    let app = sqlite3_user_data(ctx);
    assert!(!app.is_null(), "sqlite3_user_data must reach xStep");
    let bump = *(app as *const i64);
    let p = sqlite3_aggregate_context(ctx, std::mem::size_of::<MySumCtx>() as c_int)
        as *mut MySumCtx;
    assert!(!p.is_null(), "aggregate_context(n>0) must allocate");
    // The SAME pointer on every step of this aggregation, and zeroed on the
    // first — both asserted implicitly by the totals the test checks.
    (*p).total += sqlite3_value_int64(*argv) + bump;
    (*p).rows += 1;
}

unsafe extern "C" fn agg_final(ctx: *mut c_void) {
    let app = sqlite3_user_data(ctx);
    assert!(!app.is_null(), "sqlite3_user_data must reach xFinal");
    // n <= 0: never allocate. NULL here means the group was never stepped, which
    // is how sqlite (and Django) recognize an empty aggregation.
    let p = sqlite3_aggregate_context(ctx, 0) as *mut MySumCtx;
    if p.is_null() {
        sqlite3_result_null(ctx);
        return;
    }
    sqlite3_result_int64(ctx, (*p).total * 10 + (*p).rows);
}

fn fnptr1(f: unsafe extern "C" fn(*mut c_void)) -> *mut c_void {
    f as *const () as *mut c_void
}

/// A C aggregate registered through `sqlite3_create_function_v2`: bare, grouped,
/// over an empty set, and with `pApp` visible in both callbacks.
#[test]
fn create_function_aggregate_dispatch() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, grp INTEGER)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(db, "INSERT INTO t (id, grp) VALUES (1,0),(2,1),(3,0),(4,1)"),
            SQLITE_OK
        );

        // Unregistered → prepare-time error, not a silent NULL.
        let mut bad_st: *mut Stmt = ptr::null_mut();
        let bad = cs("SELECT mysum(id) FROM t");
        assert_ne!(
            sqlite3_prepare_v2(db, bad.as_ptr(), -1, &mut bad_st, ptr::null_mut()),
            SQLITE_OK,
            "an unregistered aggregate must not prepare"
        );

        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("mysum").as_ptr(),
                1,
                SQLITE_UTF8,
                mysum_app(),
                ptr::null_mut(), // xFunc NULL => aggregate
                fnptr(agg_step),
                fnptr1(agg_final),
                ptr::null_mut(),
            ),
            SQLITE_OK,
            "an aggregate registration must succeed (stage 2)"
        );

        // 1. Bare over the whole table: (1+2+3+4) + 4*100 = 410, 4 rows.
        assert_eq!(scalar_count(db, "SELECT mysum(id) FROM t"), 410 * 10 + 4);

        // 2. GROUP BY: grp 0 = {1,3} → 204, 2 rows; grp 1 = {2,4} → 206, 2 rows.
        //    Each group gets its OWN aggregate context — a shared one would
        //    show up here as the whole-table total.
        let mut q: *mut Stmt = ptr::null_mut();
        let qs = cs("SELECT mysum(id) FROM t GROUP BY grp ORDER BY grp");
        assert_eq!(
            sqlite3_prepare_v2(db, qs.as_ptr(), -1, &mut q, ptr::null_mut()),
            SQLITE_OK
        );
        let mut got = Vec::new();
        while sqlite3_step(q) == SQLITE_ROW {
            got.push(sqlite3_column_int64(q, 0));
        }
        sqlite3_finalize(q);
        assert_eq!(got, vec![204 * 10 + 2, 206 * 10 + 2]);

        // 3. An EMPTY set still yields ONE row, and `xFinal` on a never-stepped
        //    context returns NULL (Django's STDDEV of no rows).
        let mut e: *mut Stmt = ptr::null_mut();
        let es = cs("SELECT mysum(id) FROM t WHERE id > 100");
        assert_eq!(
            sqlite3_prepare_v2(db, es.as_ptr(), -1, &mut e, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(e), SQLITE_ROW, "an empty aggregation still yields a row");
        assert_eq!(sqlite3_column_type(e, 0), SQLITE_NULL);
        assert_ne!(sqlite3_step(e), SQLITE_ROW, "exactly one row");
        sqlite3_finalize(e);

        // 4. FILTER (WHERE …) narrows a host aggregate exactly as a built-in:
        //    {3,4} → 7 + 200 = 207, 2 rows.
        assert_eq!(
            scalar_count(db, "SELECT mysum(id) FILTER (WHERE id > 2) FROM t"),
            207 * 10 + 2
        );

        // 5. Deleting the registration (all callbacks NULL) makes the name
        //    unknown again — the aggregate registry is cleared, not just the
        //    scalar one.
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("mysum").as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        let mut g: *mut Stmt = ptr::null_mut();
        let gs = cs("SELECT mysum(id) FROM t");
        assert_ne!(
            sqlite3_prepare_v2(db, gs.as_ptr(), -1, &mut g, ptr::null_mut()),
            SQLITE_OK,
            "a deleted aggregate must be unknown again"
        );

        sqlite3_close(db);
    }
}

// ---- host UDFs on the WRITE path (design/DESIGN-UDF.md §Write path) -------

/// The shape CPython's `sqlite3` produces: the first DML opens an implicit
/// transaction, and every later statement — reads included — runs INSIDE it,
/// with no intervening `commit()`. A UDF called there must resolve exactly as
/// it does in autocommit; it used to fail with an internal error.
#[test]
fn udf_inside_an_open_transaction() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)"),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("plus1").as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                fnptr(udf_plus1),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("mysum").as_ptr(),
                1,
                SQLITE_UTF8,
                mysum_app(),
                ptr::null_mut(),
                fnptr(agg_step),
                fnptr1(agg_final),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );

        // BEGIN + DML: from here on the connection has an open WriteSession,
        // which is where CPython leaves it after the first execute().
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, n) VALUES (1,10),(2,20)"), SQLITE_OK);

        // 1. a SCALAR UDF in a read, inside the transaction, no commit between
        assert_eq!(scalar_count(db, "SELECT plus1(n) FROM t WHERE id = 1"), 11);
        // 2. …and in the WHERE of that read
        assert_eq!(scalar_count(db, "SELECT n FROM t WHERE plus1(id) = 2"), 10);
        // 3. an AGGREGATE UDF, inside the transaction: (10+20) + 2*100 = 230
        assert_eq!(scalar_count(db, "SELECT mysum(n) FROM t"), 230 * 10 + 2);
        // 4. a scalar UDF in a WRITE statement: SET, WHERE, and RETURNING
        assert_eq!(
            exec(db, "UPDATE t SET n = plus1(n) WHERE plus1(id) = 2"),
            SQLITE_OK
        );
        assert_eq!(scalar_count(db, "SELECT n FROM t WHERE id = 1"), 11);
        assert_eq!(
            scalar_count(db, "DELETE FROM t WHERE id = 2 RETURNING plus1(n)"),
            21
        );
        // 5. the row-producing side of an INSERT
        assert_eq!(
            exec(db, "INSERT INTO t (id, n) SELECT 7, plus1(n) FROM t WHERE id = 1"),
            SQLITE_OK
        );
        assert_eq!(scalar_count(db, "SELECT n FROM t WHERE id = 7"), 12);

        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        // committed state is what the UDFs computed
        assert_eq!(scalar_count(db, "SELECT n FROM t WHERE id = 7"), 12);
        sqlite3_close(db);
    }
}

/// The autocommit half of the same surface: a UDF in DML with no transaction
/// open at all (CPython's `isolation_level=None`).
#[test]
fn udf_in_autocommit_dml() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)"),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("plus1").as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                fnptr(udf_plus1),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(exec(db, "INSERT INTO t (id, n) VALUES (1, 10)"), SQLITE_OK);
        assert_eq!(exec(db, "UPDATE t SET n = plus1(n) WHERE plus1(id) = 2"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT n FROM t"), 11);
        assert_eq!(
            scalar_count(db, "DELETE FROM t WHERE plus1(id) = 2 RETURNING plus1(n)"),
            12
        );
        sqlite3_close(db);
    }
}

/// A UDF that raises (`sqlite3_result_error`) from a WRITE statement fails the
/// statement rather than writing a guessed value, and leaves the connection
/// usable.
#[test]
fn udf_error_from_a_write_statement() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)"),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                cs("boom").as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                fnptr(udf_boom),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, n) VALUES (1, 10)"), SQLITE_OK);
        assert_ne!(
            exec(db, "UPDATE t SET n = boom(n) WHERE id = 1"),
            SQLITE_OK,
            "a raising UDF must fail the write, not write a guess"
        );
        // the value is untouched and the transaction still commits
        assert_eq!(scalar_count(db, "SELECT n FROM t WHERE id = 1"), 10);
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT n FROM t WHERE id = 1"), 10);
        sqlite3_close(db);
    }
}

/// Django's `BooleanField`, end to end through the C API exactly as CPython's
/// `sqlite3` module drives it: the column is declared `bool`, and `True`/`False`
/// arrive as `sqlite3_bind_int` 1/0 — sqlite has no boolean type, so there is
/// nothing else for a driver to send. (Django gap #5.)
#[test]
fn django_boolean_field_through_the_c_api() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, flag bool NOT NULL)"),
            SQLITE_OK
        );

        // INSERT ... VALUES (?, ?) with True/False as CPython binds them.
        for (id, flag) in [(1, 1), (2, 0), (3, 1)] {
            let mut st: *mut Stmt = ptr::null_mut();
            let sql = cs("INSERT INTO t (id, flag) VALUES (?, ?)");
            assert_eq!(
                sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK
            );
            assert_eq!(sqlite3_bind_int(st, 1, id), SQLITE_OK);
            assert_eq!(sqlite3_bind_int(st, 2, flag), SQLITE_OK);
            assert_eq!(sqlite3_step(st), SQLITE_DONE, "insert id={id}");
            assert_eq!(sqlite3_finalize(st), SQLITE_OK);
        }

        // filter(flag=True) — the bound form, which is what Django emits.
        let mut st: *mut Stmt = ptr::null_mut();
        let sql = cs("SELECT COUNT(*) FROM t WHERE t.flag = ?");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_int(st, 1, 1), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(st, 0), 2);
        // exclude(flag=True) via the same statement, rebound to False.
        assert_eq!(sqlite3_reset(st), SQLITE_OK);
        assert_eq!(sqlite3_bind_int(st, 1, 0), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(st, 0), 1);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // The bare-column predicate, and its negation.
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t WHERE t.flag"), 2);
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t WHERE NOT t.flag"), 1);
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t WHERE t.flag = 1"), 2);
        // `flag = 2` is FALSE, not TRUE — sqlite's answer, since the column only
        // ever holds 0 or 1.
        assert_eq!(scalar_count(db, "SELECT COUNT(*) FROM t WHERE t.flag = 2"), 0);

        // Read-back is SQLITE_INTEGER 0/1, which is what Django's converter
        // (`bool(value)`) and every sqlite consumer expects.
        let mut st: *mut Stmt = ptr::null_mut();
        let sql = cs("SELECT flag FROM t WHERE id = 2");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_type(st, 0), SQLITE_INTEGER);
        assert_eq!(sqlite3_column_int(st, 0), 0);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        sqlite3_close(db);
    }
}

// ---- helpers -------------------------------------------------------------

fn sqlite_transient() -> *mut c_void {
    SQLITE_TRANSIENT as *mut c_void
}

unsafe fn col_name(st: *mut Stmt, i: c_int) -> String {
    CStr::from_ptr(sqlite3_column_name(st, i)).to_str().unwrap().to_string()
}

/// `sqlite3_bind_parameter_name(i)` as an `Option<String>` (None → anonymous).
unsafe fn param_name(st: *mut Stmt, i: c_int) -> Option<String> {
    let p = sqlite3_bind_parameter_name(st, i);
    if p.is_null() {
        None
    } else {
        Some(CStr::from_ptr(p).to_str().unwrap().to_string())
    }
}

/// `sqlite3_bind_parameter_index(name)` for a `&str` name (sigil included).
unsafe fn param_index(st: *mut Stmt, name: &str) -> c_int {
    let n = cs(name);
    sqlite3_bind_parameter_index(st, n.as_ptr())
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

/// Collect column 0 (as text) from every row of a query.
unsafe fn collect_text_col(db: *mut Sqlite3, sql: &str) -> Vec<String> {
    let s = cs(sql);
    let mut st: *mut Stmt = ptr::null_mut();
    assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK, "prepare {sql}");
    let mut out = Vec::new();
    while sqlite3_step(st) == SQLITE_ROW {
        let p = sqlite3_column_text(st, 0);
        out.push(if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p as *const c_char).to_str().unwrap().to_string()
        });
    }
    sqlite3_finalize(st);
    out
}

#[test]
fn expanded_sql_substitutes_bound_params() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)"), SQLITE_OK);
        let mut st: *mut Stmt = ptr::null_mut();
        // A positional `?` and a named `:n` share one numbering space; the
        // expansion substitutes each with its bound value as a SQL literal.
        let sql = cs("SELECT id, name FROM t WHERE id = ? AND name = :n");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_int(st, 1, 5), SQLITE_OK);
        let nm = cs("o'brien");
        assert_eq!(sqlite3_bind_text(st, 2, nm.as_ptr(), -1, sqlite_transient()), SQLITE_OK);
        let p = sqlite3_expanded_sql(st);
        assert!(!p.is_null());
        let expanded = CStr::from_ptr(p).to_str().unwrap().to_string();
        sqlite3_free(p as *mut c_void);
        assert_eq!(
            expanded,
            "SELECT id, name FROM t WHERE id = 5 AND name = 'o''brien'",
            "expanded_sql"
        );
        sqlite3_finalize(st);
    }
}

#[test]
fn interrupt_before_step_returns_interrupted_and_clears() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t VALUES (1)"), SQLITE_OK);
        let mut st: *mut Stmt = ptr::null_mut();
        let sql = cs("SELECT id FROM t");
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        sqlite3_interrupt(db);
        assert_eq!(sqlite3_step(st), SQLITE_INTERRUPT);
        sqlite3_finalize(st);
        // The flag is consumed: a fresh statement runs normally.
        let mut s2: *mut Stmt = ptr::null_mut();
        let q = cs("SELECT id FROM t");
        assert_eq!(
            sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut s2, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(s2), SQLITE_ROW);
        sqlite3_finalize(s2);
    }
}

#[test]
fn open_v2_unknown_vfs_is_refused_builtin_ok() {
    unsafe {
        let name = cs(":memory:");
        // A custom/unknown VFS cannot be honored — refuse rather than silently
        // ignore (which would be unsafe for e.g. an encryption VFS).
        let mut db: *mut Sqlite3 = ptr::null_mut();
        let vfs = cs("my-encryption-vfs");
        let rc = sqlite3_open_v2(
            name.as_ptr(),
            &mut db,
            SQLITE_OPEN_CREATE | SQLITE_OPEN_READWRITE,
            vfs.as_ptr(),
        );
        assert_eq!(rc, SQLITE_ERROR);
        assert!(!db.is_null(), "handle returned even on error (close contract)");
        let msg = CStr::from_ptr(sqlite3_errmsg(db)).to_str().unwrap();
        assert!(msg.contains("no such vfs"), "{msg}");
        sqlite3_close(db);
        // A built-in VFS name (or NULL) opens normally.
        let mut db2: *mut Sqlite3 = ptr::null_mut();
        let vfs2 = cs("unix");
        assert_eq!(
            sqlite3_open_v2(
                name.as_ptr(),
                &mut db2,
                SQLITE_OPEN_CREATE | SQLITE_OPEN_READWRITE,
                vfs2.as_ptr()
            ),
            SQLITE_OK
        );
        sqlite3_close(db2);
    }
}

#[test]
fn file_uri_size_mb_reserves_requested_geometry() {
    unsafe {
        // A `file:…?size_mb=N` URI pre-reserves an N-MiB database (mpedb
        // fallocates it): the file is created at exactly that size, and a
        // request SMALLER than the shim's 64 MiB file default is honored too —
        // mpedb does NOT always take "several MB" more than asked.
        let path = format!("/dev/shm/mpedb-capi-size-{}.mpedb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let uri = cs(&format!("file:{path}?size_mb=8"));
        let mut db: *mut Sqlite3 = ptr::null_mut();
        let rc = sqlite3_open_v2(
            uri.as_ptr(),
            &mut db,
            SQLITE_OPEN_CREATE | SQLITE_OPEN_READWRITE | SQLITE_OPEN_URI,
            ptr::null(),
        );
        assert_eq!(rc, SQLITE_OK, "open file: URI with size_mb");
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t(v) VALUES ('x')"), SQLITE_OK);
        sqlite3_close(db);
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert_eq!(bytes, 8 * 1024 * 1024, "reserved exactly 8 MiB, got {bytes}");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn column_decltype_reports_base_column_types_null_for_expressions() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB)"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "INSERT INTO t VALUES (1,'a',1.5,x'00')"), SQLITE_OK);

        let decl = |sql: &str, col: c_int| -> Option<String> {
            let mut st: *mut Stmt = ptr::null_mut();
            let s = cs(sql);
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK,
                "prepare {sql}"
            );
            let p = sqlite3_column_decltype(st, col);
            let out = if p.is_null() {
                None
            } else {
                Some(CStr::from_ptr(p).to_str().unwrap().to_string())
            };
            sqlite3_finalize(st);
            out
        };

        // Bare base-table columns report their declared type (plan-derived).
        assert_eq!(decl("SELECT id, name, score, data FROM t", 0).as_deref(), Some("INTEGER"));
        assert_eq!(decl("SELECT id, name, score, data FROM t", 1).as_deref(), Some("TEXT"));
        assert_eq!(decl("SELECT id, name, score, data FROM t", 2).as_deref(), Some("REAL"));
        assert_eq!(decl("SELECT id, name, score, data FROM t", 3).as_deref(), Some("BLOB"));
        // `SELECT *` expands to bare columns too.
        assert_eq!(decl("SELECT * FROM t", 1).as_deref(), Some("TEXT"));
        // A computed column has no declared type ⇒ NULL (like sqlite).
        assert_eq!(decl("SELECT id + 1 FROM t", 0), None);
        assert_eq!(decl("SELECT upper(name) FROM t", 0), None);
        assert_eq!(decl("SELECT count(*) FROM t", 0), None);
        // Mixed: expr NULL, bare column typed.
        assert_eq!(decl("SELECT id + 1, name FROM t", 0), None);
        assert_eq!(decl("SELECT id + 1, name FROM t", 1).as_deref(), Some("TEXT"));

        sqlite3_close(db);
    }
}

#[test]
fn compileoption_builtins_report_an_empty_option_set() {
    // Django's `register_functions()` refuses to hand out a connection until
    // `select sqlite_compileoption_used('ENABLE_MATH_FUNCTIONS')` answers. mpedb
    // defines NO sqlite compile options, so 0 / NULL is the literal truth, and
    // the 0 is what makes Django register its own math fallbacks.
    unsafe {
        let db = open_memory();

        // (value as int, sqlite type code) of the single column of `sql`.
        let scalar = |sql: &str| -> (i64, c_int) {
            let mut st: *mut Stmt = ptr::null_mut();
            let s = cs(sql);
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK,
                "prepare {sql}: {}",
                CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy()
            );
            assert_eq!(sqlite3_step(st), SQLITE_ROW, "step {sql}");
            let out = (sqlite3_column_int64(st, 0), sqlite3_column_type(st, 0));
            sqlite3_finalize(st);
            out
        };

        // No option is ever "used" — matches sqlite's answer for an undefined one.
        assert_eq!(
            scalar("SELECT sqlite_compileoption_used('ENABLE_MATH_FUNCTIONS')"),
            (0, SQLITE_INTEGER)
        );
        assert_eq!(
            scalar("SELECT sqlite_compileoption_used('THREADSAFE')"),
            (0, SQLITE_INTEGER)
        );
        // NULL in, NULL out (verified against sqlite 3.45).
        assert_eq!(scalar("SELECT sqlite_compileoption_used(NULL)").1, SQLITE_NULL);
        // The option list is empty, so every index is past the end.
        assert_eq!(scalar("SELECT sqlite_compileoption_get(0)").1, SQLITE_NULL);
        assert_eq!(scalar("SELECT sqlite_compileoption_get(100000)").1, SQLITE_NULL);

        sqlite3_close(db);
    }
}

/// `typeof()` through the shim reports EXACTLY one of sqlite's five storage
/// classes, for every value class and for every MPEdb-specific column type —
/// and always the class `sqlite3_column_type()` reports for the same value.
///
/// The contract: `typeof()` is a *sqlite* function, and its documented range is
/// `null|integer|real|text|blob`; consumers switch on exactly those five. mpedb
/// used to answer `'boolean'`/`'timestamp'` for its own first-class types —
/// honest natively, but through a libsqlite3 shim it is a DIFFERENT ANSWER
/// rather than an error, and it contradicted `sqlite3_column_type`, which has
/// always mapped `Bool`/`Timestamp` onto `SQLITE_INTEGER`.
///
/// Every expectation below was diffed against the stock `sqlite3` 3.45.1 binary.
#[test]
fn typeof_reports_only_sqlite_storage_classes_and_agrees_with_column_type() {
    unsafe {
        let db = open_memory();

        // (typeof(expr), sqlite3_column_type(expr)) for a one-column query.
        let probe = |sql: &str| -> (String, c_int) {
            let s = cs(&format!("SELECT typeof({sql}), {sql}"));
            let mut st: *mut Stmt = ptr::null_mut();
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK,
                "prepare typeof({sql}): {}",
                CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy()
            );
            assert_eq!(sqlite3_step(st), SQLITE_ROW, "step typeof({sql})");
            let name = col_text(st, 0);
            let ty = sqlite3_column_type(st, 1);
            sqlite3_finalize(st);
            (name, ty)
        };

        // --- every value class, as a literal. Stock sqlite 3.45.1:
        //     null|integer|real|text|blob|integer|real
        for (expr, want, want_ty) in [
            ("NULL", "null", SQLITE_NULL),
            ("1", "integer", SQLITE_INTEGER),
            ("-9223372036854775807", "integer", SQLITE_INTEGER),
            ("1.5", "real", SQLITE_FLOAT),
            ("'x'", "text", SQLITE_TEXT),
            ("''", "text", SQLITE_TEXT),
            ("x'00ff'", "blob", SQLITE_BLOB),
            ("2 + 3", "integer", SQLITE_INTEGER),
            ("1.0 * 2", "real", SQLITE_FLOAT),
        ] {
            let (name, ty) = probe(expr);
            assert_eq!(name, want, "typeof({expr})");
            assert_eq!(ty, want_ty, "column_type({expr})");
        }

        // --- MPEdb-specific column types.
        //
        // `bool` is a real ColumnType::Bool (mpedb's own name wins over sqlite's
        // affinity rule). `any` is mpedb's per-value column, which is what
        // sqlite's NUMERIC affinity maps onto. `timestamp` is a real
        // ColumnType::Timestamp — but NO value of it is reachable through this
        // shim (there is no bind path that produces one, and `DEFAULT
        // CURRENT_TIMESTAMP` is refused by name), so an int INSERT into one is a
        // clean type-mismatch refusal, asserted below. Its `typeof` mapping is
        // covered where it IS reachable: `mpedb-types` `expr::tests`.
        assert_eq!(
            exec(db, "CREATE TABLE t (id integer PRIMARY KEY, flag bool, v any)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(
                db,
                "INSERT INTO t VALUES (1, 1, 'str'), (2, 0, 7), (3, NULL, 1.5), \
                 (4, 1, x'0102'), (5, 0, NULL)"
            ),
            SQLITE_OK
        );

        // Stock sqlite over the same table (`bool`/`any` are NUMERIC/BLOB
        // affinity there, and hold the same per-value classes):
        //   flag: integer,integer,null,integer,integer
        //   v:    text,integer,real,blob,null
        let per_row = |col: &str| -> Vec<(String, c_int)> {
            let s = cs(&format!("SELECT typeof({col}), {col} FROM t ORDER BY id"));
            let mut st: *mut Stmt = ptr::null_mut();
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK
            );
            let mut out = Vec::new();
            while sqlite3_step(st) == SQLITE_ROW {
                out.push((col_text(st, 0), sqlite3_column_type(st, 1)));
            }
            sqlite3_finalize(st);
            out
        };

        // The `bool` column: 'boolean' was the wrong answer this test pins shut.
        assert_eq!(
            per_row("flag"),
            vec![
                ("integer".into(), SQLITE_INTEGER),
                ("integer".into(), SQLITE_INTEGER),
                ("null".into(), SQLITE_NULL),
                ("integer".into(), SQLITE_INTEGER),
                ("integer".into(), SQLITE_INTEGER),
            ]
        );
        // The `any` column: one class per VALUE, exactly as sqlite reports.
        assert_eq!(
            per_row("v"),
            vec![
                ("text".into(), SQLITE_TEXT),
                ("integer".into(), SQLITE_INTEGER),
                ("real".into(), SQLITE_FLOAT),
                ("blob".into(), SQLITE_BLOB),
                ("null".into(), SQLITE_NULL),
            ]
        );

        // A bound parameter carries the binder's class, not the column's.
        let s = cs("SELECT typeof(?), typeof(?), typeof(?), typeof(?)");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_bind_int64(st, 1, 42), SQLITE_OK);
        assert_eq!(sqlite3_bind_double(st, 2, 1.5), SQLITE_OK);
        let txt = cs("hi");
        assert_eq!(sqlite3_bind_text(st, 3, txt.as_ptr(), -1, sqlite_transient()), SQLITE_OK);
        assert_eq!(sqlite3_bind_null(st, 4), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(
            (col_text(st, 0), col_text(st, 1), col_text(st, 2), col_text(st, 3)),
            ("integer".into(), "real".into(), "text".into(), "null".into())
        );
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // `timestamp` is declarable but unreachable: a clean, named refusal —
        // never a value of a class sqlite has no name for.
        assert_eq!(
            exec(db, "CREATE TABLE ts (id integer PRIMARY KEY, t timestamp)"),
            SQLITE_OK
        );
        assert_ne!(exec(db, "INSERT INTO ts VALUES (1, 1720000000000000)"), SQLITE_OK);

        sqlite3_close(db);
    }
}

/// `PRAGMA busy_timeout` is the one setter pragma the shim honours for real:
/// it is the same knob `sqlite3_busy_timeout()` sets. Shape (one row named
/// `timeout`, returned by the SETTER form too) matches sqlite 3.45.1.
#[test]
fn pragma_busy_timeout_round_trips_and_is_the_c_api_knob() {
    unsafe {
        let db = open_memory();

        let one = |sql: &str| -> (String, i64) {
            let s = cs(sql);
            let mut st: *mut Stmt = ptr::null_mut();
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK,
                "prepare {sql}"
            );
            assert_eq!(sqlite3_step(st), SQLITE_ROW, "step {sql}");
            let out = (col_name(st, 0), sqlite3_column_int64(st, 0));
            sqlite3_finalize(st);
            out
        };

        assert_eq!(one("PRAGMA busy_timeout"), ("timeout".into(), 0));
        // The setter answers with the value now in force (sqlite's shape).
        assert_eq!(one("PRAGMA busy_timeout = 5000"), ("timeout".into(), 5000));
        assert_eq!(one("PRAGMA busy_timeout"), ("timeout".into(), 5000));
        // ... and it IS the C-API knob, not a second copy.
        assert_eq!(sqlite3_busy_timeout(db, 250), SQLITE_OK);
        assert_eq!(one("PRAGMA busy_timeout"), ("timeout".into(), 250));
        // sqlite clamps a negative to 0.
        assert_eq!(one("PRAGMA busy_timeout = -1"), ("timeout".into(), 0));

        // `foreign_keys` stays 0 through a set: mpedb enforces no foreign key,
        // and reporting 1 would promise enforcement that does not exist
        // (C-API-COMPAT gap D11). sqlite's own default is 0 too.
        assert_eq!(exec(db, "PRAGMA foreign_keys = ON"), SQLITE_OK);
        assert_eq!(one("PRAGMA foreign_keys"), ("foreign_keys".into(), 0));

        sqlite3_close(db);
    }
}
