use super::*;

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
        // Stored as integers: micros since the epoch, days since it, micros
        // since midnight.
        Value::Timestamp(us) | Value::Date(us) | Value::Time(us) => us.to_string(),
        // Its canonical digits ARE a numeric literal, so it re-parses as
        // itself rather than through a float.
        Value::Numeric(n) => n.clone(),
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
    db: *mut Sqlite3,
    op: c_int,
    a: c_int,
    b: *mut c_void,
) -> c_int {
    // The `(int onoff, int *pCurrent)` toggle ops — 1002 (ENABLE_FKEY) through
    // the 1019 range; NOT 1000/1001, whose varargs are pointers with different
    // shapes. `onoff` is 1 = on, 0 = off, -1 = report without changing, and the
    // out pointer receives the state AFTERWARDS.
    //
    // Writing back a constant 0 was the literal truth while mpedb honored none
    // of these. FOREIGN KEY enforcement is real now, so for 1002 that constant
    // became a lie in both directions: it under-reported an enabled connection,
    // and it failed the setter outright — CPython's `setconfig` compares what
    // came back against what it asked for and raises "Unable to set config"
    // when they differ, so `PRAGMA foreign_keys`'s C-API twin never worked.
    //
    // The other ops are still honored by nobody, and 0 stays their honest
    // answer: a consumer that asks whether triggers are disabled gets "no",
    // which is true, rather than an indeterminate int.
    if !(1002..=1019).contains(&op) {
        return SQLITE_OK;
    }
    let state = if op == SQLITE_DBCONFIG_ENABLE_FKEY {
        match conn(db) {
            None => return SQLITE_MISUSE,
            Some(c) => {
                // Same rule as the PRAGMA: a change inside a transaction is
                // silently ignored (measured against sqlite), so the state that
                // comes back is whatever actually holds now.
                match a {
                    0 | 1 => c.db.set_fk_enforced(a == 1),
                    _ => {}
                }
                c.db.fk_enforced()
            }
        }
    } else {
        false
    };
    if !b.is_null() {
        *(b as *mut c_int) = c_int::from(state);
    }
    SQLITE_OK
}

