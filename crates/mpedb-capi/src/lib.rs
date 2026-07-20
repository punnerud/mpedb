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

mod auth;
mod backup;
mod blob;
mod consts;
mod introspect;
mod sql;
mod udf;
mod valconv;

pub use auth::{SQLITE_DENY, SQLITE_IGNORE};
pub use backup::*;
pub use blob::*;
pub use consts::*;

use mpedb::{Config, Database, Error as DbError, ExecResult, Value, WriteSession};
use std::collections::HashMap;
use std::os::raw::{c_char, c_double, c_int, c_longlong, c_uchar, c_uint, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

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
    /// What `path` is: a real file the caller named, or the tmpfs file standing
    /// in for an in-memory database (which is removed again on close).
    backing: Backing,
    busy_timeout_ms: c_int,
    /// Set by `sqlite3_interrupt` (possibly from another thread); polled by the
    /// running statement at step entry and during the busy-retry wait. An
    /// atomic so the interrupting thread touches ONLY this field, never the
    /// rest of the connection.
    interrupted: AtomicBool,
    err_code: c_int,
    err_ext: c_int,
    err_msg: Vec<u8>, // NUL-terminated
    changes: c_int,
    total_changes: c_int,
    last_insert_rowid: c_longlong,
    /// Host scalar UDFs registered on this connection via
    /// `sqlite3_create_function[_v2]` (design/DESIGN-UDF.md). The CLOSURES live
    /// in the `Database` registry; this tracks each registration's `pApp` +
    /// `xDestroy` so the caller's destructor runs when an entry is replaced,
    /// deleted, or the connection closes — CPython wraps a Python callable in
    /// `pApp` and would otherwise leak it.
    host_fns: Vec<udf::HostFn>,
    /// Registered COLLATING SEQUENCES (`sqlite3_create_collation_v2`), tracked
    /// for the same reason as `host_fns`: sqlite runs the caller's `xDestroy`
    /// when an entry is replaced, deleted, or the connection closes, and
    /// CPython wraps a Python callable in `pApp`.
    host_colls: Vec<udf::HostColl>,
    /// `sqlite3_trace_v2` registration: event mask + `xCallback` + `pCtx`. The
    /// only event the shim emits is `SQLITE_TRACE_STMT`, fired as a statement
    /// begins running (see `trace_stmt_begin`); other mask bits are accepted
    /// and simply never fire.
    trace_mask: u32,
    trace_cb: *mut c_void,
    trace_ctx: *mut c_void,
    /// `sqlite3_progress_handler` registration. The shim has no VM opcode
    /// stream to count, so the handler fires once per statement execution — a
    /// coarse but honest "invoked periodically during evaluation" — and a
    /// non-zero return interrupts the statement (`SQLITE_INTERRUPT`), which is
    /// the part consumers (CPython) actually rely on for cancellation.
    progress_cb: *mut c_void,
    progress_ctx: *mut c_void,
    /// Per-connection run-time limits (`sqlite3_limit`), seeded with sqlite's
    /// compile-time defaults. Get/set is faithful (prior value returned, bad
    /// category -> -1). Enforced where the shim itself does the work:
    /// `VARIABLE_NUMBER` at prepare and `LENGTH` in `sqlite3_expanded_sql`;
    /// CPython enforces `SQL_LENGTH` by reading the stored value.
    limits: [c_int; SQLITE_N_LIMIT],
    /// `file:…?mode=ro` (or an open_v2 READONLY flag without READWRITE/CREATE):
    /// every non-read statement is refused with `SQLITE_READONLY`.
    readonly: bool,
    /// Open incremental-blob handles (`sqlite3_blob_open`, `blob.rs`). Each
    /// pointer is a live `Box<Sqlite3Blob>` removed by `sqlite3_blob_close`;
    /// `sqlite3_close` refuses (`SQLITE_BUSY`) while any remain, so a handle's
    /// back-pointer to this connection can never dangle.
    blobs: Vec<*mut blob::Sqlite3Blob>,
    /// `sqlite3_close_v2` was called while a blob handle was still open: the
    /// connection is logically closed but kept alive for that handle, and is
    /// freed by the last `sqlite3_blob_close` (sqlite's zombie connection).
    zombie: bool,
    /// `sqlite3_set_authorizer` registration (`auth.rs`): consulted at PREPARE
    /// for every action the statement performs. NULL = no gate, and then no
    /// extra compile happens at all.
    auth_cb: *mut c_void,
    auth_ctx: *mut c_void,
    /// Outstanding `sqlite3_backup_*` handles whose DESTINATION is this
    /// connection (`backup.rs`). Each is a live `Box<Sqlite3Backup>` holding a
    /// back-pointer here; `sqlite3_close` refuses while any remain, exactly as
    /// it does for open blob handles, so that pointer cannot dangle.
    backups: Vec<*mut backup::Sqlite3Backup>,
}

/// sqlite 3.45's compile-time limit defaults — both the initial value and the
/// hard upper bound a `sqlite3_limit` set is truncated to.
const DEFAULT_LIMITS: [c_int; SQLITE_N_LIMIT] = [
    1_000_000_000, // LENGTH
    1_000_000_000, // SQL_LENGTH
    2000,          // COLUMN
    1000,          // EXPR_DEPTH
    500,           // COMPOUND_SELECT
    250_000_000,   // VDBE_OP
    127,           // FUNCTION_ARG
    10,            // ATTACHED
    50_000,        // LIKE_PATTERN_LENGTH
    32_766,        // VARIABLE_NUMBER
    1000,          // TRIGGER_DEPTH
    0,             // WORKER_THREADS
];

/// A prepared statement: the SQL, its bound parameters, and — once stepped —
/// the materialized result it hands out one row at a time.
pub struct Stmt {
    db: *mut Sqlite3,
    /// The original statement text as prepared (used for classification, PRAGMA/
    /// `sqlite_master` introspection and `sqlite3_expanded_sql`).
    sql: String,
    /// `sql` with every bound parameter rewritten to mpedb's numbered `$K` form
    /// (see `sql::scan_params`). This is what the engine actually parses/executes,
    /// so `:name`/`@name`/`$name`/`?` all reach mpedb — which only speaks `$N` —
    /// as the numbered placeholders they were assigned.
    exec_sql: String,
    /// The sqlite parameter count (`sqlite3_bind_parameter_count`): the highest
    /// parameter number used, across all kinds sharing one numbering space.
    n_params: usize,
    /// Per-parameter spelling in number order (`sqlite3_bind_parameter_name`):
    /// NUL-terminated bytes for a named `:a`/`@a`/`$a` or an explicit `?N`/`$n`
    /// (sigil included), `None` for an anonymous `?` or a number an explicit `?N`
    /// skipped. mpedb binds positionally against `exec_sql`, so this is the map a
    /// caller uses to find a name's slot.
    param_names: Vec<Option<Vec<u8>>>,
    binds: Vec<Value>,
    /// True once the statement has run since the last `reset` (or ever).
    executed: bool,
    /// Result column names (known after execution).
    columns: Vec<String>,
    col_name_c: Vec<Vec<u8>>, // NUL-terminated, aligned to `columns`
    /// Per-column declared type (`sqlite3_column_decltype`), computed LAZILY the
    /// first time it is asked for (zero cost for consumers that never read it):
    /// `None` = not yet computed; inner `None` = this column has no decltype
    /// (NULL). NUL-terminated bytes, aligned to `columns`.
    decltype_c: Option<Vec<Option<Vec<u8>>>>,
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
        // Consumers grep sqlite's canonical phrasings ("… constraint failed",
        // "database is locked"); render those messages sqlite-shaped, with
        // mpedb's detail preserved after them.
        match valconv::sqlite_shaped_message(e) {
            Some(msg) => self.set_error(code, ext, &msg),
            None => self.set_error(code, ext, &e.to_string()),
        }
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
    // `INSERT OR ROLLBACK` is the one conflict action a statement cannot carry
    // out on its own — it aborts the enclosing TRANSACTION, and the connection
    // is what owns that. mpedb's parser refuses it by name; the shim runs it as
    // `OR ABORT` and rolls the connection back itself when the conflict fires
    // (sqlite's exact definition of the action). See `sql::rewrite_insert_or_rollback`.
    let (or_rollback_sql, or_rollback) = sql::rewrite_insert_or_rollback(sqltext);
    if or_rollback {
        let res = exec_one_inner(c, &or_rollback_sql, params);
        if let Err(e) = &res {
            if valconv::error_codes(e).0 == SQLITE_CONSTRAINT {
                rollback_txn(c);
            }
        }
        return res;
    }
    exec_one_inner(c, sqltext, params)
}

