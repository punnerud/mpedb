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

/// Read one UDF argument as UTF-8 text via `sqlite3_value_text`/`_bytes`.
unsafe fn value_string(v: *mut c_void) -> String {
    let p = sqlite3_value_text(v);
    let n = sqlite3_value_bytes(v);
    String::from_utf8_lossy(std::slice::from_raw_parts(p, n as usize)).into_owned()
}

/// The consumer's `regexp(pattern, text)` for the operator-dispatch test —
/// a stand-in for Django's `_sqlite_regexp`: a NULL argument yields NULL,
/// a `(?i)` prefix means case-insensitive, and the rest of the pattern is a
/// LITERAL substring (no metacharacters). The argument order is sqlite's for
/// the operator: `x REGEXP y` = `regexp(y, x)`, pattern FIRST.
unsafe extern "C" fn udf_regexp(ctx: *mut c_void, argc: c_int, argv: *mut *mut c_void) {
    assert_eq!(argc, 2);
    if sqlite3_value_type(*argv) == SQLITE_NULL
        || sqlite3_value_type(*argv.offset(1)) == SQLITE_NULL
    {
        sqlite3_result_null(ctx);
        return;
    }
    let pattern = value_string(*argv);
    let subject = value_string(*argv.offset(1));
    let hit = match pattern.strip_prefix("(?i)") {
        Some(p) => subject.to_lowercase().contains(&p.to_lowercase()),
        None => subject.contains(&pattern),
    };
    sqlite3_result_int64(ctx, hit as i64);
}

/// `x REGEXP y` has NO built-in meaning in real sqlite: it desugars to
/// `regexp(y, x)` and works only through the consumer's registered function.
/// The shim must dispatch the operator to the connection's `create_function`
/// `regexp/2` the same way — argument order included — which is what makes
/// Django's `__regex`/`__iregex` (always a bound, `(?i)`-prefixed pattern)
/// answer rows instead of tripping mpedb's native dialect (W3).
#[test]
fn regexp_operator_dispatches_to_registered_udf() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, h TEXT)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(
                db,
                "INSERT INTO t (id, h) VALUES (1,'hey-Foo'),(2,'foo'),(3,'bar'),(4,NULL)"
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_create_function(
                db,
                cs("regexp").as_ptr(),
                2,
                SQLITE_UTF8,
                ptr::null_mut(),
                fnptr(udf_regexp),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );

        // The `__iregex` shape: the pattern BOUND, `(?i)`-prefixed. Stock
        // sqlite + this udf answers rows 1 and 2 ('hey-Foo', 'foo'); the
        // native dialect would have refused the pattern.
        let mut q: *mut Stmt = ptr::null_mut();
        let qs = cs("SELECT id FROM t WHERE h REGEXP ? ORDER BY id");
        assert_eq!(
            sqlite3_prepare_v2(db, qs.as_ptr(), -1, &mut q, ptr::null_mut()),
            SQLITE_OK
        );
        let pat = cs("(?i)fo");
        assert_eq!(
            sqlite3_bind_text(q, 1, pat.as_ptr(), -1, sqlite_transient()),
            SQLITE_OK
        );
        let mut got = Vec::new();
        while sqlite3_step(q) == SQLITE_ROW {
            got.push(sqlite3_column_int64(q, 0));
        }
        assert_eq!(got, vec![1, 2], "host regexp must own the operator");
        sqlite3_finalize(q);

        // Argument order / precedence probe: `.` is LITERAL in this udf's
        // dialect, so 'o.' matches nothing — the native NFA (o + any char)
        // would have answered 2. And NOT REGEXP is 3VL: the NULL row passes
        // neither predicate.
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t WHERE h REGEXP 'o.'"), 0);
        assert_eq!(
            scalar_count(db, "SELECT count(*) FROM t WHERE h NOT REGEXP '(?i)fo'"),
            1,
            "only 'bar'; the NULL row is NULL under NOT too"
        );

        // In the projection the raw UDF result flows out (this udf returns
        // ints), and the NULL row propagates NULL.
        let mut p: *mut Stmt = ptr::null_mut();
        let ps = cs("SELECT h REGEXP '(?i)FOO' FROM t ORDER BY id");
        assert_eq!(
            sqlite3_prepare_v2(db, ps.as_ptr(), -1, &mut p, ptr::null_mut()),
            SQLITE_OK
        );
        let mut vals = Vec::new();
        while sqlite3_step(p) == SQLITE_ROW {
            vals.push(if sqlite3_column_type(p, 0) == SQLITE_NULL {
                None
            } else {
                Some(sqlite3_column_int64(p, 0))
            });
        }
        assert_eq!(vals, vec![Some(1), Some(1), Some(0), None]);
        sqlite3_finalize(p);

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

#[test]
fn a_failed_open_reports_why_rather_than_out_of_memory() {
    // A failed open returns NO handle, so `sqlite3_errmsg(NULL)` is the caller's
    // only channel — and sqlite's fixed answer there is "out of memory".
    // CPython's `sqlite3` reads exactly that, so every failed open surfaced to
    // Python as `InterfaceError: out of memory`, whatever had actually gone
    // wrong. Answer with the real reason instead.
    unsafe {
        let mut db: *mut Sqlite3 = ptr::null_mut();
        let name = cs("/tmp/mpedb-capi-no-such-file-open-error-test.db");
        let _ = std::fs::remove_file("/tmp/mpedb-capi-no-such-file-open-error-test.db");
        // READWRITE without CREATE on a file that does not exist.
        let rc = sqlite3_open_v2(name.as_ptr(), &mut db, SQLITE_OPEN_READWRITE, ptr::null());
        assert_eq!(rc, SQLITE_CANTOPEN);
        assert!(db.is_null());

        let msg = CStr::from_ptr(sqlite3_errmsg(ptr::null_mut())).to_string_lossy().into_owned();
        assert!(msg.contains("no such database file"), "errmsg was {msg:?}");
        assert_eq!(sqlite3_errcode(ptr::null_mut()), SQLITE_CANTOPEN);
        assert_eq!(sqlite3_extended_errcode(ptr::null_mut()), SQLITE_CANTOPEN);
    }
}

#[test]
fn named_in_memory_database_is_shared_private_and_does_not_outlive_the_process() {
    // `file:<name>?mode=memory` names an IN-MEMORY database, not a path. This is
    // how Django's test runner names every test database
    // (`file:memorydb_default?mode=memory&cache=shared`), and reading the name
    // as a path both dropped a 64 MiB file in the caller's CWD and made the
    // "in-memory" database survive the process — so the NEXT run silently
    // reopened the previous run's data.
    unsafe {
        let uri = cs("file:wb_named_mem_test?mode=memory&cache=shared");
        let cwd_artifact = std::path::Path::new("wb_named_mem_test");

        let mut a: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(uri.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(exec(a, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)"), SQLITE_OK);
        assert_eq!(exec(a, "INSERT INTO t VALUES (1, 'x')"), SQLITE_OK);

        // No file appears where the name was mistaken for a path.
        assert!(!cwd_artifact.exists(), "named in-memory db created a file in the CWD");

        // A SECOND connection to the same name sees the same database
        // (sqlite's shared-cache in-memory semantics).
        let mut b: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(uri.as_ptr(), &mut b), SQLITE_OK);
        let mut st: *mut Stmt = ptr::null_mut();
        let q = cs("SELECT v FROM t WHERE id = 1");
        assert_eq!(
            sqlite3_prepare_v2(b, q.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK,
            "prepare on second connection: {}",
            CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy()
        );
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(
            CStr::from_ptr(sqlite3_column_text(st, 0) as *const c_char).to_str().unwrap(),
            "x"
        );
        sqlite3_finalize(st);

        // Closing ONE connection leaves the database alive for the other.
        sqlite3_close(a);
        let mut st2: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(b, q.as_ptr(), -1, &mut st2, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(st2), SQLITE_ROW);
        sqlite3_finalize(st2);

        // Closing the LAST one destroys it: reopening the same name gives a
        // fresh, empty database rather than the old contents.
        sqlite3_close(b);
        let mut c: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(uri.as_ptr(), &mut c), SQLITE_OK);
        let mut st3: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(c, q.as_ptr(), -1, &mut st3, ptr::null_mut()),
            SQLITE_ERROR,
            "table t survived the last close of a named in-memory database"
        );
        sqlite3_finalize(st3);
        sqlite3_close(c);
        assert!(!cwd_artifact.exists());
    }
}

// ---- refusal-path destructor contracts (CPython heap safety) ---------------

unsafe extern "C" fn count_destroy(p: *mut c_void) {
    let c = &*(p as *const std::sync::atomic::AtomicU32);
    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// sqlite's documented contract: when `sqlite3_create_collation_v2` FAILS the
/// destructor is NOT invoked (the caller frees its context — CPython's
/// `create_collation` does exactly that), while a failing
/// `sqlite3_create_window_function` DOES invoke it (and CPython relies on
/// that by not freeing). Invoking it on the collation refusal was a
/// double-free that corrupted CPython's heap and segfaulted test_sqlite3.
#[test]
fn collation_refusal_leaves_destructor_alone_window_refusal_runs_it() {
    use std::sync::atomic::{AtomicU32, Ordering};
    unsafe {
        let db = open_memory();
        let hits = AtomicU32::new(0);
        let app = &hits as *const AtomicU32 as *mut c_void;

        let name = cs("mycoll");
        let rc = sqlite3_create_collation_v2(
            db,
            name.as_ptr(),
            1, // SQLITE_UTF8
            app,
            ptr::null_mut(),
            fnptr1(count_destroy),
        );
        assert_eq!(rc, SQLITE_ERROR, "custom collations are refused");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "collation destructor must NOT run on failure (double-free under CPython)"
        );

        let wname = cs("mywin");
        let rc = sqlite3_create_window_function(
            db,
            wname.as_ptr(),
            1,
            1, // SQLITE_UTF8
            app,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            fnptr1(count_destroy),
        );
        assert_eq!(rc, SQLITE_ERROR, "window functions are refused");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "window-function destructor MUST run on failure (CPython does not free otherwise)"
        );
        sqlite3_close(db);
    }
}

