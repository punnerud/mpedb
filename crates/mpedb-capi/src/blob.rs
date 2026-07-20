//! Incremental blob I/O — the `sqlite3_blob_*` family, backed by mpedb (compat
//! gap E2; the engine side of the story is #43's streaming APIs, which are
//! insert-only, so the HANDLE semantics live entirely in this shim).
//!
//! # Contract (derived from sqlite 3.45 by probe, not from memory)
//! - `sqlite3_blob_open(db, "main", table, column, rowid, flags, &blob)`:
//!   flags 0 = read-only, non-zero = read-write. Errors (all `SQLITE_ERROR`,
//!   message on the CONNECTION, `*pp_blob` left NULL): unknown database or
//!   table → `no such table: db.tbl`; unknown column → `no such column: "c"`;
//!   non-rowid-addressable table → `cannot open table without rowid: t`;
//!   read-write on an indexed/PK/UNIQUE column → `cannot open indexed column
//!   for writing`; missing row → `no such rowid: N`; a value that is not
//!   TEXT/BLOB → `cannot open value of type integer|real|null`. A read-write
//!   open on a read-only connection is `SQLITE_READONLY`.
//! - The size is FIXED at open: `sqlite3_blob_bytes` reports it until the
//!   handle dies, and read/write past it (or a negative n/offset) is
//!   `SQLITE_ERROR` ("SQL logic error" — sqlite sets no message text). Writes
//!   can never grow the value. `sqlite3_blob_write` on a read-only handle is
//!   `SQLITE_READONLY`.
//! - **Expiry**: modifying the handle's row through SQL (any column; UPDATE or
//!   DELETE) expires the handle — the next read/write returns `SQLITE_ABORT`
//!   ("query aborted") and the handle is dead PERMANENTLY: even
//!   `sqlite3_blob_reopen` then answers `SQLITE_ABORT` (probed; sqlite
//!   finalizes the handle's statement on the failed I/O). A failed reopen
//!   (missing row / non-blob value) reports `SQLITE_ERROR` with the open-shape
//!   message and ALSO kills the handle. Writes THROUGH a blob handle expire
//!   nothing — not the writing handle, not same-row siblings.
//!
//! # The honest MVCC mapping (mpedb has no cursor to invalidate)
//! sqlite expires eagerly (the write invalidates the handle's b-tree cursor);
//! mpedb detects LAZILY: the handle snapshots the row's visible columns at
//! open, every read/write re-reads the row through the connection's current
//! view (the open transaction if one is active, else latest committed state)
//! and compares — the row missing or ANY column changed ⇒ the tested
//! `SQLITE_ABORT` behavior. Writes go back as a full-row UPDATE **guarded on
//! the snapshot** (`WHERE rowid = ? AND every column IS its open-time value`),
//! so the read-patch-write is atomic even in autocommit: a concurrent change
//! turns the write into the expiry answer, never a lost update. Same-row
//! sibling handles on THIS connection get their snapshots refreshed after a
//! handle write (sqlite parity, probed); handles on OTHER connections see a
//! changed row and expire — that divergence (sqlite would let them read the
//! new bytes) is inherent to snapshot-compare and is accepted, as is a
//! ROLLBACK that reverts the row byte-identically leaving the handle alive
//! (which is also what sqlite itself does — probed).
//!
//! A blob write into a TEXT value whose patched bytes are not valid UTF-8 is
//! refused (`SQLITE_ERROR`): mpedb text is strictly UTF-8, and storing the
//! bytes as a blob instead would silently change `typeof()`.

