use super::*;

use std::path::Path;

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
    // 32 was chosen when this only ever backed a scratch `:memory:` database,
    // where a small reader table keeps `high_water` (and so backup progress)
    // tight. It is the wrong number for a FILE: the geometry is frozen into
    // it at creation, and a PHP-FPM pool with more than 32 concurrent read
    // transactions gets `ReadersFull` — a refusal with no way out but
    // rebuilding the file. `MPEDB_MAX_READERS` overrides it; the default
    // follows the engine's own (1024) for anything on disk.
    let max_readers = std::env::var("MPEDB_MAX_READERS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| (1..=65536).contains(n))
        .unwrap_or(1024);
    format!(
        "[database]\npath = \"{p}\"\nsize_mb = {size_mb}\nmax_readers = {max_readers}\n\n\
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
/// The 16 bytes every sqlite file starts with.
/// One mpedb value as the sqlite value it should become on the way out.
///
/// mpedb's type set is wider than sqlite's five storage classes, so a few
/// variants have to CHOOSE a representation. Each choice below is the one that
/// survives a round trip back through the importer:
///
/// * `Bool` → integer 0/1, which is how sqlite itself stores booleans.
/// * `Timestamp`/`Date`/`Time` → their integer counts (microseconds, days,
///   microseconds), the same numbers mpedb holds. Rendering them as text would
///   pick a format sqlite has no opinion about and mpedb would not read back.
/// * `Numeric` → its canonical text. An exact decimal has no lossless sqlite
///   number: REAL would round it, and INTEGER cannot hold the fraction.
///
/// A `List` is refused by name. It is a parameter-only value that never
/// belongs to a stored row, so meeting one here means something upstream is
/// wrong and quietly writing anything would hide it.
fn to_sqlite_value(v: mpedb::Value) -> Result<mpedb_sqlitefmt::Value, String> {
    use mpedb_sqlitefmt::Value as S;
    Ok(match v {
        mpedb::Value::Null => S::Null,
        mpedb::Value::Int(i) => S::Int(i),
        mpedb::Value::Float(f) => S::Float(f),
        mpedb::Value::Bool(b) => S::Int(b as i64),
        mpedb::Value::Text(t) => S::Text(t),
        mpedb::Value::Blob(b) => S::Blob(b),
        mpedb::Value::Timestamp(i) | mpedb::Value::Date(i) | mpedb::Value::Time(i) => S::Int(i),
        mpedb::Value::Numeric(n) => S::Text(n),
        other => {
            return Err(format!(
                "a {other:?} cannot be stored in a sqlite file (it is a parameter-only value)"
            ))
        }
    })
}

/// Write the sidecar's current contents back over the sqlite file it came
/// from — the C-API's `mpedb checkpoint`.
///
/// The whole database is re-serialized, not a delta. Without mpedb-mirror
/// (which a library exporting `sqlite3_*` can never link, since it pulls in a
/// real sqlite through rusqlite) there is no change log to apply, so there is
/// nothing finer to push than the full picture. That is honest but expensive:
/// cost scales with the database, not with what changed.
///
/// The image goes to a temporary file in the SAME directory and is renamed
/// over the original, so a reader either sees the whole old file or the whole
/// new one. A failure part-way leaves the original untouched.
///
/// # What a checkpoint does NOT carry back
///
/// **Indexes.** The writer emits table b-trees only, so a source that had
/// indexes comes back without them. Measured on the 944 457-row track
/// database: three indexes in, none out, and the file fell from 148 MB to
/// 87 MB. The data is complete and every query still answers correctly — it
/// answers by scanning. Re-create them with `CREATE INDEX` against the
/// checkpointed file, or keep the sidecar as the working copy.
///
/// This is the one place the shim knowingly returns less than it was given,
/// so it is stated here rather than discovered from a file that got faster to
/// write and slower to read. Writing index trees needs the index b-tree cell
/// forms (0x0a / 0x02) AND a key ordering that matches sqlite's collation
/// exactly — an index sqlite believes is sorted but is not would be a silent
/// wrong answer, which is worse than an absent one.
///
/// Returns the number of rows written.
pub(crate) fn checkpoint_to_sqlite(c: &mut Sqlite3) -> Result<u64, String> {
    let Some(src) = c.sqlite_source.clone() else {
        return Err("this database was not opened from a sqlite file — nothing to check point \
                    back into"
            .into());
    };

    let _ = c.db.refresh_schema_if_stale();
    let bundle = c.db.schema();
    let mut tables: Vec<mpedb_sqlitefmt::ImageTable> = Vec::new();
    let mut total = 0u64;

    for t in bundle.tables.iter().filter(|t| !t.dead && t.name != crate::SEED_TABLE) {
        // The CREATE text is rebuilt from the live schema rather than kept
        // from the import: the sidecar is writable, so the schema now is the
        // truth, and a remembered string would go stale the first time someone
        // ran DDL against it.
        // The VISIBLE columns only. A table imported without a primary key
        // gets one synthesized — a trailing hidden `rowid` — and it must not
        // reach the sqlite schema: sqlite maintains its own rowid, and the
        // rows here come from `SELECT *`, which already expands over the
        // visible columns alone. Writing it produced a phantom third column
        // whose every value was NULL. That was wrong before NOT NULL was
        // carried across as well; the flag only made it audible, as four
        // `NULL value in p.rowid` lines from `PRAGMA integrity_check`.
        let visible = t.visible_columns();
        let cols: Vec<String> = visible
            .iter()
            .map(|col| {
                // NOT NULL goes back out too, for the same reason the import
                // brings it in: it is what lets a planner — sqlite's or the
                // next mpedb to open the file — use a composite index. A round
                // trip that silently widened every column to nullable would
                // leave the file correct and slower on every subsequent read.
                let q = col.name.replace('"', "\"\"");
                let nn = if col.nullable { "" } else { " NOT NULL" };
                match col.decltype() {
                    Some(d) if !d.is_empty() => format!("\"{q}\" {d}{nn}"),
                    _ => format!("\"{q}\"{nn}"),
                }
            })
            .collect();
        let sql =
            format!("CREATE TABLE \"{}\" ({})", t.name.replace('"', "\"\""), cols.join(", "));

        // Indexes come from the live schema too. A partial index is refused
        // by name rather than written whole: an index sqlite believes covers
        // the table but only holds part of it would answer with missing rows.
        let mut indexes = Vec::new();
        for ix in &t.indexes {
            let Some(name) = ix.name.clone() else {
                // Derived from a column flag; it never had a name to write a
                // CREATE INDEX with, and sqlite would name its own differently.
                continue;
            };
            if ix.predicate.is_some() {
                return Err(format!(
                    "index `{name}` on `{}` is a partial index — this writer cannot carry \
                     the WHERE clause, and writing it whole would claim rows it does not hold",
                    t.name
                ));
            }
            // Resolved against the VISIBLE columns for the same reason: an
            // index keyed on the hidden rowid would name a column the written
            // schema does not have, and its ordinals would not address the
            // rows either.
            let cols: Vec<String> = ix
                .columns
                .iter()
                .filter_map(|c| visible.get(*c as usize))
                .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
                .collect();
            if cols.len() != ix.columns.len() {
                return Err(format!(
                    "index `{name}` on `{}`: a key column is not in the written schema \
                     (the synthesized rowid is not carried across)",
                    t.name
                ));
            }
            let unique = if ix.unique { "UNIQUE " } else { "" };
            indexes.push(mpedb_sqlitefmt::ImageIndex {
                sql: format!(
                    "CREATE {unique}INDEX \"{}\" ON \"{}\" ({})",
                    name.replace('"', "\"\""),
                    t.name.replace('"', "\"\""),
                    cols.join(", ")
                ),
                name,
                columns: ix.columns.iter().map(|c| *c as usize).collect(),
            });
        }

        let quoted = t.name.replace('"', "\"\"");
        let rows = match c.db.query(&format!("SELECT * FROM \"{quoted}\""), &[]) {
            Ok(mpedb::ExecResult::Rows { rows, .. }) => rows,
            Ok(_) => Vec::new(),
            Err(e) => return Err(format!("reading `{}`: {e}", t.name)),
        };
        total += rows.len() as u64;
        let rows = rows
            .into_iter()
            .map(|r| r.into_iter().map(to_sqlite_value).collect::<Result<Vec<_>, _>>())
            .collect::<Result<Vec<_>, String>>()
            .map_err(|e| format!("table `{}`: {e}", t.name))?;
        tables.push(mpedb_sqlitefmt::ImageTable { name: t.name.clone(), sql, rows, indexes });
    }

    let img = mpedb_sqlitefmt::write_image(&tables, 4096)
        .map_err(|e| format!("building the sqlite image: {e}"))?;

    let staging = {
        let mut p = src.as_os_str().to_os_string();
        p.push(".checkpointing");
        PathBuf::from(p)
    };
    std::fs::write(&staging, &img).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        format!("writing `{}`: {e}", staging.display())
    })?;
    std::fs::rename(&staging, &src).map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        format!("replacing `{}`: {e}", src.display())
    })?;

    // The sidecar is now OLDER than the source it just produced, which is
    // exactly the condition that triggers a re-import on the next open. Touch
    // it forward so a checkpoint does not cost a full re-import afterwards.
    let now = std::time::SystemTime::now();
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&c.path) {
        let _ = f.set_modified(now);
    }
    // The checkpoint just REWROTE the source, so its length is almost
    // certainly not the one the sidecar was stamped with. Restamp, or the very
    // next open re-imports the file this sidecar just produced.
    stamp_source(&src, &c.path);
    Ok(total)
}