// ---- CPython test_sqlite3 batch: trace, limits, trivia, ro, busy ----------

static TRACE_LOG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

unsafe extern "C" fn record_trace(
    ev: u32,
    _ctx: *mut c_void,
    p: *mut c_void,
    sql: *mut c_void,
) -> c_int {
    assert_eq!(ev, SQLITE_TRACE_STMT);
    // The P argument must be expandable, exactly as CPython uses it.
    let expanded = sqlite3_expanded_sql(p as *mut Stmt);
    let text = if expanded.is_null() {
        CStr::from_ptr(sql as *const c_char).to_string_lossy().into_owned()
    } else {
        let t = CStr::from_ptr(expanded).to_string_lossy().into_owned();
        sqlite3_free(expanded as *mut c_void);
        t
    };
    TRACE_LOG.lock().unwrap().push(text);
    0
}

fn trace_fnptr() -> *mut c_void {
    record_trace as unsafe extern "C" fn(u32, *mut c_void, *mut c_void, *mut c_void) -> c_int
        as *const () as *mut c_void
}

/// SQLITE_TRACE_STMT fires as a statement begins running — on the step path
/// (with bound parameters expanded, sqlite's contract via expanded_sql) and on
/// the exec path (CPython's legacy-autocommit COMMIT goes through exec).
#[test]
fn trace_v2_stmt_fires_on_step_and_exec() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)"), SQLITE_OK);
        TRACE_LOG.lock().unwrap().clear();
        assert_eq!(
            sqlite3_trace_v2(db, SQLITE_TRACE_STMT, trace_fnptr(), ptr::null_mut()),
            SQLITE_OK
        );

        // step path, with a bound parameter -> traced EXPANDED
        let sql = cs("insert into t(b) values(?)");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        let v = cs("x");
        assert_eq!(sqlite3_bind_text(st, 1, v.as_ptr(), -1, sqlite_transient()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        sqlite3_finalize(st);

        // exec path
        assert_eq!(exec(db, "delete from t"), SQLITE_OK);

        // clearing stops events
        assert_eq!(sqlite3_trace_v2(db, 0, ptr::null_mut(), ptr::null_mut()), SQLITE_OK);
        assert_eq!(exec(db, "insert into t(b) values('y')"), SQLITE_OK);

        let log = TRACE_LOG.lock().unwrap().clone();
        assert_eq!(
            log,
            vec!["insert into t(b) values('x')".to_string(), "delete from t".to_string()],
            "trace log"
        );
        sqlite3_close(db);
    }
}

#[test]
fn limits_round_trip_and_variable_number_enforced() {
    unsafe {
        let db = open_memory();
        // bad category -> negative
        assert!(sqlite3_limit(db, 99, -1) < 0);
        // round trip: prior value comes back, new value sticks
        let prior = sqlite3_limit(db, SQLITE_LIMIT_VARIABLE_NUMBER, 1);
        assert_eq!(prior, 32_766);
        assert_eq!(sqlite3_limit(db, SQLITE_LIMIT_VARIABLE_NUMBER, -1), 1);
        // enforcement at prepare, sqlite's message
        let sql = cs("select ?, ?");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_ERROR);
        let msg = CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy().into_owned();
        assert!(msg.contains("too many SQL variables"), "errmsg {msg:?}");
        // restore; expanded_sql honors SQLITE_LIMIT_LENGTH
        sqlite3_limit(db, SQLITE_LIMIT_VARIABLE_NUMBER, prior);
        let sql = cs("select ?");
        assert_eq!(sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        let v = cs("abcdefgh");
        assert_eq!(sqlite3_bind_text(st, 1, v.as_ptr(), -1, sqlite_transient()), SQLITE_OK);
        sqlite3_limit(db, SQLITE_LIMIT_LENGTH, 4);
        assert!(sqlite3_expanded_sql(st).is_null(), "expansion above LENGTH limit must be NULL");
        sqlite3_limit(db, SQLITE_LIMIT_LENGTH, 1_000_000_000);
        let e = sqlite3_expanded_sql(st);
        assert_eq!(CStr::from_ptr(e).to_string_lossy(), "select 'abcdefgh'");
        sqlite3_free(e as *mut c_void);
        sqlite3_finalize(st);
        sqlite3_close(db);
    }
}