use crate::{conn, valconv, Sqlite3};
use mpedb::{Error as DbError, ExecResult, Value};
use std::os::raw::{c_char, c_int, c_longlong, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use crate::consts::*;

/// How rows of the handle's table are addressed by rowid in generated SQL.
enum Addr {
    /// `INTEGER PRIMARY KEY` alias table: `WHERE "pk" = $n`.
    AliasPk(String),
    /// Implicit-rowid table (#94): `WHERE rowid = $n` (or an unshadowed
    /// sibling spelling).
    HiddenRowid(&'static str),
}

/// An open incremental-blob handle (`sqlite3_blob*` at the ABI).
pub struct Sqlite3Blob {
    /// The owning connection. Valid for the handle's whole life: `sqlite3_close`
    /// refuses (`SQLITE_BUSY`) while any handle is open, exactly as sqlite
    /// refuses to close with unfinalized statements.
    db: *mut Sqlite3,
    table: String, // canonical stored name
    col_name: String,
    col_idx: usize, // ordinal among VISIBLE columns == SELECT * position
    addr: Addr,
    readonly: bool,
    rowid: i64,
    /// Byte length, FIXED at open/reopen (sqlite's contract).
    size: usize,
    /// The row's visible columns as of open / this handle's last write —
    /// the expiry detector.
    snapshot: Vec<Value>,
    /// Permanently dead (expired or failed reopen): everything answers ABORT.
    aborted: bool,
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Route one generated statement through the connection's open transaction, or
/// autocommit. Deliberately NOT `exec_one`: blob I/O is not a statement — it
/// must not touch classification, `last_insert_rowid`, or the change counters.
fn conn_query(c: &mut Sqlite3, sql: &str, params: &[Value]) -> Result<ExecResult, DbError> {
    match c.txn.as_mut() {
        Some(s) => s.query(sql, params),
        None => c.db.query(sql, params),
    }
}

/// Value equality for the expiry compare. `PartialEq` except floats compare by
/// bits, so a NaN-holding row does not spuriously expire its handles.
fn values_identical(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| match (x, y) {
            (Value::Float(p), Value::Float(q)) => p.to_bits() == q.to_bits(),
            _ => x == y,
        })
}

/// sqlite's name for a stored value's type, for `cannot open value of type X`.
fn sqlite_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Int(_) | Value::Bool(_) | Value::Timestamp(_) => "integer",
        Value::Float(_) => "real",
        Value::Text(_) => "text",
        Value::Blob(_) => "blob",
        _ => "null",
    }
}

/// The value's payload bytes for blob I/O (text = its UTF-8 bytes).
fn payload(v: &Value) -> Option<&[u8]> {
    match v {
        Value::Blob(b) => Some(b),
        Value::Text(s) => Some(s.as_bytes()),
        _ => None,
    }
}

/// `SELECT *` the handle's row through the connection's current view.
/// `Ok(None)` = row gone.
fn fetch_row(c: &mut Sqlite3, table: &str, addr: &Addr, rowid: i64) -> Result<Option<Vec<Value>>, DbError> {
    let where_ = match addr {
        Addr::AliasPk(pk) => quote_ident(pk),
        Addr::HiddenRowid(sp) => (*sp).to_string(),
    };
    let sql = format!("select * from {} where {} = $1", quote_ident(table), where_);
    match conn_query(c, &sql, &[Value::Int(rowid)])? {
        ExecResult::Rows { mut rows, .. } => Ok(rows.pop()),
        _ => Ok(None),
    }
}

