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
mod introspect;
mod sql;
mod valconv;

pub use consts::*;

use mpedb::{Config, Database, Error as DbError, ExecResult, Value, WriteSession};
use std::os::raw::{c_char, c_double, c_int, c_longlong, c_uchar, c_uint, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

/// The seed table every fresh mpedb file is created with: mpedb refuses a
/// schema with no live tables, but `sqlite3_open("new.db")` carries no schema.
/// It is otherwise inert; user tables are created live via `CREATE TABLE`.
/// `pub(crate)` so `introspect` can hide it from `PRAGMA`/`sqlite_master`.
pub(crate) const SEED_TABLE: &str = "_mpedb_capi_bootstrap";

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
    /// Per-parameter name in appearance order (`sqlite3_bind_parameter_name`):
    /// NUL-terminated bytes for a named param (`:a`/`@a`/`$a`, prefix included),
    /// `None` for an anonymous/numbered `?`/`?N`/`$N`. mpedb binds positionally,
    /// so this is metadata only.
    param_names: Vec<Option<Vec<u8>>>,
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
        // PRAGMA and sqlite_master reads are answered by the shim's schema
        // introspection (mpedb has neither); they never reach the engine.
        Kind::Pragma => {
            let bundle = c.db.schema();
            let (columns, rows) = introspect::pragma(&bundle, sqltext)?;
            Ok(Outcome::Rows { columns, rows })
        }
        Kind::Read if introspect::references_sqlite_master(sqltext) => {
            let bundle = c.db.schema();
            let (columns, rows) = introspect::sqlite_master(&bundle, sqltext)?;
            Ok(Outcome::Rows { columns, rows })
        }
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
        // DDL (CREATE/DROP/ALTER) routes like any other statement (#95): to the
        // open WriteSession's txn when one is active — where it commits/rolls
        // back atomically with the transaction's DML — else to the autocommit
        // facade. Python's sqlite3 opens an implicit transaction on the first
        // DML, so a `CREATE TABLE` after an `INSERT` (and every `executescript`)
        // lands here with `c.txn` set.
        _ => {
            let res = if let Some(s) = c.txn.as_mut() {
                s.query(sqltext, params)
            } else {
                c.db.query(sqltext, params)
            };
            // Drain the rowid the engine recorded for this statement (facade
            // hook) BEFORE propagating any error, so sqlite3_last_insert_rowid
            // reflects the last row an INSERT actually wrote — even when a
            // later row of the same statement failed — and a stale value can
            // never bleed into a subsequent statement. `take_*` clears the
            // thread-local; a non-insert returns None and leaves the
            // connection's value unchanged, exactly as sqlite does.
            if let Some(rowid) = mpedb::take_last_insert_rowid() {
                c.last_insert_rowid = rowid;
            }
            Ok(to_outcome(res?))
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
    // sqlite treats a positive `nByte` as an UPPER BOUND: the statement text
    // ends at the first NUL within it. CPython passes `strlen(sql)+1`, so the
    // terminator is inside the range — feeding it to the parser would trip an
    // "unexpected character" on the trailing `\0`. Truncate at the first NUL.
    let bytes = match bytes.iter().position(|&b| b == 0) {
        Some(nul) => &bytes[..nul],
        None => bytes,
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
    // Transaction-control and DDL statements are NOT compiled to a plan by
    // mpedb (control is intercepted here; DDL runs through `parse_ddl`/
    // `apply_ddl`), so `prepare_detached` cannot validate them — it only
    // compiles queries. Skip validation and defer these to execution, exactly
    // where mpedb applies them.
    // PRAGMA and sqlite_master reads are answered by the shim (`introspect`),
    // not compiled by mpedb, so they must NOT be handed to `prepare_detached`
    // (which only compiles queries) — skip validation and defer them too.
    let skip_validation = matches!(
        kind,
        sql::Kind::Begin
            | sql::Kind::Commit
            | sql::Kind::Rollback
            | sql::Kind::Savepoint
            | sql::Kind::Release
            | sql::Kind::RollbackTo
            | sql::Kind::Ddl
            | sql::Kind::Pragma
    ) || (matches!(kind, sql::Kind::Read) && introspect::references_sqlite_master(first));

    // #95: with a transaction open, a compilable statement may reference a
    // table this session CREATED / ALTERed but has not committed — which
    // `prepare_detached` (committed schema) cannot see, and would reject as
    // "unknown table". Defer validation to execution, where the statement
    // compiles against the session's OWN schema view. Errors then surface at
    // `step` instead of `prepare` — still a clean DB error for the consumer,
    // and the only way to honor uncommitted in-transaction DDL (Python's
    // sqlite3 opens an implicit transaction on the first DML, so a later CREATE
    // and every statement touching the new table land here).
    let skip_validation = skip_validation || c.txn.is_some();

    // Validate compilable statements now (surface syntax/bind errors at
    // prepare, as sqlite does), WITHOUT executing or publishing a plan.
    if !skip_validation {
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
    let param_names = sql::param_names(first);
    let boxed = Box::new(Stmt {
        db,
        sql: first.to_string(),
        n_params,
        param_names,
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
/// the materialized rows are then served by the coming `step`s. PRAGMA (and
/// sqlite_master reads, which classify as READ) are shim-introspection reads
/// with the same "no side effects, resolve columns eagerly" property.
unsafe fn ensure_columns(s: &mut Stmt) {
    if !s.executed && matches!(sql::classify(&s.sql), sql::Kind::Read | sql::Kind::Pragma) {
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
    // Real value: each executed statement drains the facade's
    // `take_last_insert_rowid` hook (see `exec_one`), updating this field when
    // an INSERT assigned/used a rowid on a rowid-alias (INTEGER PRIMARY KEY)
    // table, and leaving it unchanged otherwise — sqlite's semantics.
    conn(db).map(|c| c.last_insert_rowid).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn sqlite3_libversion() -> *const c_char {
    // Pure `X.Y.Z` — consumers (e.g. CPython's dbapi2) parse each dotted field
    // as an integer, so no suffix here. The mpedb identity lives in
    // `sqlite3_sourceid`.
    cstr!("3.45.0")
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

/// Duplicate a string into a libc-allocated NUL-terminated buffer that the
/// caller frees with `sqlite3_free` (which is `libc::free`).
unsafe fn dup_cstr(s: &str) -> *mut c_char {
    let bytes = s.as_bytes();
    let buf = libc::malloc(bytes.len() + 1) as *mut u8;
    if buf.is_null() {
        return ptr::null_mut();
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
    *buf.add(bytes.len()) = 0;
    buf as *mut c_char
}

// ===========================================================================
// Extended surface — symbols CPython's `_sqlite3` (and other consumers) resolve
// at load time. Every entry is either a real translation to mpedb or a SAFE
// stub: a no-op / refusal that returns a documented result code and NEVER a
// wrong query answer. See C-API-COMPAT.md for the real-vs-stub table.
// ===========================================================================

// ---- library-global lifecycle / capability queries (real) ----------------

/// SQLite serializing mode: mpedb is internally synchronized, so report "fully
/// threadsafe" (1). Consumers gate `check_same_thread` on this.
#[no_mangle]
pub extern "C" fn sqlite3_threadsafe() -> c_int {
    1
}

#[no_mangle]
pub extern "C" fn sqlite3_initialize() -> c_int {
    SQLITE_OK
}

#[no_mangle]
pub extern "C" fn sqlite3_shutdown() -> c_int {
    SQLITE_OK
}

/// Sleep for `ms` milliseconds (best effort), returning the requested amount —
/// consumers use it to back off, and honoring it is harmless and correct.
#[no_mangle]
pub extern "C" fn sqlite3_sleep(ms: c_int) -> c_int {
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }
    ms.max(0)
}

/// No cooperative mid-statement interrupt: the shim materializes each result
/// synchronously, so there is nothing to signal. No-op (never wrong).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_interrupt(_db: *mut Sqlite3) {}

/// ASCII case-insensitive C-string compare (sqlite's `sqlite3_stricmp`).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_stricmp(a: *const c_char, b: *const c_char) -> c_int {
    let sa = c_bytes(a, -1).unwrap_or(&[]);
    let sb = c_bytes(b, -1).unwrap_or(&[]);
    let n = sa.len().min(sb.len());
    for i in 0..n {
        let ca = sa[i].to_ascii_lowercase() as c_int;
        let cb = sb[i].to_ascii_lowercase() as c_int;
        if ca != cb {
            return ca - cb;
        }
    }
    sa.len() as c_int - sb.len() as c_int
}

/// A static message for a primary result code (extended bits ignored), matching
/// sqlite's `sqlite3_errstr` strings closely enough for consumers that surface
/// them.
#[no_mangle]
pub extern "C" fn sqlite3_errstr(rc: c_int) -> *const c_char {
    match rc & 0xff {
        SQLITE_OK | SQLITE_ROW | SQLITE_DONE => cstr!("not an error"),
        SQLITE_ERROR => cstr!("SQL logic error"),
        SQLITE_INTERNAL => cstr!("internal logic error"),
        SQLITE_PERM => cstr!("access permission denied"),
        SQLITE_ABORT => cstr!("query aborted"),
        SQLITE_BUSY => cstr!("database is locked"),
        SQLITE_LOCKED => cstr!("database table is locked"),
        SQLITE_NOMEM => cstr!("out of memory"),
        SQLITE_READONLY => cstr!("attempt to write a readonly database"),
        SQLITE_INTERRUPT => cstr!("interrupted"),
        SQLITE_IOERR => cstr!("disk I/O error"),
        SQLITE_CORRUPT => cstr!("database disk image is malformed"),
        SQLITE_NOTFOUND => cstr!("unknown operation"),
        SQLITE_FULL => cstr!("database or disk is full"),
        SQLITE_CANTOPEN => cstr!("unable to open database file"),
        SQLITE_PROTOCOL => cstr!("locking protocol"),
        SQLITE_EMPTY => cstr!("table contains no data"),
        SQLITE_SCHEMA => cstr!("database schema has changed"),
        SQLITE_TOOBIG => cstr!("string or blob too big"),
        SQLITE_CONSTRAINT => cstr!("constraint failed"),
        SQLITE_MISMATCH => cstr!("datatype mismatch"),
        SQLITE_MISUSE => cstr!("bad parameter or other API misuse"),
        SQLITE_NOLFS => cstr!("large file support is disabled"),
        SQLITE_AUTH => cstr!("authorization denied"),
        SQLITE_FORMAT => cstr!("format error"),
        SQLITE_RANGE => cstr!("column index out of range"),
        SQLITE_NOTADB => cstr!("file is not a database"),
        SQLITE_NOTICE => cstr!("notification message"),
        SQLITE_WARNING => cstr!("warning message"),
        _ => cstr!("unknown error"),
    }
}

/// True if `sql` forms one or more complete statements (ends in `;`).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_complete(sql: *const c_char) -> c_int {
    match c_str_opt(sql) {
        Some(s) => sql::is_complete(s) as c_int,
        None => 0,
    }
}

// ---- statement / connection introspection (real) --------------------------

/// The `sqlite3*` connection that prepared this statement.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_handle(p: *mut Stmt) -> *mut Sqlite3 {
    match stmt(p) {
        Some(s) => s.db,
        None => ptr::null_mut(),
    }
}

/// Non-zero if the prepared statement makes no direct changes to the database
/// (SELECT, transaction control, blank). DML/DDL/other → 0. A NULL statement is
/// read-only by sqlite's convention.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_readonly(p: *mut Stmt) -> c_int {
    let Some(s) = stmt(p) else { return 1 };
    match sql::classify(&s.sql) {
        sql::Kind::Read
        | sql::Kind::Begin
        | sql::Kind::Commit
        | sql::Kind::Rollback
        | sql::Kind::RollbackTo
        | sql::Kind::Savepoint
        | sql::Kind::Release => 1,
        _ => sql::is_blank(&s.sql) as c_int,
    }
}

/// Non-zero while the statement is mid-iteration (stepped, not yet done/reset).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_busy(p: *mut Stmt) -> c_int {
    match stmt(p) {
        Some(s) => (s.have_row || (s.executed && s.pos < s.rows.len())) as c_int,
        None => 0,
    }
}

/// The name of the `idx`-th bound parameter (1-based), including its sigil, or
/// NULL for an anonymous/numbered `?`/`?N`/`$N`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_name(p: *mut Stmt, idx: c_int) -> *const c_char {
    let Some(s) = stmt(p) else { return ptr::null() };
    if idx < 1 {
        return ptr::null();
    }
    match s.param_names.get((idx - 1) as usize) {
        Some(Some(name)) => name.as_ptr() as *const c_char,
        _ => ptr::null(),
    }
}

/// Best-effort `sqlite3_expanded_sql`: the raw statement text (mpedb binds
/// positionally, so no literal substitution is performed). Returned in a
/// libc-allocated buffer the caller frees with `sqlite3_free`. Only consumed by
/// the trace hook, which the shim never invokes.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_expanded_sql(p: *mut Stmt) -> *mut c_char {
    match stmt(p) {
        Some(s) => dup_cstr(&s.sql),
        None => ptr::null_mut(),
    }
}

// ---- connection configuration / callbacks (safe no-op stubs) --------------

/// Per-connection limits are not configurable; report "no limit". A set request
/// is ignored. Never affects a query answer.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_limit(_db: *mut Sqlite3, _id: c_int, _new_val: c_int) -> c_int {
    0x7fff_ffff
}

/// Fixed-arg shim over the variadic `sqlite3_db_config`. On the SysV/x86-64 ABI
/// the register layout matches the common `(sqlite3*, int op, int, int*)` forms
/// consumers use; we honor no toggles, so it is a success no-op. (Consumers do
/// not call this on the connect/CRUD paths.)
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_config(
    _db: *mut Sqlite3,
    _op: c_int,
    _a: c_int,
    _b: *mut c_void,
) -> c_int {
    SQLITE_OK
}

/// Toggling the load-extension switch is harmless; actual loading is refused
/// (see `sqlite3_load_extension`).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_enable_load_extension(_db: *mut Sqlite3, _onoff: c_int) -> c_int {
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_load_extension(
    _db: *mut Sqlite3,
    _file: *const c_char,
    _entry: *const c_char,
    errmsg: *mut *mut c_char,
) -> c_int {
    if !errmsg.is_null() {
        *errmsg = dup_cstr("loadable extensions are not supported by mpedb-capi");
    }
    SQLITE_ERROR
}

/// Tracing is not wired to mpedb; accept the registration and never call back.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_trace_v2(
    _db: *mut Sqlite3,
    _mask: c_uint,
    _cb: *mut c_void,
    _ctx: *mut c_void,
) -> c_int {
    SQLITE_OK
}

