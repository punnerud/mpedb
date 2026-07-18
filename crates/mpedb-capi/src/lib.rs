//! `mpedb-capi` — a libsqlite3-compatible C-API shim backed by mpedb.
//!
//! This cdylib exports the core sqlite3 C symbols (`sqlite3_open`,
//! `sqlite3_prepare_v2`, `sqlite3_step`, `sqlite3_bind_*`, `sqlite3_column_*`,
//! `sqlite3_exec`, …) as `extern "C"`, translating each call into mpedb's Rust
//! facade (`mpedb::Database` / `WriteSession`). `LD_PRELOAD` it as `libsqlite3`
//! (or link against it) and an unmodified libsqlite3 consumer — Python's
//! `sqlite3`, a language binding, a tool — runs against mpedb. See
//! `design/DESIGN-CAPI.md` and the repo-root `C-API-COMPAT.md`.
//!
//! # Boundary discipline
//! Every exported function is an FFI boundary over hostile input: raw pointers
//! are NULL-checked, lengths are bounds-checked, and the engine call is run
//! under `catch_unwind` so an engine panic becomes `SQLITE_ERROR` rather than
//! unwinding across the C ABI (which is UB). No `unwrap` touches caller data.
#![allow(clippy::missing_safety_doc)]

mod consts;
mod sql;
mod valconv;

pub use consts::*;

