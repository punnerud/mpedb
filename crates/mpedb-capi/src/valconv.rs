//! mpedb `Value` <-> sqlite datatype/coercion helpers, plus `Error` -> result
//! code mapping. sqlite's `sqlite3_column_*` accessors coerce across types
//! (an integer read via `_text` yields its decimal text, etc.); these helpers
//! reproduce that behaviour closely enough for the borrowed test suites.

use crate::consts::*;
use mpedb::{Error as DbError, Value};
use std::os::raw::c_int;

/// The sqlite fundamental type reported by `sqlite3_column_type`. mpedb's
/// `Bool` and `Timestamp` have no sqlite peer and map onto INTEGER (a bool is
/// 0/1; a timestamp is microseconds since the epoch).
pub fn sqlite_type(v: &Value) -> c_int {
    match v {
        Value::Null => SQLITE_NULL,
        Value::Int(_) | Value::Bool(_) | Value::Timestamp(_) => SQLITE_INTEGER,
        Value::Float(_) => SQLITE_FLOAT,
        Value::Text(_) => SQLITE_TEXT,
        Value::Blob(_) => SQLITE_BLOB,
        // A context List is param-only and never appears in a result row; treat
        // defensively as NULL.
        Value::List(_) => SQLITE_NULL,
    }
}

/// Format an f64 the way sqlite renders one in text context: a finite value
/// with no fractional/exponent part still shows a trailing `.0`.
pub fn fmt_float(x: f64) -> String {
    if x.is_nan() {
        return "NULL".to_string(); // sqlite renders NaN as NULL text
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Inf".into() } else { "Inf".into() };
    }
    let s = format!("{x}");
    if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        format!("{s}.0")
    } else {
        s
    }
}

/// Parse the leading numeric prefix of a string as sqlite's text->int cast
/// does (leading sign + digits, stopping at the first non-digit). No prefix
/// yields 0.
fn text_to_i64(s: &str) -> i64 {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if let Some(&c) = bytes.first() {
        if c == b'+' || c == b'-' {
            neg = c == b'-';
            i = 1;
        }
    }
    let start = i;
    let mut acc: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        acc = acc.saturating_mul(10).saturating_add((bytes[i] - b'0') as i64);
        i += 1;
    }
    if i == start {
        return 0;
    }
    if neg {
        -acc
    } else {
        acc
    }
}

fn text_to_f64(s: &str) -> f64 {
    // Longest leading prefix that parses as a float (sqlite's numeric affinity
    // on a text value). Iterate CHAR boundaries — slicing at `i + c.len_utf8()`
    // never splits a multibyte char, so a value like "héllo→" coerces to 0.0
    // instead of panicking on a non-char-boundary slice.
    let t = s.trim_start();
    let mut best = 0.0;
    for (i, c) in t.char_indices() {
        if let Ok(v) = t[..i + c.len_utf8()].parse::<f64>() {
            best = v;
        }
    }
    best
}

/// Coerce to i64 for `sqlite3_column_int/int64`.
pub fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(x) => *x,
        Value::Bool(b) => *b as i64,
        Value::Timestamp(t) => *t,
        Value::Float(f) => *f as i64,
        Value::Text(s) => text_to_i64(s),
        _ => 0,
    }
}

/// Coerce to f64 for `sqlite3_column_double`.
pub fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        Value::Int(x) => *x as f64,
        Value::Bool(b) => *b as i64 as f64,
        Value::Timestamp(t) => *t as f64,
        Value::Text(s) => text_to_f64(s),
        _ => 0.0,
    }
}

/// The canonical byte payload for `sqlite3_column_text`/`_blob`/`_bytes`.
/// `None` for NULL (the accessors then return a NULL pointer / 0 length).
/// Non-blob scalars render to their text form; a blob returns its raw bytes.
pub fn as_bytes(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Blob(b) => Some(b.clone()),
        Value::Text(s) => Some(s.clone().into_bytes()),
        Value::Int(x) => Some(x.to_string().into_bytes()),
        Value::Timestamp(t) => Some(t.to_string().into_bytes()),
        Value::Bool(b) => Some((*b as i64).to_string().into_bytes()),
        Value::Float(f) => Some(fmt_float(*f).into_bytes()),
        Value::List(_) => Some(Vec::new()),
    }
}