/// Progress callbacks are not wired to mpedb; accept and never call back.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_progress_handler(
    _db: *mut Sqlite3,
    _n: c_int,
    _cb: *mut c_void,
    _ctx: *mut c_void,
) {
}

/// No authorization layer: accept every statement (a permissive no-op — mpedb
/// enforces its own RLS/policies independently). Registration succeeds.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_authorizer(
    _db: *mut Sqlite3,
    _cb: *mut c_void,
    _ctx: *mut c_void,
) -> c_int {
    SQLITE_OK
}

// ---- user-defined functions / collations (refused — next milestone) -------

/// Invoke a caller-supplied destructor (`void(*)(void*)`) for `app` if present —
/// sqlite's contract on a failed `create_*` registration, so the caller does
/// not leak the wrapped state (e.g. CPython's Python callable).
unsafe fn call_destroy(destroy: *mut c_void, app: *mut c_void) {
    if !destroy.is_null() {
        let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(destroy);
        f(app);
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function_v2(
    db: *mut Sqlite3,
    _name: *const c_char,
    _n_arg: c_int,
    _enc: c_int,
    app: *mut c_void,
    _x_func: *mut c_void,
    _x_step: *mut c_void,
    _x_final: *mut c_void,
    x_destroy: *mut c_void,
) -> c_int {
    call_destroy(x_destroy, app);
    if let Some(c) = conn(db) {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "user-defined functions are not supported");
    }
    SQLITE_ERROR
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_window_function(
    db: *mut Sqlite3,
    _name: *const c_char,
    _n_arg: c_int,
    _enc: c_int,
    app: *mut c_void,
    _x_step: *mut c_void,
    _x_final: *mut c_void,
    _x_value: *mut c_void,
    _x_inverse: *mut c_void,
    x_destroy: *mut c_void,
) -> c_int {
    call_destroy(x_destroy, app);
    if let Some(c) = conn(db) {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "user-defined window functions are not supported");
    }
    SQLITE_ERROR
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation_v2(
    db: *mut Sqlite3,
    _name: *const c_char,
    _enc: c_int,
    arg: *mut c_void,
    _x_compare: *mut c_void,
    x_destroy: *mut c_void,
) -> c_int {
    call_destroy(x_destroy, arg);
    if let Some(c) = conn(db) {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "user-defined collations are not supported");
    }
    SQLITE_ERROR
}

// ---- UDF-callback accessors (only reachable from a UDF, which never fires) --

#[no_mangle]
pub unsafe extern "C" fn sqlite3_user_data(_ctx: *mut c_void) -> *mut c_void {
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_aggregate_context(_ctx: *mut c_void, _n: c_int) -> *mut c_void {
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_context_db_handle(_ctx: *mut c_void) -> *mut Sqlite3 {
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_null(_ctx: *mut c_void) {}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_int64(_ctx: *mut c_void, _v: c_longlong) {}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_double(_ctx: *mut c_void, _v: c_double) {}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_text(
    _ctx: *mut c_void,
    _t: *const c_char,
    _n: c_int,
    _d: *mut c_void,
) {
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_blob(
    _ctx: *mut c_void,
    _b: *const c_void,
    _n: c_int,
    _d: *mut c_void,
) {
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error(_ctx: *mut c_void, _t: *const c_char, _n: c_int) {}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_nomem(_ctx: *mut c_void) {}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_toobig(_ctx: *mut c_void) {}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_type(_v: *mut c_void) -> c_int {
    SQLITE_NULL
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int64(_v: *mut c_void) -> c_longlong {
    0
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_double(_v: *mut c_void) -> c_double {
    0.0
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_bytes(_v: *mut c_void) -> c_int {
    0
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_text(_v: *mut c_void) -> *const c_uchar {
    ptr::null()
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_blob(_v: *mut c_void) -> *const c_void {
    ptr::null()
}

// ---- online backup (refused — use `mpedb mirror`) -------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_init(
    _dst: *mut Sqlite3,
    _dst_name: *const c_char,
    _src: *mut Sqlite3,
    _src_name: *const c_char,
) -> *mut c_void {
    ptr::null_mut()
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_step(_b: *mut c_void, _n: c_int) -> c_int {
    SQLITE_ERROR
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_finish(_b: *mut c_void) -> c_int {
    SQLITE_OK
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_remaining(_b: *mut c_void) -> c_int {
    0
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_pagecount(_b: *mut c_void) -> c_int {
    0
}

// ---- incremental blob (refused — maps to mpedb #43 later) -----------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_open(
    _db: *mut Sqlite3,
    _dbname: *const c_char,
    _table: *const c_char,
    _column: *const c_char,
    _row: c_longlong,
    _flags: c_int,
    pp_blob: *mut *mut c_void,
) -> c_int {
    if !pp_blob.is_null() {
        *pp_blob = ptr::null_mut();
    }
    SQLITE_ERROR
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_close(_b: *mut c_void) -> c_int {
    SQLITE_OK
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_bytes(_b: *mut c_void) -> c_int {
    0
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_read(
    _b: *mut c_void,
    _out: *mut c_void,
    _n: c_int,
    _off: c_int,
) -> c_int {
    SQLITE_ERROR
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_write(
    _b: *mut c_void,
    _data: *const c_void,
    _n: c_int,
    _off: c_int,
) -> c_int {
    SQLITE_ERROR
}

// ---- serialize / deserialize (refused) ------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3_serialize(
    _db: *mut Sqlite3,
    _schema: *const c_char,
    p_size: *mut c_longlong,
    _flags: c_uint,
) -> *mut c_uchar {
    if !p_size.is_null() {
        *p_size = 0;
    }
    ptr::null_mut()
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_deserialize(
    _db: *mut Sqlite3,
    _schema: *const c_char,
    _data: *mut c_uchar,
    _sz: c_longlong,
    _sz_buf: c_longlong,
    _flags: c_uint,
) -> c_int {
    SQLITE_ERROR
}