use mpedb::{Config, Database, Error as DbError, ExecResult, Value, WriteSession};
use std::os::raw::{c_char, c_double, c_int, c_longlong, c_uchar, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

/// The seed table every fresh mpedb file is created with: mpedb refuses a
/// schema with no live tables, but `sqlite3_open("new.db")` carries no schema.
/// It is otherwise inert; user tables are created live via `CREATE TABLE`.
const SEED_TABLE: &str = "_mpedb_capi_bootstrap";

static EPHEMERAL_SEQ: AtomicU64 = AtomicU64::new(0);

// ===========================================================================
// Opaque handles (returned to C as `sqlite3*` / `sqlite3_stmt*`).
// ===========================================================================

/// A connection: the mpedb engine handle plus sqlite's per-connection state
/// (open transaction, busy timeout, last error, change counters).
pub struct Sqlite3 {
    // `txn` borrows `db` (self-referential via the 'static transmute in
    // `begin`), so it MUST be declared — and therefore dropped — before `db`.
    txn: Option<WriteSession<'static>>,
    db: Database,
    path: PathBuf,
    /// A `:memory:`/temp database: its backing file is removed on close.
    ephemeral: bool,
    busy_timeout_ms: c_int,
    err_code: c_int,
    err_ext: c_int,
    err_msg: Vec<u8>, // NUL-terminated
    changes: c_int,
    total_changes: c_int,
    last_insert_rowid: c_longlong,
}

/// A prepared statement: the SQL, its bound parameters, and — once stepped —
/// the materialized result it hands out one row at a time.
pub struct Stmt {
    db: *mut Sqlite3,
    sql: String,
    n_params: usize,
    binds: Vec<Value>,
    /// True once the statement has run since the last `reset` (or ever).
    executed: bool,
    /// Result column names (known after execution).
    columns: Vec<String>,
    col_name_c: Vec<Vec<u8>>, // NUL-terminated, aligned to `columns`
    rows: Vec<Vec<Value>>,
    /// Index of the NEXT row to yield; the current row is `pos - 1`.
    pos: usize,
    have_row: bool,
    /// Per-column rendered cells for the current row (valid until the next
    /// step/reset/finalize — sqlite's pointer-lifetime contract).
    cells: Vec<Cell>,
}

/// A rendered result cell: everything the `sqlite3_column_*` family needs,
/// with owned buffers whose pointers stay valid until the next step.
struct Cell {
    ty: c_int,
    is_null: bool,
    i64v: c_longlong,
    f64v: c_double,
    /// Canonical payload followed by a NUL terminator. `_text` returns the
    /// start; `_blob` returns the same start; `_bytes` returns `len` below.
    text_c: Vec<u8>,
    len: c_int,
}

/// One executed statement's result, before it becomes stmt/cursor state.
enum Outcome {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Affected(u64),
    /// Transaction control (BEGIN/COMMIT/…): no rows, does not touch counters.
    Control,
}

// ===========================================================================
// Pointer / string helpers.
// ===========================================================================

unsafe fn conn<'a>(p: *mut Sqlite3) -> Option<&'a mut Sqlite3> {
    if p.is_null() {
        None
    } else {
        Some(&mut *p)
    }
}

unsafe fn stmt<'a>(p: *mut Stmt) -> Option<&'a mut Stmt> {
    if p.is_null() {
        None
    } else {
        Some(&mut *p)
    }
}

/// Read a C string with an explicit byte length: `n < 0` means NUL-terminated.
/// Returns the bytes (not including any terminator). NULL pointer -> None.
unsafe fn c_bytes<'a>(p: *const c_char, n: c_int) -> Option<&'a [u8]> {
    if p.is_null() {
        return None;
    }
    let len = if n < 0 {
        libc::strlen(p)
    } else {
        n as usize
    };
    Some(std::slice::from_raw_parts(p as *const u8, len))
}

unsafe fn c_str_opt<'a>(p: *const c_char) -> Option<&'a str> {
    let bytes = c_bytes(p, -1)?;
    std::str::from_utf8(bytes).ok()
}

/// A static NUL-terminated C string usable as a `const char*`.
macro_rules! cstr {
    ($s:literal) => {
        concat!($s, "\0").as_ptr() as *const c_char
    };
}

// ===========================================================================
// Connection error state.
// ===========================================================================

impl Sqlite3 {
    fn clear_error(&mut self) {
        self.err_code = SQLITE_OK;
        self.err_ext = SQLITE_OK;
        self.err_msg = b"not an error\0".to_vec();
    }

    fn set_error(&mut self, code: c_int, ext: c_int, msg: &str) {
        self.err_code = code;
        self.err_ext = ext;
        self.err_msg = msg.as_bytes().to_vec();
        self.err_msg.push(0);
    }

    fn set_db_error(&mut self, e: &DbError) -> c_int {
        let (code, ext) = valconv::error_codes(e);
        self.set_error(code, ext, &e.to_string());
        code
    }
}

// ===========================================================================
// Transaction control + statement execution (shared by step and exec).
// ===========================================================================

fn begin_txn(c: &mut Sqlite3) -> Result<(), DbError> {
    if c.txn.is_some() {
        return Err(DbError::Unsupported(
            "cannot start a transaction within a transaction".into(),
        ));
    }
    // The session borrows `c.db`, which lives at a stable heap address (the
    // Sqlite3 is boxed and never moved) and is declared after `txn`, so the
    // borrow is always dropped before its referent. Same discipline as
    // mpedb-py's PyTransaction.
    let db_ptr: *const Database = &c.db;
    let session = unsafe { (*db_ptr).begin()? };
    let session: WriteSession<'static> =
        unsafe { std::mem::transmute::<WriteSession<'_>, WriteSession<'static>>(session) };
    c.txn = Some(session);
    Ok(())
}

fn commit_txn(c: &mut Sqlite3) -> Result<(), DbError> {
    match c.txn.take() {
        Some(s) => s.commit(),
        None => Ok(()), // lenient: COMMIT with no active transaction is a no-op
    }
}

fn rollback_txn(c: &mut Sqlite3) {
    if let Some(s) = c.txn.take() {
        s.rollback();
    }
}

/// Run one statement against the connection, honoring the current transaction
/// state. Transaction-control statements are intercepted; everything else is
/// routed to the open `WriteSession` (if any) or the autocommit facade.
fn exec_one(c: &mut Sqlite3, sqltext: &str, params: &[Value]) -> Result<Outcome, DbError> {
    use sql::Kind;
    match sql::classify(sqltext) {
        Kind::Begin => {
            begin_txn(c)?;
            Ok(Outcome::Control)
        }
        Kind::Commit => {
            commit_txn(c)?;
            Ok(Outcome::Control)
        }
        Kind::Rollback => {
            rollback_txn(c);
            Ok(Outcome::Control)
        }
        Kind::Savepoint => {
            if c.txn.is_none() {
                begin_txn(c)?;
            }
            c.txn.as_mut().unwrap().query(sqltext, params)?;
            Ok(Outcome::Control)
        }
        Kind::Release | Kind::RollbackTo => {
            let Some(s) = c.txn.as_mut() else {
                return Err(DbError::Unsupported(
                    "no active transaction for this savepoint operation".into(),
                ));
            };
            s.query(sqltext, params)?;
            Ok(Outcome::Control)
        }
        _ => {
            let res = if let Some(s) = c.txn.as_mut() {
                s.query(sqltext, params)?
            } else {
                c.db.query(sqltext, params)?
            };
            Ok(to_outcome(res))
        }
    }
}

fn to_outcome(res: ExecResult) -> Outcome {
    match res {
        ExecResult::Rows { columns, rows } => Outcome::Rows { columns, rows },
        ExecResult::Affected(n) => Outcome::Affected(n),
        // mpedb EXPLAIN yields plan text; present it as a single "plan" column
        // so a caller stepping/reading it behaves.
        ExecResult::Explain(text) => Outcome::Rows {
            columns: vec!["plan".to_string()],
            rows: vec![vec![Value::Text(text)]],
        },
    }
}

/// Execute `stmt` (first step). Updates connection counters. Returns the
/// primary result code to hand back from the failing API on error, else OK.
fn run_stmt(s: &mut Stmt) -> c_int {
    let Some(c) = (unsafe { conn(s.db) }) else {
        return SQLITE_MISUSE;
    };
    let is_dml = matches!(sql::classify(&s.sql), sql::Kind::Dml { .. });
    let params = s.binds.clone();
    let outcome = catch_unwind(AssertUnwindSafe(|| exec_one(c, &s.sql, &params)));
    let outcome = match outcome {
        Ok(r) => r,
        Err(_) => {
            c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) in engine");
            return SQLITE_ERROR;
        }
    };
    match outcome {
        Ok(Outcome::Rows { columns, rows }) => {
            if is_dml {
                c.changes = rows.len() as c_int; // INSERT/…/RETURNING row count
                c.total_changes = c.total_changes.saturating_add(rows.len() as c_int);
            }
            s.col_name_c = columns
                .iter()
                .map(|n| {
                    let mut v = n.as_bytes().to_vec();
                    v.push(0);
                    v
                })
                .collect();
            s.columns = columns;
            s.rows = rows;
            s.pos = 0;
            s.executed = true;
            c.clear_error();
            SQLITE_OK
        }
        Ok(Outcome::Affected(n)) => {
            if is_dml {
                c.changes = n as c_int;
                c.total_changes = c.total_changes.saturating_add(n as c_int);
            }
            s.columns.clear();
            s.col_name_c.clear();
            s.rows.clear();
            s.pos = 0;
            s.executed = true;
            c.clear_error();
            SQLITE_OK
        }
        Ok(Outcome::Control) => {
            s.columns.clear();
            s.col_name_c.clear();
            s.rows.clear();
            s.pos = 0;
            s.executed = true;
            c.clear_error();
            SQLITE_OK
        }
        Err(e) => c.set_db_error(&e),
    }
}