/// The `.mpedb` that mirrors a sqlite file, and the import that fills it.
///
/// Returns the sidecar path, ready to open. It is rebuilt when the sqlite file
/// is newer than it — the coarse check, since without mirror's triggers there
/// is nothing finer to go on — and reused untouched otherwise. A rebuild is a
/// full re-import, so an edit to the source costs one import on the next open,
/// not on every statement.
///
/// Is the sidecar usable as-is for this source?
///
/// mtime ALONE is not enough, and the failure is silent. `cp -p`, `rsync -t`
/// and every restore-from-backup preserve the source's timestamp, so a
/// database put back in place looks OLDER than a sidecar built from the
/// database it replaced — and the stale sidecar keeps being served, with no
/// error and no hint.
///
/// So the stamp is the source's LENGTH plus its 100-byte sqlite HEADER, which
/// carries the page size, the page count, the change counter and the schema
/// cookie. A rebuilt database differs in at least one of those essentially
/// always, and reading 100 bytes costs nothing against the import it guards.
///
/// It remains a heuristic, and the residual gap is stated rather than papered
/// over: a replacement with the same length AND the same header differs only
/// in page CONTENT, and finding that needs a full hash of 142 MB on every
/// open — more expensive than the import. `a_same_size_replacement_is_not_seen`
/// pins that limit so nobody assumes more than is here.
///
/// `MPEDB_SIDECAR_STAMP=off` skips the check for a caller that manages the
/// sidecar itself — the nightly-build case, where it is produced deliberately
/// and nothing should re-import it.
fn sidecar_is_fresh(src: &Path, side: &Path) -> bool {
    if !side.exists() {
        return false;
    }
    if std::env::var("MPEDB_SIDECAR_STAMP").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return true;
    }
    let (Ok(sm), Ok(dm)) = (std::fs::metadata(src), std::fs::metadata(side)) else {
        return false;
    };
    let (Ok(st), Ok(dt)) = (sm.modified(), dm.modified()) else {
        return false; // a clock that will not answer: import
    };
    if dt < st {
        return false;
    }
    // The source's length as it was at import, stamped beside the sidecar.
    // Absent (a sidecar from before this existed) means unknown, and unknown
    // means fresh-by-mtime, exactly as before — no forced re-import of every
    // sidecar in existence.
    let stamp = stamp_path(side);
    match std::fs::read_to_string(&stamp) {
        Ok(t) => t.trim() == source_stamp(src, sm.len()),
        // No stamp: a sidecar from before this existed. Unknown means
        // fresh-by-mtime, exactly as before — no forced re-import of every
        // sidecar already on disk.
        Err(_) => true,
    }
}