/// `SQLITE_DBCONFIG_ENABLE_FKEY` — the C-API twin of `PRAGMA foreign_keys`.
const SQLITE_DBCONFIG_ENABLE_FKEY: c_int = 1002;

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
    e_text_rep: c_int,
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
        deterministic: (e_text_rep & SQLITE_DETERMINISTIC) != 0,
        x_destroy,
        p_app: app,
        x_func,
        x_step,
        x_final,
        x_value: ptr::null_mut(),
        x_inverse: ptr::null_mut(),
    });
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function(
    db: *mut Sqlite3,
    name: *const c_char,
    n_arg: c_int,
    enc: c_int,
    app: *mut c_void,
    x_func: *mut c_void,
    x_step: *mut c_void,
    x_final: *mut c_void,
) -> c_int {
    create_function_impl(
        db,
        name,
        n_arg,
        enc,
        app,
        x_func,
        x_step,
        x_final,
        ptr::null_mut(),
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function_v2(
    db: *mut Sqlite3,
    name: *const c_char,
    n_arg: c_int,
    enc: c_int,
    app: *mut c_void,
    x_func: *mut c_void,
    x_step: *mut c_void,
    x_final: *mut c_void,
    x_destroy: *mut c_void,
) -> c_int {
    create_function_impl(db, name, n_arg, enc, app, x_func, x_step, x_final, x_destroy)
}

/// `sqlite3_create_window_function(db, name, nArg, enc, pApp, xStep, xFinal,
/// xValue, xInverse, xDestroy)` — a user-defined WINDOW aggregate
/// (design/DESIGN-UDF.md stage 4).
///
/// This is a strictly stronger registration than `create_function`'s aggregate
/// form, and that is the whole point: `xValue` reports the CURRENT frame's
/// result without consuming the aggregate context, and `xInverse` retracts a row
/// that has left the frame. With both, mpedb's window executor SLIDES the frame
/// — stepping rows that enter, inverting rows that leave, calling `xValue` once
/// per row and `xFinal` once per partition, which is the call sequence a
/// consumer's callbacks are written for.
///
/// **Scope, and what is refused by name.** The registration is ONE argument
/// (`nArg` 1 or the variadic `-1`) — the sliding protocol feeds `xInverse` the
/// same single row's arguments `xStep` got — and a wider arity is accepted here
/// but refused at the call site rather than silently mis-fed. `xStep`/`xFinal`
/// are mandatory; `xValue`/`xInverse` missing makes it a plain aggregate
/// registration, exactly as sqlite documents, so `OVER` then refuses by name.
/// All-NULL callbacks DELETE the entry (sqlite's rule, and CPython's
/// `create_window_function(name, n, None)`).
///
/// # Safety
/// `db` must be a connection this shim opened; the callbacks must match sqlite's
/// signatures and stay valid until the registration is replaced or the
/// connection closes.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_window_function(
    db: *mut Sqlite3,
    name: *const c_char,
    n_arg: c_int,
    _enc: c_int,
    app: *mut c_void,
    x_step: *mut c_void,
    x_final: *mut c_void,
    x_value: *mut c_void,
    x_inverse: *mut c_void,
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
            "create_window_function: NULL or non-UTF-8 function name",
        );
        return SQLITE_MISUSE;
    };
    if !(-1..=127).contains(&n_arg) {
        call_destroy(x_destroy, app);
        c.set_error(
            SQLITE_MISUSE,
            SQLITE_MISUSE,
            "create_window_function: nArg must be between -1 and 127",
        );
        return SQLITE_MISUSE;
    }
    let fname = raw_name.to_ascii_lowercase();
    let deleting = x_step.is_null() && x_final.is_null() && x_value.is_null() && x_inverse.is_null();
    if !deleting && (x_step.is_null() || x_final.is_null()) {
        call_destroy(x_destroy, app);
        c.set_error(
            SQLITE_MISUSE,
            SQLITE_MISUSE,
            "create_window_function: a window aggregate needs BOTH xStep and xFinal",
        );
        return SQLITE_MISUSE;
    }
    // A repeat registration REPLACES — run the previous entry's destructor and
    // clear whichever registry it went into (see `create_function_impl`).
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
    if deleting {
        c.db.unregister_host_function(&fname, n_arg);
        c.db.unregister_host_aggregate(&fname, n_arg);
        call_destroy(x_destroy, app);
        return SQLITE_OK;
    }
    let step: udf::XStep = std::mem::transmute(x_step);
    let fin: udf::XFinal = std::mem::transmute(x_final);
    let val: Option<udf::XFinal> = (!x_value.is_null()).then(|| std::mem::transmute(x_value));
    let inv: Option<udf::XStep> = (!x_inverse.is_null()).then(|| std::mem::transmute(x_inverse));
    let factory = udf::make_window_agg_factory(step, fin, val, inv, app);
    // Only a COMPLETE window registration goes into the window registry; a
    // half one is a plain aggregate, which `OVER` then refuses by name.
    if val.is_some() && inv.is_some() {
        c.db.register_host_window_aggregate(&fname, n_arg, factory);
    } else {
        c.db.register_host_aggregate(&fname, n_arg, factory);
    }
    c.host_fns.push(udf::HostFn {
        name: fname,
        n_arg,
        aggregate: true,
        deterministic: false,
        x_destroy,
        p_app: app,
        x_func: ptr::null_mut(),
        x_step,
        x_final,
        x_value,
        x_inverse,
    });
    SQLITE_OK
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

// ---- serialize / deserialize (plan §5) -------------------------------------

const SQLITE_SERIALIZE_NOCOPY: c_uint = 0x001;
const SQLITE_DESERIALIZE_FREEONCLOSE: c_uint = 1;
const SQLITE_DESERIALIZE_RESIZEABLE: c_uint = 2;