/// Render the current row (`rows[pos]`) into `cells` and advance.
fn load_current_row(s: &mut Stmt) {
    let row = &s.rows[s.pos];
    s.cells = row
        .iter()
        .map(|v| {
            let ty = valconv::sqlite_type(v);
            let is_null = matches!(v, Value::Null);
            let (text_c, len) = match valconv::as_bytes(v) {
                Some(mut payload) => {
                    let len = payload.len() as c_int;
                    payload.push(0);
                    (payload, len)
                }
                None => (vec![0u8], 0),
            };
            Cell {
                ty,
                is_null,
                i64v: valconv::as_i64(v),
                f64v: valconv::as_f64(v),
                text_c,
                len,
            }
        })
        .collect();
    s.pos += 1;
    s.have_row = true;
}

// ===========================================================================
// open / close
// ===========================================================================

enum Target {
    Ephemeral,
    File(PathBuf),
}

fn resolve_target(filename: Option<&str>, flags: c_int) -> Target {
    let name = filename.unwrap_or("").trim();
    if flags & SQLITE_OPEN_MEMORY != 0 {
        return Target::Ephemeral;
    }
    // Minimal file: URI handling.
    let name = if let Some(rest) = name.strip_prefix("file:") {
        let path = rest.split('?').next().unwrap_or("");
        if path == ":memory:" || path.is_empty() {
            return Target::Ephemeral;
        }
        path
    } else {
        name
    };
    if name.is_empty() || name == ":memory:" {
        Target::Ephemeral
    } else {
        Target::File(PathBuf::from(name))
    }
}