/// `<length>:<hex of the 100-byte header>` — the source's identity, cheaply.
fn source_stamp(src: &Path, len: u64) -> String {
    use std::io::Read;
    let mut head = [0u8; 100];
    let n = std::fs::File::open(src).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    let mut out = format!("{len}:");
    for b in &head[..n] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn stamp_path(side: &Path) -> PathBuf {
    let mut p = side.as_os_str().to_os_string();
    p.push(".src");
    PathBuf::from(p)
}

/// An advisory `flock` held for the length of one import.
///
/// Best-effort by design: a filesystem that cannot lock (NFS without a lock
/// daemon — which is what `/home` is on the deployment this was written for)
/// returns an error, and the import proceeds unserialized rather than
/// refusing to open the database. That is the pre-existing behaviour, so the
/// lock can only make things better, never worse.
struct ImportLock(Option<std::fs::File>);

impl ImportLock {
    fn acquire(path: &Path) -> Self {
        let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)
        else {
            return Self(None);
        };
        // Blocking: the point is that the waiter finds the finished sidecar,
        // not that it races the winner.
        let rc = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&f), libc::LOCK_EX) };
        if rc != 0 {
            return Self(None);
        }
        Self(Some(f))
    }
}

impl Drop for ImportLock {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            unsafe {
                libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(f), libc::LOCK_UN);
            }
        }
    }
}

