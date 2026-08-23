use super::*;
use super::helpers::*;

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

        // A DDL-declared `timestamp` is sqlite's NUMERIC affinity (task #113):
        // the per-value column, exactly like `date`/`datetime`. It used to be a
        // rigid ColumnType::Timestamp, which no value reachable through this
        // shim could fill — every consumer sends an integer or an ISO string,
        // and both were refused. `typeof` now answers a class sqlite HAS a name
        // for, per value, and `PARSE_DECLTYPES` still sees `TIMESTAMP` because
        // the decltype is the verbatim declared text.
        assert_eq!(
            exec(db, "CREATE TABLE ts (id integer PRIMARY KEY, t timestamp)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(
                db,
                "INSERT INTO ts VALUES (1, 1720000000000000), (2, '2004-02-14 07:15:00')"
            ),
            SQLITE_OK
        );
        let s = cs("SELECT typeof(t), t FROM ts ORDER BY id");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        let mut got = Vec::new();
        while sqlite3_step(st) == SQLITE_ROW {
            got.push((col_text(st, 0), sqlite3_column_type(st, 1)));
        }
        assert_eq!(
            got,
            vec![
                ("integer".into(), SQLITE_INTEGER),
                ("text".to_string(), SQLITE_TEXT),
            ]
        );
        assert_eq!(
            CStr::from_ptr(sqlite3_column_decltype(st, 1)).to_string_lossy(),
            "timestamp"
        );
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

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

        // `foreign_keys` is REAL since #194 — it was 0-through-a-set while
        // mpedb enforced nothing (old C-API-COMPAT gap D11), and reporting 1
        // then would have promised enforcement that did not exist. Now the
        // setter moves the connection's state and the getter reports it.
        // sqlite's default is OFF, and so is mpedb's.
        assert_eq!(one("PRAGMA foreign_keys"), ("foreign_keys".into(), 0));
        assert_eq!(exec(db, "PRAGMA foreign_keys = ON"), SQLITE_OK);
        assert_eq!(one("PRAGMA foreign_keys"), ("foreign_keys".into(), 1));
        // INSIDE a transaction it is a SILENT no-op — measured against sqlite
        // 3.45.1, which keeps the old value rather than erroring.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "PRAGMA foreign_keys = OFF"), SQLITE_OK);
        assert_eq!(one("PRAGMA foreign_keys"), ("foreign_keys".into(), 1));
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(exec(db, "PRAGMA foreign_keys = OFF"), SQLITE_OK);
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


#[test]
fn collation_registration_orders_sorts_and_honors_the_destructor_contract() {
    use std::sync::atomic::{AtomicU32, Ordering};
    unsafe {
        let db = open_memory();
        let hits = AtomicU32::new(0);
        let app = &hits as *const AtomicU32 as *mut c_void;

        // A registration SUCCEEDS and the destructor does NOT run (it runs when
        // the entry is replaced/deleted/closed, not now).
        let name = cs("mycoll");
        let rc = sqlite3_create_collation_v2(
            db,
            name.as_ptr(),
            1, // SQLITE_UTF8
            app,
            cmpptr(reverse_cmp),
            fnptr1(count_destroy),
        );
        assert_eq!(rc, SQLITE_OK, "custom collations are registered");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "destructor does not run on success");

        // …and it ORDERS: `reverse_cmp` reverses byte order.
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')"), SQLITE_OK);
        assert_eq!(texts(db, "SELECT s FROM t ORDER BY s COLLATE mycoll"), ["c", "b", "a"]);
        // A plain ORDER BY is untouched by the registration.
        assert_eq!(texts(db, "SELECT s FROM t ORDER BY s"), ["a", "b", "c"]);

        // Re-registering under the same name REPLACES and runs the OLD
        // destructor (sqlite's rule; CPython's `test_collation_register_twice`).
        let rc = sqlite3_create_collation_v2(
            db,
            name.as_ptr(),
            1,
            app,
            cmpptr(forward_cmp),
            ptr::null_mut(),
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "replaced entry's destructor ran");
        assert_eq!(texts(db, "SELECT s FROM t ORDER BY s COLLATE mycoll"), ["a", "b", "c"]);

        // A NULL xCompare DELETES it, and a statement naming it then fails with
        // sqlite's exact wording — never a silent fallback to BINARY, which
        // would be a different row ORDER with no error.
        let rc =
            sqlite3_create_collation_v2(db, name.as_ptr(), 1, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        assert_eq!(rc, SQLITE_OK);
        let mut st: *mut Stmt = ptr::null_mut();
        let sql = cs("SELECT s FROM t ORDER BY s COLLATE mycoll");
        let rc = sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut());
        let msg = CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy().into_owned();
        assert_ne!(rc, SQLITE_OK, "a deregistered collation cannot be used");
        assert_eq!(msg, "no such collation sequence: mycoll", "sqlite's exact wording");

        // The window-function registration's destructor contract, both ways:
        // sqlite runs `xDestroy(pApp)` on a FAILED registration (CPython relies
        // on that by not freeing), and also on the all-NULL DELETE form, which
        // succeeds.
        let wname = cs("mywin");
        let rc = sqlite3_create_window_function(
            db,
            wname.as_ptr(),
            -2, // outside -1..=127
            1,  // SQLITE_UTF8
            app,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            fnptr1(count_destroy),
        );
        assert_eq!(rc, SQLITE_MISUSE, "nArg outside -1..=127 is a misuse");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "window-function destructor MUST run on failure (CPython does not free otherwise)"
        );
        let rc = sqlite3_create_window_function(
            db,
            wname.as_ptr(),
            1,
            1,
            app,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            fnptr1(count_destroy),
        );
        assert_eq!(rc, SQLITE_OK, "all-NULL callbacks DELETE the entry");
        assert_eq!(hits.load(Ordering::SeqCst), 3, "...and still run the destructor");
        sqlite3_close(db);
    }
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