/// mpedb's parser does not skip leading comments; the shim strips them
/// (classification AND the text the engine sees), as sqlite's parser does.
#[test]
fn leading_comments_and_maintenance_statements() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "  -- leading comment\n  insert into t(b) values('x')"), SQLITE_OK);
        assert_eq!(exec(db, "/* block */ insert into t(b) values('y')"), SQLITE_OK);
        // prepare path too
        let sql = cs("-- c\nselect count(*) from t");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(st, 0), 2);
        sqlite3_finalize(st);
        // VACUUM / ANALYZE: accepted no-ops (housekeeping with nothing to do)
        assert_eq!(exec(db, "VACUUM"), SQLITE_OK);
        assert_eq!(exec(db, "ANALYZE"), SQLITE_OK);
        sqlite3_close(db);
    }
}

/// A NaN has no sqlite representation: binding one stores NULL.
#[test]
fn nan_binds_as_null() {
    unsafe {
        let db = open_memory();
        let sql = cs("select ? is null");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_bind_double(st, 1, f64::NAN), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(st, 0), 1);
        sqlite3_finalize(st);
        sqlite3_close(db);
    }
}

/// `file:…?mode=ro`: a missing file is not created (CANTOPEN), and writes on
/// an existing one refuse with SQLITE_READONLY.
#[test]
fn uri_mode_ro_is_enforced() {
    unsafe {
        let path = "/tmp/mpedb-capi-ro-test.mpedb";
        let _ = std::fs::remove_file(path);
        let uri = cs("file:/tmp/mpedb-capi-ro-test.mpedb?mode=ro");
        let mut db: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(
            sqlite3_open_v2(&raw const *uri.as_ptr(), &mut db, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | 0x40, ptr::null()),
            SQLITE_CANTOPEN,
            "mode=ro must not create"
        );
        assert!(!std::path::Path::new(path).exists());

        // create it read-write, then reopen ro
        let plain = cs(path);
        assert_eq!(sqlite3_open(plain.as_ptr(), &mut db), SQLITE_OK);
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY)"), SQLITE_OK);
        sqlite3_close(db);
        assert_eq!(
            sqlite3_open_v2(uri.as_ptr(), &mut db, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | 0x40, ptr::null()),
            SQLITE_OK
        );
        assert_eq!(exec(db, "insert into t(a) values(1)"), SQLITE_READONLY);
        let msg = CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy().into_owned();
        assert!(msg.contains("readonly"), "errmsg {msg:?}");
        // reads still fine
        let q = cs("select count(*) from t");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        sqlite3_finalize(st);
        sqlite3_close(db);
        let _ = std::fs::remove_file(path);
    }
}

/// A `file:` URI's path is percent-decoded, byte-wise.
#[test]
fn uri_path_percent_decodes() {
    unsafe {
        let path = "/tmp/mpedb capi pct test.mpedb";
        let _ = std::fs::remove_file(path);
        let uri = cs("file:/tmp/mpedb%20capi%20pct%20test.mpedb");
        let mut db: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(uri.as_ptr(), &mut db), SQLITE_OK);
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert!(std::path::Path::new(path).exists(), "decoded path was not the one created");
        sqlite3_close(db);
        let _ = std::fs::remove_file(path);
    }
}

/// A SIBLING connection on the same thread that needs the writer lock while
/// this one holds it gets sqlite's BUSY ("database is locked"), not an
/// "internal error (bug in mpedb)" — and the busy-timeout retry clears it.
#[test]
fn sibling_connection_write_is_busy_not_internal() {
    unsafe {
        let path = "/tmp/mpedb-capi-busy-sibling.mpedb";
        let _ = std::fs::remove_file(path);
        let name = cs(path);
        let (mut a, mut b): (*mut Sqlite3, *mut Sqlite3) = (ptr::null_mut(), ptr::null_mut());
        assert_eq!(sqlite3_open(name.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(sqlite3_open(name.as_ptr(), &mut b), SQLITE_OK);
        assert_eq!(exec(a, "create table t (a INTEGER PRIMARY KEY, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(a, "insert into t(b) values('x')"), SQLITE_OK);
        // b: BEGIN needs the writer lock a holds -> BUSY, sqlite's message
        assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY);
        let msg = CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy().into_owned();
        assert_eq!(msg, "database is locked");
        // commit on a, then b proceeds
        assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
        assert_eq!(exec(b, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(b, "insert into t(b) values('y')"), SQLITE_OK);
        assert_eq!(exec(b, "COMMIT"), SQLITE_OK);
        sqlite3_close(a);
        sqlite3_close(b);
        let _ = std::fs::remove_file(path);
    }
}

/// #109 (compat gap E1): `sqlite3_busy_timeout` bounds the ENGINE's
/// writer-lock wait. A contended write answers SQLITE_BUSY *at the deadline*
/// — with elapsed-time evidence — and timeout 0 answers immediately; it
/// never blocks forever. Exercises both the interactive path (BEGIN →
/// `Database::begin`) and autocommit DML (`run_write_plan`'s bounded direct
/// path). Same-thread sibling connections contend on the same single writer
/// lock the cross-process case uses (the two-process arm lives in
/// `crates/mpedb/tests/busy_timeout.rs` and the CPython MultiprocessTests).
#[test]
fn busy_timeout_bounds_writer_lock_wait() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    let path = format!("/tmp/mpedb-capi-busy-deadline-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    unsafe {
        // Seed the file + table from the main thread.
        let name = cs(&path);
        let mut seed: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(name.as_ptr(), &mut seed), SQLITE_OK);
        assert_eq!(exec(seed, "create table t (a INTEGER PRIMARY KEY, b TEXT)"), SQLITE_OK);
        sqlite3_close(seed);
    }

    // Holder THREAD with its own connection (the ERRORCHECK mutex is
    // per-thread-owned, so this contends exactly like a second process):
    // BEGIN + INSERT, signal, hold ~1.5 s, COMMIT.
    let held = Arc::new(AtomicBool::new(false));
    let holder = {
        let (path, held) = (path.clone(), held.clone());
        std::thread::spawn(move || unsafe {
            let name = cs(&path);
            let mut a: *mut Sqlite3 = ptr::null_mut();
            assert_eq!(sqlite3_open(name.as_ptr(), &mut a), SQLITE_OK);
            assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
            assert_eq!(exec(a, "insert into t(b) values('x')"), SQLITE_OK);
            held.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
            sqlite3_close(a);
        })
    };
    let t0 = Instant::now();
    while !held.load(std::sync::atomic::Ordering::Acquire) {
        assert!(t0.elapsed() < Duration::from_secs(10), "holder never signalled");
        std::thread::sleep(Duration::from_millis(2));
    }

    unsafe {
        let name = cs(&path);
        let mut b: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(name.as_ptr(), &mut b), SQLITE_OK);

        // timeout 0 (sqlite's default, the connection's initial state):
        // one immediate attempt, immediate BUSY.
        let t0 = Instant::now();
        assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY);
        let dt = t0.elapsed();
        assert!(dt < Duration::from_millis(300), "timeout 0 was not immediate: {dt:?}");
        let msg = CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy().into_owned();
        assert_eq!(msg, "database is locked");

        // timeout 200 ms: BUSY only after the timeout has genuinely elapsed.
        assert_eq!(sqlite3_busy_timeout(b, 200), SQLITE_OK);
        let t0 = Instant::now();
        assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY);
        let dt = t0.elapsed();
        assert!(dt >= Duration::from_millis(200), "BUSY before the timeout: {dt:?}");
        assert!(dt < Duration::from_millis(1400), "waited far past the timeout: {dt:?}");
        let msg = CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy().into_owned();
        assert_eq!(msg, "database is locked");

        // Autocommit DML takes the same bounded wait.
        let t0 = Instant::now();
        assert_eq!(exec(b, "insert into t(b) values('y')"), SQLITE_BUSY);
        let dt = t0.elapsed();
        assert!(dt >= Duration::from_millis(200), "BUSY before the timeout: {dt:?}");
        assert!(dt < Duration::from_millis(1400), "waited far past the timeout: {dt:?}");

        // A timeout LONGER than the holder's remaining grip: the bounded
        // poll must ACQUIRE once the holder commits — the wait clears
        // instead of running to its deadline.
        assert_eq!(sqlite3_busy_timeout(b, 5000), SQLITE_OK);
        let t0 = Instant::now();
        assert_eq!(exec(b, "BEGIN"), SQLITE_OK);
        let dt = t0.elapsed();
        assert!(dt < Duration::from_millis(4000), "acquire should beat the deadline: {dt:?}");
        assert_eq!(exec(b, "insert into t(b) values('z')"), SQLITE_OK);
        assert_eq!(exec(b, "COMMIT"), SQLITE_OK);
        sqlite3_close(b);
    }
    holder.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// Same-THREAD sibling contention is an unwinnable wait (the lock's owner is