/// The import goes to a temporary name and is renamed into place at the end,
/// so an interrupted one leaves no half-filled sidecar for the next open to
/// mistake for a complete one.
fn sqlite_sidecar(src: &Path) -> Result<PathBuf, String> {
    let mut side = src.as_os_str().to_os_string();
    side.push(".mpedb");
    let side = PathBuf::from(side);

    if sidecar_is_fresh(src, &side) {
        return Ok(side);
    }

    // SERIALIZE the import. Without this every concurrent open that finds the
    // sidecar stale starts its own — N full imports of the same file, all
    // writing the SAME staging path, and the losers hand a real user
    // `SQLITE_CANTOPEN`. After a nightly rebuild that is every request at once.
    //
    // The lock is a separate file, taken for the whole check-and-build: a
    // waiter that gets the lock re-checks freshness and finds the winner's
    // sidecar already in place, so it pays the wait and not the import.
    let lock_path = {
        let mut p = side.as_os_str().to_os_string();
        p.push(".lock");
        PathBuf::from(p)
    };
    let _guard = ImportLock::acquire(&lock_path);
    if sidecar_is_fresh(src, &side) {
        return Ok(side);
    }

    // Per-process staging name. Two processes that somehow reach here at once
    // (an unlockable filesystem — NFS without a lock daemon) must not write
    // the same bytes: better two imports than one corrupt file.
    let staging = {
        let mut p = side.as_os_str().to_os_string();
        p.push(format!(".importing.{}", std::process::id()));
        PathBuf::from(p)
    };
    let _ = std::fs::remove_file(&staging);
    // Size the sidecar from the source. mpedb preallocates, so this has to be
    // decided up front: 4x the sqlite file plus 32 MiB of slack, since mpedb's
    // per-row layout is not sqlite's and an import that runs out of space
    // fails the open rather than growing.
    //
    // The 4x is not slack that costs nothing. This comment used to say the
    // file is sparse until written and that overshooting costs address space
    // rather than disk; that is FALSE below 2 GiB, where the reservation is
    // zero-filled outright. Measured on the 142 MB track database: a 597 MB
    // sidecar, 597 MB actually allocated, of which 96 % holds real data — so
    // the multiplier is roughly right for the data and the file genuinely
    // costs what it says. Overshooting costs disk, and on a tmpfs staging
    // directory it costs RAM.
    let src_mb = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0) / (1024 * 1024);
    let size_mb = (src_mb * 4 + 32).max(16);
    let (db, tmp) = open_blank_database_sized(size_mb).map_err(|e| {
        format!("unable to open database file: cannot stage `{}`: {e}", side.display())
    })?;
    let out = import_sqlite_file(src, &db).map_err(|e| {
        format!("unable to open database file: cannot import `{}`: {e}", src.display())
    });
    drop(db);
    if let Err(e) = out {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // The staged database was created wherever ephemeral files live, which is
    // routinely a different filesystem from the sqlite file (/tmp or /dev/shm
    // vs the data directory). rename cannot cross that boundary — EXDEV — so
    // fall back to a copy, still landing on the final name through a rename
    // within the destination directory so no partial sidecar is ever visible.
    let placed = std::fs::rename(&tmp, &staging).or_else(|e| {
        if e.raw_os_error() == Some(libc::EXDEV) {
            std::fs::copy(&tmp, &staging).map(|_| ())
        } else {
            Err(e)
        }
    });
    let placed = placed.and_then(|()| std::fs::rename(&staging, &side));
    let _ = std::fs::remove_file(&tmp);
    placed.map_err(|e| {
        let _ = std::fs::remove_file(&staging);
        format!("unable to open database file: cannot place `{}`: {e}", side.display())
    })?;
    // Stamp the source length this sidecar was built from — the second opinion
    // `sidecar_is_fresh` needs when a restore preserves mtime. Written AFTER
    // the rename, so a stamp never claims a sidecar that is not there; a
    // failure to write it only costs the next open its mtime-only reading,
    // which is where this started.
    stamp_source(src, &side);
    Ok(side)
}