/// `sqlite3_extended_result_codes()` decides what `sqlite3_errcode()` answers.
///
/// The extended code has always been tracked — `sqlite3_extended_errcode()`
/// returned 1555 for a primary-key collision long before this test existed —
/// but the toggle was a no-op, so `sqlite3_errcode()` handed out the coarse 19
/// no matter what the caller had asked for. That is never a WRONG answer, only
/// a less precise one, which is exactly why nothing caught it: a consumer that
/// enables extended codes and receives base codes cannot tell the difference
/// from a database that simply has nothing finer to say.
///
/// Two of PHP's own tests read this door and no other: SQLite3::lastErrorCode()
/// and PDO's errorInfo()[1] both call `sqlite3_errcode()`. The expectations
/// below are theirs — 19 with the toggle off, 1555 with it on, for a collision
/// on INTEGER PRIMARY KEY (SQLITE_CONSTRAINT_PRIMARYKEY, not _UNIQUE).
#[test]
fn extended_result_codes_toggle_governs_errcode_and_not_extended_errcode() {
    unsafe {
        let db = open_memory();
        exec(db, "CREATE TABLE dog (id INTEGER PRIMARY KEY, name TEXT)");
        exec(db, "INSERT INTO dog VALUES (1, 'Annoying Dog')");

        let collide = || {
            let s = cs("INSERT INTO dog VALUES (1, 'Annoying Dog')");
            let mut st: *mut Stmt = ptr::null_mut();
            assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
            let rc = sqlite3_step(st);
            sqlite3_finalize(st);
            rc
        };

        // Off by default, as in sqlite — a caller that never asks keeps the
        // coarse code, and PHP's first two lastErrorCode() reads expect 19.
        collide();
        assert_eq!(sqlite3_errcode(db), SQLITE_CONSTRAINT, "base code with the toggle off");
        assert_eq!(
            sqlite3_extended_errcode(db),
            SQLITE_CONSTRAINT_PRIMARYKEY,
            "the extended code is tracked either way — the toggle governs who is TOLD"
        );

        assert_eq!(sqlite3_extended_result_codes(db, 1), SQLITE_OK);
        collide();
        assert_eq!(sqlite3_errcode(db), SQLITE_CONSTRAINT_PRIMARYKEY, "1555 once asked for");
        assert_eq!(sqlite3_extended_errcode(db), SQLITE_CONSTRAINT_PRIMARYKEY);

        // And back: the switch is a switch, not a latch.
        assert_eq!(sqlite3_extended_result_codes(db, 0), SQLITE_OK);
        collide();
        assert_eq!(sqlite3_errcode(db), SQLITE_CONSTRAINT);

        // A NULL handle is misuse, not a silent success.
        assert_eq!(sqlite3_extended_result_codes(ptr::null_mut(), 1), SQLITE_MISUSE);

        sqlite3_close(db);
    }
}