fn exec_one_inner(c: &mut Sqlite3, sqltext: &str, params: &[Value]) -> Result<Outcome, DbError> {
    use sql::Kind;
    // sqlite's parser skips leading comments; mpedb's does not — strip them
    // here so `-- comment\nINSERT …` (a shape CPython's suite and iterdump
    // scripts use) reaches the engine as the statement it is.
    let sqltext = sql::strip_leading_trivia(sqltext);
    // `zeroblob(<const>)` → the byte-identical blob literal, so it is accepted
    // in INSERT-values position where mpedb refuses a function call (blob.rs).
    // Idempotent: the step path already rewrote at prepare, leaving no call.
    let rewritten_zb = sql::rewrite_zeroblob(sqltext);
    let sqltext: &str = &rewritten_zb;
    // `EXPLAIN QUERY PLAN <stmt>` → mpedb's own `EXPLAIN <stmt>`, reshaped by
    // `eqp_outcome` below. The rewrite happens here rather than at prepare so
    // `sqlite3_sql()` still reports the text the consumer wrote.
    let eqp_rewritten;
    let (sqltext, eqp) = match sql::explain_query_plan_body(sqltext) {
        Some(body) => {
            eqp_rewritten = format!("EXPLAIN {body}");
            (eqp_rewritten.as_str(), true)
        }
        None => (sqltext, false),
    };
    match sql::classify(sqltext) {
        // PRAGMA and sqlite_master reads are answered by the shim's schema
        // introspection (mpedb has neither); they never reach the engine.
        Kind::Pragma => {
            // #51: `PRAGMA database_list` needs the connection's attach list,
            // which introspect (schema-only) cannot see — answer it here.
            // Shape derived from sqlite (probe P9): seq 0 = main (path, or ''
            // for an in-memory database), attached start at seq 2 (1 is
            // temp's reserved slot, which mpedb does not have).
            if introspect::parse_pragma(sqltext)
                .0
                .eq_ignore_ascii_case("database_list")
            {
                let main_file = match c.backing {
                    Backing::File => c.path.to_string_lossy().into_owned(),
                    _ => String::new(),
                };
                let mut rows = vec![vec![
                    Value::Int(0),
                    Value::Text("main".into()),
                    Value::Text(main_file),
                ]];
                for (i, (name, path)) in c.db.attached_databases().into_iter().enumerate() {
                    rows.push(vec![
                        Value::Int(i as i64 + 2),
                        Value::Text(name),
                        Value::Text(path.to_string_lossy().into_owned()),
                    ]);
                }
                return Ok(Outcome::Rows {
                    columns: vec!["seq".into(), "name".into(), "file".into()],
                    rows,
                });
            }
            let bundle = c.db.schema();
            let (columns, rows) = introspect::pragma(&bundle, sqltext, &mut c.busy_timeout_ms)?;
            // `PRAGMA busy_timeout = N` may have moved the knob — mirror it
            // into the engine's writer-lock deadline (#109), same as
            // `sqlite3_busy_timeout`. Unconditional: an atomic store, cheap.
            c.db.set_busy_timeout(Some(Duration::from_millis(c.busy_timeout_ms.max(0) as u64)));
            Ok(Outcome::Rows { columns, rows })
        }
        // `EXPLAIN QUERY PLAN SELECT … FROM sqlite_master` is excluded: the
        // mini-evaluator answers ROWS, not a plan, and mpedb has no such table
        // to plan against — it refuses by name instead of answering the wrong
        // shape.
        Kind::Read if !eqp && introspect::references_sqlite_master(sqltext) => {
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
        // VACUUM / ANALYZE: nothing to do (see `Kind::Maintenance`) — succeed
        // with no rows and no change counters, as sqlite's do on a tidy file.
        Kind::Maintenance => Ok(Outcome::Control),
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
            let res = res?;
            Ok(if eqp { eqp_outcome(res) } else { to_outcome(res) })
        }
    }
}

/// mpedb's plan text in sqlite's `EXPLAIN QUERY PLAN` shape: four columns
/// `(id, parent, notused, detail)`, one row per line of the plan.
///
/// The `detail` strings are mpedb's own (`Select t`, `access: FullScan`, …),
/// not sqlite's (`SCAN t`, `SEARCH t USING INDEX …`) — sqlite documents EQP
/// output as human-facing and explicitly unstable between releases, so the
/// honest answer here is a description of the plan mpedb will actually run.
/// Indentation in mpedb's text nests the plan; it is preserved in `detail` and
/// also expressed structurally, with an indented line's `parent` pointing at
/// the nearest less-indented line above it, as sqlite's tree does.
fn eqp_outcome(res: ExecResult) -> Outcome {
    let ExecResult::Explain(text) = res else {
        return to_outcome(res);
    };
    let columns = ["id", "parent", "notused", "detail"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // (indent, id) of the lines still eligible to be a parent.
    let mut stack: Vec<(usize, i64)> = Vec::new();
    let mut rows = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let indent = line.len() - line.trim_start().len();
        while stack.last().is_some_and(|&(i, _)| i >= indent) {
            stack.pop();
        }
        let id = rows.len() as i64 + 1;
        let parent = stack.last().map_or(0, |&(_, p)| p);
        stack.push((indent, id));
        rows.push(vec![
            Value::Int(id),
            Value::Int(parent),
            Value::Int(0),
            Value::Text(line.trim_end().to_string()),
        ]);
    }
    Outcome::Rows { columns, rows }
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

/// A contention error a RETRY can clear — an optimistic-mode `WriteConflict`
/// (the loser rolled back, nothing applied), a full reader table, or an evicted
/// read snapshot. valconv maps all three to `SQLITE_BUSY`; `busy_timeout` waits
/// on exactly these.
///
/// `DbError::Busy` is deliberately NOT here (#109): it means the ENGINE
/// already waited out this connection's busy timeout at the writer lock
/// (`Database::set_busy_timeout`, wired at open / `sqlite3_busy_timeout` /
/// `PRAGMA busy_timeout`) — retrying it in this loop would double the wait.
/// It maps straight to `SQLITE_BUSY` ("database is locked").
fn is_busy_err(e: &DbError) -> bool {
    matches!(
        e,
        DbError::WriteConflict | DbError::ReadersFull | DbError::SnapshotEvicted
    ) || valconv::is_writer_lock_reentry(e)
}

/// sqlite's own default-busy-handler delay table (ms), then 100 ms steady.
fn busy_backoff(tries: u32) -> Duration {
    const DELAYS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];
    Duration::from_millis(DELAYS[(tries as usize).min(DELAYS.len() - 1)])
}

fn run_stmt(s: &mut Stmt) -> c_int {
    let Some(c) = (unsafe { conn(s.db) }) else {
        return SQLITE_MISUSE;
    };
    let is_dml = matches!(sql::classify(&s.sql), sql::Kind::Dml { .. });
    let params = s.binds.clone();
    // An interrupt requested before we start aborts this step and is consumed
    // (sqlite clears the flag when the interrupted statement finishes).
    if c.interrupted.swap(false, Ordering::SeqCst) {
        c.set_error(SQLITE_INTERRUPT, SQLITE_INTERRUPT, "interrupted");
        return SQLITE_INTERRUPT;
    }
    // A read-only connection (`file:…?mode=ro`) refuses every statement that
    // could write. Transaction control is allowed (as sqlite does): the write
    // inside it is what gets refused.
    if c.readonly && matches!(sql::classify(&s.sql), sql::Kind::Dml { .. } | sql::Kind::Ddl) {
        c.set_error(SQLITE_READONLY, SQLITE_READONLY, "attempt to write a readonly database");
        return SQLITE_READONLY;
    }
    // The statement is about to run: drain any stale UDF-error stash so an
    // error surfaced by THIS run is attributable to this run alone.
    udf::take_last_udf_error();
    // `busy_timeout(ms)`: on a RETRYABLE contention error (`is_busy_err`),
    // sleep with sqlite's backoff and retry until the deadline, exactly as
    // sqlite's default busy handler does — a transient conflict clears
    // instead of failing the call. Zero timeout (the default) = no retry,
    // immediate BUSY, as sqlite. Writer-LOCK contention never reaches this
    // loop: the engine itself waits out the same timeout at the lock
    // (`Database::set_busy_timeout`, #109) and returns the terminal
    // `DbError::Busy` — retrying that here would double the wait.
    let deadline =
        (c.busy_timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(c.busy_timeout_ms as u64));
    // Execute the parameter-rewritten text (`$K` placeholders) so mpedb binds
    // the caller's values by number; classification/introspection are unaffected
    // by the rewrite (only placeholders change), so they still use `s.sql`.
    let mut tries = 0u32;
    let outcome = loop {
        match catch_unwind(AssertUnwindSafe(|| exec_one(c, &s.exec_sql, &params))) {
            Ok(Err(ref e)) if is_busy_err(e) && deadline.is_some_and(|d| Instant::now() < d) => {
                // sqlite3_interrupt breaks the busy wait instead of sleeping on.
                if c.interrupted.swap(false, Ordering::SeqCst) {
                    c.set_error(SQLITE_INTERRUPT, SQLITE_INTERRUPT, "interrupted");
                    return SQLITE_INTERRUPT;
                }
                std::thread::sleep(busy_backoff(tries));
                tries += 1;
                continue;
            }
            Ok(r) => break r,
            Err(_) => {
                c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) in engine");
                return SQLITE_ERROR;
            }
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
            s.decltype_c = None;
            s.rows.clear();
            s.pos = 0;
            s.executed = true;
            c.clear_error();
            SQLITE_OK
        }
        Ok(Outcome::Control) => {
            s.columns.clear();
            s.col_name_c.clear();
            s.decltype_c = None;
            s.rows.clear();
            s.pos = 0;
            s.executed = true;
            c.clear_error();
            SQLITE_OK
        }
        Err(e) => {
            // A host UDF that called `sqlite3_result_error*` failed this
            // statement: the engine tunnels that as an opaque `Unsupported`
            // wrapper ("user function raised: …"), but the CONSUMER'S contract
            // is the code and text the callback itself set — CPython maps
            // NOMEM -> MemoryError, TOOBIG -> DataError, and asserts the exact
            // message. Present the callback's own error when this statement's
            // failure is that error.
            if let Some((code, msg)) = udf::take_last_udf_error() {
                if e.to_string().ends_with(&msg) {
                    let primary = code & 0xff;
                    c.set_error(primary, code, &msg);
                    return primary;
                }
            }
            c.set_db_error(&e)
        }
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

/// How a connection's backing file is owned. mpedb always has a file; what
/// differs is whether the CALLER named it (and therefore keeps it) or asked for
/// an in-memory database (and must not find it again afterwards).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backing {
    /// Unnamed in-memory (`:memory:`): removed when this connection closes.
    Ephemeral,
    /// Named in-memory (`file:n?mode=memory`): removed when the LAST connection
    /// to the name in this process closes.
    NamedMemory,
    /// A real file the caller named: never removed.
    File,
}