/// Record the source's length beside the sidecar. Best effort.
fn stamp_source(src: &Path, side: &Path) {
    if let Ok(m) = std::fs::metadata(src) {
        let _ = std::fs::write(stamp_path(side), source_stamp(src, m.len()));
    }
}

pub(crate) const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Does `path` hold a sqlite database? Decided by the magic, never by the
/// extension — a sqlite file is routinely called `.db`, `.sqlite` or nothing
/// at all, and an mpedb file is just as often called `.db`.
pub(crate) fn is_sqlite_file(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut m = [0u8; 16];
    f.read_exact(&mut m).is_ok() && &m == SQLITE_MAGIC
}

/// Rows per import transaction. `mpedb-mirror`'s number, and for its reason:
/// big enough that the per-commit cost disappears, small enough that the COW
/// pages one transaction holds do not.
const IMPORT_BATCH: usize = 4096;

/// One transaction's worth of rows, committed and cleared.
fn flush_batch(dest: &Database, ins: &str, rows: &mut Vec<Vec<Value>>) -> Result<u64, String> {
    if rows.is_empty() {
        return Ok(0);
    }
    let n = rows.len() as u64;
    let mut s = dest.begin().map_err(|e| format!("{e}"))?;
    s.query_many(ins, rows).map_err(|e| format!("{e}"))?;
    s.commit().map_err(|e| format!("{e}"))?;
    rows.clear();
    Ok(n)
}