/// `sqlite3_serialize(db, "main", &size, flags)` — the database as one
/// malloc'd byte image (mpedb's OWN format; `sqlite3_deserialize` below and
/// nothing else adopts it). The buffer comes from this shim's
/// `sqlite3_malloc64`, so the caller's `sqlite3_free` pairs with it —
/// CPython copies then frees.
///
/// `SQLITE_SERIALIZE_NOCOPY` asks for a borrowed pointer to a contiguous
/// in-memory image; mpedb's pages live in a file mapping, so the answer is
/// NULL — the documented "no such image" outcome, after which CPython simply
/// calls again without the flag. Named narrowings, each answered with NULL
/// (CPython raises `unable to serialize` from it): a non-`main` schema, and
/// a connection holding an open transaction (the capture takes the writer
/// lock, which that transaction already owns).
///
/// # Safety
/// `db` must be a connection this shim opened (or NULL); `p_size` NULL is
/// tolerated (sqlite crashes on it — nothing depends on matching that).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_serialize(
    db: *mut Sqlite3,
    schema: *const c_char,
    p_size: *mut c_longlong,
    flags: c_uint,
) -> *mut c_uchar {
    if !p_size.is_null() {
        *p_size = 0;
    }
    let Some(c) = conn(db) else {
        return ptr::null_mut();
    };
    c.clear_error();
    if flags & SQLITE_SERIALIZE_NOCOPY != 0 {
        return ptr::null_mut();
    }
    let is_main = schema.is_null()
        || c_str_opt(schema).is_none_or(|s| s.is_empty() || s.eq_ignore_ascii_case("main"));
    if !is_main || c.txn.is_some() {
        return ptr::null_mut();
    }
    // Plan §11: the image is a REAL sqlite file of the logical content —
    // interop no consumer can call a differ. Out of the writer's v1 scope
    // (declared PKs, indexes, non-scalar values) -> NULL, which CPython
    // reports as `unable to serialize` — a named refusal, never a foreign
    // format under sqlite's name.
    let bytes = match c.db.sqlite_image(&[SEED_TABLE]) {
        Ok(Some(b)) => b,
        _ => return ptr::null_mut(),
    };
    let p = sqlite3_malloc64(bytes.len() as u64) as *mut c_uchar;
    if p.is_null() {
        return ptr::null_mut();
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    if !p_size.is_null() {
        *p_size = bytes.len() as c_longlong;
    }
    p
}

/// `sqlite3_deserialize(db, "main", data, sz, szBuf, flags)` — adopt a
/// serialized image: the connection DETACHES from whatever it was open on
/// and reopens on the image, written to a fresh scratch file (tmpfs when
/// available) that closes out like an `:memory:` database. The caller's
/// original FILE is never touched — sqlite likewise leaves it behind rather
/// than writing the image over it.
///
/// Ownership: with `SQLITE_DESERIALIZE_FREEONCLOSE` the buffer is OURS from
/// this call on, success or failure (sqlite's documented contract, and
/// CPython passes the flag and never frees) — every exit path below frees
/// it. `RESIZEABLE` is moot (the bytes are copied out); a flag beyond the
/// two known ones refuses by name.
///
/// Per-connection state (FK pragma, busy timeout, UDFs, collations) carries
/// over — `adopt_reopened`, shared with the backup install. Statements
/// prepared before the call keep the backup path's semantics.
///
/// # Safety
/// `db` must be a connection this shim opened (or NULL); `data` must point
/// at `sz` readable bytes (and, under FREEONCLOSE, come from this shim's
/// `sqlite3_malloc`).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_deserialize(
    db: *mut Sqlite3,
    schema: *const c_char,
    data: *mut c_uchar,
    sz: c_longlong,
    _sz_buf: c_longlong,
    flags: c_uint,
) -> c_int {
    let free_buf = || {
        if flags & SQLITE_DESERIALIZE_FREEONCLOSE != 0 {
            sqlite3_free(data as *mut c_void);
        }
    };
    let Some(c) = conn(db) else {
        free_buf();
        return SQLITE_MISUSE;
    };
    c.clear_error();
    let fail = |c: &mut Sqlite3, code: c_int, msg: &str| -> c_int {
        c.set_error(code, code, msg);
        code
    };
    if flags & !(SQLITE_DESERIALIZE_FREEONCLOSE | SQLITE_DESERIALIZE_RESIZEABLE) != 0 {
        let rc = fail(c, SQLITE_ERROR, "unsupported sqlite3_deserialize flags");
        free_buf();
        return rc;
    }
    let is_main = schema.is_null()
        || c_str_opt(schema).is_none_or(|s| s.is_empty() || s.eq_ignore_ascii_case("main"));
    if !is_main {
        let rc = fail(c, SQLITE_ERROR, "deserialize into an attached schema is not supported");
        free_buf();
        return rc;
    }
    if c.txn.is_some() || !c.blobs.is_empty() || !c.backups.is_empty() {
        let rc = fail(c, SQLITE_BUSY, "database is locked");
        free_buf();
        return rc;
    }
    // A REAL sqlite image (what serialize now emits, and what any sqlite
    // producer hands us): imported through the NATIVE reader — tables
    // re-created, rows re-inserted into a fresh blank database, then adopted
    // exactly like the mpedb-format path below. No sqlite library anywhere.
    if sz >= 16 && std::slice::from_raw_parts(data, 16) == b"SQLite format 3\0" {
        let bytes = std::slice::from_raw_parts(data, sz as usize);
        let staged = ephemeral_path();
        let rc = (|| -> Result<(), c_int> {
            std::fs::write(&staged, bytes).map_err(|_| SQLITE_IOERR)?;
            let f = mpedb_sqlitefmt::SqliteFile::open(&staged).map_err(|_| SQLITE_NOTADB)?;
            let tables = f.tables().map_err(|_| SQLITE_NOTADB)?;
            let (newdb, newpath) = open_blank_database().map_err(|_| SQLITE_IOERR)?;
            for t in &tables {
                let cols: Vec<String> = t
                    .columns
                    .iter()
                    .zip(&t.decl_types)
                    .map(|(n, d)| {
                        let q = n.replace('"', "\"\"");
                        if d.is_empty() {
                            format!("\"{q}\"")
                        } else {
                            format!("\"{q}\" {d}")
                        }
                    })
                    .collect();
                let create = format!(
                    "CREATE TABLE \"{}\" ({})",
                    t.name.replace('"', "\"\""),
                    cols.join(", ")
                );
                newdb.query(&create, &[]).map_err(|_| SQLITE_NOTADB)?;
                let placeholders: Vec<String> =
                    (1..=t.columns.len()).map(|i| format!("${i}")).collect();
                let ins = format!(
                    "INSERT INTO \"{}\" VALUES ({})",
                    t.name.replace('"', "\"\""),
                    placeholders.join(", ")
                );
                let mut err = false;
                f.scan_table(t, &mut |_rowid, vals| {
                    let params: Vec<Value> = vals
                        .into_iter()
                        .map(|v| match v {
                            mpedb_sqlitefmt::Value::Null => Value::Null,
                            mpedb_sqlitefmt::Value::Int(i) => Value::Int(i),
                            mpedb_sqlitefmt::Value::Float(x) => Value::Float(x),
                            mpedb_sqlitefmt::Value::Text(s) => Value::Text(s),
                            mpedb_sqlitefmt::Value::Blob(b) => Value::Blob(b),
                        })
                        .collect();
                    if newdb.query(&ins, &params).is_err() {
                        err = true;
                    }
                    Ok(())
                })
                .map_err(|_| SQLITE_NOTADB)?;
                if err {
                    return Err(SQLITE_NOTADB);
                }
            }
            adopt_reopened(c, newdb);
            let old_path = std::mem::replace(&mut c.path, newpath);
            match std::mem::replace(&mut c.backing, Backing::Ephemeral) {
                Backing::Ephemeral => {
                    let _ = std::fs::remove_file(&old_path);
                }
                Backing::NamedMemory => {
                    if named_memory_release(&old_path) {
                        let _ = std::fs::remove_file(&old_path);
                    }
                }
                Backing::File => {}
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(&staged);
        free_buf();
        return match rc {
            Ok(()) => SQLITE_OK,
            Err(code) => {
                let msg = if code == SQLITE_NOTADB {
                    "file is not a database"
                } else {
                    "could not stage the deserialized image"
                };
                fail(c, code, msg)
            }
        };
    }
    // Bytes that are not an mpedb image at all: sqlite's own words and code
    // (CPython's `test_deserialize_corrupt_database` asserts the message; the
    // error may legally surface here rather than on the first query).
    if !is_database_image(data, sz) {
        let rc = fail(c, SQLITE_NOTADB, "file is not a database");
        free_buf();
        return rc;
    }
    let bytes = std::slice::from_raw_parts(data, sz as usize);
    let tmp = ephemeral_path();
    if std::fs::write(&tmp, bytes).is_err() {
        let rc = fail(c, SQLITE_IOERR, "could not stage the deserialized image");
        free_buf();
        return rc;
    }
    match Database::open_from_file(&tmp) {
        Ok(newdb) => {
            adopt_reopened(c, newdb);
            // Detach from the old backing exactly as a close would have:
            // an ephemeral file is removed, a named-memory refcount drops.
            // A real FILE is left exactly as it was.
            let old_path = std::mem::replace(&mut c.path, tmp);
            match std::mem::replace(&mut c.backing, Backing::Ephemeral) {
                Backing::Ephemeral => {
                    let _ = std::fs::remove_file(&old_path);
                }
                Backing::NamedMemory => {
                    if named_memory_release(&old_path) {
                        let _ = std::fs::remove_file(&old_path);
                    }
                }
                Backing::File => {}
            }
            free_buf();
            SQLITE_OK
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            // An mpedb magic that fails validation is still "not a database
            // we can open" — defer nothing, say it now.
            let rc = fail(c, SQLITE_NOTADB, "file is not a database");
            free_buf();
            rc
        }
    }
}

/// Could `data[..sz]` be a database image? Only a header test — enough to tell
/// "these bytes are not a database" from "mpedb cannot adopt this database".
///
/// mpedb writes its magic at offset 0 of page 0 (`mpedb_core::shm`'s `MAGIC`,
/// `MPEDB` + a format digit). The prefix rather than all 8 bytes, so a future
/// format revision still reads as *a* database rather than as garbage — the
/// refusal that follows is the same either way, only the wording differs.
unsafe fn is_database_image(data: *const c_uchar, sz: c_longlong) -> bool {
    const PREFIX: &[u8] = b"MPEDB";
    if data.is_null() || sz < PREFIX.len() as c_longlong {
        return false;
    }
    std::slice::from_raw_parts(data, PREFIX.len()) == PREFIX
}