/// the caller's own thread — it cannot release while we poll), so a busy
/// timeout is deliberately NOT burned down: immediate BUSY, sqlite's message.
#[test]
// glibc-only: Apple's libpthread answers EBUSY (not EDEADLK) for an
// owner's errorcheck trylock (rdar://16261552), so on macOS the reentry
// fold is dead and a same-thread sibling waits out its full deadline —
// bounded, just not immediate.
#[cfg_attr(target_os = "macos", ignore = "EBUSY-on-relock: no immediate fold on macOS")]
fn busy_timeout_same_thread_sibling_is_immediate() {
    use std::time::{Duration, Instant};
    unsafe {
        let path = format!("/tmp/mpedb-capi-busy-sibling-timeout-{}.mpedb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let name = cs(&path);
        let (mut a, mut b): (*mut Sqlite3, *mut Sqlite3) = (ptr::null_mut(), ptr::null_mut());
        assert_eq!(sqlite3_open(name.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(sqlite3_open(name.as_ptr(), &mut b), SQLITE_OK);
        assert_eq!(exec(a, "create table t (a INTEGER PRIMARY KEY, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(a, "insert into t(b) values('x')"), SQLITE_OK);

        assert_eq!(sqlite3_busy_timeout(b, 2000), SQLITE_OK);
        let t0 = Instant::now();
        assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY);
        let dt = t0.elapsed();
        assert!(dt < Duration::from_millis(500), "same-thread BUSY should not wait: {dt:?}");
        let msg = CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy().into_owned();
        assert_eq!(msg, "database is locked");

        assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
        assert_eq!(exec(b, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(b, "insert into t(b) values('y')"), SQLITE_OK);
        assert_eq!(exec(b, "COMMIT"), SQLITE_OK);
        sqlite3_close(a);
        sqlite3_close(b);
        let _ = std::fs::remove_file(&path);
    }
}

/// Refusal stubs must leave the refusal ON THE HANDLE (backup: the
/// destination), or CPython raises bare SystemError.
#[test]
fn refusal_stubs_set_handle_error() {
    unsafe {
        let dst = open_memory();
        let src = open_memory();
        let main = cs("main");
        assert!(sqlite3_backup_init(dst, main.as_ptr(), src, main.as_ptr()).is_null());
        let msg = CStr::from_ptr(sqlite3_errmsg(dst)).to_string_lossy().into_owned();
        assert!(msg.contains("backup"), "backup errmsg {msg:?}");

        // Incremental blob I/O is REAL now (see the blob_* tests below); what
        // this asserts is only that a FAILING open still leaves its reason on
        // the handle rather than a bare non-OK code.
        let mut blob: *mut c_void = ptr::null_mut();
        let t = cs("t");
        assert_eq!(
            sqlite3_blob_open(src, main.as_ptr(), t.as_ptr(), t.as_ptr(), 1, 0, &mut blob),
            SQLITE_ERROR
        );
        assert!(blob.is_null(), "a failed open must leave *ppBlob NULL");
        let msg = CStr::from_ptr(sqlite3_errmsg(src)).to_string_lossy().into_owned();
        assert_eq!(msg, "no such table: main.t", "blob errmsg {msg:?}");
        sqlite3_close(dst);
        sqlite3_close(src);
    }
}

// ===========================================================================
// Incremental blob I/O (`sqlite3_blob_*`). CPython's `test_blob` covers the
// happy paths and the Python-level slicing; these pin the corners it never
// reaches but sqlite defines — each expectation PROBED against sqlite 3.45.1.
// ===========================================================================

/// Open a blob handle, asserting success.
unsafe fn blob_open_ok(db: *mut Sqlite3, table: &str, col: &str, row: i64, rw: c_int) -> *mut c_void {
    let (main, t, c) = (cs("main"), cs(table), cs(col));
    let mut b: *mut c_void = ptr::null_mut();
    let rc = sqlite3_blob_open(db, main.as_ptr(), t.as_ptr(), c.as_ptr(), row, rw, &mut b);
    assert_eq!(rc, SQLITE_OK, "blob_open {table}.{col}#{row}: {}", errmsg(db));
    assert!(!b.is_null());
    b
}

/// Attempt an open, returning `(rc, errmsg)`.
unsafe fn blob_open_err(db: *mut Sqlite3, dbname: &str, table: &str, col: &str, row: i64, rw: c_int) -> (c_int, String) {
    let (d, t, c) = (cs(dbname), cs(table), cs(col));
    let mut b: *mut c_void = ptr::null_mut();
    let rc = sqlite3_blob_open(db, d.as_ptr(), t.as_ptr(), c.as_ptr(), row, rw, &mut b);
    assert!(b.is_null(), "a failed open must leave *ppBlob NULL");
    (rc, errmsg(db))
}

unsafe fn errmsg(db: *mut Sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy().into_owned()
}

unsafe fn blob_read_vec(b: *mut c_void, n: usize, off: c_int) -> (c_int, Vec<u8>) {
    let mut buf = vec![0u8; n];
    let rc = sqlite3_blob_read(b, buf.as_mut_ptr() as *mut c_void, n as c_int, off);
    (rc, buf)
}

/// Every `blob_open` refusal shape, with sqlite's exact code and message.
#[test]
fn blob_open_error_shapes_match_sqlite() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB, c TEXT, d REAL, e INTEGER)"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "create index idx_c on t(c)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'00010203', 'hello', 1.5, 42)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t (a, b) values (3, NULL)"), SQLITE_OK);

        // Unknown database/table/column, and a missing row.
        assert_eq!(
            blob_open_err(db, "nope", "t", "b", 1, 0),
            (SQLITE_ERROR, "no such table: nope.t".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "nope", "b", 1, 0),
            (SQLITE_ERROR, "no such table: main.nope".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "t", "nope", 1, 0),
            (SQLITE_ERROR, "no such column: \"nope\"".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "t", "b", 999, 0),
            (SQLITE_ERROR, "no such rowid: 999".into())
        );
        // A value that is not text/blob names its own type, NULL included.
        assert_eq!(
            blob_open_err(db, "main", "t", "e", 1, 0),
            (SQLITE_ERROR, "cannot open value of type integer".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "t", "d", 1, 0),
            (SQLITE_ERROR, "cannot open value of type real".into())
        );
        assert_eq!(
            blob_open_err(db, "main", "t", "b", 3, 0),
            (SQLITE_ERROR, "cannot open value of type null".into())
        );
        // Read-WRITE on an indexed or primary-key column is refused; the same
        // column opens fine read-ONLY.
        assert_eq!(
            blob_open_err(db, "main", "t", "c", 1, 1),
            (SQLITE_ERROR, "cannot open indexed column for writing".into())
        );
        let ro = blob_open_ok(db, "t", "c", 1, 0);
        assert_eq!(sqlite3_blob_bytes(ro), 5); // 'hello' — TEXT opens as bytes
        assert_eq!(sqlite3_blob_close(ro), SQLITE_OK);

        // The schema check precedes the row fetch: a missing row still reports
        // the indexed-column refusal (sqlite's order).
        assert_eq!(
            blob_open_err(db, "main", "t", "c", 999, 1),
            (SQLITE_ERROR, "cannot open indexed column for writing".into())
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// The size is FIXED at open: out-of-range reads/writes are SQLITE_ERROR and a
/// write can never grow the value. A zero-length I/O at the very end is OK.
#[test]
fn blob_io_bounds_are_the_open_time_size() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'00010203040506070809')"), SQLITE_OK);
        let b = blob_open_ok(db, "t", "b", 1, 1);
        assert_eq!(sqlite3_blob_bytes(b), 10);

        // In range.
        let (rc, got) = blob_read_vec(b, 4, 6);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(got, vec![0x06, 0x07, 0x08, 0x09]);

        // One byte past the end, a negative count, a negative offset.
        for (n, off) in [(4, 7), (11, 0), (-1, 0), (1, -1), (1, 10)] {
            let mut buf = [0u8; 16];
            assert_eq!(
                sqlite3_blob_read(b, buf.as_mut_ptr() as *mut c_void, n, off),
                SQLITE_ERROR,
                "read(n={n}, off={off}) must be out of range"
            );
            assert_eq!(errmsg(db), "SQL logic error");
        }
        // A ZERO-length read at exactly the end is fine (sqlite: probed).
        let mut none = [0u8; 1];
        assert_eq!(sqlite3_blob_read(b, none.as_mut_ptr() as *mut c_void, 0, 10), SQLITE_OK);

        // Writes obey the same bounds and never grow the value.
        let data = [0xAAu8, 0xBB];
        assert_eq!(
            sqlite3_blob_write(b, data.as_ptr() as *const c_void, 2, 9),
            SQLITE_ERROR,
            "a write may not extend the blob"
        );
        assert_eq!(sqlite3_blob_write(b, data.as_ptr() as *const c_void, 2, 8), SQLITE_OK);
        assert_eq!(sqlite3_blob_bytes(b), 10, "size is fixed at open");
        let (rc, got) = blob_read_vec(b, 2, 8);
        assert_eq!((rc, got), (SQLITE_OK, vec![0xAA, 0xBB]), "own write reads back");
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);

        // …and it landed in the row, with the length unchanged.
        let mut st: *mut Stmt = ptr::null_mut();
        let q = cs("select hex(b) from t where a = 1");
        assert_eq!(sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(col_text(st, 0), "0001020304050607AABB");
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// A read-only handle refuses `blob_write` with SQLITE_READONLY, and the
/// refusal does not kill the handle (reads keep working).
#[test]
fn blob_write_on_readonly_handle_is_readonly() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'0102')"), SQLITE_OK);
        let b = blob_open_ok(db, "t", "b", 1, 0);
        let d = [0x99u8];
        assert_eq!(sqlite3_blob_write(b, d.as_ptr() as *const c_void, 1, 0), SQLITE_READONLY);
        assert_eq!(errmsg(db), "attempt to write a readonly database");
        // Still alive.
        assert_eq!(blob_read_vec(b, 2, 0), (SQLITE_OK, vec![0x01, 0x02]));
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// The expiry rule under mpedb's MVCC (see `blob.rs`): a row modified or
/// deleted under a handle makes every later call SQLITE_ABORT, permanently —
/// `reopen` on a dead handle stays ABORT, exactly as sqlite does.
#[test]
fn blob_handle_expires_when_its_row_changes() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB, c TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'0102', 'x'), (2, x'0304', 'y')"), SQLITE_OK);

        // A write to a DIFFERENT row leaves the handle alone.
        let b = blob_open_ok(db, "t", "b", 1, 1);
        assert_eq!(exec(db, "update t set c = 'zz' where a = 2"), SQLITE_OK);
        assert_eq!(blob_read_vec(b, 2, 0), (SQLITE_OK, vec![0x01, 0x02]));

        // A write to ANOTHER COLUMN of the handle's own row expires it.
        assert_eq!(exec(db, "update t set c = 'changed' where a = 1"), SQLITE_OK);
        let (rc, _) = blob_read_vec(b, 1, 0);
        assert_eq!(rc, SQLITE_ABORT);
        assert_eq!(errmsg(db), "query aborted");
        // Dead is dead: bytes reads 0, write and reopen both stay ABORT.
        assert_eq!(sqlite3_blob_bytes(b), 0);
        let d = [0u8];
        assert_eq!(sqlite3_blob_write(b, d.as_ptr() as *const c_void, 1, 0), SQLITE_ABORT);
        assert_eq!(sqlite3_blob_reopen(b, 2), SQLITE_ABORT);
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);

        // DELETE expires a handle too.
        let b2 = blob_open_ok(db, "t", "b", 2, 0);
        assert_eq!(exec(db, "delete from t where a = 2"), SQLITE_OK);
        assert_eq!(blob_read_vec(b2, 1, 0).0, SQLITE_ABORT);
        assert_eq!(sqlite3_blob_close(b2), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// A write THROUGH a blob handle expires nothing — not the writer, and not a
/// same-row sibling handle on this connection (sqlite parity, probed).
#[test]
fn blob_write_does_not_expire_sibling_handles() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'0102')"), SQLITE_OK);
        let a = blob_open_ok(db, "t", "b", 1, 1);
        let b = blob_open_ok(db, "t", "b", 1, 1);
        let d = [0x51u8];
        assert_eq!(sqlite3_blob_write(a, d.as_ptr() as *const c_void, 1, 0), SQLITE_OK);
        // Both handles are alive, and the sibling sees the new byte.
        assert_eq!(blob_read_vec(a, 1, 0), (SQLITE_OK, vec![0x51]));
        assert_eq!(blob_read_vec(b, 1, 0), (SQLITE_OK, vec![0x51]));
        assert_eq!(sqlite3_blob_close(a), SQLITE_OK);
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// `sqlite3_blob_reopen` moves a live handle to another row of the same
/// table/column, re-reading the size; a FAILED reopen kills the handle.
#[test]
fn blob_reopen_moves_the_row_and_a_failure_kills_the_handle() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(
            exec(db, "insert into t values (1, x'0102'), (2, x'030405'), (4, NULL)"),
            SQLITE_OK
        );
        let h = blob_open_ok(db, "t", "b", 1, 0);
        assert_eq!(sqlite3_blob_bytes(h), 2);
        assert_eq!(sqlite3_blob_reopen(h, 2), SQLITE_OK);
        assert_eq!(sqlite3_blob_bytes(h), 3, "size follows the new row");
        assert_eq!(blob_read_vec(h, 3, 0), (SQLITE_OK, vec![0x03, 0x04, 0x05]));

        // Reopen onto a missing rowid: SQLITE_ERROR with the open-shape
        // message, and the handle is finalized (everything after is ABORT).
        assert_eq!(sqlite3_blob_reopen(h, 777), SQLITE_ERROR);
        assert_eq!(errmsg(db), "no such rowid: 777");
        assert_eq!(blob_read_vec(h, 1, 0).0, SQLITE_ABORT);
        assert_eq!(sqlite3_blob_close(h), SQLITE_OK);

        // Reopen onto a NULL value fails the same way.
        let h2 = blob_open_ok(db, "t", "b", 1, 0);
        assert_eq!(sqlite3_blob_reopen(h2, 4), SQLITE_ERROR);
        assert_eq!(errmsg(db), "cannot open value of type null");
        assert_eq!(sqlite3_blob_close(h2), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// The two closes differ when a blob handle is still open, and the shim
/// follows sqlite on both (probed on 3.45.1): `sqlite3_close` refuses with
/// SQLITE_BUSY, while `sqlite3_close_v2` succeeds and leaves a ZOMBIE — the
/// connection stays alive for the handle, which keeps working, and is freed by
/// the last `sqlite3_blob_close`.
#[test]
fn close_with_an_open_blob_handle_is_busy_but_close_v2_zombies() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, b BLOB)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, x'0102')"), SQLITE_OK);

        // v1: refused, and the connection is untouched — the handle and the
        // connection both still work.
        let b = blob_open_ok(db, "t", "b", 1, 0);
        assert_eq!(sqlite3_close(db), SQLITE_BUSY);
        assert_eq!(blob_read_vec(b, 2, 0), (SQLITE_OK, vec![0x01, 0x02]));
        assert_eq!(sqlite3_blob_close(b), SQLITE_OK);

        // v2 with a live handle: OK, and the handle keeps reading afterwards.
        let b2 = blob_open_ok(db, "t", "b", 1, 0);
        assert_eq!(sqlite3_close_v2(db), SQLITE_OK);
        assert_eq!(
            blob_read_vec(b2, 2, 0),
            (SQLITE_OK, vec![0x01, 0x02]),
            "a zombie connection still serves its outstanding blob handle"
        );
        // This free is the connection's too; ASAN/valgrind would catch a leak
        // or a double free here.
        assert_eq!(sqlite3_blob_close(b2), SQLITE_OK);
    }
}

