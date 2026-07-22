//! mpedb-capi FFI tests (capi_ext): drive the exported sqlite3 C-API exactly as a C caller
//! would (raw pointers, 1-based binds, 0-based columns) and assert both the
//! result-code integers and the returned values.

#![allow(dead_code)] // each of the three capi test binaries uses the subset of
// these shared FFI helpers its own cases need (split out of capi.rs in 62c8f20).

use mpedb_sqlite3::*;
use std::ffi::{c_char, c_void, CStr, CString};
use std::os::raw::c_int;
use std::ptr;

/// A writable scratch directory: `/dev/shm` where it exists (mpedb's natural
/// habitat on Linux), the platform temp dir otherwise (macOS has no `/dev/shm`).
/// Same fallback `attach_ffi.rs` and the shim itself already use.
fn scratch_dir() -> String {
    if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm".to_string()
    } else {
        std::env::temp_dir()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string()
    }
}

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

/// The same, for a collating sequence's `xCompare`.
fn cmpptr(
    f: unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
) -> *mut c_void {
    f as *const () as *mut c_void
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

// ---- refusal-path destructor contracts (CPython heap safety) ---------------

unsafe extern "C" fn count_destroy(p: *mut c_void) {
    let c = &*(p as *const std::sync::atomic::AtomicU32);
    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// A collating sequence that reverses byte order, and its identity twin.
/// sqlite passes BYTE runs that are not NUL-terminated, so both use the lengths.
unsafe extern "C" fn reverse_cmp(
    _app: *mut c_void,
    na: c_int,
    pa: *const c_void,
    nb: c_int,
    pb: *const c_void,
) -> c_int {
    -forward_cmp(_app, na, pa, nb, pb)
}

unsafe extern "C" fn forward_cmp(
    _app: *mut c_void,
    na: c_int,
    pa: *const c_void,
    nb: c_int,
    pb: *const c_void,
) -> c_int {
    let a = std::slice::from_raw_parts(pa as *const u8, na as usize);
    let b = std::slice::from_raw_parts(pb as *const u8, nb as usize);
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Every TEXT value of the single-column query `sql`, in row order.
unsafe fn texts(db: *mut Sqlite3, sql: &str) -> Vec<String> {
    let mut st: *mut Stmt = ptr::null_mut();
    let s = cs(sql);
    assert_eq!(
        sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
        SQLITE_OK,
        "prepare {sql}: {}",
        CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy()
    );
    let mut out = Vec::new();
    while sqlite3_step(st) == SQLITE_ROW {
        let p = sqlite3_column_text(st, 0);
        out.push(CStr::from_ptr(p as *const c_char).to_string_lossy().into_owned());
    }
    sqlite3_finalize(st);
    out
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

// ===========================================================================
// `INSERT OR ROLLBACK` — the one conflict action that lives in the SHIM
// (#112 wave 3, bucket C). mpedb's parser refuses it by name because a
// statement cannot abort the transaction that contains it; the connection
// can, so the shim runs the statement as `OR ABORT` and rolls back itself.
// ===========================================================================

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

/// Column names from `PRAGMA table_info(<arg>)` (column index 1).
unsafe fn collect_pragma_col_names(db: *mut Sqlite3, table_arg: &str) -> Vec<String> {
    let s = cs(&format!("PRAGMA table_info({table_arg})"));
    let mut st: *mut Stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
        SQLITE_OK,
        "prepare table_info({table_arg})"
    );
    let mut out = Vec::new();
    while sqlite3_step(st) == SQLITE_ROW {
        let p = sqlite3_column_text(st, 1);
        if !p.is_null() {
            out.push(CStr::from_ptr(p as *const c_char).to_str().unwrap().to_string());
        }
    }
    sqlite3_finalize(st);
    out
}

/// Read column `n` of every row as text.
unsafe fn collect_col_text(db: *mut Sqlite3, sql: &str, n: i32) -> Vec<String> {
    let s = cs(sql);
    let mut st: *mut Stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
        SQLITE_OK,
        "prepare {sql}"
    );
    let mut out = Vec::new();
    while sqlite3_step(st) == SQLITE_ROW {
        let p = sqlite3_column_text(st, n);
        out.push(if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p as *const c_char).to_str().unwrap().to_string()
        });
    }
    sqlite3_finalize(st);
    out
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
        // The backup API is REAL now; what still refuses is a schema name mpedb
        // has no equivalent for, and the refusal must land on the DESTINATION.
        // (`temp` is no longer one of those — see
        // `backup_of_the_temp_schema_is_an_empty_database`.)
        let other = cs("attached_db");
        assert!(sqlite3_backup_init(dst, main.as_ptr(), src, other.as_ptr()).is_null());
        let msg = CStr::from_ptr(sqlite3_errmsg(dst)).to_string_lossy().into_owned();
        assert_eq!(msg, "unknown database attached_db", "backup errmsg {msg:?}");

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

        // Both come back as the caller wrote them (the verbatim record), which
        // is what sqlite hands back and never mentions a rowid either.
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'alpha'"),
            ["CREATE TABLE \"alpha\" (\"one\")"]
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'beta'"),
            ["CREATE TABLE beta (id INTEGER PRIMARY KEY, v TEXT)"]
        );

        // A RENAME moves the table off the name its recorded text is filed
        // under, so the answer falls back to the RECONSTRUCTION — and that is
        // the path this test is really about: it must still elide the hidden
        // rowid, column AND primary key.
        assert_eq!(exec(db, "ALTER TABLE \"alpha\" RENAME TO alpha2"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'alpha2'"),
            ["CREATE TABLE \"alpha2\" (\"one\")"]
        );
        assert_eq!(collect_text_col(db, "PRAGMA table_info(alpha2)"), ["0"]);
        // Same for a shape change under an unchanged name: `beta` grows a
        // column, and the recorded text no longer describes it.
        assert_eq!(exec(db, "ALTER TABLE beta ADD COLUMN w TEXT"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'beta'"),
            ["CREATE TABLE \"beta\" (\"id\" INTEGER NOT NULL, \"v\" TEXT, \"w\" TEXT, PRIMARY KEY (\"id\"))"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// `sqlite_master.sql` is the caller's OWN `CREATE TABLE`, byte for byte —
/// sqlite stores the statement text, and consumers diff against it (CPython's
/// `test_dump_custom_row_factory` asserts `iterdump()` re-emits
/// `CREATE TABLE test(t);` exactly). mpedb's catalog keeps the resolved schema
/// rather than the bytes, so the shim files the text in the catalog's
/// sys-keyspace and hands it back — but ONLY while it still describes this
/// exact shape, because an almost-right `CREATE TABLE` replays as a DIFFERENT
/// table.
#[test]
fn sqlite_master_returns_the_verbatim_create_table() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE test(t);"), SQLITE_OK);
        // The trailing `;` is not part of what sqlite stores.
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'test'"),
            ["CREATE TABLE test(t)"]
        );

        // sqlite does NOT store the raw bytes: it rebuilds the head as the
        // literal `CREATE TABLE ` and keeps the text from the NAME token on.
        // So the head is uppercased and re-spaced while the tail is verbatim,
        // and a trailing comment (not a token) is dropped. All four verified
        // against sqlite 3.45 — see `introspect::ddl_verbatim`.
        assert_eq!(
            exec(db, "-- lead\n  create   table   spaced ( a  int ) ; -- trail"),
            SQLITE_OK
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'spaced'"),
            ["CREATE TABLE spaced ( a  int )"]
        );
        // A `;` inside a string literal is not a terminator, and the text runs
        // to the last real token past it.
        assert_eq!(exec(db, "CREATE TABLE semi (a TEXT DEFAULT 'x;y')"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'semi'"),
            ["CREATE TABLE semi (a TEXT DEFAULT 'x;y')"]
        );

        // A DROP forgets the text: a table recreated by some other route (or
        // in another process) must not inherit the old spelling.
        assert_eq!(exec(db, "DROP TABLE test"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE \"test\" (\"t\")"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'test'"),
            ["CREATE TABLE \"test\" (\"t\")"]
        );

        // A `CREATE TABLE` inside an open transaction IS recorded: the shim
        // fingerprints against the WriteSession schema and the sys-record
        // rides the same txn (CPython `test_table_dump` / iterdump mid-session).
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE intxn (a int NOT NULL, b TEXT)"), SQLITE_OK);
        // Visible mid-transaction with the caller's own text.
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'intxn'"),
            ["CREATE TABLE intxn (a int NOT NULL, b TEXT)"]
        );
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'intxn'"),
            ["CREATE TABLE intxn (a int NOT NULL, b TEXT)"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// VIEW/TRIGGER `sqlite_master.sql` is also the caller's own text (CPython
/// `test_table_dump` asserts spelling of both, not a reconstruction).
#[test]
fn sqlite_master_returns_verbatim_create_view_and_trigger() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE t1(id integer primary key, t1_i1 integer, i2 integer)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(db, "CREATE TABLE t2(id integer primary key, t2_i1 integer, t2_i2 integer)"),
            SQLITE_OK
        );
        assert_eq!(
            exec(
                db,
                "CREATE TRIGGER trigger_1 update of t1_i1 on t1 begin \
                 update t2 set t2_i1 = new.t1_i1 where t2_i1 = old.t1_i1; end;"
            ),
            SQLITE_OK
        );
        assert_eq!(
            exec(db, "CREATE VIEW v1 as select * from t1 left join t2 using (id);"),
            SQLITE_OK
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'v1'"),
            ["CREATE VIEW v1 as select * from t1 left join t2 using (id)"]
        );
        assert_eq!(
            collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 'trigger_1'"),
            ["CREATE TRIGGER trigger_1 update of t1_i1 on t1 begin \
              update t2 set t2_i1 = new.t1_i1 where t2_i1 = old.t1_i1; end"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// CPython `test_table_dump` / `iterdump`: (1) `PRAGMA table_info("quoted""table")`
/// must un-escape doubled quotes; (2) mid-transaction `CREATE TABLE` (Python's
/// default isolation starts a txn on INSERT) must still answer `table_info`
/// from the open WriteSession so the dump does not emit bare `VALUES()`.
#[test]
fn table_info_unescapes_quotes_and_sees_mid_txn_creates() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, r#"CREATE TABLE "quoted""table"("quoted""field" text)"#),
            SQLITE_OK
        );
        assert_eq!(
            collect_pragma_col_names(db, r#""quoted""table""#),
            [r#"quoted"field"#],
            "doubled quotes in table_info arg must resolve"
        );
        // Mid-txn create after an INSERT (mirrors CPython isolation).
        assert_eq!(exec(db, "CREATE TABLE seed(x)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO seed VALUES (1)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE later(y TEXT)"), SQLITE_OK);
        let cols = collect_pragma_col_names(db, "later");
        assert_eq!(cols, ["y"], "mid-txn table_info saw {cols:?}");
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


// ---------------------------------------------------------- index catalog (#119)

/// Every row of `sql` as `"<typename>:<text>"`, so a test asserts the value AND
/// the storage class — a NULL `sqlite_master.sql` is what tells a consumer
/// "constraint index", and `''` would be a different answer, not a near one.
unsafe fn typed_rows(db: *mut Sqlite3, sql: &str) -> Vec<String> {
    let s = cs(sql);
    let mut st: *mut Stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
        SQLITE_OK,
        "prepare {sql}: {}",
        errmsg(db)
    );
    let mut out = Vec::new();
    while sqlite3_step(st) == SQLITE_ROW {
        let mut cells = Vec::new();
        for i in 0..sqlite3_column_count(st) {
            let ty = sqlite3_column_type(st, i);
            let tyname = match ty {
                SQLITE_NULL => "null",
                SQLITE_INTEGER => "int",
                SQLITE_FLOAT => "real",
                SQLITE_TEXT => "text",
                _ => "blob",
            };
            let p = sqlite3_column_text(st, i);
            let txt = if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p as *const c_char).to_str().unwrap().to_string()
            };
            cells.push(format!("{tyname}:{txt}"));
        }
        out.push(cells.join("|"));
    }
    sqlite3_finalize(st);
    out
}

/// `sqlite_master` carries INDEX rows, with the caller's own `CREATE INDEX`
/// text — the half of the catalog that used to be missing entirely, and the
/// reason a dump replayed into a schema without its indexes.
///
/// Every expectation is the bundled 3.45.0 oracle's own answer to the same
/// script (`crates/mpedb/tests/sqlite_oracle`), including the two rules that
/// are NOT guessable and differ from `CREATE TABLE`:
///
/// * a `CREATE INDEX`'s stored text runs to just before the `;`, so trailing
///   whitespace and interior comments survive where a table's do not;
/// * `UNIQUE` belongs to the head sqlite rebuilds, so it comes back uppercased.
///
/// A constraint index (`UNIQUE`/`PRIMARY KEY` inside the `CREATE TABLE`) is
/// named `sqlite_autoindex_<table>_<k>` and its `sql` is NULL, exactly as
/// sqlite reports it.
#[test]
fn sqlite_master_lists_indexes_with_the_callers_own_text() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT)"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(exec(db, "CREATE INDEX ix_b ON t(b)"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(exec(db, "CREATE UNIQUE INDEX ux_a ON t(a)"), SQLITE_OK, "{}", errmsg(db));
        // Head normalized and uppercased, tail verbatim; `IF NOT EXISTS` and a
        // `main.` qualifier are dropped.
        assert_eq!(
            exec(db, "create   unique index  if not exists  main.\"quoted idx\" on t ( b )   ;  -- trail"),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );

        assert_eq!(
            typed_rows(db, "SELECT type, name, tbl_name, sql FROM sqlite_master"),
            [
                "text:table|text:t|text:t|text:CREATE TABLE t (a INTEGER, b TEXT)",
                "text:index|text:ix_b|text:t|text:CREATE INDEX ix_b ON t(b)",
                "text:index|text:ux_a|text:t|text:CREATE UNIQUE INDEX ux_a ON t(a)",
                // Three trailing spaces: sqlite keeps everything up to the `;`.
                "text:index|text:quoted idx|text:t|text:CREATE UNIQUE INDEX \"quoted idx\" on t ( b )   ",
            ]
        );

        // A UNIQUE column constraint is an index sqlite NAMES but gives no
        // statement text — and the NULL is load-bearing (Django's
        // `get_constraints` does `if not sql: continue`).
        assert_eq!(exec(db, "CREATE TABLE u (a INTEGER PRIMARY KEY, b TEXT UNIQUE)"), SQLITE_OK);
        assert_eq!(
            typed_rows(db, "SELECT name, sql FROM sqlite_master WHERE tbl_name = 'u'"),
            [
                "text:u|text:CREATE TABLE u (a INTEGER PRIMARY KEY, b TEXT UNIQUE)",
                "text:sqlite_autoindex_u_1|null:",
            ]
        );
        // CPython's iterdump second pass filters on exactly that NULL.
        assert_eq!(
            collect_text_col(
                db,
                "SELECT name FROM sqlite_master WHERE sql NOT NULL AND type IN ('index','trigger','view')"
            ),
            ["ix_b", "ux_a", "quoted idx"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// `PRAGMA index_list` reports the REAL index name, the right `origin`, and the
/// `partial` bit — and reports newest-first, as sqlite does. `PRAGMA index_info`
/// exists at all, which is the third call in Django's `get_constraints` chain.
///
/// Oracle (3.45.0) for the same script, verbatim:
/// ```text
/// --index_list u--   0|part|0|c|1   1|spaced|0|c|0   2|sqlite_autoindex_u_1|1|u|0
/// --index_info ix_b--   0|1|b
/// ```
/// Before this, EVERY entry came back as `sqlite_autoindex_<t>_<k>` with origin
/// `c`: a fabricated name for a real `CREATE INDEX`, which then resolved to no
/// `sqlite_master` row at all.
#[test]
fn pragma_index_list_and_index_info_match_sqlites_shape() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(db, "CREATE TABLE u (a INTEGER PRIMARY KEY, b TEXT UNIQUE, c TEXT)"),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );
        assert_eq!(exec(db, "CREATE INDEX spaced ON u(c)"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(
            exec(db, "CREATE INDEX part ON u(c) WHERE c IS NOT NULL"),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );
        assert_eq!(
            typed_rows(db, "PRAGMA index_list(u)"),
            [
                "int:0|text:part|int:0|text:c|int:1",
                "int:1|text:spaced|int:0|text:c|int:0",
                "int:2|text:sqlite_autoindex_u_1|int:1|text:u|int:0",
            ]
        );
        // seqno | cid | name, with `cid` in `table_info`'s numbering.
        assert_eq!(typed_rows(db, "PRAGMA index_info(spaced)"), ["int:0|int:2|text:c"]);
        assert_eq!(
            typed_rows(db, "PRAGMA index_info(sqlite_autoindex_u_1)"),
            ["int:0|int:1|text:b"]
        );
        // An unknown index is zero rows, not an error (sqlite's answer).
        assert!(typed_rows(db, "PRAGMA index_info(nope)").is_empty());

        // An `INTEGER PRIMARY KEY` is a rowid alias: sqlite builds NO index for
        // it, and neither does this — but a TEXT or composite PK does get one.
        assert_eq!(exec(db, "CREATE TABLE v (a TEXT PRIMARY KEY, b TEXT)"), SQLITE_OK);
        assert_eq!(
            typed_rows(db, "PRAGMA index_list(v)"),
            ["int:0|text:sqlite_autoindex_v_1|int:1|text:pk|int:0"]
        );
        assert_eq!(exec(db, "CREATE TABLE w (a INTEGER, b TEXT, PRIMARY KEY (a, b))"), SQLITE_OK);
        assert_eq!(
            typed_rows(db, "PRAGMA index_info(sqlite_autoindex_w_1)"),
            ["int:0|int:0|text:a", "int:1|int:1|text:b"]
        );
        // A table with a HIDDEN rowid (#94: no declared PK) has no PK index at
        // all — emitting one would advertise a column `SELECT *` cannot see.
        assert_eq!(exec(db, "CREATE TABLE h (a INTEGER, b TEXT)"), SQLITE_OK);
        assert!(typed_rows(db, "PRAGMA index_list(h)").is_empty());
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// Django's `get_constraints` reaches `sqlite_master` ONLY through bound
/// parameters (`WHERE type='table' and name=%s`, then `type='index' AND
/// name=%s`). The shim's mini-evaluator used to read a literal or refuse, so
/// the whole method raised `unsupported` on the very first query.
#[test]
fn sqlite_master_where_accepts_a_bound_parameter() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT UNIQUE)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE INDEX ix ON t(a)"), SQLITE_OK);

        let bound = |sql: &str, v: &str| -> Vec<String> {
            let s = cs(sql);
            let mut st: *mut Stmt = ptr::null_mut();
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
                SQLITE_OK,
                "{}",
                errmsg(db)
            );
            let val = cs(v);
            assert_eq!(
                sqlite3_bind_text(st, 1, val.as_ptr(), -1, sqlite_transient()),
                SQLITE_OK
            );
            let mut out = Vec::new();
            loop {
                let rc = sqlite3_step(st);
                if rc == SQLITE_ROW {
                    let ty = sqlite3_column_type(st, 0);
                    let p = sqlite3_column_text(st, 0);
                    out.push(if ty == SQLITE_NULL {
                        "null:".to_string()
                    } else {
                        format!(
                            "text:{}",
                            CStr::from_ptr(p as *const c_char).to_str().unwrap()
                        )
                    });
                    continue;
                }
                assert_eq!(rc, SQLITE_DONE, "step {sql}: {}", errmsg(db));
                break;
            }
            sqlite3_finalize(st);
            out
        };

        assert_eq!(
            bound("SELECT sql FROM sqlite_master WHERE type='table' and name=?", "t"),
            ["text:CREATE TABLE t (a INTEGER, b TEXT UNIQUE)"]
        );
        assert_eq!(
            bound("SELECT sql FROM sqlite_master WHERE type='index' AND name=?", "ix"),
            ["text:CREATE INDEX ix ON t(a)"]
        );
        // The constraint index resolves too — to a NULL, which is the answer
        // Django reads as "inline constraint, already parsed".
        assert_eq!(
            bound(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name=?",
                "sqlite_autoindex_t_1"
            ),
            ["null:"]
        );
        // Named and numbered bindings go through the same rewrite.
        assert_eq!(bound("SELECT name FROM sqlite_master WHERE name = :n", "ix"), ["text:ix"]);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// A dropped table takes its index NAMES with it: a table re-created with the
/// same shape must not inherit a `CREATE INDEX` text nobody wrote for it.
#[test]
fn dropping_a_table_forgets_its_index_names() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE INDEX ix_b ON t(b)"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE type = 'index'"),
            ["ix_b"]
        );
        assert_eq!(exec(db, "DROP TABLE t"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT UNIQUE)"), SQLITE_OK);
        // The UNIQUE constraint's index has the SAME shape the dropped
        // `ix_b`... it does not: `unique` differs. Assert the name anyway —
        // a leaked record would have surfaced as `ix_b`.
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE type = 'index'"),
            ["sqlite_autoindex_t_1"]
        );
        assert_eq!(exec(db, "DROP TABLE t"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE INDEX other ON t(b)"), SQLITE_OK);
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE type = 'index'"),
            ["other"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// Read-your-own-writes at the METADATA level: every introspection surface sees
/// a table the previous statement committed, with nothing in between — on the
/// connection that created it AND on a sibling connection to the same file.
///
/// `Database::schema()` hands back this process's cached bundle, which a DDL
/// commit does not touch; only `schema_gen` in the flipping meta moves. Every
/// surface here therefore has to consult it. The SQL paths do that through the
/// facade's own `gate_cache_on_schema`; `sqlite_master`/`PRAGMA` do it in
/// `exec_one_inner`; `sqlite3_blob_open` bypasses SQL entirely and used to
/// answer `no such table` for a table that demonstrably existed, until some
/// other statement on the handle happened to reload the bundle.
#[test]
fn introspection_sees_what_the_previous_statement_committed() {
    unsafe {
        // Same connection, no intervening statement.
        let db = open_memory();
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE fresh (a INTEGER, s TEXT)"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(
            collect_text_col(db, "SELECT name FROM sqlite_master WHERE type='table'"),
            ["fresh"]
        );
        assert_eq!(collect_text_col(db, "PRAGMA table_info(fresh)"), ["0", "1"]);
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        // Two connections on one file: B has never seen the table.
        let path = format!("{}/mpedb-capi-fresh-{}.db", scratch_dir(), std::process::id());
        let _ = std::fs::remove_file(&path);
        let mut a: *mut Sqlite3 = ptr::null_mut();
        let mut b: *mut Sqlite3 = ptr::null_mut();
        let p = cs(&path);
        assert_eq!(sqlite3_open(p.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(sqlite3_open(p.as_ptr(), &mut b), SQLITE_OK);
        assert_eq!(exec(a, "CREATE TABLE seed (x INTEGER)"), SQLITE_OK, "{}", errmsg(a));
        // B's FIRST look already sees it.
        assert_eq!(collect_text_col(b, "SELECT name FROM sqlite_master"), ["seed"]);

        assert_eq!(exec(a, "CREATE INDEX ix_x ON seed(x)"), SQLITE_OK, "{}", errmsg(a));
        assert_eq!(collect_text_col(b, "PRAGMA index_list(seed)"), ["0"]);
        assert_eq!(collect_col_text(b, "PRAGMA index_list(seed)", 1), ["ix_x"]);

        // Incremental blob I/O on a table+row B has never touched.
        assert_eq!(
            exec(a, "CREATE TABLE bl (id INTEGER PRIMARY KEY, d BLOB)"),
            SQLITE_OK,
            "{}",
            errmsg(a)
        );
        assert_eq!(exec(a, "INSERT INTO bl VALUES (1, x'0011223344')"), SQLITE_OK, "{}", errmsg(a));
        let mut blob: *mut c_void = ptr::null_mut();
        let (m, t, col) = (cs("main"), cs("bl"), cs("d"));
        assert_eq!(
            sqlite3_blob_open(b, m.as_ptr(), t.as_ptr(), col.as_ptr(), 1, 0, &mut blob),
            SQLITE_OK,
            "blob_open on a sibling connection: {}",
            errmsg(b)
        );
        assert_eq!(sqlite3_blob_bytes(blob), 5);
        let (rc, bytes) = blob_read_vec(blob, 5, 0);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(bytes, vec![0x00, 0x11, 0x22, 0x33, 0x44]);
        assert_eq!(sqlite3_blob_close(blob), SQLITE_OK);

        assert_eq!(sqlite3_close(a), SQLITE_OK);
        assert_eq!(sqlite3_close(b), SQLITE_OK);
        let _ = std::fs::remove_file(&path);
    }
}