fn ephemeral_path() -> PathBuf {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let seq = EPHEMERAL_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("mpedb-capi-{}-{}.mpedb", std::process::id(), seq))
}

fn seed_toml(path: &std::path::Path, size_mb: u64) -> String {
    // Escape for a TOML basic string.
    let p = path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "[database]\npath = \"{p}\"\nsize_mb = {size_mb}\nmax_readers = 1024\n\n\
         [[table]]\nname = \"{SEED_TABLE}\"\nprimary_key = [\"id\"]\n\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n"
    )
}

fn open_impl(filename: Option<&str>, flags: c_int) -> Result<Box<Sqlite3>, (c_int, String)> {
    let target = resolve_target(filename, flags);
    let (path, ephemeral, size_mb) = match target {
        Target::Ephemeral => (ephemeral_path(), true, 16),
        Target::File(p) => (p, false, 64),
    };

    let exists = !ephemeral && path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false);
    if ephemeral {
        let _ = std::fs::remove_file(&path);
    }

    let db = if exists {
        // Attach an existing mpedb file config-free (reads its stored schema).
        Database::open_from_file(&path)
            .map_err(|e| (SQLITE_CANTOPEN, format!("cannot open `{}`: {e}", path.display())))?
    } else {
        // Fresh database: creating requires the CREATE flag (open_v2 semantics;
        // plain sqlite3_open always sets it — see the callers).
        if flags & SQLITE_OPEN_CREATE == 0 {
            return Err((
                SQLITE_CANTOPEN,
                format!("no such database file: {}", path.display()),
            ));
        }
        let cfg = Config::from_toml_str(&seed_toml(&path, size_mb))
            .map_err(|e| (SQLITE_CANTOPEN, format!("config error: {e}")))?;
        Database::open_with_config(cfg)
            .map_err(|e| (SQLITE_CANTOPEN, format!("cannot create `{}`: {e}", path.display())))?
    };

    let mut c = Box::new(Sqlite3 {
        txn: None,
        db,
        path,
        ephemeral,
        busy_timeout_ms: 0,
        err_code: SQLITE_OK,
        err_ext: SQLITE_OK,
        err_msg: Vec::new(),
        changes: 0,
        total_changes: 0,
        last_insert_rowid: 0,
    });
    c.clear_error();
    Ok(c)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_open(filename: *const c_char, pp_db: *mut *mut Sqlite3) -> c_int {
    // Plain open always allows create+readwrite.
    open_common(filename, pp_db, SQLITE_OPEN_CREATE | SQLITE_OPEN_READWRITE)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    pp_db: *mut *mut Sqlite3,
    flags: c_int,
    _vfs: *const c_char,
) -> c_int {
    open_common(filename, pp_db, flags)
}

