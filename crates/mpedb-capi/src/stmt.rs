use super::*;

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
    // What `sqlite3_sql` must hand back is the span sqlite CONSUMED, and that
    // includes the terminating `;` — `split_first` drops it because the engine
    // parses what comes before. Two different questions, so two strings:
    // `sql` is what the caller asked for, `exec_sql` is what runs.
    // `SQLite3Stmt::getSQL` answered "SELECT :a, :b, ?" where sqlite answers
    // "SELECT :a, :b, ?;" until this existed.
    let asked = match full.as_bytes().get(first.len()) {
        Some(b';') => &full[..first.len() + 1],
        _ => first,
    };
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
        // `sqlite_sequence` WRITES are answered by the exec arm's counter
        // mapping, never compiled — prepare-time validation would refuse them
        // with "no such table" before the arm could run. The detector is
        // TARGET-position only, so this skips validation for nothing else.
        || introspect::sqlite_sequence_write_target(first)
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

    // SQLITE_LIMIT_FUNCTION_ARG: sqlite refuses at parse when a CALL carries
    // more arguments than the connection allows, with exactly this message.
    // The count comes from the text (`sql::max_function_args`), so it is gated
    // twice over: only below the compile-time default — nobody but a test ever
    // lowers this — and only for names the connection can actually call.
    let fn_arg_limit = c.limits[SQLITE_LIMIT_FUNCTION_ARG as usize].max(0) as usize;
    if fn_arg_limit < DEFAULT_LIMITS[SQLITE_LIMIT_FUNCTION_ARG as usize] as usize {
        let host: Vec<String> = c.host_fns.iter().map(|h| h.name.to_ascii_lowercase()).collect();
        if let Some((name, args)) =
            sql::max_function_args(first_zb, |n| is_callable_name(n) || host.iter().any(|h| h == n))
        {
            if args > fn_arg_limit {
                c.set_error(
                    SQLITE_ERROR,
                    SQLITE_ERROR,
                    &format!("too many arguments on function {name}"),
                );
                return SQLITE_ERROR;
            }
        }
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
        // `INSERT OR ROLLBACK` and CREATE … `ON CONFLICT ROLLBACK` are executed
        // as ABORT plus a connection rollback (see `exec_one`), so validate the
        // text the engine will actually be handed — the engine's parser refuses
        // ROLLBACK by name.
        let (to_validate, _) = sql::rewrite_insert_or_rollback(to_validate);
        let (to_validate, _) = sql::rewrite_on_conflict_rollback(&to_validate);
        if let Some(msg) = nondeterministic_in_partial_index(c, &to_validate) {
            c.set_error(SQLITE_ERROR, SQLITE_ERROR, &msg);
            return SQLITE_ERROR;
        }
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
        sql: asked.to_string(),
        exec_sql: scan.rewritten,
        n_params,
        param_names: scan.names,
        binds: vec![Value::Null; n_params],
        executed: false,
        columns: Vec::new(),
        col_name_c: Vec::new(),
        decltype_c: None,
        table_name_c: None,
        sql_c: None,
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
                    // `sqlite3_exec`'s throwaway statement is never handed
                    // to a caller, so no one can ask it for its SQL — the
                    // consumed-span distinction above does not arise here.
                    sql: first.to_string(),
                    exec_sql: first.to_string(),
                    n_params: 0,
                    param_names: Vec::new(),
                    binds: Vec::new(),
                    executed: false,
                    columns: Vec::new(),
                    col_name_c: Vec::new(),
                    decltype_c: None,
                    table_name_c: None,
                    sql_c: None,
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

pub(super) unsafe fn bind(p: *mut Stmt, idx: c_int, v: Value) -> c_int {
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
pub(super) unsafe fn maybe_free(destructor: *mut c_void, data: *mut c_void) {
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
            s.binds.fill(Value::Null);
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

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_table_name(p: *mut Stmt, col: c_int) -> *const c_char {
    let Some(s) = stmt(p) else { return ptr::null() };
    if col < 0 {
        return ptr::null();
    }
    // Same lazy shape and the same plan-derived source mapping as
    // `sqlite3_column_decltype`: an output column that is a bare base-table
    // column reports that table, anything computed reports NULL. Real sqlite
    // only exports this call when built with SQLITE_ENABLE_COLUMN_METADATA;
    // consumers that link it (PHP's pdo_sqlite) treat NULL as "no table",
    // which is also what sqlite answers for an expression.
    if s.table_name_c.is_none() {
        let names = conn(s.db)
            .and_then(|c| c.db.output_table_names(&s.exec_sql).ok())
            .unwrap_or_default();
        s.table_name_c = Some(
            names
                .into_iter()
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
    match s.table_name_c.as_ref().unwrap().get(col as usize) {
        Some(Some(bytes)) => bytes.as_ptr() as *const c_char,
        _ => ptr::null(),
    }
}

/// `sqlite3_sql`: the statement text as it was handed to prepare — the
/// ORIGINAL spelling, parameters unsubstituted (that is `expanded_sql`). The
/// pointer belongs to the statement and stays valid until it is finalized, so
/// the NUL-terminated copy is kept on the statement rather than allocated per
/// call. sqlite answers NULL for a null handle.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_sql(p: *mut Stmt) -> *const c_char {
    let Some(s) = stmt(p) else { return ptr::null() };
    if s.sql_c.is_none() {
        let mut v = s.sql.clone().into_bytes();
        v.push(0);
        s.sql_c = Some(v);
    }
    s.sql_c.as_ref().unwrap().as_ptr() as *const c_char
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
pub(super) unsafe fn dup_cstr(s: &str) -> *mut c_char {
    let bytes = s.as_bytes();
    let buf = libc::malloc(bytes.len() + 1) as *mut u8;
    if buf.is_null() {
        return ptr::null_mut();
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
    *buf.add(bytes.len()) = 0;
    buf as *mut c_char
}