enum Target {
    /// A private, unnamed in-memory database: one per open, gone on close.
    Ephemeral,
    /// A NAMED in-memory database (`file:name?mode=memory`): private to this
    /// process, but every open of the same name within it sees the same data
    /// (sqlite's `cache=shared` in-memory semantics). Gone when the last
    /// connection to the name closes.
    NamedMemory(PathBuf),
    File(PathBuf),
}

/// Value of a `key=` parameter in a `file:` URI's query string.
fn uri_param<'a>(filename: Option<&'a str>, key: &str) -> Option<&'a str> {
    let query = filename?.trim().strip_prefix("file:")?.split_once('?')?.1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// Map a named in-memory database to its backing path. mpedb has no pure
/// in-memory pager — an "in-memory" database is a small file in `/dev/shm` (a
/// tmpfs, so it never touches a disk) — but that file must behave like memory:
/// PRIVATE TO THIS PROCESS (hence the pid) and NOT SURVIVING it. The name is
/// sanitized because it comes from a URI and becomes a path component.
fn named_memory_path(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(64)
        .collect();
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    dir.join(format!("mpedb-capi-{}-mem-{}.mpedb", std::process::id(), safe))
}

/// Percent-decode a `file:` URI's path portion, byte-wise. sqlite decodes %HH
/// escapes in URI filenames, and the RESULT is OS path bytes — not necessarily
/// UTF-8 (CPython encodes undecodable paths with surrogateescape and quotes
/// them into the URI).
fn pct_decode(s: &str) -> Vec<u8> {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn os_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

fn resolve_target(filename: Option<&str>, raw: Option<&[u8]>, flags: c_int) -> Target {
    if flags & SQLITE_OPEN_MEMORY != 0 {
        return Target::Ephemeral;
    }
    // A filename that is not valid UTF-8 cannot be a `file:` URI (URIs are
    // ASCII once percent-encoded): it is a plain OS path, byte-for-byte.
    let Some(name) = filename else {
        return match raw {
            Some(b) if !b.is_empty() => Target::File(os_path(b)),
            _ => Target::Ephemeral,
        };
    };
    let name = name.trim();
    // Minimal file: URI handling.
    if let Some(rest) = name.strip_prefix("file:") {
        let path = rest.split('?').next().unwrap_or("");
        if path == ":memory:" || path.is_empty() {
            return Target::Ephemeral;
        }
        // `mode=memory` makes the name an IN-MEMORY database's name, not a
        // path — sqlite creates no file for it. Django's test runner names its
        // test databases exactly this way (`file:memorydb_default?mode=memory&
        // cache=shared`), so reading the name as a path both dropped a 64 MiB
        // file in the caller's CWD and, worse, made the "in-memory" database
        // SURVIVE the process and be silently reopened by the next run.
        if uri_param(filename, "mode") == Some("memory") {
            return Target::NamedMemory(named_memory_path(path));
        }
        // sqlite percent-decodes the URI's path (the bytes may be non-UTF-8).
        return Target::File(os_path(&pct_decode(path)));
    }
    if name.is_empty() || name == ":memory:" {
        Target::Ephemeral
    } else {
        Target::File(PathBuf::from(name))
    }
}

/// Open count per named in-memory database, for this process. The first open
/// of a name starts it EMPTY (a fresh in-memory database), later opens attach
/// to the same one, and the last close removes the backing file.
static NAMED_MEMORY: Mutex<Option<HashMap<PathBuf, usize>>> = Mutex::new(None);

fn named_memory_acquire(path: &std::path::Path) -> bool {
    let mut g = NAMED_MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    let n = map.entry(path.to_path_buf()).or_insert(0);
    *n += 1;
    *n == 1 // first opener: start from empty
}

fn named_memory_release(path: &std::path::Path) -> bool {
    let mut g = NAMED_MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = g.as_mut() else { return false };
    match map.get_mut(path) {
        Some(n) if *n > 1 => {
            *n -= 1;
            false
        }
        Some(_) => {
            map.remove(path);
            true // last one out: the database ceases to exist
        }
        None => false,
    }
}

/// A `size_mb=N` (or `max_size_mb=N`) query parameter on a `file:` URI — the
/// pre-reserved maximum size of a NEW database (mpedb fallocates it, so this is
/// "reserve N MiB and never grow"; exceeding it is `SQLITE_FULL`). Clamped to
/// the engine cap. Ignored for an existing file, whose geometry is fixed at
/// creation. Lets a C-API caller open a large (e.g. 800 GiB) mpedb the shim
/// would otherwise cap at its 64 MiB default.
fn requested_size_mb(filename: Option<&str>) -> Option<u64> {
    let query = filename?.trim().strip_prefix("file:")?.split_once('?')?.1;
    for kv in query.split('&') {
        if let Some(v) = kv
            .strip_prefix("size_mb=")
            .or_else(|| kv.strip_prefix("max_size_mb="))
        {
            if let Ok(n) = v.parse::<u64>() {
                return Some(n.clamp(1, mpedb::MAX_DB_SIZE_MB));
            }
        }
    }
    None
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

/// SQL functions that describe the sqlite **build** rather than the data.
/// mpedb's binder has no notion of them (it is not sqlite and has no compile
/// options), yet a consumer may call them at connection setup — Django's
/// `register_functions()` runs `select sqlite_compileoption_used(
/// 'ENABLE_MATH_FUNCTIONS')` before it will hand out a connection at all.
///
/// Both are answered with the LITERAL TRUTH about mpedb, never a guess: mpedb
/// defines an EMPTY set of sqlite compile options, so no name was ever "used"
/// (0) and no index into the list is in range (NULL). For Django that 0 is also
/// the useful answer: it makes Django register its own `ACOS`/`CEILING`/
/// `POWER`/… fallbacks — its spellings, its semantics — instead of assuming
/// sqlite's math built-ins are present under sqlite's exact names.
///
/// Registered per connection, at open, before any statement can run.
fn register_shim_builtins(db: &Database) {
    // sqlite: 1 iff the named option was defined at compile time; NULL in, NULL
    // out (verified against sqlite 3.45).
    db.register_host_function("sqlite_compileoption_used", 1, |args: &[Value]| {
        Ok(match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(_) => Value::Int(0),
        })
    });
    // sqlite: the N-th compile option's name, NULL once N runs past the end.
    // mpedb's list is empty, so every N is past the end.
    db.register_host_function("sqlite_compileoption_get", 1, |_args: &[Value]| {
        Ok(Value::Null)
    });
    // `zeroblob(N)`: N zero bytes (sqlite core function; CPython's suite uses
    // it to seed blob rows). mpedb has no lazy zero-run representation, so the
    // blob is materialized — semantically identical; `blob::MAX_BLOB_LEN`
    // guards the allocation with sqlite's own SQLITE_MAX_LENGTH refusal.
    db.register_host_function("zeroblob", 1, |args: &[Value]| blob::zeroblob_value(args));
}

fn open_impl(raw_name: Option<&[u8]>, flags: c_int) -> Result<Box<Sqlite3>, (c_int, String)> {
    // URI/`:memory:` recognition needs text; a non-UTF-8 name is a plain path.
    let filename = raw_name.and_then(|b| std::str::from_utf8(b).ok());
    let target = resolve_target(filename, raw_name, flags);
    // `file:…?mode=ro` (sqlite's URI read-only mode) or a READONLY flag with
    // neither READWRITE nor CREATE: the connection refuses every write with
    // SQLITE_READONLY, and a missing file is NOT created.
    let readonly = uri_param(filename, "mode") == Some("ro")
        || (flags & SQLITE_OPEN_READONLY != 0
            && flags & (SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE) == 0);
    // `file:…?size_mb=N` requests a specific pre-reserved size (mpedb fallocates
    // it — reserve, don't grow); otherwise a small default. Only meaningful for a
    // NEW file; an existing one keeps the geometry it was created with.
    let req = requested_size_mb(filename);
    let (path, kind, size_mb) = match target {
        Target::Ephemeral => (ephemeral_path(), Backing::Ephemeral, req.unwrap_or(16)),
        Target::NamedMemory(p) => (p, Backing::NamedMemory, req.unwrap_or(16)),
        Target::File(p) => (p, Backing::File, req.unwrap_or(64)),
    };

    // A named in-memory database starts empty on its FIRST open in this
    // process and is attached (not recreated) by every later one.
    let fresh_memory = matches!(kind, Backing::NamedMemory) && named_memory_acquire(&path);
    let exists = match kind {
        Backing::Ephemeral => false,
        Backing::NamedMemory => {
            if fresh_memory {
                let _ = std::fs::remove_file(&path);
            }
            !fresh_memory
        }
        Backing::File => path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false),
    };
    if matches!(kind, Backing::Ephemeral) {
        let _ = std::fs::remove_file(&path);
    }
    let attach = || -> Result<Database, (c_int, String)> {
        if exists {
            // Attach an existing mpedb file config-free (reads its stored schema).
            // The message leads with sqlite's canonical phrase — consumers
            // (CPython's tests included) grep for "unable to open database
            // file" — and keeps the real reason after it.
            return Database::open_from_file(&path).map_err(|e| {
                (
                    SQLITE_CANTOPEN,
                    format!("unable to open database file: cannot open `{}`: {e}", path.display()),
                )
            });
        }
        // Fresh database: creating requires the CREATE flag (open_v2 semantics;
        // plain sqlite3_open always sets it — see the callers), and a read-only
        // open never creates, whatever the flags say (sqlite's mode=ro rule).
        if flags & SQLITE_OPEN_CREATE == 0 || readonly {
            return Err((
                SQLITE_CANTOPEN,
                format!("unable to open database file: no such database file: {}", path.display()),
            ));
        }
        let mut cfg = Config::from_toml_str(&seed_toml(&path, size_mb))
            .map_err(|e| (SQLITE_CANTOPEN, format!("config error: {e}")))?;
        // The TOML carried a lossy rendering of the path (TOML strings are
        // UTF-8; an OS path need not be). Overwrite with the exact bytes.
        cfg.options.path = path.clone();
        Database::open_with_config(cfg).map_err(|e| {
            (
                SQLITE_CANTOPEN,
                format!("unable to open database file: cannot create `{}`: {e}", path.display()),
            )
        })
    };
    let db = match attach() {
        Ok(db) => db,
        Err(e) => {
            // A failed open holds no reference: undo the acquire, or the name
            // would never be freshened again in this process.
            if matches!(kind, Backing::NamedMemory) {
                named_memory_release(&path);
            }
            return Err(e);
        }
    };

    register_shim_builtins(&db);

    // #109: bound the facade's writer-lock waits from the very first
    // statement. sqlite's default is NO busy handler — immediate SQLITE_BUSY
    // on contention — which is timeout 0; `sqlite3_busy_timeout` / `PRAGMA
    // busy_timeout` raise it. Without this the engine would block forever
    // under cross-process writer contention (compat gap E1).
    db.set_busy_timeout(Some(Duration::ZERO));

    let mut c = Box::new(Sqlite3 {
        txn: None,
        db,
        path,
        backing: kind,
        busy_timeout_ms: 0,
        interrupted: AtomicBool::new(false),
        err_code: SQLITE_OK,
        err_ext: SQLITE_OK,
        err_msg: Vec::new(),
        changes: 0,
        total_changes: 0,
        last_insert_rowid: 0,
        host_fns: Vec::new(),
        host_colls: Vec::new(),
        trace_mask: 0,
        trace_cb: ptr::null_mut(),
        trace_ctx: ptr::null_mut(),
        progress_cb: ptr::null_mut(),
        progress_ctx: ptr::null_mut(),
        limits: DEFAULT_LIMITS,
        readonly,
        blobs: Vec::new(),
        zombie: false,
        auth_cb: ptr::null_mut(),
        auth_ctx: ptr::null_mut(),
        backups: Vec::new(),
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
    vfs: *const c_char,
) -> c_int {
    let rc = open_common(filename, pp_db, flags);
    // A named VFS: mpedb runs no sqlite VFS modules (it has its own storage
    // engine, not sqlite's pager). The built-in VFS names denote ordinary OS
    // file I/O, which mpedb provides its own way — honor them as a no-op. A
    // CUSTOM/unknown VFS (encryption, cloud, in-memory shim) CANNOT be honored,
    // and silently ignoring one would be unsafe (plaintext where an encryption
    // VFS was expected). So refuse it with an error — as sqlite refuses an
    // unregistered VFS — rather than pretend it is active. The handle is still
    // returned (sqlite contract: close it even on open error).
    if rc == SQLITE_OK && !pp_db.is_null() {
        if let Some(name) = c_str_opt(vfs) {
            const BUILTIN: &[&str] = &[
                "unix", "unix-none", "unix-dotfile", "unix-excl", "unix-namedsem",
                "win32", "win32-none", "win32-longpath", "memdb",
            ];
            if !BUILTIN.iter().any(|b| b.eq_ignore_ascii_case(name)) {
                if let Some(c) = conn(*pp_db) {
                    c.set_error(SQLITE_ERROR, SQLITE_ERROR, &format!("no such vfs: {name}"));
                }
                return SQLITE_ERROR;
            }
        }
    }
    rc
}

/// Why the last `sqlite3_open*` in this process failed: `(code, NUL-terminated
/// message)`.
///
/// A failed open hands back NO handle (sqlite may, but only when it got far
/// enough to allocate one), so the caller's only way to ask "why" is
/// `sqlite3_errmsg(NULL)` — for which sqlite has the fixed, useless answer
/// "out of memory". CPython's `sqlite3` does exactly that and reported EVERY
/// failed open as `InterfaceError: out of memory`, hiding e.g. a real
/// "cannot open `x`: schema format v6, expected v7". Answering the real reason
/// there cannot break a consumer that expects sqlite's constant — no consumer
/// can act on "out of memory" — and it is the difference between a diagnosable
/// failure and a lie.
static LAST_OPEN_ERR: Mutex<Option<(c_int, Vec<u8>)>> = Mutex::new(None);

fn set_open_error(code: c_int, msg: String) {
    let mut bytes = msg.into_bytes();
    bytes.retain(|b| *b != 0);
    bytes.push(0);
    *LAST_OPEN_ERR.lock().unwrap_or_else(|e| e.into_inner()) = Some((code, bytes));
}

thread_local! {
    /// Per-thread copy of `LAST_OPEN_ERR`'s text, so `sqlite3_errmsg(NULL)` can
    /// hand out a pointer that stays valid until this thread's next such call —
    /// sqlite's own lifetime rule for an error string.
    static OPEN_ERR_TLS: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn last_open_error() -> Option<(c_int, *const c_char)> {
    let (code, bytes) = LAST_OPEN_ERR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    let ptr = OPEN_ERR_TLS.with(|t| {
        let mut t = t.borrow_mut();
        *t = bytes;
        t.as_ptr() as *const c_char
    });
    Some((code, ptr))
}

unsafe fn open_common(filename: *const c_char, pp_db: *mut *mut Sqlite3, flags: c_int) -> c_int {
    if pp_db.is_null() {
        return SQLITE_MISUSE;
    }
    let name = c_bytes(filename, -1);
    match catch_unwind(AssertUnwindSafe(|| open_impl(name, flags))) {
        Ok(Ok(boxed)) => {
            *pp_db = Box::into_raw(boxed);
            SQLITE_OK
        }
        Ok(Err((code, msg))) => {
            *pp_db = ptr::null_mut();
            set_open_error(code, msg);
            code
        }
        Err(_) => {
            *pp_db = ptr::null_mut();
            set_open_error(SQLITE_CANTOPEN, "panic while opening database".to_string());
            SQLITE_CANTOPEN
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_close(db: *mut Sqlite3) -> c_int {
    close_common(db, false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_close_v2(db: *mut Sqlite3) -> c_int {
    close_common(db, true)
}

/// Shared close. An open incremental-blob handle holds a back-pointer to the
/// connection, so the connection cannot be freed under it — which is exactly
/// the situation sqlite's two closes answer differently (both probed on
/// 3.45.1):
///
/// * `sqlite3_close` → `SQLITE_BUSY`, connection untouched.
/// * `sqlite3_close_v2` → `SQLITE_OK`, and the connection becomes a **zombie**:
///   already logically closed, but kept alive so the outstanding blob handle
///   stays usable; the real free happens when the last handle closes
///   (`blob::reap_zombie`). This is what GC'd consumers rely on.
unsafe fn close_common(db: *mut Sqlite3, v2: bool) -> c_int {
    if db.is_null() {
        return SQLITE_OK;
    }
    // An outstanding BACKUP holds a raw back-pointer to this connection and
    // will write through it, so — unlike a blob handle — there is no zombie
    // form that would keep it valid. sqlite reports the same BUSY here.
    if !(*db).backups.is_empty() {
        (*db).set_error(
            SQLITE_BUSY,
            SQLITE_BUSY,
            "unable to close due to unfinalized statements or unfinished backups",
        );
        return SQLITE_BUSY;
    }
    if !(*db).blobs.is_empty() {
        if !v2 {
            (*db).set_error(
                SQLITE_BUSY,
                SQLITE_BUSY,
                "unable to close due to unfinalized statements or unfinished backups",
            );
            return SQLITE_BUSY;
        }
        // Zombie: drop the write transaction now (the close is logically
        // done), then wait for the last blob handle. Blob I/O on a zombie
        // still reads/writes through the engine, as sqlite's does.
        (*db).txn = None;
        (*db).zombie = true;
        return SQLITE_OK;
    }
    free_connection(db);
    SQLITE_OK
}

/// Free the connection for real. Only ever called with no blob handles left.
pub(crate) unsafe fn free_connection(db: *mut Sqlite3) {
    let mut boxed = Box::from_raw(db);
    // Drop any open transaction before the engine (borrow discipline).
    boxed.txn = None;
    // Run each registered UDF's `xDestroy(pApp)` — sqlite's contract on close,
    // and what keeps CPython from leaking the wrapped Python callables.
    for h in std::mem::take(&mut boxed.host_fns) {
        h.destroy();
    }
    for h in std::mem::take(&mut boxed.host_colls) {
        h.destroy();
    }
    let path = boxed.path.clone();
    let backing = boxed.backing;
    // The engine handle must be gone before the file is: mpedb unmaps on drop.
    drop(boxed);
    let remove = match backing {
        Backing::Ephemeral => true,
        Backing::NamedMemory => named_memory_release(&path),
        Backing::File => false,
    };
    if remove {
        let _ = std::fs::remove_file(&path);
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_busy_timeout(db: *mut Sqlite3, ms: c_int) -> c_int {
    match conn(db) {
        Some(c) => {
            c.busy_timeout_ms = ms;
            // The same knob bounds the ENGINE's writer-lock wait (#109):
            // cross-process contention returns Busy → SQLITE_BUSY at this
            // deadline instead of blocking forever. `ms <= 0` = sqlite's
            // handler-cleared state: one immediate attempt, immediate BUSY.
            c.db.set_busy_timeout(Some(Duration::from_millis(ms.max(0) as u64)));
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
            | sql::Kind::Maintenance
    ) || (matches!(kind, sql::Kind::Read) && introspect::references_sqlite_master(first))
        // `EXPLAIN QUERY PLAN <stmt>` is rewritten to mpedb's `EXPLAIN <stmt>`
        // on the execution path (so `sqlite3_sql()` keeps the consumer's text);
        // `prepare_detached` would reject the un-rewritten form here.
        || sql::explain_query_plan_body(first).is_some();

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

    // #51: with databases ATTACHed, statement names resolve against the
    // connection's attach list on the execution path (`Database::query`);
    // `prepare_detached` refuses cross-file statements by design. Defer
    // validation to step, exactly like the open-transaction case above.
    let skip_validation = skip_validation || c.db.has_attached_databases();

    // `zeroblob(<const>)` → the equivalent blob literal FIRST, so both the
    // prepare-time validation below and the stored `exec_sql` see a literal
    // where mpedb's binder would otherwise refuse the function call (blob.rs).
    // This never touches parameter tokens, so it composes with `scan_params`.
    let zb = sql::rewrite_zeroblob(first);
    let first_zb: &str = &zb;

    // Rewrite named/positional parameters to mpedb's numbered `$K` form so the
    // engine — which only speaks `?`/`$N` — sees `:name`/`@name`/`$name`/`?` as
    // the numbered placeholders sqlite assigned them. Everything downstream
    // (validation, execution) uses this rewritten text; the maps answer the
    // `bind_parameter_*` family.
    let scan = sql::scan_params(first_zb);

    // SQLITE_LIMIT_VARIABLE_NUMBER: sqlite refuses at parse when a statement
    // uses more parameters than the connection's limit allows, with exactly
    // this message (CPython's suite regex-matches it).
    if scan.count > c.limits[SQLITE_LIMIT_VARIABLE_NUMBER as usize].max(0) as usize {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "too many SQL variables");
        return SQLITE_ERROR;
    }

    // The authorizer sees every action this statement performs, BEFORE it is
    // accepted — sqlite consults it during prepare, and a DENY fails the
    // prepare with SQLITE_AUTH. A no-op (and no extra compile) when none is
    // registered. It runs on the parameter-rewritten text, which is what the
    // engine compiles. See `auth.rs`.
    if let Err((code, msg)) = auth::authorize(c, &scan.rewritten) {
        c.set_error(code, code, &msg);
        return code;
    }

    // Validate compilable statements now (surface syntax/bind errors at
    // prepare, as sqlite does), WITHOUT executing or publishing a plan. Validate
    // the REWRITTEN text — mpedb's parser rejects a bare `:` — not the original.
    if !skip_validation {
        // The engine's parser does not skip leading comments (exec_one strips
        // them at execution) — validate the same stripped text it will run.
        let to_validate = sql::strip_leading_trivia(&scan.rewritten);
        // `INSERT OR ROLLBACK` is executed as `OR ABORT` plus a connection
        // rollback (see `exec_one`), so validate the text the engine will
        // actually be handed — the engine's parser refuses ROLLBACK by name.
        let (to_validate, _) = sql::rewrite_insert_or_rollback(to_validate);
        match catch_unwind(AssertUnwindSafe(|| c.db.prepare_detached(&to_validate))) {
            Ok(Ok(_plan)) => {}
            Ok(Err(e)) => return c.set_db_error(&e),
            Err(_) => {
                c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) preparing");
                return SQLITE_ERROR;
            }
        }
    }

    let n_params = scan.count;
    let boxed = Box::new(Stmt {
        db,
        sql: first.to_string(),
        exec_sql: scan.rewritten,
        n_params,
        param_names: scan.names,
        binds: vec![Value::Null; n_params],
        executed: false,
        columns: Vec::new(),
        col_name_c: Vec::new(),
        decltype_c: None,
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
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    if !(*p).executed {
        // Both callbacks run with NO Rust borrow of the statement or the
        // connection live: a trace callback (CPython's, for one) re-enters the
        // API with this very Stmt* (`sqlite3_expanded_sql`, `sqlite3_db_handle`).
        trace_stmt_begin(p);
        if progress_says_interrupt(p) {
            if let Some(c) = conn((*p).db) {
                c.set_error(SQLITE_INTERRUPT, SQLITE_INTERRUPT, "interrupted");
            }
            return SQLITE_INTERRUPT;
        }
        let code = run_stmt(&mut *p);
        if code != SQLITE_OK {
            return code;
        }
    }
    let s = &mut *p;
    if s.pos < s.rows.len() {
        load_current_row(s);
        SQLITE_ROW
    } else {
        s.have_row = false;
        s.cells.clear();
        SQLITE_DONE
    }
}

/// Fire a registered `SQLITE_TRACE_STMT` callback: `p` is about to (re)run.
/// The P argument is the statement handle (the callback may expand it), the X
/// argument the unexpanded statement text, NUL-terminated, valid for the call.
unsafe fn trace_stmt_begin(p: *mut Stmt) {
    let db = (*p).db;
    if db.is_null() {
        return;
    }
    let (mask, cb, ctx) = ((*db).trace_mask, (*db).trace_cb, (*db).trace_ctx);
    if cb.is_null() || mask & SQLITE_TRACE_STMT == 0 {
        return;
    }
    let mut sql_c = (*p).sql.as_bytes().to_vec();
    sql_c.retain(|b| *b != 0);
    sql_c.push(0);
    let f: unsafe extern "C" fn(u32, *mut c_void, *mut c_void, *mut c_void) -> c_int =
        std::mem::transmute(cb);
    // The return value of an SQLITE_TRACE_STMT callback is ignored, as sqlite.
    let _ = f(SQLITE_TRACE_STMT, ctx, p as *mut c_void, sql_c.as_ptr() as *mut c_void);
}

/// Fire a registered progress handler once for this statement execution.
/// Non-zero (CPython returns -1 when the Python handler raised) interrupts.
unsafe fn progress_says_interrupt(p: *mut Stmt) -> bool {
    let db = (*p).db;
    if db.is_null() {
        return false;
    }
    let (cb, ctx) = ((*db).progress_cb, (*db).progress_ctx);
    if cb.is_null() {
        return false;
    }
    let f: unsafe extern "C" fn(*mut c_void) -> c_int = std::mem::transmute(cb);
    f(ctx) != 0
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_reset(p: *mut Stmt) -> c_int {
    match stmt(p) {
        Some(s) => {
            s.executed = false;
            s.rows.clear();
            s.columns.clear();
            s.col_name_c.clear();
            s.decltype_c = None;
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
            // Same read-only refusal as the step path (`run_stmt`).
            if c.readonly && matches!(sql::classify(first), sql::Kind::Dml { .. } | sql::Kind::Ddl)
            {
                c.set_error(SQLITE_READONLY, SQLITE_READONLY, "attempt to write a readonly database");
                set_exec_errmsg(c, errmsg);
                return SQLITE_READONLY;
            }
            // sqlite's exec is prepare/step inside, so SQLITE_TRACE_STMT fires
            // for exec'd statements too — CPython's legacy-autocommit COMMIT
            // goes through exec and its suite asserts it is traced. A
            // throwaway Stmt backs the callback's P argument (it may call
            // sqlite3_expanded_sql / sqlite3_db_handle on it).
            if !c.trace_cb.is_null() && c.trace_mask & SQLITE_TRACE_STMT != 0 {
                let tmp = Box::new(Stmt {
                    db,
                    sql: first.to_string(),
                    exec_sql: first.to_string(),
                    n_params: 0,
                    param_names: Vec::new(),
                    binds: Vec::new(),
                    executed: false,
                    columns: Vec::new(),
                    col_name_c: Vec::new(),
                    decltype_c: None,
                    rows: Vec::new(),
                    pos: 0,
                    have_row: false,
                    cells: Vec::new(),
                });
                let p = Box::into_raw(tmp);
                trace_stmt_begin(p);
                drop(Box::from_raw(p));
            }
            // sqlite's exec PREPARES each statement, so the authorizer gates
            // exec'd statements exactly as it gates the step path.
            if let Err((code, msg)) = auth::authorize(c, first) {
                c.set_error(code, code, &msg);
                set_exec_errmsg(c, errmsg);
                return code;
            }
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
    // sqlite has no NaN: binding one stores NULL (CPython relies on this).
    if v.is_nan() {
        return bind(p, idx, Value::Null);
    }
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
    let Some(s) = stmt(p) else { return 0 };
    let Some(nm) = c_bytes(name, -1) else { return 0 };
    // Look up the parameter whose spelling (sigil included) matches `name`,
    // returning its 1-based number — exactly as sqlite does. The stored spelling
    // carries a trailing NUL; compare against the bytes before it. Absent → 0.
    for (k, entry) in s.param_names.iter().enumerate() {
        if let Some(spelling) = entry {
            if spelling.split_last().map(|(_, b)| b) == Some(nm) {
                return (k + 1) as c_int;
            }
        }
    }
    0
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
pub unsafe extern "C" fn sqlite3_column_decltype(p: *mut Stmt, col: c_int) -> *const c_char {
    let Some(s) = stmt(p) else { return ptr::null() };
    if col < 0 {
        return ptr::null();
    }
    // Compute once, on first ask: derive each output column's declared type from
    // the plan's projection (a bare base-table column reports its type; anything
    // computed reports NULL) — the plan-derived source mapping, not a heuristic.
    if s.decltype_c.is_none() {
        let decl = conn(s.db)
            .and_then(|c| c.db.output_decltypes(&s.exec_sql).ok())
            .unwrap_or_default();
        s.decltype_c = Some(
            decl.into_iter()
                .map(|o| {
                    o.map(|name| {
                        let mut v = name.into_bytes();
                        v.push(0); // NUL-terminate
                        v
                    })
                })
                .collect(),
        );
    }
    match s.decltype_c.as_ref().unwrap().get(col as usize) {
        Some(Some(bytes)) => bytes.as_ptr() as *const c_char,
        _ => ptr::null(),
    }
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
        // No handle: the caller is almost always asking why an open failed (it
        // got NULL back). Answer with THAT, not sqlite's constant lie.
        None => match last_open_error() {
            Some((_, msg)) => msg,
            None => cstr!("out of memory"),
        },
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errcode(db: *mut Sqlite3) -> c_int {
    match conn(db) {
        Some(c) => c.err_code,
        None => last_open_error().map(|(c, _)| c).unwrap_or(SQLITE_MISUSE),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_errcode(db: *mut Sqlite3) -> c_int {
    match conn(db) {
        Some(c) => c.err_ext,
        None => last_open_error().map(|(c, _)| c).unwrap_or(SQLITE_MISUSE),
    }
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
pub unsafe extern "C" fn sqlite3_interrupt(db: *mut Sqlite3) {
    if !db.is_null() {
        // Touch ONLY the atomic flag — never the rest of the connection — so
        // this is safe to call from another thread while a statement runs. The
        // running statement polls it at step entry and during the busy-retry
        // wait (mpedb materializes a result synchronously, so those are the
        // points at which an interrupt can take effect; a runaway scan is
        // bounded instead by the per-statement runtime budget).
        (*db).interrupted.store(true, Ordering::SeqCst);
    }
}

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

/// One bound value as a SQL literal for `sqlite3_expanded_sql`.
fn value_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        Value::Float(f) if f.is_finite() => {
            let s = format!("{f}");
            // Keep it recognizably a float (sqlite renders `5.0`, not `5`).
            if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") }
        }
        Value::Float(_) => "NULL".to_string(), // NaN/inf: no SQL literal
        Value::Timestamp(us) => us.to_string(), // stored as int microseconds
        // A session-context list is not a value a C-API caller can bind, so it
        // never reaches here; render defensively rather than match-panic.
        Value::List(_) => "NULL".to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => {
            let mut o = String::with_capacity(3 + b.len() * 2);
            o.push_str("X'");
            for byte in b {
                o.push_str(&format!("{byte:02X}"));
            }
            o.push('\'');
            o
        }
    }
}

/// Expand the numbered-`$K` statement by substituting each parameter with its
/// bound value as a SQL literal — quote/comment aware, so a `$K` inside a string
/// literal or a comment is left untouched. (The shim rewrites `?`/`:name`/… to
/// `$K` at prepare, so this covers every sqlite parameter spelling.)
fn expand_sql(exec_sql: &str, binds: &[Value]) -> String {
    let mut out = String::with_capacity(exec_sql.len() + 16);
    let mut chars = exec_sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                out.push('\'');
                while let Some(d) = chars.next() {
                    out.push(d);
                    if d == '\'' {
                        if matches!(chars.peek(), Some('\'')) {
                            out.push('\''); // doubled '' — stays in the string
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if matches!(chars.peek(), Some('-')) => {
                out.push('-');
                for d in chars.by_ref() {
                    out.push(d);
                    if d == '\n' {
                        break;
                    }
                }
            }
            '/' if matches!(chars.peek(), Some('*')) => {
                out.push('/');
                out.push('*');
                chars.next();
                let mut prev = ' ';
                for d in chars.by_ref() {
                    out.push(d);
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
            }
            '$' => {
                let mut num = String::new();
                while let Some(d) = chars.peek() {
                    if d.is_ascii_digit() {
                        num.push(*d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if num.is_empty() {
                    out.push('$');
                } else {
                    let lit = num
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|k| binds.get(k))
                        .map(value_literal)
                        .unwrap_or_else(|| "NULL".to_string());
                    out.push_str(&lit);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// `sqlite3_expanded_sql`: the statement with its bound parameters substituted
/// as literals (sqlite semantics). Returned in a libc-allocated buffer the
/// caller frees with `sqlite3_free`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_expanded_sql(p: *mut Stmt) -> *mut c_char {
    match stmt(p) {
        Some(s) => {
            let out = expand_sql(&s.exec_sql, &s.binds);
            // sqlite subjects the expanded string to SQLITE_LIMIT_LENGTH and
            // answers NULL past it (CPython's trace path then falls back to
            // the unexpanded text).
            if let Some(c) = conn(s.db) {
                if out.len() > c.limits[SQLITE_LIMIT_LENGTH as usize] as usize {
                    return ptr::null_mut();
                }
            }
            dup_cstr(&out)
        }
        None => ptr::null_mut(),
    }
}

// ---- connection configuration / callbacks (safe no-op stubs) --------------

/// Per-connection run-time limits: REAL get/set over `Sqlite3::limits`, seeded
/// with sqlite's defaults. Enforced where the shim itself does the work
/// (`VARIABLE_NUMBER` at prepare, `LENGTH` in `expanded_sql`); `SQL_LENGTH` is
/// enforced by CPython, which reads the stored value through this call.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_limit(db: *mut Sqlite3, id: c_int, new_val: c_int) -> c_int {
    let Some(c) = conn(db) else {
        return -1;
    };
    if !(0..SQLITE_N_LIMIT as c_int).contains(&id) {
        return -1; // sqlite: out-of-range category answers a negative value
    }
    let idx = id as usize;
    let prior = c.limits[idx];
    if new_val >= 0 {
        // The compile-time default doubles as the hard upper bound; a larger
        // request is silently truncated, exactly as sqlite.
        c.limits[idx] = new_val.min(DEFAULT_LIMITS[idx]);
    }
    prior
}

/// Fixed-arg shim over the variadic `sqlite3_db_config`. On the SysV/x86-64 ABI
/// the register layout matches the common `(sqlite3*, int op, int, int*)` forms
/// consumers use; we honor no toggles, so it is a success no-op. (Consumers do
/// not call this on the connect/CRUD paths.)
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_config(
    _db: *mut Sqlite3,
    op: c_int,
    _a: c_int,
    b: *mut c_void,
) -> c_int {
    // The `(int onoff, int *pCurrent)` toggle ops — 1002 (ENABLE_FKEY) through
    // the 1019 range; NOT 1000/1001, whose varargs are pointers with different
    // shapes: mpedb honors none of them, so the CURRENT state written back is
    // always 0 — the literal truth (FK enforcement, triggers, … are not
    // active), never a lie a consumer can build on. Leaving the out pointer
    // unwritten made CPython's getconfig read an indeterminate int.
    if (1002..=1019).contains(&op) && !b.is_null() {
        *(b as *mut c_int) = 0;
    }
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
    db: *mut Sqlite3,
    mask: c_uint,
    cb: *mut c_void,
    ctx: *mut c_void,
) -> c_int {
    let Some(c) = conn(db) else {
        return SQLITE_MISUSE;
    };
    if cb.is_null() || mask == 0 {
        c.trace_mask = 0;
        c.trace_cb = ptr::null_mut();
        c.trace_ctx = ptr::null_mut();
    } else {
        c.trace_mask = mask;
        c.trace_cb = cb;
        c.trace_ctx = ctx;
    }
    SQLITE_OK
}

/// Register a progress handler. The shim fires it once per statement execution
/// (it has no VM opcode stream to count `n` against — see the field's doc);
/// `n <= 0` or a NULL callback clears, as sqlite.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_progress_handler(
    db: *mut Sqlite3,
    n: c_int,
    cb: *mut c_void,
    ctx: *mut c_void,
) {
    if let Some(c) = conn(db) {
        if cb.is_null() || n <= 0 {
            c.progress_cb = ptr::null_mut();
            c.progress_ctx = ptr::null_mut();
        } else {
            c.progress_cb = cb;
            c.progress_ctx = ctx;
        }
    }
}

/// Register (or, with a NULL callback, clear) the compile-time access gate.
/// Every prepared statement's actions are then shown to `cb` before it is
/// accepted — see `auth.rs` for the action set and the refusal rules.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_authorizer(
    db: *mut Sqlite3,
    cb: *mut c_void,
    ctx: *mut c_void,
) -> c_int {
    match conn(db) {
        Some(c) => {
            c.auth_cb = cb;
            c.auth_ctx = if cb.is_null() { ptr::null_mut() } else { ctx };
            SQLITE_OK
        }
        None => SQLITE_MISUSE,
    }
}

// ---- user-defined functions (scalar + aggregate) / collations (refused) ----

/// Invoke a caller-supplied destructor (`void(*)(void*)`) for `app` if present —
/// sqlite's contract on a failed `create_*` registration, so the caller does
/// not leak the wrapped state (e.g. CPython's Python callable).
unsafe fn call_destroy(destroy: *mut c_void, app: *mut c_void) {
    if !destroy.is_null() {
        let f: unsafe extern "C" fn(*mut c_void) = std::mem::transmute(destroy);
        f(app);
    }
}

/// The one implementation behind `sqlite3_create_function` and
/// `sqlite3_create_function_v2` (design/DESIGN-UDF.md §1 + stage 2).
///
/// `xFunc` set registers a SCALAR; `xStep` + `xFinal` register an AGGREGATE (a
/// half-supplied pair is a misuse and refuses). All three NULL DELETES the
/// `(name, nArg)` registration in both namespaces; a repeat registration
/// REPLACES it, running the previous entry's `xDestroy`. Every refusal path runs
/// the caller's `xDestroy(pApp)` so wrapped state (e.g. a CPython callable) is
/// not leaked.
#[allow(clippy::too_many_arguments)]
unsafe fn create_function_impl(
    db: *mut Sqlite3,
    name: *const c_char,
    n_arg: c_int,
    app: *mut c_void,
    x_func: *mut c_void,
    x_step: *mut c_void,
    x_final: *mut c_void,
    x_destroy: *mut c_void,
) -> c_int {
    let Some(c) = conn(db) else {
        call_destroy(x_destroy, app);
        return SQLITE_MISUSE;
    };
    c.clear_error();
    let Some(raw_name) = c_str_opt(name) else {
        call_destroy(x_destroy, app);
        c.set_error(
            SQLITE_MISUSE,
            SQLITE_MISUSE,
            "create_function: NULL or non-UTF-8 function name",
        );
        return SQLITE_MISUSE;
    };
    // sqlite refuses an argument count outside -1..=127 (SQLITE_MAX_FUNCTION_ARG)
    // with SQLITE_MISUSE; the destructor still runs (create_function_v2's
    // failure contract). CPython turns any non-OK into OperationalError.
    if !(-1..=127).contains(&n_arg) {
        call_destroy(x_destroy, app);
        c.set_error(
            SQLITE_MISUSE,
            SQLITE_MISUSE,
            "create_function: nArg must be between -1 and 127",
        );
        return SQLITE_MISUSE;
    }
    // sqlite function names are case-insensitive, and mpedb's parser lowercases
    // them before the binder resolves — register under the same spelling.
    let fname = raw_name.to_ascii_lowercase();
    let is_agg = !x_step.is_null() || !x_final.is_null();
    if is_agg && (x_step.is_null() || x_final.is_null()) {
        // sqlite requires the pair: half of one is a misuse, not an aggregate.
        call_destroy(x_destroy, app);
        c.set_error(
            SQLITE_MISUSE,
            SQLITE_MISUSE,
            "create_function: an aggregate needs BOTH xStep and xFinal",
        );
        return SQLITE_MISUSE;
    }
    if is_agg && !x_func.is_null() {
        call_destroy(x_destroy, app);
        c.set_error(
            SQLITE_MISUSE,
            SQLITE_MISUSE,
            "create_function: a function is either scalar (xFunc) or aggregate \
             (xStep/xFinal), not both",
        );
        return SQLITE_MISUSE;
    }
    // A repeat registration replaces: run the previous entry's destructor first.
    // The stored entry knows which registry it went into, so a name re-registered
    // from scalar to aggregate (or back) leaves nothing stale behind.
    if let Some(i) = c
        .host_fns
        .iter()
        .position(|h| h.name == fname && h.n_arg == n_arg)
    {
        let old = c.host_fns.remove(i);
        if old.aggregate {
            c.db.unregister_host_aggregate(&fname, n_arg);
        } else {
            c.db.unregister_host_function(&fname, n_arg);
        }
        old.destroy();
    }
    if x_func.is_null() && !is_agg {
        // sqlite: all-NULL callbacks delete the function. The `(name, nArg)` may
        // have been either kind, and the replace above already dropped whichever
        // this connection tracked — clear both registries to be certain.
        c.db.unregister_host_function(&fname, n_arg);
        c.db.unregister_host_aggregate(&fname, n_arg);
        call_destroy(x_destroy, app);
        return SQLITE_OK;
    }
    if is_agg {
        let step: udf::XStep = std::mem::transmute(x_step);
        let fin: udf::XFinal = std::mem::transmute(x_final);
        c.db.register_host_aggregate(&fname, n_arg, udf::make_agg_factory(step, fin, app));
    } else {
        let f: udf::XFunc = std::mem::transmute(x_func);
        c.db
            .register_host_function(&fname, n_arg, udf::make_scalar_closure(f, app));
    }
    c.host_fns.push(udf::HostFn {
        name: fname,
        n_arg,
        aggregate: is_agg,
        x_destroy,
        p_app: app,
        x_func,
        x_step,
        x_final,
    });
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function(
    db: *mut Sqlite3,
    name: *const c_char,
    n_arg: c_int,
    _enc: c_int,
    app: *mut c_void,
    x_func: *mut c_void,
    x_step: *mut c_void,
    x_final: *mut c_void,
) -> c_int {
    create_function_impl(db, name, n_arg, app, x_func, x_step, x_final, ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function_v2(
    db: *mut Sqlite3,
    name: *const c_char,
    n_arg: c_int,
    _enc: c_int,
    app: *mut c_void,
    x_func: *mut c_void,
    x_step: *mut c_void,
    x_final: *mut c_void,
    x_destroy: *mut c_void,
) -> c_int {
    create_function_impl(db, name, n_arg, app, x_func, x_step, x_final, x_destroy)
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

/// `sqlite3_create_collation_v2(db, name, enc, pArg, xCompare, xDestroy)`
/// (design/DESIGN-UDF.md stage 3).
///
/// **Honest scope.** A registered collation is a COMPARATOR, and mpedb uses it
/// where a comparator is all that is needed: `ORDER BY <expr> COLLATE <name>`.
/// It cannot re-order an INDEX — an mpedb index (and every PRIMARY KEY) is a
/// B+tree in memcmp order under a BUILT-IN collation, and no callback can
/// produce sort bytes — so a host collation on a column's declared `COLLATE`,
/// or as the fold of a `GROUP BY`/`DISTINCT` key, is REFUSED by name
/// ("no such collation sequence") rather than answered under BINARY. The engine
/// enforces that structurally: those paths take a built-in `Collation`, which no
/// registration can construct.
///
/// `xCompare == NULL` DELETES the entry (CPython's `create_collation(name,
/// None)`); a statement that already named it then fails with sqlite's
/// "no such collation sequence: <name>" rather than silently sorting BINARY.
/// The encoding argument is accepted and ignored: mpedb text is UTF-8, which is
/// what `SQLITE_UTF8` asks for, and CPython only ever passes that.
///
/// On FAILURE `xDestroy` is NOT called — unlike `create_function_v2`, sqlite
/// documents the collation destructor as not running on a failed registration,
/// and CPython frees `pArg` itself on a non-OK return. Calling it here was a
/// double-free that corrupted the interpreter's heap.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation_v2(
    db: *mut Sqlite3,
    name: *const c_char,
    _enc: c_int,
    arg: *mut c_void,
    x_compare: *mut c_void,
    x_destroy: *mut c_void,
) -> c_int {
    let Some(c) = conn(db) else { return SQLITE_MISUSE };
    c.clear_error();
    let Some(raw_name) = c_str_opt(name) else {
        c.set_error(
            SQLITE_MISUSE,
            SQLITE_MISUSE,
            "create_collation: NULL or non-UTF-8 collation name",
        );
        return SQLITE_MISUSE;
    };
    let cname = raw_name.to_string();
    // A repeat registration REPLACES (sqlite's rule, and CPython's
    // `test_collation_register_twice` asserts the LAST one wins): run the
    // previous entry's destructor once the new one is in.
    let previous = c
        .host_colls
        .iter()
        .position(|h| h.name == cname)
        .map(|i| c.host_colls.remove(i));
    if x_compare.is_null() {
        c.db.unregister_host_collation(&cname);
        if let Some(p) = previous {
            p.destroy();
        }
        call_destroy(x_destroy, arg);
        return SQLITE_OK;
    }
    let cmp: udf::XCompare = std::mem::transmute(x_compare);
    c.db
        .register_host_collation(&cname, udf::make_collation_closure(cmp, arg));
    c.host_colls.push(udf::HostColl {
        name: cname,
        x_destroy,
        p_app: arg,
        x_compare,
    });
    if let Some(p) = previous {
        p.destroy();
    }
    SQLITE_OK
}

/// `sqlite3_create_collation` — the same, without a destructor.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation(
    db: *mut Sqlite3,
    name: *const c_char,
    enc: c_int,
    arg: *mut c_void,
    x_compare: *mut c_void,
) -> c_int {
    sqlite3_create_collation_v2(db, name, enc, arg, x_compare, ptr::null_mut())
}

// ---- UDF-callback accessors (design/DESIGN-UDF.md §1) ----------------------
//
// These operate on the shim's own `sqlite3_context` / `sqlite3_value` (see
// `udf.rs`), which the C callback holds as opaque pointers. Outside a UDF call
// the pointers are NULL/foreign, so every accessor is NULL-guarded and falls
// back to sqlite's "no value" answer rather than dereferencing.

/// The shim `sqlite3_context*` a UDF callback was handed.
unsafe fn udf_ctx<'a>(p: *mut c_void) -> Option<&'a mut udf::SqliteContext> {
    if p.is_null() {
        None
    } else {
        Some(&mut *(p as *mut udf::SqliteContext))
    }
}

/// One `sqlite3_value*` from a UDF callback's `argv`.
unsafe fn udf_val<'a>(p: *mut c_void) -> Option<&'a udf::SqliteValue> {
    if p.is_null() {
        None
    } else {
        Some(&*(p as *const udf::SqliteValue))
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_user_data(ctx: *mut c_void) -> *mut c_void {
    udf_ctx(ctx).map(|c| c.p_app()).unwrap_or(ptr::null_mut())
}

/// `sqlite3_aggregate_context(ctx, nBytes)` (design/DESIGN-UDF.md stage 2).
///
/// First call of an aggregation with `nBytes > 0` allocates that many ZEROED
/// bytes and returns them; every later call in the SAME aggregation — including
/// `xFinal`'s — returns the SAME pointer. `nBytes <= 0` never allocates: it
/// returns the existing buffer, or NULL when the group was never stepped, which
/// is exactly how a well-behaved `xFinal` recognizes an empty group and yields
/// NULL. Outside an aggregate callback (a scalar's context, a NULL pointer) it
/// returns NULL, as sqlite does for the same misuse.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_aggregate_context(ctx: *mut c_void, n: c_int) -> *mut c_void {
    match udf_ctx(ctx) {
        Some(c) => c.aggregate_context(n),
        None => ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_context_db_handle(_ctx: *mut c_void) -> *mut Sqlite3 {
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_null(ctx: *mut c_void) {
    if let Some(c) = udf_ctx(ctx) {
        c.set_result(Value::Null);
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_int(ctx: *mut c_void, v: c_int) {
    if let Some(c) = udf_ctx(ctx) {
        c.set_result(Value::Int(v as i64));
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_int64(ctx: *mut c_void, v: c_longlong) {
    if let Some(c) = udf_ctx(ctx) {
        c.set_result(Value::Int(v));
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_double(ctx: *mut c_void, v: c_double) {
    if let Some(c) = udf_ctx(ctx) {
        // sqlite has no NaN: a NaN result is NULL (CPython's test suite pins it).
        if v.is_nan() {
            c.set_result(Value::Null);
        } else {
            c.set_result(Value::Float(v));
        }
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_text(
    ctx: *mut c_void,
    t: *const c_char,
    n: c_int,
    d: *mut c_void,
) {
    // Copy in immediately, then honor the caller's destructor exactly as the
    // bind_* path does — we never alias the caller's buffer.
    let bytes = udf::copy_result_bytes(t, n);
    maybe_free(d, t as *mut c_void);
    if let Some(c) = udf_ctx(ctx) {
        c.set_result(Value::Text(String::from_utf8_lossy(&bytes).into_owned()));
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_blob(
    ctx: *mut c_void,
    b: *const c_void,
    n: c_int,
    d: *mut c_void,
) {
    let bytes = if b.is_null() || n < 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(b as *const u8, n as usize).to_vec()
    };
    maybe_free(d, b as *mut c_void);
    if let Some(c) = udf_ctx(ctx) {
        c.set_result(Value::Blob(bytes));
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error(ctx: *mut c_void, t: *const c_char, n: c_int) {
    let msg = c_bytes(t, n)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_else(|| "user function error".to_string());
    if let Some(c) = udf_ctx(ctx) {
        c.set_error(SQLITE_ERROR, msg);
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_code(ctx: *mut c_void, code: c_int) {
    if let Some(c) = udf_ctx(ctx) {
        c.set_error(code, format!("user function error (code {code})"));
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_nomem(ctx: *mut c_void) {
    if let Some(c) = udf_ctx(ctx) {
        c.set_error(SQLITE_NOMEM, "out of memory".to_string());
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_toobig(ctx: *mut c_void) {
    if let Some(c) = udf_ctx(ctx) {
        c.set_error(SQLITE_TOOBIG, "string or blob too big".to_string());
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_type(v: *mut c_void) -> c_int {
    udf_val(v)
        .map(|x| valconv::sqlite_type(x.value()))
        .unwrap_or(SQLITE_NULL)
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int(v: *mut c_void) -> c_int {
    udf_val(v)
        .map(|x| valconv::as_i64(x.value()) as c_int)
        .unwrap_or(0)
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int64(v: *mut c_void) -> c_longlong {
    udf_val(v).map(|x| valconv::as_i64(x.value())).unwrap_or(0)
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_double(v: *mut c_void) -> c_double {
    udf_val(v).map(|x| valconv::as_f64(x.value())).unwrap_or(0.0)
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_bytes(v: *mut c_void) -> c_int {
    udf_val(v).map(|x| x.bytes_len()).unwrap_or(0)
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_text(v: *mut c_void) -> *const c_uchar {
    match udf_val(v) {
        Some(x) if !matches!(x.value(), Value::Null) => x.text_ptr(),
        _ => ptr::null(),
    }
}
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_blob(v: *mut c_void) -> *const c_void {
    match udf_val(v) {
        Some(x) if !matches!(x.value(), Value::Null) => x.blob_ptr(),
        _ => ptr::null(),
    }
}

// ---- online backup: REAL — see `backup.rs` (sqlite3_backup_init/step/
// finish/remaining/pagecount) ---------------------------------------------

// ---- incremental blob: REAL — see `blob.rs` (sqlite3_blob_open/read/write/
// bytes/reopen/close + zeroblob/bind_zeroblob) ------------------------------

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
    db: *mut Sqlite3,
    _schema: *const c_char,
    _data: *mut c_uchar,
    _sz: c_longlong,
    _sz_buf: c_longlong,
    _flags: c_uint,
) -> c_int {
    if let Some(c) = conn(db) {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "deserialize is not supported by mpedb");
    }
    SQLITE_ERROR
}