/// Resolve `name` among `candidates` sqlite-style: exact match first, then a
/// unique case-insensitive one.
fn resolve<'a, T>(candidates: impl Iterator<Item = (&'a str, T)> + Clone, name: &str) -> Option<T> {
    let mut exact = candidates
        .clone()
        .filter(|(n, _)| *n == name)
        .map(|(_, t)| t);
    if let Some(t) = exact.next() {
        return Some(t);
    }
    let mut ci = candidates
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, t)| t);
    match (ci.next(), ci.next()) {
        (Some(t), None) => Some(t),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Open.
// ---------------------------------------------------------------------------

fn blob_open_impl(
    c: &mut Sqlite3,
    dbname: Option<&str>,
    table_arg: Option<&str>,
    col_arg: Option<&str>,
    rowid: i64,
    flags: c_int,
) -> Result<Box<Sqlite3Blob>, (c_int, String)> {
    let want_write = flags != 0;
    let (Some(dbname), Some(table_arg), Some(col_arg)) = (dbname, table_arg, col_arg) else {
        return Err((SQLITE_MISUSE, "invalid argument".into()));
    };
    if c.readonly && want_write {
        return Err((SQLITE_READONLY, "attempt to write a readonly database".into()));
    }
    if !dbname.eq_ignore_ascii_case("main") {
        // sqlite reports an unknown database THROUGH the table message (probed:
        // `no such table: nope.t`). An mpedb attach is a read-only SQL surface
        // (#51) with no rowid discipline, so blob I/O on one is refused by name.
        let attached = c.db.attached_databases();
        if attached.iter().any(|(n, _)| n.eq_ignore_ascii_case(dbname)) {
            return Err((
                SQLITE_ERROR,
                format!("incremental blob I/O on an attached database is not supported by mpedb (`{dbname}`)"),
            ));
        }
        return Err((SQLITE_ERROR, format!("no such table: {dbname}.{table_arg}")));
    }
    let bundle = c.db.schema();
    let Some(tid) = resolve(
        bundle
            .tables
            .iter()
            .filter(|t| !t.dead && t.name != crate::SEED_TABLE)
            .map(|t| (t.name.as_str(), t.id)),
        table_arg,
    ) else {
        return Err((SQLITE_ERROR, format!("no such table: main.{table_arg}")));
    };
    let t = bundle.table(tid).expect("id from resolve");
    // Rowid addressing: an INTEGER-PRIMARY-KEY alias, or #94's hidden rowid.
    // Anything else (text/composite PK, FTS) is exactly sqlite's WITHOUT ROWID
    // answer. For the hidden rowid, generated SQL uses the first of sqlite's
    // three spellings not shadowed by a real column (the binder gives a real
    // column precedence, #94).
    let addr = if let Some(pk) = t.rowid_alias_col() {
        Addr::AliasPk(t.columns[pk as usize].name.clone())
    } else if t.hidden_rowid_col().is_some() {
        let spelling = ["rowid", "_rowid_", "oid"].into_iter().find(|sp| {
            !t.visible_columns().iter().any(|col| col.name.eq_ignore_ascii_case(sp))
        });
        match spelling {
            Some(sp) => Addr::HiddenRowid(sp),
            None => {
                return Err((
                    SQLITE_ERROR,
                    format!("cannot address rows of `{}` by rowid: every rowid spelling is shadowed by a column", t.name),
                ))
            }
        }
    } else {
        return Err((SQLITE_ERROR, format!("cannot open table without rowid: {table_arg}")));
    };
    let Some(col_idx) = resolve(
        t.visible_columns().iter().enumerate().map(|(i, col)| (col.name.as_str(), i)),
        col_arg,
    ) else {
        return Err((SQLITE_ERROR, format!("no such column: \"{col_arg}\"")));
    };
    // Schema-shape checks come BEFORE the row fetch, as in sqlite: a read-write
    // open on a PK/indexed/UNIQUE column fails even for a missing row.
    if want_write {
        let ord = col_idx as u16;
        let indexed = t.primary_key.contains(&ord) || t.indexes.iter().any(|ix| ix.columns.contains(&ord));
        if indexed {
            return Err((SQLITE_ERROR, "cannot open indexed column for writing".into()));
        }
    }
    let table = t.name.clone();
    let col_name = t.visible_columns()[col_idx].name.clone();
    drop(bundle);
    let row = fetch_row(c, &table, &addr, rowid)
        .map_err(|e| (valconv::error_codes(&e).0, e.to_string()))?;
    let Some(row) = row else {
        return Err((SQLITE_ERROR, format!("no such rowid: {rowid}")));
    };
    let Some(bytes) = row.get(col_idx).and_then(payload) else {
        let tyname = sqlite_type_name(row.get(col_idx).unwrap_or(&Value::Null));
        return Err((SQLITE_ERROR, format!("cannot open value of type {tyname}")));
    };
    let size = bytes.len();
    Ok(Box::new(Sqlite3Blob {
        db: c as *mut Sqlite3,
        table,
        col_name,
        col_idx,
        addr,
        readonly: !want_write,
        rowid,
        size,
        snapshot: row,
        aborted: false,
    }))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_open(
    db: *mut Sqlite3,
    dbname: *const c_char,
    table: *const c_char,
    column: *const c_char,
    row: c_longlong,
    flags: c_int,
    pp_blob: *mut *mut c_void,
) -> c_int {
    if !pp_blob.is_null() {
        *pp_blob = ptr::null_mut();
    }
    let Some(c) = conn(db) else {
        return SQLITE_MISUSE;
    };
    if pp_blob.is_null() {
        c.set_error(SQLITE_MISUSE, SQLITE_MISUSE, "bad parameter or other API misuse");
        return SQLITE_MISUSE;
    }
    let dbname = crate::c_str_opt(dbname);
    let table = crate::c_str_opt(table);
    let column = crate::c_str_opt(column);
    let out = catch_unwind(AssertUnwindSafe(|| {
        blob_open_impl(c, dbname, table, column, row, flags)
    }));
    match out {
        Ok(Ok(handle)) => {
            let p = Box::into_raw(handle);
            c.blobs.push(p);
            *pp_blob = p as *mut c_void;
            c.clear_error();
            SQLITE_OK
        }
        Ok(Err((code, msg))) => {
            c.set_error(code, code, &msg);
            code
        }
        Err(_) => {
            c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) in engine");
            SQLITE_ERROR
        }
    }
}

// ---------------------------------------------------------------------------
// Read / write / bytes / reopen / close.
// ---------------------------------------------------------------------------

unsafe fn blob<'a>(p: *mut c_void) -> Option<&'a mut Sqlite3Blob> {
    if p.is_null() {
        None
    } else {
        Some(&mut *(p as *mut Sqlite3Blob))
    }
}

/// Re-read the handle's row and compare with its snapshot. `Ok(row)` = alive
/// and unchanged; `Err(code)` already transitioned the handle/connection state.
fn check_current_row(b: &mut Sqlite3Blob, c: &mut Sqlite3) -> Result<Vec<Value>, c_int> {
    match fetch_row(c, &b.table, &b.addr, b.rowid) {
        Ok(Some(row)) if values_identical(&row, &b.snapshot) => Ok(row),
        Ok(_) => {
            // Modified or deleted under the handle: sqlite's expiry. The
            // handle is dead for good (a failed I/O finalizes it — probed).
            b.aborted = true;
            c.set_error(SQLITE_ABORT, SQLITE_ABORT, "query aborted");
            Err(SQLITE_ABORT)
        }
        Err(e) => Err(c.set_db_error(&e)),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_read(
    pb: *mut c_void,
    out: *mut c_void,
    n: c_int,
    off: c_int,
) -> c_int {
    let Some(b) = blob(pb) else {
        return SQLITE_MISUSE;
    };
    let Some(c) = conn(b.db) else {
        return SQLITE_MISUSE;
    };
    // sqlite's check order (probed/source): bounds, then the aborted state.
    if n < 0 || off < 0 || (off as i64 + n as i64) > b.size as i64 {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "SQL logic error");
        return SQLITE_ERROR;
    }
    if b.aborted {
        c.set_error(SQLITE_ABORT, SQLITE_ABORT, "query aborted");
        return SQLITE_ABORT;
    }
    if out.is_null() {
        return SQLITE_MISUSE;
    }
    let res = catch_unwind(AssertUnwindSafe(|| -> c_int {
        let row = match check_current_row(b, c) {
            Ok(row) => row,
            Err(code) => return code,
        };
        let bytes = row.get(b.col_idx).and_then(payload).unwrap_or(&[]);
        // In-bounds by the size check: identical row ⇒ identical value.
        let src = &bytes[off as usize..off as usize + n as usize];
        ptr::copy_nonoverlapping(src.as_ptr(), out as *mut u8, src.len());
        c.clear_error();
        SQLITE_OK
    }));
    res.unwrap_or_else(|_| {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) in engine");
        SQLITE_ERROR
    })
}

fn blob_write_impl(
    b: &mut Sqlite3Blob,
    self_ptr: *mut Sqlite3Blob,
    c: &mut Sqlite3,
    data: &[u8],
    off: usize,
) -> c_int {
    let row = match check_current_row(b, c) {
        Ok(row) => row,
        Err(code) => return code,
    };
    let old = &row[b.col_idx];
    let mut patched = payload(old).unwrap_or(&[]).to_vec();
    patched[off..off + data.len()].copy_from_slice(data);
    let new_value = match old {
        Value::Text(_) => match String::from_utf8(patched) {
            Ok(s) => Value::Text(s),
            Err(_) => {
                c.set_error(
                    SQLITE_ERROR,
                    SQLITE_ERROR,
                    "blob write would make a text value invalid UTF-8 (mpedb text is strictly UTF-8)",
                );
                return SQLITE_ERROR;
            }
        },
        _ => Value::Blob(patched),
    };
    // Guarded full-row UPDATE: the WHERE re-asserts the snapshot column by
    // column (IS, so NULLs compare), making read-patch-write atomic even in
    // autocommit — 0 rows affected means the row moved underneath us, which
    // IS the expiry answer.
    let mut sql = format!(
        "update {} set {} = $1 where ",
        quote_ident(&b.table),
        quote_ident(&b.col_name)
    );
    match &b.addr {
        Addr::AliasPk(pk) => sql.push_str(&format!("{} = $2", quote_ident(pk))),
        Addr::HiddenRowid(sp) => sql.push_str(&format!("{sp} = $2")),
    }
    let bundle = c.db.schema();
    let names: Vec<String> = bundle
        .table(bundle.table_id(&b.table).unwrap_or(u32::MAX))
        .map(|t| t.visible_columns().iter().map(|col| col.name.clone()).collect())
        .unwrap_or_default();
    drop(bundle);
    if names.len() != row.len() {
        // DDL moved the table underneath the handle: treat as expired.
        b.aborted = true;
        c.set_error(SQLITE_ABORT, SQLITE_ABORT, "query aborted");
        return SQLITE_ABORT;
    }
    let mut params = vec![new_value.clone(), Value::Int(b.rowid)];
    for (i, name) in names.iter().enumerate() {
        sql.push_str(&format!(" and {} is ${}", quote_ident(name), i + 3));
        params.push(row[i].clone());
    }
    match conn_query(c, &sql, &params) {
        Ok(ExecResult::Affected(1)) => {
            // Success. Writes through a blob handle expire NOTHING (probed):
            // this handle's snapshot moves with the write, and any same-row
            // sibling handle on THIS connection whose snapshot matched the
            // pre-write row is refreshed too — a sibling that was ALREADY
            // stale keeps its stale snapshot and expires on its next I/O.
            let before = row;
            let mut snap = before.clone();
            snap[b.col_idx] = new_value;
            b.snapshot = snap.clone();
            let (tbl, rid) = (b.table.clone(), b.rowid);
            for &sib_ptr in &c.blobs {
                if sib_ptr == self_ptr {
                    continue; // `b` is already refreshed (and mutably borrowed)
                }
                // SAFETY: every pointer in `c.blobs` is a live Box'd handle
                // (removed at blob_close), and `self_ptr` — the only aliased
                // one — is skipped above.
                let sib = unsafe { &mut *sib_ptr };
                if !sib.aborted
                    && sib.table == tbl
                    && sib.rowid == rid
                    && values_identical(&sib.snapshot, &before)
                {
                    sib.snapshot = snap.clone();
                }
            }
            c.clear_error();
            SQLITE_OK
        }
        Ok(_) => {
            b.aborted = true;
            c.set_error(SQLITE_ABORT, SQLITE_ABORT, "query aborted");
            SQLITE_ABORT
        }
        Err(e) => c.set_db_error(&e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_write(
    pb: *mut c_void,
    data: *const c_void,
    n: c_int,
    off: c_int,
) -> c_int {
    let Some(b) = blob(pb) else {
        return SQLITE_MISUSE;
    };
    let Some(c) = conn(b.db) else {
        return SQLITE_MISUSE;
    };
    // Order (sqlite source): bounds, then aborted, then read-only.
    if n < 0 || off < 0 || (off as i64 + n as i64) > b.size as i64 {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "SQL logic error");
        return SQLITE_ERROR;
    }
    if b.aborted {
        c.set_error(SQLITE_ABORT, SQLITE_ABORT, "query aborted");
        return SQLITE_ABORT;
    }
    if b.readonly {
        c.set_error(SQLITE_READONLY, SQLITE_READONLY, "attempt to write a readonly database");
        return SQLITE_READONLY;
    }
    if data.is_null() && n > 0 {
        return SQLITE_MISUSE;
    }
    let data = if n == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(data as *const u8, n as usize)
    };
    let self_ptr = pb as *mut Sqlite3Blob;
    catch_unwind(AssertUnwindSafe(|| {
        blob_write_impl(b, self_ptr, c, data, off as usize)
    }))
    .unwrap_or_else(|_| {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) in engine");
        SQLITE_ERROR
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_bytes(pb: *mut c_void) -> c_int {
    // sqlite: 0 once the handle is dead, its open-time size otherwise.
    match blob(pb) {
        Some(b) if !b.aborted => b.size as c_int,
        _ => 0,
    }
}

fn blob_reopen_impl(b: &mut Sqlite3Blob, c: &mut Sqlite3, rowid: i64) -> c_int {
    // A dead handle stays dead — even reopen answers ABORT (probed).
    if b.aborted {
        c.set_error(SQLITE_ABORT, SQLITE_ABORT, "query aborted");
        return SQLITE_ABORT;
    }
    // The OLD row's state is irrelevant (probed: a handle whose current row
    // was just modified reopens cleanly); only the new row is examined.
    let row = match fetch_row(c, &b.table, &b.addr, rowid) {
        Ok(Some(row)) => row,
        Ok(None) => {
            // Failed reopen kills the handle (sqlite finalizes it).
            b.aborted = true;
            let msg = format!("no such rowid: {rowid}");
            c.set_error(SQLITE_ERROR, SQLITE_ERROR, &msg);
            return SQLITE_ERROR;
        }
        Err(e) => return c.set_db_error(&e),
    };
    let Some(bytes) = row.get(b.col_idx).and_then(payload) else {
        b.aborted = true;
        let tyname = sqlite_type_name(row.get(b.col_idx).unwrap_or(&Value::Null));
        let msg = format!("cannot open value of type {tyname}");
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, &msg);
        return SQLITE_ERROR;
    };
    b.size = bytes.len();
    b.rowid = rowid;
    b.snapshot = row;
    c.clear_error();
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_reopen(pb: *mut c_void, rowid: c_longlong) -> c_int {
    let Some(b) = blob(pb) else {
        return SQLITE_MISUSE;
    };
    let Some(c) = conn(b.db) else {
        return SQLITE_MISUSE;
    };
    catch_unwind(AssertUnwindSafe(|| blob_reopen_impl(b, c, rowid))).unwrap_or_else(|_| {
        c.set_error(SQLITE_ERROR, SQLITE_ERROR, "internal error (panic) in engine");
        SQLITE_ERROR
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_close(pb: *mut c_void) -> c_int {
    let Some(b) = blob(pb) else {
        return SQLITE_OK; // sqlite: closing a NULL handle is a harmless no-op
    };
    let db = b.db;
    drop(Box::from_raw(pb as *mut Sqlite3Blob));
    if let Some(c) = conn(db) {
        c.blobs.retain(|&p| p != pb as *mut Sqlite3Blob);
        // The connection was `sqlite3_close_v2`'d while this handle was open
        // and only stayed alive for it: this is the last one, so free it now.
        if c.zombie && c.blobs.is_empty() {
            crate::free_connection(db);
        }
    }
    SQLITE_OK
}

// ---------------------------------------------------------------------------
// zeroblob — the SQL function and the bind.
// ---------------------------------------------------------------------------

/// sqlite's compile-time `SQLITE_MAX_LENGTH`: `zeroblob(N)` beyond it is
/// `SQLITE_TOOBIG` ("string or blob too big"). mpedb has no lazy zero-filled
/// representation, so a zeroblob is a REAL allocation of N zero bytes —
/// semantically identical, and this cap is the memory guard.
pub(crate) const MAX_BLOB_LEN: i64 = 1_000_000_000;

/// `sqlite3_value_int64`'s cast, for zeroblob's argument (probed: NULL → 0,
/// `'7'` → 7, `'x'` → 0, 3.9 → 3).
pub(crate) fn sqlite_int64_of(v: &Value) -> i64 {
    match v {
        Value::Null => 0,
        Value::Int(i) => *i,
        Value::Bool(b) => i64::from(*b),
        Value::Timestamp(t) => *t,
        Value::Float(f) => *f as i64, // trunc toward zero, saturating
        Value::Text(s) => text_to_i64(s),
        Value::Blob(b) => text_to_i64(&String::from_utf8_lossy(b)),
        _ => 0,
    }
}

/// sqlite's text→integer: the longest leading numeric prefix (integer or
/// real), truncated toward zero; no prefix ⇒ 0.
pub(crate) fn text_to_i64(s: &str) -> i64 {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut end = 0usize;
    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
        end += 1;
    }
    let int_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let int_end = end;
    // Fast path: a pure integer prefix that fits.
    if int_end > int_start {
        if let Ok(i) = t[..int_end].parse::<i64>() {
            // Continue only if a real-number tail follows; otherwise done.
            let real_tail = bytes.get(end) == Some(&b'.')
                || bytes.get(end) == Some(&b'e')
                || bytes.get(end) == Some(&b'E');
            if !real_tail {
                return i;
            }
        }
    }
    // Real-shaped (or overflowing) prefix: parse as f64 and truncate.
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut e = end + 1;
        if e < bytes.len() && (bytes[e] == b'+' || bytes[e] == b'-') {
            e += 1;
        }
        if e < bytes.len() && bytes[e].is_ascii_digit() {
            while e < bytes.len() && bytes[e].is_ascii_digit() {
                e += 1;
            }
            end = e;
        }
    }
    if end == int_start || (end == int_start + 1 && bytes[int_start] == b'.') {
        return 0;
    }
    t[..end].parse::<f64>().map(|f| f as i64).unwrap_or(0)
}

/// The `zeroblob(N)` SQL function body: N zero bytes (N ≤ 0 ⇒ empty), or the
/// TOOBIG refusal routed through the UDF error stash so the consumer sees
/// sqlite's exact code and text (`SQLITE_TOOBIG`, "string or blob too big").
pub(crate) fn zeroblob_value(args: &[Value]) -> mpedb::Result<Value> {
    let n = sqlite_int64_of(args.first().unwrap_or(&Value::Null));
    if n > MAX_BLOB_LEN {
        let msg = "string or blob too big";
        crate::udf::stash_udf_error(SQLITE_TOOBIG, msg);
        return Err(DbError::Unsupported(format!("user function raised: {msg}")));
    }
    Ok(Value::Blob(vec![0u8; n.max(0) as usize]))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_zeroblob(p: *mut crate::Stmt, idx: c_int, n: c_int) -> c_int {
    if (n as i64) > MAX_BLOB_LEN {
        return SQLITE_TOOBIG;
    }
    crate::bind(p, idx, Value::Blob(vec![0u8; n.max(0) as usize]))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_zeroblob64(p: *mut crate::Stmt, idx: c_int, n: u64) -> c_int {
    if n > MAX_BLOB_LEN as u64 {
        return SQLITE_TOOBIG;
    }
    crate::bind(p, idx, Value::Blob(vec![0u8; n as usize]))
}