/// Map an mpedb `Error` to sqlite's `(primary, extended)` result codes.
pub fn error_codes(e: &DbError) -> (c_int, c_int) {
    match e {
        DbError::PrimaryKeyViolation { .. } => (SQLITE_CONSTRAINT, SQLITE_CONSTRAINT_PRIMARYKEY),
        DbError::UniqueViolation { .. } => (SQLITE_CONSTRAINT, SQLITE_CONSTRAINT_UNIQUE),
        DbError::NotNullViolation { .. } => (SQLITE_CONSTRAINT, SQLITE_CONSTRAINT_NOTNULL),
        DbError::CheckViolation { .. } => (SQLITE_CONSTRAINT, SQLITE_CONSTRAINT_CHECK),
        DbError::PolicyViolation { .. } | DbError::WriteRejected { .. } => {
            (SQLITE_CONSTRAINT, SQLITE_CONSTRAINT)
        }
        DbError::TypeMismatch(_) => (SQLITE_MISMATCH, SQLITE_MISMATCH),
        DbError::Corrupt(_) => (SQLITE_CORRUPT, SQLITE_CORRUPT),
        DbError::Io(_) => (SQLITE_IOERR, SQLITE_IOERR),
        DbError::DbFull => (SQLITE_FULL, SQLITE_FULL),
        DbError::OutOfMemory { .. } => (SQLITE_NOMEM, SQLITE_NOMEM),
        DbError::ReadersFull | DbError::SnapshotEvicted | DbError::WriteConflict => {
            (SQLITE_BUSY, SQLITE_BUSY)
        }
        // #109: the engine's writer-lock wait ran out the connection's busy
        // timeout (`Database::set_busy_timeout`) — sqlite's SQLITE_BUSY,
        // message "database is locked" (shaped below). Terminal for the
        // shim's retry loop: the timeout has already been honored in full.
        DbError::Busy => (SQLITE_BUSY, SQLITE_BUSY),
        DbError::Frozen { .. } => (SQLITE_LOCKED, SQLITE_LOCKED),
        // ANOTHER connection on this thread holds the writer lock (the robust
        // ERRORCHECK mutex answers EDEADLK for its own thread, so the engine
        // cannot tell same-connection re-entry from a sibling connection).
        // Through the C API the sibling-connection case is the ONLY reachable
        // one — the shim's `begin_txn` already refuses nested BEGIN — and its
        // sqlite-truthful answer is BUSY ("database is locked"), which the
        // busy-timeout retry then waits out, not "internal error (bug)".
        e if is_writer_lock_reentry(e) => (SQLITE_BUSY, SQLITE_BUSY),
        DbError::Internal(_) => (SQLITE_INTERNAL, SQLITE_INTERNAL),
        // Parse/Bind/param-count/plan/arith/budget/unsupported/config/schema:
        // all "the statement is wrong" -> SQLITE_ERROR.
        _ => (SQLITE_ERROR, SQLITE_ERROR),
    }
}

/// The engine's EDEADLK answer from the writer mutex. See the mapping arm in
/// [`error_codes`]: through the C API this means "a sibling connection on this
/// thread holds the writer lock", which is sqlite's BUSY, not an engine bug.
/// (mpedb needs the writer lock even for a first-time READ of a new statement
/// text — publishing the compiled plan to the shared registry is a write.)
pub fn is_writer_lock_reentry(e: &DbError) -> bool {
    matches!(e, DbError::Internal(m) if m.contains("writer lock re-entered by its owner"))
}

/// sqlite's canonical message for errors whose TEXT consumers match on
/// (CPython's own tests regex-search these), with mpedb's detail kept after
/// the canonical phrase. `None` = use mpedb's message as-is.
pub fn sqlite_shaped_message(e: &DbError) -> Option<String> {
    let raw = e.to_string();
    match e {
        DbError::CheckViolation { .. } => Some(format!("CHECK constraint failed: {raw}")),
        DbError::PrimaryKeyViolation { .. } | DbError::UniqueViolation { .. } => {
            Some(format!("UNIQUE constraint failed: {raw}"))
        }
        DbError::NotNullViolation { .. } => Some(format!("NOT NULL constraint failed: {raw}")),
        DbError::Busy => Some("database is locked".to_string()),
        // "no such collation sequence: <name>" is sqlite's own wording and
        // consumers assert on it EXACTLY (CPython's `test_deregister_collation`
        // compares `str(exc)`), so the engine's `bind error: ` / `unsupported: `
        // prefix has to come off. The message itself is already sqlite's.
        DbError::Bind(m) | DbError::Unsupported(m)
            if m.starts_with("no such collation sequence: ") =>
        {
            Some(m.clone())
        }
        e if is_writer_lock_reentry(e) => Some("database is locked".to_string()),
        _ => None,
    }
}
