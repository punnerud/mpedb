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
mod exec;
mod ext;
mod introspect;
mod open;
mod printf;
mod sql;
mod stmt;
mod udf;
mod valconv;

pub use auth::{SQLITE_DENY, SQLITE_IGNORE};
pub use backup::*;
pub use blob::*;
pub use consts::*;
use exec::*;
pub use ext::*;
pub use open::*;
pub use stmt::*;

use mpedb::{Config, Database, Error as DbError, ExecResult, Value, WriteSession};
use std::collections::HashMap;
use std::os::raw::{c_char, c_double, c_int, c_longlong, c_uchar, c_uint, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// The seed table every fresh mpedb file is created with. mpedb accepts a
/// zero-table seed nowadays, but existing shim-created files were seeded with
/// this table and the frozen SEED hash must keep matching on re-attach, so it
/// stays. It is otherwise inert; user tables are created live via `CREATE
/// TABLE`. `pub(crate)` so `introspect` hides it from `PRAGMA`/`sqlite_master`.
pub(crate) const SEED_TABLE: &str = "_mpedb_capi_bootstrap";

static EPHEMERAL_SEQ: AtomicU64 = AtomicU64::new(0);

// ===========================================================================
// Opaque handles (returned to C as `sqlite3*` / `sqlite3_stmt*`).
// ===========================================================================

/// A connection: the mpedb engine handle plus sqlite's per-connection state
/// (open transaction, busy timeout, last error, change counters).
/// A pragma's answer: `(column names, rows)`.
type PragmaAnswer = (Vec<String>, Vec<Vec<Value>>);

/// `PRAGMA wal_checkpoint[(<mode>)]` — for a database opened FROM a sqlite
/// file, re-serialize the sidecar back over that file. `Ok(None)` = not this
/// pragma, carry on.
///
/// Here rather than in `introspect::pragma` for the same reason `fk_pragma`
/// is: it needs the connection, both to read the rows out and to know which
/// sqlite file they came from.
///
/// The answer is sqlite's `(busy, log, checkpointed)`. sqlite reports
/// `(0, -1, -1)` for a database not in WAL mode, and a native mpedb database
/// never is, so that is what it gets. A sqlite-backed one reports the row
/// count in both slots — not a WAL frame count, since nothing here is a WAL
/// frame, but the honest measure of what was written.
/// `PRAGMA mpedb_filesystem` — what kind of storage this database sits on.
///
/// mpedb's design assumes a local filesystem, and says so
/// (`design/DESIGN.md`): the robust `PROCESS_SHARED` mutex, the meta pages and
/// the reader table live INSIDE the memory-mapped file, which needs
/// `MAP_SHARED` coherence and OFD lock semantics that a network filesystem
/// does not promise.
///
/// It is a report, not a refusal, and the reason is the failure mode rather
/// than politeness: on a network filesystem mpedb usually WORKS from one host,
/// because a Linux client keeps one page cache per file. Refusing would break
/// deployments that are fine today; staying silent would let one grow into a
/// second host with no error anywhere. So it answers when asked.
///
/// Answers `(path, kind, ok)` — `ok = 1` for a local filesystem, `0` for a
/// network one, and `1` when nothing could be determined, since an unknown
/// answer must not read as an accusation.
fn filesystem_pragma(c: &Sqlite3, sqltext: &str) -> Option<PragmaAnswer> {
    let (name, _arg) = introspect::parse_pragma(sqltext);
    if !name.eq_ignore_ascii_case("mpedb_filesystem") {
        return None;
    }
    // The sqlite source when there is one: that is the file a caller thinks of
    // as "the database", and the sidecar lives beside it anyway.
    let path = c.sqlite_source.clone().unwrap_or_else(|| c.path.clone());
    let (kind, ok) = match mpedb::fs_kind(&path) {
        mpedb::FsKind::Local => ("local".to_string(), 1),
        mpedb::FsKind::Network(n) => (n.to_string(), 0),
        mpedb::FsKind::Unknown => ("unknown".to_string(), 1),
    };
    Some((
        vec!["path".to_string(), "kind".to_string(), "ok".to_string()],
        vec![vec![
            Value::Text(path.to_string_lossy().into_owned()),
            Value::Text(kind),
            Value::Int(ok),
        ]],
    ))
}

fn checkpoint_pragma(c: &mut Sqlite3, sqltext: &str) -> Result<Option<PragmaAnswer>, String> {
    let (name, _arg) = introspect::parse_pragma(sqltext);
    if !name.eq_ignore_ascii_case("wal_checkpoint") {
        return Ok(None);
    }
    let cols =
        vec!["busy".to_string(), "log".to_string(), "checkpointed".to_string()];
    if c.sqlite_source.is_none() {
        return Ok(Some((cols, vec![vec![Value::Int(0), Value::Int(-1), Value::Int(-1)]])));
    }
    let n = open::checkpoint_to_sqlite(c)? as i64;
    Ok(Some((cols, vec![vec![Value::Int(0), Value::Int(n), Value::Int(n)]])))
}

/// `PRAGMA foreign_keys [= …]` and `PRAGMA foreign_key_check [(t)]` (#194).
/// `Ok(None)` = not one of these, carry on with the ordinary pragma handler.
///
/// Split out from `introspect::pragma` because both need something a pure
/// schema function does not have: the setter needs to know whether a
/// transaction is open, and the check needs to run a read.
fn fk_pragma(
    c: &mut Sqlite3,
    sqltext: &str,
) -> Result<Option<PragmaAnswer>, DbError> {
    let (name, arg) = introspect::parse_pragma(sqltext);
    match name.to_ascii_lowercase().as_str() {
        "foreign_keys" => {
            let Some(a) = arg.as_deref() else {
                return Ok(None); // getter: the ordinary handler reports it
            };
            // sqlite is a SILENT no-op inside a transaction (measured, 3.45.1:
            // `BEGIN; PRAGMA foreign_keys=OFF; PRAGMA foreign_keys` still
            // answers 1). Erroring would be a different answer, not a stricter
            // one.
            if c.txn.is_none() {
                let on = match a.trim().to_ascii_lowercase().as_str() {
                    "1" | "on" | "yes" | "true" => true,
                    "0" | "off" | "no" | "false" => false,
                    // sqlite ignores an unparsable value rather than erroring.
                    _ => return Ok(Some((Vec::new(), Vec::new()))),
                };
                c.db.set_fk_enforced(on);
            }
            // A setter returns no rows, as sqlite does.
            Ok(Some((Vec::new(), Vec::new())))
        }
        "foreign_key_check" => {
            // Through the SESSION when one is open, like every other catalog
            // surface here. A fresh read snapshot cannot see the transaction's
            // own uncommitted rows, and a DEFERRED violation lives exactly
            // there until COMMIT settles it — so answering from `db` reported
            // a clean database to a caller looking straight at a broken one.
            let rows = match c.txn.as_mut() {
                Some(s) => s.foreign_key_check(arg.as_deref())?,
                None => c.db.foreign_key_check(arg.as_deref())?,
            };
            let rows = rows
                .into_iter()
                .map(|(table, pk, parent, fkid)| {
                    vec![
                        Value::Text(table),
                        // sqlite reports the child's ROWID here. A composite or
                        // non-integer primary key has none, and sqlite answers
                        // NULL for those too (a WITHOUT ROWID child).
                        match pk.as_slice() {
                            [Value::Int(id)] => Value::Int(*id),
                            _ => Value::Null,
                        },
                        Value::Text(parent),
                        Value::Int(fkid as i64),
                    ]
                })
                .collect();
            Ok(Some((
                introspect::pragma_cols(&["table", "rowid", "parent", "fkid"]),
                rows,
            )))
        }
        _ => Ok(None),
    }
}

pub struct Sqlite3 {
    // `txn` borrows `db` (self-referential via the 'static transmute in
    // `begin`), so it MUST be declared — and therefore dropped — before `db`.
    txn: Option<WriteSession<'static>>,
    db: Database,
    path: PathBuf,
    /// The sqlite file this handle was opened from, when it was one. `path`
    /// then points at the sidecar that actually backs the connection, and this
    /// is where `checkpoint` writes back to. `None` for a native mpedb file.
    sqlite_source: Option<PathBuf>,
    /// What `path` is: a real file the caller named, or the tmpfs file standing
    /// in for an in-memory database (which is removed again on close).
    backing: Backing,
    busy_timeout_ms: c_int,
    /// Pragmas stored and echoed per connection, never honoured. PER
    /// CONNECTION and not per database: `read_uncommitted` is an isolation
    /// setting, and a fresh connection must report the default even when a
    /// sibling has raised it.
    echo_pragmas: introspect::EchoPragmas,
    /// Set by `sqlite3_interrupt` (possibly from another thread); polled by the
    /// running statement at step entry and during the busy-retry wait. An
    /// atomic so the interrupting thread touches ONLY this field, never the
    /// rest of the connection.
    interrupted: AtomicBool,
    err_code: c_int,
    err_ext: c_int,
    /// `sqlite3_extended_result_codes()`. OFF by default, as in sqlite, and it
    /// governs `sqlite3_errcode()` only: `sqlite3_extended_errcode()` answers
    /// with the extended code either way. Both PHP's `lastErrorCode()` and
    /// PDO's `errorInfo()[1]` read the former, which is why the toggle is
    /// visible at all rather than being the no-op it was.
    extended_codes: bool,
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
    /// Tables whose `CREATE TABLE` carried `ON CONFLICT ROLLBACK` on a
    /// constraint (rewritten to ABORT for the engine — see
    /// `sql::rewrite_on_conflict_rollback`). A UNIQUE/PK constraint failure
    /// naming one of these rolls the connection's transaction back, which is
    /// sqlite's definition of the action and what CPython's
    /// `test_on_conflict_rollback` asserts.
    unique_rollback_tables: std::collections::HashSet<String>,
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
    /// Per-column source table (`sqlite3_column_table_name`), computed LAZILY
    /// on the same terms as `decltype_c`: `None` = not yet computed; inner
    /// `None` = this column has no base table (NULL). NUL-terminated bytes,
    /// aligned to `columns`.
    table_name_c: Option<Vec<Option<Vec<u8>>>>,
    /// `sql` NUL-terminated, for `sqlite3_sql`. That call hands out a pointer
    /// the statement owns for its whole life, so it cannot be a temporary.
    /// Built once on first ask.
    sql_c: Option<Vec<u8>>,
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
pub(crate) use cstr;

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