unsafe fn open_common(filename: *const c_char, pp_db: *mut *mut Sqlite3, flags: c_int) -> c_int {
    if pp_db.is_null() {
        return SQLITE_MISUSE;
    }
    let name = c_str_opt(filename);
    match catch_unwind(AssertUnwindSafe(|| open_impl(name, flags))) {
        Ok(Ok(boxed)) => {
            *pp_db = Box::into_raw(boxed);
            SQLITE_OK
        }
        Ok(Err((code, _msg))) => {
            *pp_db = ptr::null_mut();
            code
        }
        Err(_) => {
            *pp_db = ptr::null_mut();
            SQLITE_CANTOPEN
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_close(db: *mut Sqlite3) -> c_int {
    close_common(db)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_close_v2(db: *mut Sqlite3) -> c_int {
    close_common(db)
}

unsafe fn close_common(db: *mut Sqlite3) -> c_int {
    if db.is_null() {
        return SQLITE_OK;
    }
    let mut boxed = Box::from_raw(db);
    // Drop any open transaction before the engine (borrow discipline).
    boxed.txn = None;
    let path = boxed.path.clone();
    let ephemeral = boxed.ephemeral;
    drop(boxed);
    if ephemeral {
        let _ = std::fs::remove_file(&path);
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_busy_timeout(db: *mut Sqlite3, ms: c_int) -> c_int {
    match conn(db) {
        Some(c) => {
            c.busy_timeout_ms = ms;
            SQLITE_OK
        }
        None => SQLITE_MISUSE,
    }
}

/// Non-standard-but-common helpers some consumers (incl. Python's sqlite3) call.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_result_codes(db: *mut Sqlite3, _onoff: c_int) -> c_int {
    // The shim always tracks an extended code; the toggle is a no-op.
    if db.is_null() {
        SQLITE_MISUSE
    } else {
        SQLITE_OK
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_get_autocommit(db: *mut Sqlite3) -> c_int {
    match conn(db) {
        Some(c) => c.txn.is_none() as c_int,
        None => 1,
    }
}

// ===========================================================================
// prepare / step / reset / finalize / exec
// ===========================================================================

#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    db: *mut Sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut Stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_common(db, z_sql, n_byte, pp_stmt, pz_tail)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare(
    db: *mut Sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut Stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    prepare_common(db, z_sql, n_byte, pp_stmt, pz_tail)
}

unsafe fn prepare_common(
    db: *mut Sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut Stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    if pp_stmt.is_null() {
        return SQLITE_MISUSE;
    }
    *pp_stmt = ptr::null_mut();
    let Some(c) = conn(db) else {
        return SQLITE_MISUSE;
    };
    let Some(bytes) = c_bytes(z_sql, n_byte) else {
        c.set_error(SQLITE_MISUSE, SQLITE_MISUSE, "null SQL");
        return SQLITE_MISUSE;
    };
    let Ok(full) = std::str::from_utf8(bytes) else {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "SQL is not valid UTF-8");
        return SQLITE_ERROR;
    };

    let (first, tail) = sql::split_first(full);
    // pz_tail points at the first byte past this statement (into z_sql).
    if !pz_tail.is_null() {
        let off = full.len() - tail.len();
        *pz_tail = z_sql.add(off);
    }

    // A blank statement (whitespace/comments only) prepares to a NULL stmt.
    if sql::is_blank(first) {
        c.clear_error();
        return SQLITE_OK;
    }

    let kind = sql::classify(first);
    let is_control = matches!(
        kind,
        sql::Kind::Begin
            | sql::Kind::Commit
            | sql::Kind::Rollback
            | sql::Kind::Savepoint
            | sql::Kind::Release
            | sql::Kind::RollbackTo
    );

    // Validate non-control statements now (surface syntax/bind errors at
    // prepare, as sqlite does), WITHOUT executing or publishing a plan.
    if !is_control {
        match catch_unwind(AssertUnwindSafe(|| c.db.prepare_detached(first))) {
            Ok(Ok(_plan)) => {}
            Ok(Err(e)) => return c.set_db_error(&e),
            Err(_) => {
                c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) preparing");
                return SQLITE_ERROR;
            }
        }
    }

    let n_params = sql::param_count(first);
    let boxed = Box::new(Stmt {
        db,
        sql: first.to_string(),
        n_params,
        binds: vec![Value::Null; n_params],
        executed: false,
        columns: Vec::new(),
        col_name_c: Vec::new(),
        rows: Vec::new(),
        pos: 0,
        have_row: false,
        cells: Vec::new(),
    });
    *pp_stmt = Box::into_raw(boxed);
    c.clear_error();
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_step(p: *mut Stmt) -> c_int {
    let Some(s) = stmt(p) else {
        return SQLITE_MISUSE;
    };
    if !s.executed {
        let code = run_stmt(s);
        if code != SQLITE_OK {
            return code;
        }
    }
    if s.pos < s.rows.len() {
        load_current_row(s);
        SQLITE_ROW
    } else {
        s.have_row = false;
        s.cells.clear();
        SQLITE_DONE
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_reset(p: *mut Stmt) -> c_int {
    match stmt(p) {
        Some(s) => {
            s.executed = false;
            s.rows.clear();
            s.columns.clear();
            s.col_name_c.clear();
            s.cells.clear();
            s.pos = 0;
            s.have_row = false;
            SQLITE_OK
        }
        None => SQLITE_MISUSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_finalize(p: *mut Stmt) -> c_int {
    if p.is_null() {
        return SQLITE_OK;
    }
    drop(Box::from_raw(p));
    SQLITE_OK
}

type ExecCallback =
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>;

#[no_mangle]
pub unsafe extern "C" fn sqlite3_exec(
    db: *mut Sqlite3,
    z_sql: *const c_char,
    callback: ExecCallback,
    arg: *mut c_void,
    errmsg: *mut *mut c_char,
) -> c_int {
    if !errmsg.is_null() {
        *errmsg = ptr::null_mut();
    }
    let Some(c) = conn(db) else {
        return SQLITE_MISUSE;
    };
    let Some(full) = c_str_opt(z_sql) else {
        c.set_error(SQLITE_MISUSE, SQLITE_MISUSE, "null or non-UTF-8 SQL");
        return SQLITE_MISUSE;
    };

    let mut remaining = full;
    loop {
        let (first, tail) = sql::split_first(remaining);
        if !sql::is_blank(first) {
            let outcome = catch_unwind(AssertUnwindSafe(|| exec_one(c, first, &[])));
            let outcome = match outcome {
                Ok(r) => r,
                Err(_) => {
                    c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) in engine");
                    set_exec_errmsg(c, errmsg);
                    return SQLITE_ERROR;
                }
            };
            match outcome {
                Ok(Outcome::Rows { columns, rows }) => {
                    if let Some(cb) = callback {
                        if invoke_callback(cb, arg, &columns, &rows) != 0 {
                            c.set_error(SQLITE_ABORT, SQLITE_ABORT, "callback requested abort");
                            set_exec_errmsg(c, errmsg);
                            return SQLITE_ABORT;
                        }
                    }
                    c.clear_error();
                }
                Ok(Outcome::Affected(n)) => {
                    if matches!(sql::classify(first), sql::Kind::Dml { .. }) {
                        c.changes = n as c_int;
                        c.total_changes = c.total_changes.saturating_add(n as c_int);
                    }
                    c.clear_error();
                }
                Ok(Outcome::Control) => c.clear_error(),
                Err(e) => {
                    let code = c.set_db_error(&e);
                    set_exec_errmsg(c, errmsg);
                    return code;
                }
            }
        }
        if tail.is_empty() {
            break;
        }
        remaining = tail;
    }
    SQLITE_OK
}

unsafe fn set_exec_errmsg(c: &Sqlite3, errmsg: *mut *mut c_char) {
    if errmsg.is_null() {
        return;
    }
    // sqlite3_exec's errmsg must be freeable with sqlite3_free -> libc alloc.
    let msg = &c.err_msg; // NUL-terminated
    let p = libc::malloc(msg.len()) as *mut u8;
    if !p.is_null() {
        ptr::copy_nonoverlapping(msg.as_ptr(), p, msg.len());
    }
    *errmsg = p as *mut c_char;
}

unsafe fn invoke_callback(
    cb: unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
    arg: *mut c_void,
    columns: &[String],
    rows: &[Vec<Value>],
) -> c_int {
    // Column names live for the whole call.
    let name_bufs: Vec<Vec<u8>> = columns
        .iter()
        .map(|n| {
            let mut v = n.as_bytes().to_vec();
            v.push(0);
            v
        })
        .collect();
    let mut name_ptrs: Vec<*mut c_char> =
        name_bufs.iter().map(|b| b.as_ptr() as *mut c_char).collect();

    for row in rows {
        let val_bufs: Vec<Option<Vec<u8>>> = row
            .iter()
            .map(|v| {
                valconv::as_bytes(v).map(|mut b| {
                    b.push(0);
                    b
                })
            })
            .collect();
        let mut val_ptrs: Vec<*mut c_char> = val_bufs
            .iter()
            .map(|b| match b {
                Some(bytes) => bytes.as_ptr() as *mut c_char,
                None => ptr::null_mut(),
            })
            .collect();
        let rc = cb(
            arg,
            columns.len() as c_int,
            val_ptrs.as_mut_ptr(),
            name_ptrs.as_mut_ptr(),
        );
        if rc != 0 {
            return rc;
        }
    }
    0
}

// ===========================================================================
// bind_*  (1-based parameter index)
// ===========================================================================

unsafe fn bind(p: *mut Stmt, idx: c_int, v: Value) -> c_int {
    let Some(s) = stmt(p) else {
        return SQLITE_MISUSE;
    };
    if idx < 1 || idx as usize > s.n_params {
        return SQLITE_RANGE;
    }
    s.binds[(idx - 1) as usize] = v;
    // A new binding invalidates a prior execution's rows.
    s.executed = false;
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int(p: *mut Stmt, idx: c_int, v: c_int) -> c_int {
    bind(p, idx, Value::Int(v as i64))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int64(p: *mut Stmt, idx: c_int, v: c_longlong) -> c_int {
    bind(p, idx, Value::Int(v))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_double(p: *mut Stmt, idx: c_int, v: c_double) -> c_int {
    bind(p, idx, Value::Float(v))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_null(p: *mut Stmt, idx: c_int) -> c_int {
    bind(p, idx, Value::Null)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text(
    p: *mut Stmt,
    idx: c_int,
    text: *const c_char,
    n: c_int,
    destructor: *mut c_void,
) -> c_int {
    let bytes = match c_bytes(text, n) {
        Some(b) => b.to_vec(),
        None => Vec::new(),
    };
    maybe_free(destructor, text as *mut c_void);
    let s = String::from_utf8_lossy(&bytes).into_owned();
    bind(p, idx, Value::Text(s))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob(
    p: *mut Stmt,
    idx: c_int,
    data: *const c_void,
    n: c_int,
    destructor: *mut c_void,
) -> c_int {
    let bytes = if data.is_null() || n < 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data as *const u8, n as usize).to_vec()
    };
    maybe_free(destructor, data as *mut c_void);
    bind(p, idx, Value::Blob(bytes))
}

/// Honor a caller-supplied destructor (anything that is not STATIC/TRANSIENT):
/// we copy the bytes immediately, so we can release the caller's buffer now,
/// exactly as sqlite would once it no longer needs the value.
unsafe fn maybe_free(destructor: *mut c_void, data: *mut c_void) {
    let d = destructor as isize;
    if d != SQLITE_STATIC && d != SQLITE_TRANSIENT && !destructor.is_null() {
        let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(destructor);
        f(data);
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_count(p: *mut Stmt) -> c_int {
    match stmt(p) {
        Some(s) => s.n_params as c_int,
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_index(p: *mut Stmt, name: *const c_char) -> c_int {
    let Some(_s) = stmt(p) else { return 0 };
    let Some(nm) = c_str_opt(name) else { return 0 };
    // mpedb supports positional `?`/`$N`; map "?N"/"$N"/":N" to the number.
    let digits = nm.trim_start_matches(['?', '$', ':', '@']);
    digits.parse::<c_int>().unwrap_or(0).max(0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_clear_bindings(p: *mut Stmt) -> c_int {
    match stmt(p) {
        Some(s) => {
            for b in &mut s.binds {
                *b = Value::Null;
            }
            SQLITE_OK
        }
        None => SQLITE_MISUSE,
    }
}

// ===========================================================================
// column_*  (0-based column index; valid on the current row after SQLITE_ROW)
// ===========================================================================

unsafe fn cell(p: *mut Stmt, col: c_int) -> Option<&'static Cell> {
    let s = stmt(p)?;
    if !s.have_row || col < 0 {
        return None;
    }
    s.cells.get(col as usize).map(|c| &*(c as *const Cell))
}

/// Column metadata (`column_count`/`column_name`) is read BEFORE the first
/// `step` by many consumers (Python's `sqlite3` builds `description` this way).
/// mpedb only names the output once the statement runs, so a not-yet-run READ
/// statement is executed here — safe because reads have no side effects, and
/// the materialized rows are then served by the coming `step`s.
unsafe fn ensure_columns(s: &mut Stmt) {
    if !s.executed && matches!(sql::classify(&s.sql), sql::Kind::Read) {
        let _ = run_stmt(s);
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_count(p: *mut Stmt) -> c_int {
    match stmt(p) {
        Some(s) => {
            ensure_columns(s);
            s.columns.len() as c_int
        }
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_data_count(p: *mut Stmt) -> c_int {
    match stmt(p) {
        Some(s) if s.have_row => s.cells.len() as c_int,
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_type(p: *mut Stmt, col: c_int) -> c_int {
    cell(p, col).map(|c| c.ty).unwrap_or(SQLITE_NULL)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int(p: *mut Stmt, col: c_int) -> c_int {
    cell(p, col).map(|c| c.i64v as c_int).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int64(p: *mut Stmt, col: c_int) -> c_longlong {
    cell(p, col).map(|c| c.i64v).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_double(p: *mut Stmt, col: c_int) -> c_double {
    cell(p, col).map(|c| c.f64v).unwrap_or(0.0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_text(p: *mut Stmt, col: c_int) -> *const c_uchar {
    match cell(p, col) {
        Some(c) if !c.is_null => c.text_c.as_ptr(),
        _ => ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_blob(p: *mut Stmt, col: c_int) -> *const c_void {
    match cell(p, col) {
        Some(c) if !c.is_null && c.len > 0 => c.text_c.as_ptr() as *const c_void,
        _ => ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_bytes(p: *mut Stmt, col: c_int) -> c_int {
    cell(p, col).map(|c| c.len).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_name(p: *mut Stmt, col: c_int) -> *const c_char {
    if let Some(s) = stmt(p) {
        ensure_columns(s);
    }
    match stmt(p) {
        Some(s) if col >= 0 => s
            .col_name_c
            .get(col as usize)
            .map(|b| b.as_ptr() as *const c_char)
            .unwrap_or(ptr::null()),
        _ => ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_decltype(_p: *mut Stmt, _col: c_int) -> *const c_char {
    // mpedb's result metadata carries names, not declared types (an expression
    // has no decltype anyway). NULL is a legal sqlite answer.
    ptr::null()
}

// ===========================================================================
// status / misc
// ===========================================================================

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errmsg(db: *mut Sqlite3) -> *const c_char {
    match conn(db) {
        Some(c) => {
            if c.err_msg.is_empty() {
                cstr!("not an error")
            } else {
                c.err_msg.as_ptr() as *const c_char
            }
        }
        None => cstr!("out of memory"),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errcode(db: *mut Sqlite3) -> c_int {
    conn(db).map(|c| c.err_code).unwrap_or(SQLITE_MISUSE)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_errcode(db: *mut Sqlite3) -> c_int {
    conn(db).map(|c| c.err_ext).unwrap_or(SQLITE_MISUSE)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes(db: *mut Sqlite3) -> c_int {
    conn(db).map(|c| c.changes).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_total_changes(db: *mut Sqlite3) -> c_int {
    conn(db).map(|c| c.total_changes).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_last_insert_rowid(db: *mut Sqlite3) -> c_longlong {
    // mpedb does not surface the assigned rowid through the facade result
    // (ExecResult carries only an affected count), so this reports 0 unless a
    // future facade hook exposes it. Documented in C-API-COMPAT.md.
    conn(db).map(|c| c.last_insert_rowid).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn sqlite3_libversion() -> *const c_char {
    cstr!("3.45.0-mpedb")
}

#[no_mangle]
pub extern "C" fn sqlite3_libversion_number() -> c_int {
    3_045_000
}

#[no_mangle]
pub extern "C" fn sqlite3_sourceid() -> *const c_char {
    cstr!("mpedb-capi shim")
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_free(p: *mut c_void) {
    if !p.is_null() {
        libc::free(p);
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_malloc(n: c_int) -> *mut c_void {
    if n <= 0 {
        ptr::null_mut()
    } else {
        libc::malloc(n as usize)
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_malloc64(n: u64) -> *mut c_void {
    if n == 0 {
        ptr::null_mut()
    } else {
        libc::malloc(n as usize)
    }
}