/// A blob write that would leave a TEXT value invalid UTF-8 is refused rather
/// than silently restyling the cell as a blob: mpedb text is strictly UTF-8,
/// so this is a documented divergence from sqlite (which stores raw bytes).
#[test]
fn blob_write_refuses_to_break_a_text_value() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "create table t (a INTEGER PRIMARY KEY, c TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "insert into t values (1, 'hello')"), SQLITE_OK);
        let h = blob_open_ok(db, "t", "c", 1, 1);
        // ASCII in, ASCII out: fine.
        let ok = [b'H'];
        assert_eq!(sqlite3_blob_write(h, ok.as_ptr() as *const c_void, 1, 0), SQLITE_OK);
        // A lone continuation byte is not valid UTF-8: refused, value intact.
        let bad = [0xFFu8];
        assert_eq!(sqlite3_blob_write(h, bad.as_ptr() as *const c_void, 1, 0), SQLITE_ERROR);
        assert!(errmsg(db).contains("UTF-8"), "errmsg {:?}", errmsg(db));
        assert_eq!(blob_read_vec(h, 5, 0), (SQLITE_OK, b"Hello".to_vec()));
        assert_eq!(sqlite3_blob_close(h), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// `sqlite3_bind_zeroblob` binds N zero bytes, and refuses past
/// SQLITE_MAX_LENGTH with SQLITE_TOOBIG (sqlite's cap; mpedb materializes the
/// zeros, so the cap is also the memory guard).
#[test]
fn bind_zeroblob_binds_zeros_and_caps() {
    unsafe {
        let db = open_memory();
        let mut st: *mut Stmt = ptr::null_mut();
        let q = cs("select ?, typeof(?)");
        assert_eq!(sqlite3_prepare_v2(db, q.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_bind_zeroblob(st, 1, 5), SQLITE_OK);
        // A negative length is an empty blob, as sqlite does.
        assert_eq!(sqlite3_bind_zeroblob(st, 2, -3), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_type(st, 0), SQLITE_BLOB);
        assert_eq!(sqlite3_column_bytes(st, 0), 5);
        let p = sqlite3_column_blob(st, 0) as *const u8;
        assert_eq!(std::slice::from_raw_parts(p, 5), &[0u8; 5]);
        assert_eq!(col_text(st, 1), "blob");
        // Past the cap.
        assert_eq!(sqlite3_bind_zeroblob(st, 1, 1_000_000_001), SQLITE_TOOBIG);
        assert_eq!(sqlite3_bind_zeroblob64(st, 1, 1_000_000_001), SQLITE_TOOBIG);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

// ===========================================================================
// `INSERT OR ROLLBACK` — the one conflict action that lives in the SHIM
// (#112 wave 3, bucket C). mpedb's parser refuses it by name because a
// statement cannot abort the transaction that contains it; the connection
// can, so the shim runs the statement as `OR ABORT` and rolls back itself.
// ===========================================================================

/// The three corners CPython's suite does not reach:
/// 1. a SUCCESSFUL `OR ROLLBACK` leaves the transaction alone;
/// 2. a conflicting one discards work done EARLIER in the same transaction;
/// 3. a NON-constraint failure of the same statement does not roll back —
///    sqlite's action fires on conflict resolution, not on any error.
#[test]
fn insert_or_rollback_aborts_the_transaction_only_on_a_conflict() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE)"), SQLITE_OK);

        // (1) No conflict: the row stands and so does the transaction.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, u) VALUES (1, 'a')"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT OR ROLLBACK INTO t (id, u) VALUES (2, 'b')"), SQLITE_OK);
        assert_eq!(sqlite3_get_autocommit(db), 0, "still inside the transaction");
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t"), 2);

        // (2) A conflict discards the whole transaction, including the row
        // inserted by an EARLIER statement in it.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, u) VALUES (3, 'c')"), SQLITE_OK);
        assert_eq!(
            exec(db, "INSERT OR ROLLBACK INTO t (id, u) VALUES (4, 'a')"),
            SQLITE_CONSTRAINT
        );
        assert_eq!(sqlite3_get_autocommit(db), 1, "the transaction is gone");
        // Row 3 never happened; the pre-transaction rows are intact.
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t"), 2);

        // (3) A type error is not a conflict: the transaction survives it,
        // exactly as `OR ABORT` would.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, u) VALUES (5, 'e')"), SQLITE_OK);
        assert_ne!(exec(db, "INSERT OR ROLLBACK INTO t (id, u) VALUES ('x', 'f')"), SQLITE_OK);
        assert_eq!(sqlite3_get_autocommit(db), 0, "a non-conflict error keeps the transaction");
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t"), 3);

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// Comments are the parser's business now, at every position — not just the
/// leading one the shim strips. A `;` or a parameter marker inside a comment
/// must not be seen by the statement splitter or the bind-parameter scanner
/// either, and `==` is `=`.
#[test]
fn interior_comments_and_eq_alias_reach_the_engine() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, v) VALUES (1, 10), (2, 20)"), SQLITE_OK);

        assert_eq!(scalar_count(db, "SELECT v FROM t WHERE id == 1"), 10);
        assert_eq!(scalar_count(db, "SELECT v FROM t -- trailing comment"), 10);
        assert_eq!(scalar_count(db, "SELECT /* inline */ v FROM t WHERE id = 2"), 20);
        // A `;` inside a comment is not a statement boundary.
        assert_eq!(scalar_count(db, "SELECT v FROM t WHERE id = 1 -- ; SELECT 99"), 10);
        // A `?` inside a comment is not a bound parameter.
        let s = cs("SELECT v FROM t WHERE id = ? /* not a ? here */");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_bind_parameter_count(st), 1);
        assert_eq!(sqlite3_bind_int(st, 1, 2), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(st, 0), 20);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // An unquoted identifier may carry bytes >= 0x80.
        let s = cs("SELECT 1 AS \u{00ff}");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(
            CStr::from_ptr(sqlite3_column_name(st, 0)).to_str().unwrap(),
            "\u{00ff}"
        );
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

// ===========================================================================
// `sqlite3_set_authorizer` — the compile-time access gate (#112 wave 3).
// ===========================================================================

use std::sync::Mutex as StdMutex;

/// Every consultation this process's test authorizer saw, as
/// `(action, arg1, arg2)`. One test at a time touches it.
static AUTH_LOG: StdMutex<Vec<(c_int, String, String)>> = StdMutex::new(Vec::new());
/// What the callback returns; `None` = SQLITE_OK.
static AUTH_VERDICT: StdMutex<Option<(c_int, c_int)>> = StdMutex::new(None);

unsafe extern "C" fn logging_auth(
    _ctx: *mut c_void,
    action: c_int,
    a1: *const c_char,
    a2: *const c_char,
    _db: *const c_char,
    _src: *const c_char,
) -> c_int {
    let s = |p: *const c_char| {
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    AUTH_LOG.lock().unwrap().push((action, s(a1), s(a2)));
    match *AUTH_VERDICT.lock().unwrap() {
        Some((on, rc)) if on == action => rc,
        _ => SQLITE_OK,
    }
}

fn auth_reset(verdict: Option<(c_int, c_int)>) {
    AUTH_LOG.lock().unwrap().clear();
    *AUTH_VERDICT.lock().unwrap() = verdict;
}

fn auth_log() -> Vec<(c_int, String, String)> {
    AUTH_LOG.lock().unwrap().clone()
}

unsafe fn set_auth(db: *mut Sqlite3) {
    assert_eq!(
        sqlite3_set_authorizer(db, logging_auth as *mut c_void, ptr::null_mut()),
        SQLITE_OK
    );
}

/// The action stream for the write statements CPython's own authorizer tests
/// never reach, and the DENY message shape for each: a denied column read
/// names the object ("access to t.c is prohibited"), everything else is
/// sqlite's generic "not authorized". Both carry SQLITE_AUTH.
#[test]
fn authorizer_sees_writes_and_ddl_and_denies_with_sqlites_two_messages() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (id, a, b) VALUES (1, 2, 'x')"), SQLITE_OK);
        set_auth(db);

        // INSERT names its table; nothing is read.
        auth_reset(None);
        assert_eq!(exec(db, "INSERT INTO t (id, a, b) VALUES (2, 3, 'y')"), SQLITE_OK);
        assert_eq!(auth_log(), [(18, "t".to_string(), String::new())]);

        // UPDATE names the ASSIGNED column, and reads the ones it consults.
        auth_reset(None);
        assert_eq!(exec(db, "UPDATE t SET b = 'z' WHERE a = 3"), SQLITE_OK);
        let log = auth_log();
        assert!(log.contains(&(23, "t".into(), "b".into())), "{log:?}");
        assert!(!log.contains(&(23, "t".into(), "a".into())), "a is read, not written: {log:?}");
        assert!(log.contains(&(20, "t".into(), "a".into())), "{log:?}");

        // DELETE names its table.
        auth_reset(None);
        assert_eq!(exec(db, "DELETE FROM t WHERE id = 2"), SQLITE_OK);
        assert!(auth_log().contains(&(9, "t".into(), String::new())));

        // DDL and transaction control are described too.
        auth_reset(None);
        assert_eq!(exec(db, "CREATE TABLE u (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(auth_log(), [(2, "u".to_string(), String::new())]);
        auth_reset(None);
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(auth_log(), [(22, "BEGIN".to_string(), String::new())]);
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);

        // DENY on a column read: sqlite's object-naming message.
        auth_reset(Some((20, SQLITE_DENY)));
        assert_eq!(exec(db, "SELECT b FROM t"), SQLITE_AUTH);
        assert_eq!(errmsg(db), "access to t.b is prohibited");

        // DENY on anything else: the generic message.
        auth_reset(Some((18, SQLITE_DENY)));
        assert_eq!(exec(db, "INSERT INTO t (id) VALUES (9)"), SQLITE_AUTH);
        assert_eq!(errmsg(db), "not authorized");
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t WHERE id = 9"), 0);

        // A verdict outside {OK, DENY, IGNORE} is sqlite's malfunction.
        auth_reset(Some((21, 42)));
        assert_eq!(exec(db, "SELECT b FROM t"), SQLITE_ERROR);
        assert_eq!(errmsg(db), "authorizer malfunction");

        // SQLITE_IGNORE means "read this column as NULL"; mpedb has no plan
        // rewrite for that, so it refuses rather than handing back the value
        // the callback asked to hide.
        auth_reset(Some((20, SQLITE_IGNORE)));
        assert_eq!(exec(db, "SELECT b FROM t"), SQLITE_ERROR);
        assert!(errmsg(db).contains("SQLITE_IGNORE"), "{}", errmsg(db));
        assert!(errmsg(db).contains("NULL"), "{}", errmsg(db));

        // Clearing restores the ungated connection, and the callback stops
        // being consulted at all.
        assert_eq!(sqlite3_set_authorizer(db, ptr::null_mut(), ptr::null_mut()), SQLITE_OK);
        auth_reset(Some((20, SQLITE_DENY)));
        assert_eq!(exec(db, "SELECT b FROM t"), SQLITE_OK);
        assert!(auth_log().is_empty());

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// The hidden implicit rowid (#94) must be invisible to INTROSPECTION, not
/// just to `SELECT *`: `PRAGMA table_info` listed it and the reconstructed
/// `sqlite_master.sql` declared it, so a consumer that rebuilds a schema or a
/// column list from either (CPython's `iterdump`) got a column the caller
/// never wrote — and a dump that replayed as a different table.
#[test]
fn introspection_hides_the_implicit_rowid() {
    unsafe {
        let db = open_memory();
        // No PRIMARY KEY: the engine synthesizes a hidden rowid.
        assert_eq!(exec(db, "CREATE TABLE \"alpha\" (\"one\")"), SQLITE_OK);
        // With one: nothing is hidden and the PK is real.
        assert_eq!(exec(db, "CREATE TABLE beta (id INTEGER PRIMARY KEY, v TEXT)"), SQLITE_OK);

        // One row (cid 0) — the hidden rowid is not a seventh column.
        assert_eq!(collect_text_col(db, "PRAGMA table_info(\"alpha\")"), ["0"]);
        assert_eq!(collect_text_col(db, "PRAGMA table_info(beta)"), ["0", "1"]);

        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'alpha'"),
            ["CREATE TABLE \"alpha\" (\"one\")"]
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'beta'"),
            ["CREATE TABLE \"beta\" (\"id\" INTEGER NOT NULL, \"v\" TEXT, PRIMARY KEY (\"id\"))"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// The `sqlite_master` mini-evaluator has to survive the shapes real
/// consumers write, not just single-line ones: CPython's `iterdump` breaks its
/// query across lines, quotes every identifier, uses `==`, and tests
/// `"sql" NOT NULL`.
#[test]
fn sqlite_master_evaluator_takes_the_iterdump_query_shape() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE beta (id INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE alpha (id INTEGER PRIMARY KEY)"), SQLITE_OK);

        let q = "
        SELECT \"name\"
        FROM \"sqlite_master\"
            WHERE \"sql\" NOT NULL AND
            \"type\" == 'table'
            ORDER BY \"name\"
        ";
        assert_eq!(collect_text_col(db, q), ["alpha", "beta"]);
        // Descending, and on a column other than `name`.
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master ORDER BY \"name\" DESC"),
            ["beta", "alpha"]
        );
        // `IS NULL` is the other half of the NULL test, and matches nothing:
        // every row this shim emits carries its DDL.
        assert!(collect_text_col(db, "SELECT name FROM sqlite_master WHERE sql IS NULL").is_empty());
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
                .len(),
            2
        );
        // A shape it cannot evaluate REFUSES rather than answering wrongly.
        let s = cs("SELECT name FROM sqlite_master ORDER BY nosuchcol");
        let mut st: *mut Stmt = ptr::null_mut();
        let rc = sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut());
        if rc == SQLITE_OK {
            assert_ne!(sqlite3_step(st), SQLITE_ROW, "an unevaluable ORDER BY must not answer");
            sqlite3_finalize(st);
        }
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// A consumer could not create a TRIGGER through this API at all: the
/// statement splitter cut the body at its first `;`, so `execute` reported
/// "you can only execute one statement at a time". End to end, through
/// prepare/step, including the trigger actually firing.
#[test]
fn a_trigger_can_be_created_and_fires() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t1 (a INTEGER PRIMARY KEY, b INTEGER)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE t2 (a INTEGER PRIMARY KEY, b INTEGER)"), SQLITE_OK);

        // No BEFORE/AFTER word: sqlite's documented default is BEFORE.
        let ddl = "CREATE TRIGGER tr UPDATE OF b ON t1 \
                   BEGIN UPDATE t2 SET b = new.b WHERE a = old.a; END;";
        let s = cs(ddl);
        let mut st: *mut Stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, &mut tail), SQLITE_OK);
        // The whole trigger is ONE statement: nothing is left over.
        assert!(tail.is_null() || CStr::from_ptr(tail).to_bytes().is_empty(), "tail must be empty");
        assert_eq!(sqlite3_step(st), SQLITE_DONE);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        assert_eq!(exec(db, "INSERT INTO t2 (a, b) VALUES (1, 0)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t1 (a, b) VALUES (1, 0)"), SQLITE_OK);
        assert_eq!(exec(db, "UPDATE t1 SET b = 42 WHERE a = 1"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT b FROM t2 WHERE a = 1"), 42);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
