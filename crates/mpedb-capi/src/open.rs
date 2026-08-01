use super::*;

// ===========================================================================
// open / close
// ===========================================================================

/// How a connection's backing file is owned. mpedb always has a file; what
/// differs is whether the CALLER named it (and therefore keeps it) or asked for
/// an in-memory database (and must not find it again afterwards).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Backing {
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

pub(super) fn named_memory_release(path: &std::path::Path) -> bool {
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

pub(super) fn ephemeral_path() -> PathBuf {
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
    // One shared escape (mpedb_types::toml_escape). This used to be an inline
    // pair of `replace` calls here and in openpath.rs, absent in cli/util.rs,
    // and a LOSSY rewrite in the Python binding — four sites, three behaviours,
    // two of them wrong on Windows. #159 found it by running on Windows.
    let p = mpedb::toml_escape(&path.to_string_lossy());
    // Modest max_readers keeps the reader-table pages (and thus high_water for
    // a nearly empty :memory: DB) small — backup progress paces over that.
    format!(
        "[database]\npath = \"{p}\"\nsize_mb = {size_mb}\nmax_readers = 32\n\n\
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

/// Is `lower` (already lowercased) the name of something this connection can
/// CALL — a core scalar or aggregate?
///
/// Used ONLY by the `SQLITE_LIMIT_FUNCTION_ARG` gate (`sql::max_function_args`),
/// where the question is "may I count the parenthesized list after this name as
/// an argument list?". Being conservative here is free: an unrecognized name is
/// simply not counted, and mpedb's binder still rejects a call to a function it
/// does not have. Host registrations are checked separately, against the
/// connection's own list.
pub(super) fn is_callable_name(lower: &str) -> bool {
    const NAMES: &[&str] = &[
        // core scalars
        "abs", "changes", "char", "coalesce", "concat", "concat_ws", "format", "glob", "hex",
        "iif", "ifnull", "instr", "last_insert_rowid", "length", "like", "likelihood", "likely",
        "lower", "ltrim", "max", "min", "nullif", "octet_length", "printf", "quote", "random",
        "randomblob", "replace", "round", "rtrim", "sign", "soundex", "substr", "substring",
        "trim", "typeof", "unhex", "unicode", "unlikely", "upper", "zeroblob",
        // date/time
        "date", "time", "datetime", "julianday", "unixepoch", "strftime", "timediff",
        // json
        "json", "json_array", "json_array_length", "json_error_position", "json_extract",
        "json_insert", "json_object", "json_patch", "json_quote", "json_remove", "json_replace",
        "json_set", "json_type", "json_valid",
        // math
        "acos", "asin", "atan", "atan2", "ceil", "ceiling", "cos", "degrees", "exp", "floor",
        "ln", "log", "log10", "log2", "mod", "pi", "pow", "power", "radians", "sin", "sqrt",
        "tan", "trunc",
        // aggregates + window functions
        "avg", "count", "group_concat", "string_agg", "sum", "total", "cume_dist", "dense_rank",
        "first_value", "last_value", "lead", "nth_value", "ntile", "percent_rank",
        "rank", "row_number", "lag",
    ];
    NAMES.contains(&lower)
}

/// A brand-new, EMPTY database in its own throwaway file, plus that path so the
/// caller can unlink it. Same geometry and bootstrap table as any `:memory:`
/// connection, so it is indistinguishable from one that never had a statement
/// run against it.
///
/// Used by `backup.rs` to answer a backup of the `temp` schema: mpedb has no
/// temp database, and refuses every statement that would put anything in one
/// (`CREATE TEMP TABLE`/`VIEW`/`TRIGGER` all fail to parse), so mpedb's temp
/// schema is provably EMPTY — an empty image is the exact answer, not an
/// approximation of one.
pub(crate) fn open_blank_database() -> Result<(Database, PathBuf), String> {
    let path = ephemeral_path();
    let _ = std::fs::remove_file(&path);
    let mut cfg =
        Config::from_toml_str(&seed_toml(&path, 16)).map_err(|e| format!("config error: {e}"))?;
    cfg.options.path = path.clone();
    match Database::open_with_config(cfg) {
        Ok(db) => Ok((db, path)),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(format!("cannot create `{}`: {e}", path.display()))
        }
    }
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
        // Ephemeral / named-memory used to default to 1 MiB, to make CPython's
        // `test_backup.test_progress` report a small step count. That was a
        // global default tuned to flatter ONE test, and it was measured to cost
        // seven others: a named-memory database died after **7 749** one-row
        // inserts with `database is out of space`, which is what broke Django's
        // whole `delete` label (the six labels in G1 share one test database,
        // and by the time `delete` ran the 1 MiB was gone — running `delete`
        // alone passes 59/59). sqlite grows on demand and never hits this.
        //
        // The tuning bought nothing even for its own test: `test_progress`
        // asserts a page count of exactly 2, and mpedb reported 73 at 1 MiB —
        // it failed then and it fails now. So the default is what a database
        // needs, and the backup page arithmetic is fixed where it lives
        // (DESIGN-CAPI / the S4 position), not by starving every consumer.
        //
        // Callers that want a different reservation still set `file:…?size_mb=N`.
        Target::Ephemeral => (ephemeral_path(), Backing::Ephemeral, req.unwrap_or(64)),
        Target::NamedMemory(p) => (p, Backing::NamedMemory, req.unwrap_or(64)),
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
        echo_pragmas: introspect::EchoPragmas::default(),
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
        unique_rollback_tables: std::collections::HashSet::new(),
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

pub(super) fn last_open_error() -> Option<(c_int, *const c_char)> {
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
/// Point this connection at a REOPENED engine handle, carrying over every
/// piece of per-connection state that lives in the `Database` being replaced:
/// the shim builtins and the caller's UDFs/collations (a reopened `Database`
/// starts with an empty function registry), the busy timeout, and the
/// FK-enforcement pragma — per-CONNECTION state in sqlite, so a backup or a
/// deserialize must not silently reset it to the default.
///
/// Shared by `sqlite3_backup_step`'s install and `sqlite3_deserialize`; a
/// third copy of this list is how one of them would drift.
///
/// # Safety
/// The connection's registered UDF/collation `pApp` pointers must still be
/// valid — true for a live connection (they are freed only on close/replace).
pub(crate) unsafe fn adopt_reopened(c: &mut Sqlite3, newdb: Database) {
    let fk = c.db.fk_enforced();
    c.db = newdb;
    c.db.set_fk_enforced(fk);
    register_shim_builtins(&c.db);
    for h in &c.host_fns {
        h.reinstall(&c.db);
    }
    for h in &c.host_colls {
        h.reinstall(&c.db);
    }
    if c.busy_timeout_ms > 0 {
        c.db.set_busy_timeout(Some(Duration::from_millis(c.busy_timeout_ms as u64)));
    }
}

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