/// Read a sqlite database with the native reader and re-create it inside an
/// already-open mpedb database: each table declared, then its rows inserted.
///
/// The reader is `mpedb-sqlitefmt`, pure Rust. That is not a preference — this
/// crate EXPORTS the `sqlite3_*` symbols, so it can never link a real sqlite
/// (nor `mpedb-mirror`, which pulls one in through rusqlite). Whatever this
/// shim knows about the sqlite file format, it knows on its own.
pub(crate) fn import_sqlite_file(src: &Path, dest: &Database) -> Result<u64, String> {
    let f = mpedb_sqlitefmt::SqliteFile::open(src).map_err(|e| format!("{e}"))?;
    let tables = f.tables().map_err(|e| format!("{e}"))?;
    let mut rows = 0u64;
    for t in &tables {
        // NOT NULL is carried across, and it is not cosmetic. mpedb's planner
        // reads declared nullability: a composite index whose trailing column
        // is nullable cannot serve a range over the leading one — a row with a
        // NULL in that column has no index entry at all, so the probe would
        // miss rows the range covers (`planner/access.rs`, `suffix_not_null`).
        //
        // Dropping the flag here therefore turned an indexed lookup into a
        // full scan, silently. Measured on a 945 234-row track table:
        // `(lat, lon)` declared NOT NULL in the source, discarded on import,
        // and the area query fell to FullScan.
        //
        // The reader already parses it (`sqlitefmt::Table::not_null`), for the
        // same reason in reverse: dropping it let the overlay accept a row
        // sqlite refuses.
        let cols: Vec<String> = t
            .columns
            .iter()
            .enumerate()
            .zip(&t.decl_types)
            .map(|((i, n), d)| {
                let q = n.replace('"', "\"\"");
                let nn = if t.not_null.get(i).copied().unwrap_or(false) { " NOT NULL" } else { "" };
                if d.is_empty() {
                    format!("\"{q}\"{nn}")
                } else {
                    format!("\"{q}\" {d}{nn}")
                }
            })
            .collect();
        let create =
            format!("CREATE TABLE \"{}\" ({})", t.name.replace('"', "\"\""), cols.join(", "));
        dest.query(&create, &[]).map_err(|e| format!("{e}"))?;
        let placeholders: Vec<String> = (1..=t.columns.len()).map(|i| format!("${i}")).collect();
        let ins = format!(
            "INSERT INTO \"{}\" VALUES ({})",
            t.name.replace('"', "\"\""),
            placeholders.join(", ")
        );
        // BATCHED, not a row at a time. `dest.query()` is autocommit: it takes
        // the writer lock, applies one row and flips the meta page — 945 234
        // times for the track table, which is where the 15 seconds went. One
        // transaction per BATCH rows amortises all three, and `query_many`
        // ("executemany's engine half") compiles the INSERT once for the whole
        // batch instead of once per row.
        //
        // Per batch rather than one transaction for the table: a single
        // transaction over a million rows holds every COW page it touches
        // until commit, which is a hard `DbFull` on a sidecar sized for the
        // DATA. 4096 is `mpedb-mirror`'s number, for the same reason.
        let mut failed = None;
        let mut pending: Vec<Vec<Value>> = Vec::with_capacity(IMPORT_BATCH);
        f.scan_table(t, &mut |_rowid, vals| {
            if failed.is_some() {
                return Ok(()); // drain the scan; the first error is the one to report
            }
            pending.push(
                vals.into_iter()
                    .map(|v| match v {
                        mpedb_sqlitefmt::Value::Null => Value::Null,
                        mpedb_sqlitefmt::Value::Int(i) => Value::Int(i),
                        mpedb_sqlitefmt::Value::Float(x) => Value::Float(x),
                        mpedb_sqlitefmt::Value::Text(s) => Value::Text(s),
                        mpedb_sqlitefmt::Value::Blob(b) => Value::Blob(b),
                    })
                    .collect(),
            );
            if pending.len() >= IMPORT_BATCH {
                match flush_batch(dest, &ins, &mut pending) {
                    Ok(n) => rows += n,
                    Err(e) => failed = Some(e),
                }
            }
            Ok(())
        })
        .map_err(|e| format!("{e}"))?;
        if failed.is_none() {
            match flush_batch(dest, &ins, &mut pending) {
                Ok(n) => rows += n,
                Err(e) => failed = Some(e),
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }
    }

    // Indexes LAST, after the rows: building one index over a filled table is
    // one sort, where maintaining it across every insert is a b-tree update
    // per row. Skipping them entirely was measured on the 944 457-row track
    // database — the query fell back to a full scan and took 469 ms against
    // sqlite's 122 ms, and the import is pointless if what comes out cannot be
    // queried at the same speed.
    //
    // A failed index is NOT fatal. The data is already correct and queryable;
    // an index sqlite accepts but mpedb's parser does not (a partial index, an
    // expression index) should cost speed, not the whole open.
    for (_name, sql) in f.indexes().map_err(|e| format!("{e}"))? {
        let _ = dest.query(&sql, &[]);
    }
    Ok(rows)
}

pub(crate) fn open_blank_database() -> Result<(Database, PathBuf), String> {
    open_blank_database_sized(16)
}

/// A blank database of a chosen size in MiB.
///
/// mpedb files are preallocated, so the size has to be decided before the
/// first row goes in — too small and the import stops with "database is out of
/// space" partway through, which is what a fixed 16 MiB did to a 148 MB sqlite
/// file.
pub(crate) fn open_blank_database_sized(size_mb: u64) -> Result<(Database, PathBuf), String> {
    let path = ephemeral_path();
    let _ = std::fs::remove_file(&path);
    let mut cfg = Config::from_toml_str(&seed_toml(&path, size_mb))
        .map_err(|e| format!("config error: {e}"))?;
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
    let (mut path, kind, size_mb) = match target {
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
    let mut exists = match kind {
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
    // A SQLITE file opened by path. mpedb's own reader cannot make sense of
    // one — it would report "database file is not initialized (READY marker
    // absent)", which is true and useless — so the file is imported into a
    // SIDECAR `<path>.mpedb` and that is what gets opened. Same shape the CLI
    // uses for `mpedb data.db`, with one difference forced by this crate: the
    // CLI keeps the sidecar in step through mpedb-mirror's change tracking,
    // and mirror links a real sqlite through rusqlite, which a library that
    // EXPORTS sqlite3_* can never do. So the sidecar here is refreshed by
    // re-import whenever the source is newer, and never pushed back.
    //
    // What that means for a caller, stated plainly because it is the sharp
    // edge: reads see the sqlite data, writes land in the sidecar, and the
    // sqlite file itself is left exactly as it was found.
    let mut sqlite_source = None;
    if matches!(kind, Backing::File) && exists && is_sqlite_file(&path) {
        match sqlite_sidecar(&path) {
            Ok(side) => {
                sqlite_source = Some(std::mem::replace(&mut path, side));
                exists = true;
            }
            Err(msg) => {
                if matches!(kind, Backing::NamedMemory) {
                    named_memory_release(&path);
                }
                return Err((SQLITE_CANTOPEN, msg));
            }
        }
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
        sqlite_source,
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

