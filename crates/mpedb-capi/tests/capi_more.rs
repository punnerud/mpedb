//! mpedb-capi FFI tests (capi_more): drive the exported sqlite3 C-API exactly as a C caller
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


/// `EXPLAIN QUERY PLAN <stmt>` — sqlite's plan statement, which mpedb spells
/// `EXPLAIN <stmt>`. Django's `QuerySet.explain()` and every sqlite tool emit
/// the sqlite spelling, and it used to fail to parse ("expected a statement").
/// The shape is sqlite's: four columns `(id, parent, notused, detail)`, at
/// least one row, `detail` non-empty. The CONTENT is mpedb's own plan text —
/// sqlite documents EQP output as human-facing and unstable — so this asserts
/// the shape and that the plan names the table, not sqlite's wording.
#[test]
fn explain_query_plan_answers_in_sqlites_shape() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE u (id INTEGER PRIMARY KEY, tid INTEGER NOT NULL)"), SQLITE_OK);

        let s = cs("EXPLAIN QUERY PLAN SELECT * FROM t WHERE name = 'x'");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_count(st), 4);
        for (i, want) in ["id", "parent", "notused", "detail"].iter().enumerate() {
            let p = sqlite3_column_name(st, i as c_int);
            assert_eq!(CStr::from_ptr(p).to_str().unwrap(), *want);
        }
        // Row 1 is the root: id 1, parent 0, notused 0, and it names the table.
        assert_eq!(sqlite3_column_int64(st, 0), 1);
        assert_eq!(sqlite3_column_int64(st, 1), 0);
        assert_eq!(sqlite3_column_int64(st, 2), 0);
        let root = col_text(st, 3);
        assert!(root.contains('t'), "root detail should name the plan: {root}");
        // Every further row is a child of a row above it, never of itself.
        let mut n = 1i64;
        while sqlite3_step(st) == SQLITE_ROW {
            n += 1;
            let id = sqlite3_column_int64(st, 0);
            let parent = sqlite3_column_int64(st, 1);
            assert_eq!(id, n);
            assert!(parent < id, "parent {parent} must precede id {id}");
            assert!(!col_text(st, 3).is_empty());
        }
        assert!(n > 1, "a plan should describe more than one line");
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // Lowercase, and a join — the words are matched case-insensitively and
        // the nesting produces a non-zero parent somewhere.
        let sql = "explain query plan SELECT t.id FROM t LEFT OUTER JOIN u ON t.id = u.tid";
        let s = cs(sql);
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        let mut nested = false;
        let mut rows = 0;
        while sqlite3_step(st) == SQLITE_ROW {
            rows += 1;
            nested |= sqlite3_column_int64(st, 1) != 0;
        }
        assert!(rows > 0 && nested, "join plan should nest: {rows} rows");
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // `EXPLAIN` alone is NOT taken: sqlite answers it with a VDBE opcode
        // listing mpedb has no equivalent of, so it keeps mpedb's own
        // single-column plan text rather than a fabricated opcode table.
        let s = cs("EXPLAIN SELECT * FROM t");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()), SQLITE_OK);
        assert_eq!(sqlite3_step(st), SQLITE_ROW);
        assert_eq!(sqlite3_column_count(st), 1);
        assert_eq!(sqlite3_finalize(st), SQLITE_OK);

        // A statement that does not compile still reports the compile error.
        assert_ne!(exec(db, "EXPLAIN QUERY PLAN SELECT * FROM nosuch"), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// `PRAGMA table_info` HIDES generated columns and renumbers `cid` over the ones
/// it lists; `PRAGMA table_xinfo` lists them all and adds the seventh `hidden`
/// column (0 = ordinary, 2 = VIRTUAL, 3 = STORED). Django's sqlite3
/// introspection unpacks exactly seven values from `table_xinfo` and filters on
/// `hidden`, so an aliased six-column answer is not a narrower one — it is an
/// unpacking error on every table in the project.
#[test]
fn table_info_hides_generated_columns_and_xinfo_reports_hidden() {
    unsafe {
        let db = open_memory();
        assert_eq!(
            exec(
                db,
                "CREATE TABLE t (a INTEGER PRIMARY KEY, \
                 g INTEGER GENERATED ALWAYS AS (a * 2) STORED, \
                 b INTEGER, v INTEGER GENERATED ALWAYS AS (a + 1) VIRTUAL)"
            ),
            SQLITE_OK
        );

        // table_info: only `a` and `b`, renumbered 0,1 (sqlite numbers the rows
        // it emits, not the columns of the table).
        assert_eq!(collect_text_col(db, "PRAGMA table_info(t)"), ["0", "1"]);
        assert_eq!(
            collect_col_text(db, "PRAGMA table_info(t)", 1),
            ["a", "b"]
        );

        // table_xinfo: all four, TRUE ordinals, with the hidden codes.
        assert_eq!(
            collect_col_text(db, "PRAGMA table_xinfo(t)", 0),
            ["0", "1", "2", "3"]
        );
        assert_eq!(
            collect_col_text(db, "PRAGMA table_xinfo(t)", 1),
            ["a", "g", "b", "v"]
        );
        assert_eq!(
            collect_col_text(db, "PRAGMA table_xinfo(t)", 6),
            ["0", "3", "0", "2"]
        );

        // The reconstructed sqlite_master DDL carries the clause, so a dump
        // replays as the same table rather than one with an ordinary column
        // that the dump's INSERTs (built from `table_info`) never fill.
        let ddl = collect_text_col(db, "SELECT sql FROM sqlite_master WHERE name = 't'");
        assert!(
            ddl[0].contains("GENERATED ALWAYS AS (a * 2) STORED")
                && ddl[0].contains("GENERATED ALWAYS AS (a + 1) VIRTUAL"),
            "{ddl:?}"
        );

        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}


/// The online backup API, end to end: a consistent copy of the source lands in
/// the destination, the destination's own contents are GONE (a backup replaces,
/// it does not merge), and progress is reported in real pages.
#[test]
fn backup_copies_the_source_over_the_destination() {
    unsafe {
        let src = open_memory();
        let dst = open_memory();
        assert_eq!(exec(src, "CREATE TABLE foo (key INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(src, "INSERT INTO foo (key) VALUES (3), (4)"), SQLITE_OK);
        // The destination starts with a DIFFERENT schema, which must not survive.
        assert_eq!(exec(dst, "CREATE TABLE gone (x INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(dst, "INSERT INTO gone (x) VALUES (1)"), SQLITE_OK);

        let main = cs("main");
        let b = sqlite3_backup_init(dst, main.as_ptr(), src, main.as_ptr());
        assert!(!b.is_null(), "backup_init: {}", errmsg(dst));
        let total = sqlite3_backup_pagecount(b);
        assert!(total > 0, "pagecount {total}");
        assert_eq!(sqlite3_backup_remaining(b), total);
        // Paced: one page at a time until DONE, and `remaining` walks down.
        let mut steps = 0;
        loop {
            let rc = sqlite3_backup_step(b, 1);
            steps += 1;
            if rc == SQLITE_DONE {
                break;
            }
            assert_eq!(rc, SQLITE_OK, "step {steps}: {}", errmsg(dst));
            assert_eq!(sqlite3_backup_remaining(b), total - steps);
        }
        assert_eq!(steps, total, "one page per step");
        assert_eq!(sqlite3_backup_remaining(b), 0);
        assert_eq!(sqlite3_backup_finish(b), SQLITE_OK);

        assert_eq!(collect_text_col(dst, "SELECT key FROM foo ORDER BY key"), ["3", "4"]);
        // The destination's old table is gone, and the copy is WRITABLE (the
        // image's volatile control state was voided, so the writer mutex and
        // reader table were re-initialized on attach).
        assert_eq!(exec(dst, "SELECT x FROM gone"), SQLITE_ERROR);
        assert_eq!(exec(dst, "INSERT INTO foo (key) VALUES (5)"), SQLITE_OK);
        assert_eq!(collect_text_col(dst, "SELECT key FROM foo ORDER BY key"), ["3", "4", "5"]);
        // ...and the SOURCE is untouched by the write to its copy.
        assert_eq!(collect_text_col(src, "SELECT key FROM foo ORDER BY key"), ["3", "4"]);

        sqlite3_close(dst);
        sqlite3_close(src);
    }
}


/// A backup abandoned before `SQLITE_DONE` leaves the destination exactly as it
/// was — the image is captured into a temporary file and only installed by the
/// step that completes it.
#[test]
fn an_abandoned_backup_leaves_the_destination_alone() {
    unsafe {
        let src = open_memory();
        let dst = open_memory();
        assert_eq!(exec(src, "CREATE TABLE foo (key INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(dst, "CREATE TABLE keep (x INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(dst, "INSERT INTO keep (x) VALUES (1)"), SQLITE_OK);

        let main = cs("main");
        let b = sqlite3_backup_init(dst, main.as_ptr(), src, main.as_ptr());
        assert!(!b.is_null());
        assert_eq!(sqlite3_backup_step(b, 1), SQLITE_OK);
        // A connection may not be closed with a backup outstanding.
        assert_eq!(sqlite3_close(dst), SQLITE_BUSY);
        assert_eq!(sqlite3_backup_finish(b), SQLITE_OK);

        assert_eq!(collect_text_col(dst, "SELECT x FROM keep"), ["1"]);
        sqlite3_close(dst);
        sqlite3_close(src);
    }
}


/// `sqlite3_backup_init` refuses — with the reason on the DESTINATION — every
/// shape mpedb cannot honor, and a destination mid-transaction is one of them.
#[test]
fn backup_init_refuses_by_name() {
    unsafe {
        let src = open_memory();
        let dst = open_memory();
        let main = cs("main");
        assert_eq!(exec(src, "CREATE TABLE t (a INTEGER PRIMARY KEY)"), SQLITE_OK);

        // Same connection.
        assert!(sqlite3_backup_init(dst, main.as_ptr(), dst, main.as_ptr()).is_null());
        assert_eq!(errmsg(dst), "source and destination must be distinct");

        // An ATTACHed schema name mpedb has no equivalent for.
        let other = cs("attached_db");
        assert!(sqlite3_backup_init(dst, main.as_ptr(), src, other.as_ptr()).is_null());
        assert_eq!(errmsg(dst), "unknown database attached_db");

        // `temp` as a DESTINATION: it exists, but there would be nothing to
        // read the copy back out of, so it is refused with the real reason
        // rather than the wrong one ("unknown database").
        let temp = cs("temp");
        assert!(sqlite3_backup_init(dst, temp.as_ptr(), src, main.as_ptr()).is_null());
        assert_eq!(errmsg(dst), "cannot back up INTO temp: mpedb has no temp database");

        // Destination mid-transaction: it is about to be REPLACED.
        assert_eq!(exec(dst, "BEGIN"), SQLITE_OK);
        assert!(sqlite3_backup_init(dst, main.as_ptr(), src, main.as_ptr()).is_null());
        assert_eq!(errmsg(dst), "target is in transaction");
        assert_eq!(exec(dst, "ROLLBACK"), SQLITE_OK);

        // Source mid-transaction: it holds the writer lock the capture needs.
        assert_eq!(exec(src, "BEGIN"), SQLITE_OK);
        assert!(sqlite3_backup_init(dst, main.as_ptr(), src, main.as_ptr()).is_null());
        assert_eq!(errmsg(dst), "source database is locked");
        assert_eq!(exec(src, "ROLLBACK"), SQLITE_OK);

        sqlite3_close(dst);
        sqlite3_close(src);
    }
}


/// Every sqlite connection has a `temp` schema, so `backup(name='temp')` is
/// never an error there — CPython's `test_backup.test_database_source_name`
/// asserts exactly that. mpedb refuses every statement that could put anything
/// in a temp schema, so its temp is provably EMPTY, and the copy of an empty
/// database is what the destination must end up as: the backup SUCCEEDS and
/// replaces the destination with a blank database.
#[test]
fn backup_of_the_temp_schema_is_an_empty_database() {
    unsafe {
        let src = open_memory();
        let dst = open_memory();
        // Both connections carry real tables; neither may end up in the copy.
        assert_eq!(exec(src, "CREATE TABLE insrc (a INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(dst, "CREATE TABLE indst (a INTEGER PRIMARY KEY)"), SQLITE_OK);
        // Nothing can be put in temp in the first place — the premise.
        assert_eq!(exec(src, "CREATE TEMP TABLE tt (a INTEGER)"), SQLITE_ERROR);

        let (main, temp) = (cs("main"), cs("temp"));
        let b = sqlite3_backup_init(dst, main.as_ptr(), src, temp.as_ptr());
        assert!(!b.is_null(), "backup_init: {}", errmsg(dst));
        assert_eq!(sqlite3_backup_step(b, -1), SQLITE_DONE);
        assert_eq!(sqlite3_backup_finish(b), SQLITE_OK);

        // Empty: the destination's own table is gone and the source's never
        // arrived. The copy is a working database, not a husk.
        assert!(collect_text_col(dst, "SELECT name FROM sqlite_master").is_empty());
        assert_eq!(exec(dst, "SELECT a FROM indst"), SQLITE_ERROR);
        assert_eq!(exec(dst, "SELECT a FROM insrc"), SQLITE_ERROR);
        assert_eq!(exec(dst, "CREATE TABLE fresh (a INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(dst, "INSERT INTO fresh (a) VALUES (1)"), SQLITE_OK);
        assert_eq!(collect_text_col(dst, "SELECT a FROM fresh"), ["1"]);
        // The source is untouched.
        assert_eq!(collect_text_col(src, "SELECT name FROM sqlite_master"), ["insrc"]);

        sqlite3_close(dst);
        sqlite3_close(src);
    }
}


/// `sqlite3_deserialize` refuses, but with the RIGHT refusal: bytes that are
/// not a database image get sqlite's own `SQLITE_NOTADB` / "file is not a
/// database" (CPython's `test_deserialize_corrupt_database` asserts that
/// message), because that statement is simply true of them. A plausible image
/// gets the actual gap instead — mpedb cannot adopt a page image into an open
/// connection.
#[test]
fn deserialize_separates_not_a_database_from_not_supported() {
    unsafe {
        let db = open_memory();
        let mut junk = *b"\0\x01\x03";
        assert_eq!(
            sqlite3_deserialize(db, ptr::null(), junk.as_mut_ptr(), 3, 3, 0),
            SQLITE_NOTADB
        );
        assert_eq!(errmsg(db), "file is not a database");
        // Empty and NULL buffers are not databases either.
        assert_eq!(sqlite3_deserialize(db, ptr::null(), ptr::null_mut(), 0, 0, 0), SQLITE_NOTADB);

        let mut img = *b"MPEDB1\0\0rest of a database image";
        assert_eq!(
            sqlite3_deserialize(db, ptr::null(), img.as_mut_ptr(), img.len() as _, img.len() as _, 0),
            SQLITE_ERROR
        );
        assert_eq!(errmsg(db), "deserialize is not supported by mpedb");
        sqlite3_close(db);
    }
}


/// A UDF registered on the DESTINATION survives a backup: the connection's
/// database is reopened underneath it, and the registry is repopulated.
#[test]
fn a_backup_keeps_the_destinations_registered_functions() {
    unsafe extern "C" fn plus_one(ctx: *mut c_void, _argc: c_int, argv: *mut *mut c_void) {
        let v = *argv;
        sqlite3_result_int64(ctx, sqlite3_value_int64(v) + 1);
    }
    unsafe {
        let src = open_memory();
        let dst = open_memory();
        assert_eq!(exec(src, "CREATE TABLE foo (key INTEGER PRIMARY KEY)"), SQLITE_OK);
        assert_eq!(exec(src, "INSERT INTO foo (key) VALUES (1)"), SQLITE_OK);
        let fname = cs("answer");
        assert_eq!(
            sqlite3_create_function(
                dst,
                fname.as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                plus_one as *mut c_void,
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );

        let main = cs("main");
        let b = sqlite3_backup_init(dst, main.as_ptr(), src, main.as_ptr());
        assert!(!b.is_null());
        assert_eq!(sqlite3_backup_step(b, -1), SQLITE_DONE);
        assert_eq!(sqlite3_backup_finish(b), SQLITE_OK);
        // The copied source's row, through the DESTINATION's own UDF.
        assert_eq!(scalar_count(dst, "SELECT answer(key) FROM foo"), 2);
        // ...and zeroblob(), one of the shim's OWN builtins, is back too (a
        // non-constant argument, so this is the registered host function and
        // not the constant-folding rewrite in `sql::rewrite_zeroblob`).
        assert_eq!(collect_text_col(dst, "SELECT typeof(zeroblob(key)) FROM foo"), ["blob"]);
        sqlite3_close(dst);
        sqlite3_close(src);
    }
}


/// `SQLITE_LIMIT_FUNCTION_ARG` is enforced at prepare, with sqlite's message —
/// and, because the count is read out of the SQL TEXT, the guards that keep it
/// from mistaking a column list or an `IN`/`VALUES` list for a call.
#[test]
fn function_arg_limit_counts_calls_and_nothing_else() {
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER, c INTEGER)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (a, b, c) VALUES (1, 2, 3)"), SQLITE_OK);
        // Default limit: nothing is counted at all.
        assert_eq!(scalar_count(db, "SELECT max(a, b, c) FROM t"), 3);

        let prior = sqlite3_limit(db, SQLITE_LIMIT_FUNCTION_ARG, 1);
        assert_eq!(prior, 127, "sqlite's compile-time default");
        // One argument is fine; two is not.
        assert_eq!(scalar_count(db, "SELECT abs(-1) FROM t"), 1);
        let s = cs("SELECT max(a, b) FROM t");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_ERROR
        );
        assert_eq!(errmsg(db), "too many arguments on function max");

        // NOT calls, even under the lowered limit: a VALUES tuple, an IN list,
        // a column list on a table whose NAME is a function's, and commas
        // inside a string literal.
        assert_eq!(exec(db, "INSERT INTO t (a, b, c) VALUES (4, 5, 6)"), SQLITE_OK);
        assert_eq!(scalar_count(db, "SELECT count(*) FROM t WHERE a IN (1, 4)"), 2);
        assert_eq!(exec(db, "CREATE TABLE max (x INTEGER PRIMARY KEY, y INTEGER)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO max (x, y) VALUES (1, 2)"), SQLITE_OK);
        // Commas inside a string literal are invisible to the scan.
        assert_eq!(scalar_count(db, "SELECT length('a,b,c') FROM t WHERE a = 1"), 5);

        sqlite3_limit(db, SQLITE_LIMIT_FUNCTION_ARG, prior);
        assert_eq!(scalar_count(db, "SELECT max(a, b) FROM t WHERE a = 1"), 2);
        sqlite3_close(db);
    }
}


/// `sqlite3_create_window_function` — a user-defined WINDOW aggregate, end to
/// end. The answers are sqlite's own worked example for the API, and the CALL
/// SEQUENCE is asserted too: a moving frame must retract through `xInverse`
/// rather than being re-aggregated, because that is what a consumer's callbacks
/// are written for.
#[test]
fn a_host_window_function_slides_its_frame() {
    use std::sync::atomic::{AtomicI64, Ordering as AtOrd};
    // The accumulator lives in the aggregate context; the counters are process
    // -wide because this is the only test that registers these callbacks.
    static STEPS: AtomicI64 = AtomicI64::new(0);
    static INVERSES: AtomicI64 = AtomicI64::new(0);
    static VALUES: AtomicI64 = AtomicI64::new(0);
    static FINALS: AtomicI64 = AtomicI64::new(0);

    unsafe fn total(ctx: *mut c_void) -> *mut i64 {
        sqlite3_aggregate_context(ctx, 8) as *mut i64
    }
    unsafe extern "C" fn w_step(ctx: *mut c_void, _argc: c_int, argv: *mut *mut c_void) {
        STEPS.fetch_add(1, AtOrd::Relaxed);
        *total(ctx) += sqlite3_value_int64(*argv);
    }
    unsafe extern "C" fn w_inverse(ctx: *mut c_void, _argc: c_int, argv: *mut *mut c_void) {
        INVERSES.fetch_add(1, AtOrd::Relaxed);
        *total(ctx) -= sqlite3_value_int64(*argv);
    }
    unsafe extern "C" fn w_value(ctx: *mut c_void) {
        VALUES.fetch_add(1, AtOrd::Relaxed);
        sqlite3_result_int64(ctx, *total(ctx));
    }
    unsafe extern "C" fn w_final(ctx: *mut c_void) {
        FINALS.fetch_add(1, AtOrd::Relaxed);
        sqlite3_result_int64(ctx, *total(ctx));
    }

    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (x INTEGER PRIMARY KEY, y INTEGER)"), SQLITE_OK);
        assert_eq!(
            exec(db, "INSERT INTO t (x, y) VALUES (1,4),(2,5),(3,3),(4,8),(5,1)"),
            SQLITE_OK
        );
        let name = cs("sumint");
        assert_eq!(
            sqlite3_create_window_function(
                db,
                name.as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                w_step as *mut c_void,
                w_final as *mut c_void,
                w_value as *mut c_void,
                w_inverse as *mut c_void,
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        let q = "SELECT sumint(y) OVER (ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) \
                 FROM t ORDER BY x";
        assert_eq!(collect_text_col(db, q), ["9", "12", "16", "12", "9"]);
        assert_eq!(STEPS.load(AtOrd::Relaxed), 5);
        assert_eq!(INVERSES.load(AtOrd::Relaxed), 3, "the frame must SLIDE, not re-aggregate");
        assert_eq!(VALUES.load(AtOrd::Relaxed), 5);
        assert_eq!(FINALS.load(AtOrd::Relaxed), 1, "xFinal runs once per partition");

        // All-NULL callbacks DELETE the registration (sqlite's rule), and the
        // window query then refuses rather than answering.
        assert_eq!(
            sqlite3_create_window_function(
                db,
                name.as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            SQLITE_OK
        );
        let s = cs(q);
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_ERROR
        );
        sqlite3_close(db);
    }
}


/// An aggregate registered through `create_function` (no xValue/xInverse) is
/// still refused under `OVER` — by NAME, and naming what is missing. Answering
/// it with a whole-partition value under a bounded frame would be a wrong
/// answer, which is exactly what this refusal exists to prevent.
#[test]
fn a_plain_aggregate_is_still_refused_under_over() {
    unsafe extern "C" fn a_step(ctx: *mut c_void, _argc: c_int, argv: *mut *mut c_void) {
        let p = sqlite3_aggregate_context(ctx, 8) as *mut i64;
        *p += sqlite3_value_int64(*argv);
    }
    unsafe extern "C" fn a_final(ctx: *mut c_void) {
        let p = sqlite3_aggregate_context(ctx, 8) as *mut i64;
        sqlite3_result_int64(ctx, if p.is_null() { 0 } else { *p });
    }
    unsafe {
        let db = open_memory();
        assert_eq!(exec(db, "CREATE TABLE t (x INTEGER PRIMARY KEY, y INTEGER)"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t (x, y) VALUES (1,4),(2,5)"), SQLITE_OK);
        let name = cs("plainsum");
        assert_eq!(
            sqlite3_create_function(
                db,
                name.as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                ptr::null_mut(),
                a_step as *mut c_void,
                a_final as *mut c_void,
            ),
            SQLITE_OK
        );
        // Grouped: fine.
        assert_eq!(scalar_count(db, "SELECT plainsum(y) FROM t"), 9);
        let s = cs("SELECT plainsum(y) OVER (ORDER BY x) FROM t");
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_ERROR
        );
        let msg = errmsg(db);
        assert!(
            msg.contains("create_window_function"),
            "refusal must name the missing registration: {msg}"
        );
        sqlite3_close(db);
    }
}

