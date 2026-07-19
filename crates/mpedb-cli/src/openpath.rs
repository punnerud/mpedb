//! `mpedb <path>` — the sqlite3-shaped entry (#69 v0). A bare path opens a
//! repl exactly like `sqlite3 data.db` does, and a trailing statement runs
//! one-shot (`mpedb data.db 'SELECT …'`). What the path IS decides the flow:
//!
//! - `.toml` config or `.mpedb` file: open directly (repl/exec as today).
//! - a sqlite database (detected by its 16-byte magic, never by extension):
//!   the sqlite-backed v0 flow — a `<file>.mpedb` SIDECAR mirror, imported on
//!   first open (which also installs mirror's tracked-mode triggers in the
//!   base — v0's named honest edge), incrementally PULLED on every later
//!   open, and pushed back with `mpedb checkpoint <file>`.
//!
//! v0 is deliberately a full-copy mirror with mirror's own authority and
//! conflict rules — not the delta overlay. The overlay, the lock modes, and
//! the stamp machinery are design/DESIGN-SQLITE-BACKED.md v2; this is the
//! one-command UX proving the shape.
//!
//! A MISSING path is CREATED, as `sqlite3 db.db` creates one: `.mpedb` seeds a
//! native database (see [`create_native`]), anything else an empty sqlite base
//! so the file stays readable by every sqlite tool.

use std::path::{Path, PathBuf};

use crate::line::{LineSource, Names};
use crate::render::print_result;
use crate::util::{open_target, parse_param, runtime, usage, CliResult, Failure};

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// The inert table a freshly created `.mpedb` is seeded with: mpedb refuses a
/// schema with no tables, and a config-seeded file is how one comes into
/// existence. `CREATE TABLE` then works normally on the live schema.
const SEED_TABLE: &str = "_mpedb_bootstrap";

fn is_sqlite(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(md) = std::fs::metadata(path) else {
        return false;
    };
    // A ZERO-BYTE file is a valid, empty sqlite database — that is what
    // `sqlite3 new.db` leaves behind when you create nothing, and what the
    // first `CREATE TABLE` materializes. Treat it as one instead of falling
    // through to the mpedb reader, which can only report "not a database".
    if md.len() == 0 {
        return md.is_file();
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut m = [0u8; 16];
    f.read_exact(&mut m).is_ok() && &m == SQLITE_MAGIC
}

/// `app.db` → `app.db.mpedb` — the sidecar keeps the base's FULL name so two
/// bases differing only in extension cannot collide on one sidecar.
fn sidecar(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(".mpedb");
    PathBuf::from(s)
}

fn strs(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Default geometry for a database created by a bare open. mpedb pre-RESERVES
/// its file (it never grows), so this is the size the new file takes on disk —
/// same default the C-API shim picks for a fresh `sqlite3_open`.
const NEW_DB_SIZE_MB: u64 = 64;

/// The TOML that seeds a brand-new native database: geometry plus one inert
/// bootstrap table, because a schema with no tables is refused.
fn seed_toml(path: &Path, size_mb: u64) -> String {
    let p = path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "[database]\npath = \"{p}\"\nsize_mb = {size_mb}\n\n\
         [[table]]\nname = \"{SEED_TABLE}\"\nprimary_key = [\"id\"]\n\n  \
         [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n"
    )
}

/// Create a new NATIVE mpedb database at `path` (the `.mpedb` case).
fn create_native(path: &Path) -> CliResult {
    let cfg = mpedb::Config::from_toml_str(&seed_toml(path, NEW_DB_SIZE_MB))
        .map_err(|e| Failure::Runtime(format!("cannot create {}: {e}", path.display())))?;
    let db = mpedb::Database::open_with_config(cfg)
        .map_err(|e| Failure::Runtime(format!("cannot create {}: {e}", path.display())))?;
    drop(db);
    eprintln!(
        "created {} ({NEW_DB_SIZE_MB} MB mpedb database) — use CREATE TABLE to define it",
        path.display()
    );
    Ok(())
}

/// Create a new EMPTY SQLITE database at `path` (everything that is not
/// `.mpedb`). A 0-byte file is ALSO a legal empty sqlite database — and
/// `is_sqlite` accepts one, so a `touch`ed name works too — but materializing
/// the real header keeps `file`, `sqlite3` and every sniffing tool happy from
/// the first moment. `VACUUM` on a fresh file is exactly that header write and
/// nothing else.
fn create_sqlite_base(path: &Path) -> CliResult {
    let conn = rusqlite::Connection::open(path)
        .map_err(|e| Failure::Runtime(format!("cannot create {}: {e}", path.display())))?;
    conn.execute_batch("VACUUM;")
        .map_err(|e| Failure::Runtime(format!("cannot create {}: {e}", path.display())))?;
    drop(conn);
    eprintln!(
        "created {} (empty sqlite database) — use CREATE TABLE to define it",
        path.display()
    );
    Ok(())
}

/// A missing path: create the database, as `sqlite3 db.db` does. Only the
/// PARENT directory is a hard error — inventing directories is not our call,
/// and neither is inventing a config file.
fn create_missing(path: &str, p: &Path) -> CliResult {
    if let Some(dir) = p.parent() {
        if !dir.as_os_str().is_empty() && !dir.is_dir() {
            return runtime(format!(
                "cannot create {path}: no such directory {}",
                dir.display()
            ));
        }
    }
    let ext = p.extension().unwrap_or_default();
    if ext == "toml" {
        return runtime(format!(
            "no such config file: {path} — a `.toml` describes a database, so there \
             is nothing to create from it; write one, or name a `.mpedb`/`.db` file \
             to have it created"
        ));
    }
    if ext == "mpedb" {
        create_native(p)
    } else {
        create_sqlite_base(p)
    }
}

/// The bare-path entry: dispatch on what the file actually is.
pub fn run(path: &str, rest: &[String]) -> CliResult {
    let p = Path::new(path);
    if !p.exists() {
        create_missing(path, p)?;
    }
    // `--direct`: read-only SQL straight over the sqlite file — the native
    // reader, no sidecar, no import, no sqlite library. Quiescence is the
    // caller's responsibility (or use `--overlay`, which locks).
    let mut rest: Vec<String> = rest.to_vec();
    let direct = if let Some(i) = rest.iter().position(|a| a == "--direct") {
        rest.remove(i);
        true
    } else {
        false
    };
    // `--overlay [--mode locked|optimistic|offline]`: the v2 delta overlay —
    // read-write SQL over the base with only CHANGES in <file>.overlay.mpedb;
    // `.checkpoint` (repl) / `mpedb checkpoint <file> --overlay` pushes them
    // into the base and empties the overlay.
    let overlay = if let Some(i) = rest.iter().position(|a| a == "--overlay") {
        rest.remove(i);
        true
    } else {
        false
    };
    // `--mirror` (alias `--sidecar`): opt OUT of the default overlay into the v0
    // full sidecar mirror — import the whole base into a `<file>.mpedb` and
    // `checkpoint` writes back. Useful for migration / round-trip validation.
    let mirror = if let Some(i) =
        rest.iter().position(|a| a == "--mirror" || a == "--sidecar")
    {
        rest.remove(i);
        true
    } else {
        false
    };
    let mode = if let Some(i) = rest.iter().position(|a| a == "--mode") {
        if i + 1 >= rest.len() {
            return usage("--mode needs a value: locked|optimistic|offline");
        }
        let m = rest.remove(i + 1);
        rest.remove(i);
        match m.as_str() {
            "locked" => mpedb::LockMode::Locked,
            "optimistic" => mpedb::LockMode::Optimistic,
            "offline" => mpedb::LockMode::Offline,
            other => return usage(format!("unknown --mode `{other}`: locked|optimistic|offline")),
        }
    } else {
        mpedb::LockMode::Locked
    };
    // `--reconcile ours|theirs`: when the base moved under unpushed deltas,
    // resolve per-PK conflicts by this policy at open instead of refusing.
    let reconcile = if let Some(i) = rest.iter().position(|a| a == "--reconcile") {
        if i + 1 >= rest.len() {
            return usage("--reconcile needs a value: ours|theirs");
        }
        let m = rest.remove(i + 1);
        rest.remove(i);
        match m.as_str() {
            "ours" => Some(mpedb::ReconcilePolicy::Ours),
            "theirs" => Some(mpedb::ReconcilePolicy::Theirs),
            other => return usage(format!("unknown --reconcile `{other}`: ours|theirs")),
        }
    } else {
        None
    };
    let rest = rest.as_slice();
    if direct {
        if !is_sqlite(p) {
            return runtime(format!("--direct needs a sqlite file, {path} is not one"));
        }
        return run_direct(p, rest);
    }
    // A sqlite `.db` opens as the delta-WAL OVERLAY by DEFAULT: the
    // `<file>.overlay.mpedb` beside the base holds only your changes, reads fall
    // through to the base via the native reader, and `checkpoint` folds them in.
    // `--mirror` chooses the full sidecar import instead; `--overlay` is accepted
    // for back-compat but is now the default.
    if is_sqlite(p) && !mirror {
        return run_overlay(p, mode, reconcile, rest);
    }
    if overlay {
        return runtime(format!("--overlay needs a sqlite file, {path} is not one"));
    }
    let target = if is_sqlite(p) {
        let side = sidecar(p);
        if side.exists() {
            // Later opens: incremental refresh from the base (tracked-mode
            // triggers were installed by the import).
            crate::mirror::run(&strs(&[
                "pull",
                "--source",
                path,
                "--db",
                side.to_str().expect("utf-8 path"),
            ]))?;
        } else {
            println!(
                "first open of {path}: importing into sidecar {} (schema + data + \
                 change tracking; later opens pull incrementally)",
                side.display()
            );
            crate::mirror::run(&strs(&[
                "import",
                "--source",
                path,
                "--dest",
                side.to_str().expect("utf-8 path"),
            ]))?;
        }
        println!(
            "note: local writes stay in the sidecar until `mpedb checkpoint {path}` \
             pushes them back to the sqlite file"
        );
        side.to_string_lossy().into_owned()
    } else {
        path.to_string()
    };

    match rest {
        [] => crate::repl::run(&[target]),
        [sql, params @ ..] => {
            let db = open_target(&target)?;
            let vals: Vec<mpedb::Value> = params.iter().map(|p| parse_param(p)).collect();
            let res = db.query(sql, &vals)?;
            print_result(&res);
            Ok(())
        }
    }
}

/// `mpedb data.db ['SQL' ...]` over a sqlite base — the v2 delta overlay:
/// read-write, zero-copy, deltas + tombstones beside the base, checkpoint on
/// demand. The overlay is opened LAZILY (see [`OverlaySession`]) so a brand-new,
/// table-less base still gives you a session to `CREATE TABLE` in.
fn run_overlay(
    p: &Path,
    mode: mpedb::LockMode,
    reconcile: Option<mpedb::ReconcilePolicy>,
    rest: &[String],
) -> CliResult {
    let mut s = OverlaySession {
        base: p.to_path_buf(),
        mode,
        reconcile,
        handle: None,
    };
    match rest {
        [sql, params @ ..] => {
            let vals: Vec<mpedb::Value> = params.iter().map(|s| parse_param(s)).collect();
            s.exec(sql, &vals)
        }
        [] => s.repl(),
    }
}

/// The sqlite-base session. Two things it does that a bare `SqliteOverlay`
/// cannot:
///
/// - opens the overlay LAZILY, so a base with no tables yet (a just-created
///   one) is a usable session rather than an open error — the overlay's own
///   tables mirror the base's schema, so there is nothing to build until the
///   base HAS a schema;
/// - routes DDL to the BASE (see [`OverlaySession::base_ddl`]): the schema is
///   the base's, exactly as it is for every other sqlite tool.
struct OverlaySession {
    base: PathBuf,
    mode: mpedb::LockMode,
    reconcile: Option<mpedb::ReconcilePolicy>,
    handle: Option<mpedb::SqliteOverlay>,
}

impl OverlaySession {
    /// The open overlay, built on first use.
    fn handle(&mut self) -> Result<&mut mpedb::SqliteOverlay, Failure> {
        if self.handle.is_none() {
            if base_user_tables(&self.base)? == 0 {
                return runtime(format!(
                    "{} has no tables yet — run `CREATE TABLE …` first (DDL goes \
                     straight into the sqlite file)",
                    self.base.display()
                ));
            }
            self.handle = Some(mpedb::SqliteOverlay::open_with_options(
                &self.base,
                self.mode,
                self.reconcile,
            )?);
        }
        Ok(self.handle.as_mut().expect("just opened"))
    }

    /// Run one statement: DDL against the base, everything else against the
    /// merged overlay view.
    fn exec(&mut self, sql: &str, params: &[mpedb::Value]) -> CliResult {
        if is_ddl(sql) {
            return self.base_ddl(sql);
        }
        let res = self.handle()?.query(sql, params)?;
        print_result(&res);
        Ok(())
    }

    /// DDL is the BASE's business: the sqlite file owns the schema, and the
    /// overlay's mpedb tables are derived from it — a changed schema retires
    /// them. So: drop the handle (releasing the base's SHARED lock), FOLD any
    /// unpushed deltas so nothing is lost, remove the now-stale overlay file,
    /// then execute the statement with sqlite itself. The next statement
    /// rebuilds an overlay against the new schema.
    fn base_ddl(&mut self, sql: &str) -> CliResult {
        self.handle = None;
        let ovl = overlay_file(&self.base);
        if ovl.exists() {
            let mut o = mpedb::SqliteOverlay::open(&self.base)?;
            let r = o.checkpoint()?;
            drop(o);
            if r.upserts + r.deletes > 0 {
                println!(
                    "checkpoint before DDL: epoch {} pushed ({} upserts, {} deletes)",
                    r.epoch, r.upserts, r.deletes
                );
            }
            std::fs::remove_file(&ovl)?;
        }
        let conn = rusqlite::Connection::open(&self.base)
            .map_err(|e| Failure::Runtime(format!("open {}: {e}", self.base.display())))?;
        conn.execute_batch(sql)
            .map_err(|e| Failure::Runtime(e.to_string()))?;
        Ok(())
    }

    /// Refresh the completer's table/column snapshot from whatever the base
    /// currently is. Never fails the session: a base we cannot attach yet just
    /// completes nothing.
    fn refresh_names(&mut self, names: &std::rc::Rc<std::cell::RefCell<Names>>) {
        if self.handle.is_none() && base_user_tables(&self.base).unwrap_or(0) == 0 {
            names.borrow_mut().tables.clear();
            return;
        }
        if let Ok(h) = self.handle() {
            let schema = h.schema().clone();
            names.borrow_mut().set_schema(&schema);
        }
    }

    /// The interactive/piped session over a sqlite base.
    fn repl(&mut self) -> CliResult {
        use std::cell::RefCell;
        use std::rc::Rc;

        const DOTS: &[&str] = &[".checkpoint", ".reconcile", ".quit", ".exit"];
        let names = Rc::new(RefCell::new(Names::new(DOTS)));
        let mut input = LineSource::new("mpedb(ovl)> ", names.clone());
        if input.prompts() {
            self.refresh_names(&names);
        }
        while let Some(line) = input.next_line() {
            let line = line?;
            let stmt = line.trim().trim_end_matches(';').trim();
            if stmt.is_empty() {
                continue;
            }
            if stmt == ".quit" || stmt == ".exit" {
                break;
            }
            if stmt == ".checkpoint" {
                match self.handle().and_then(|h| h.checkpoint().map_err(Failure::from)) {
                    Ok(r) => println!(
                        "checkpoint: epoch {} pushed ({} upserts, {} deletes), overlay emptied",
                        r.epoch, r.upserts, r.deletes
                    ),
                    Err(e) => eprintln!("error: {}", failure_msg(&e)),
                }
                continue;
            }
            if let Some(pol) = stmt.strip_prefix(".reconcile") {
                let pol = match pol.trim() {
                    "ours" => mpedb::ReconcilePolicy::Ours,
                    "theirs" => mpedb::ReconcilePolicy::Theirs,
                    other => {
                        eprintln!("usage: .reconcile ours|theirs (got `{other}`)");
                        continue;
                    }
                };
                match self.handle().and_then(|h| h.reconcile(pol).map_err(Failure::from)) {
                    Ok(r) => println!(
                        "reconcile: {} unchanged, {} ours-kept, {} theirs-dropped",
                        r.unchanged, r.ours, r.theirs
                    ),
                    Err(e) => eprintln!("error: {}", failure_msg(&e)),
                }
                continue;
            }
            if let Err(e) = self.exec(stmt, &[]) {
                eprintln!("error: {}", failure_msg(&e));
            }
            if input.prompts() {
                self.refresh_names(&names);
            }
        }
        Ok(())
    }
}

fn failure_msg(f: &Failure) -> &str {
    match f {
        Failure::Usage(m) | Failure::Runtime(m) => m,
    }
}

/// Does this statement change the sqlite base's SCHEMA? Lexical on purpose —
/// the decision is which engine runs it, made before either parses it.
fn is_ddl(sql: &str) -> bool {
    let word = sql
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("");
    ["CREATE", "DROP", "ALTER"]
        .iter()
        .any(|k| word.eq_ignore_ascii_case(k))
}

/// User tables in the sqlite base (0 for a fresh, empty database).
fn base_user_tables(base: &Path) -> Result<i64, Failure> {
    use rusqlite::OpenFlags;
    let conn = rusqlite::Connection::open_with_flags(
        base,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| Failure::Runtime(format!("open {}: {e}", base.display())))?;
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )
    .map_err(|e| Failure::Runtime(format!("read {}: {e}", base.display())))
}

/// `mpedb checkpoint <sqlite.db>` — push local writes back into the base.
/// Default: the v0 sidecar via mirror push (one sqlite transaction; conflicts
/// park per DESIGN-MIRROR §8 and are reported, never silently dropped).
/// `--overlay`: the v2 delta overlay's checkpoint (design §5).
pub fn checkpoint(args: &[String]) -> CliResult {
    let mut args: Vec<String> = args.to_vec();
    let force_overlay = pop_flag(&mut args, &["--overlay"]);
    let force_mirror = pop_flag(&mut args, &["--mirror", "--sidecar"]);
    let [path] = &args[..] else {
        return usage("checkpoint needs <sqlite.db> (the base file, not the sidecar)");
    };
    let p = Path::new(path);
    if !is_sqlite(p) {
        return runtime(format!("{path} is not a sqlite database"));
    }
    let side = sidecar(p); // <db>.mpedb (v0 full sidecar)
    let ovl = overlay_file(p); // <db>.overlay.mpedb (default delta overlay)
    // Match the OPEN default: fold the OVERLAY delta unless a `--mirror` sidecar
    // is what exists (or was asked for). `--overlay` forces the overlay.
    let use_overlay = force_overlay || (!force_mirror && (ovl.exists() || !side.exists()));
    if use_overlay {
        let mut o = mpedb::SqliteOverlay::open(p)?;
        let r = o.checkpoint()?;
        println!(
            "checkpoint: epoch {} pushed ({} upserts, {} deletes), overlay emptied",
            r.epoch, r.upserts, r.deletes
        );
        return Ok(());
    }
    if !side.exists() {
        return runtime(format!(
            "no sidecar {} — open the database first (`mpedb {path} --mirror`)",
            side.display()
        ));
    }
    crate::mirror::run(&strs(&[
        "push",
        "--source",
        path,
        "--db",
        side.to_str().expect("utf-8 path"),
    ]))
}

/// Remove the first occurrence of any of `names` from `args`; returns whether
/// one was present.
fn pop_flag(args: &mut Vec<String>, names: &[&str]) -> bool {
    if let Some(i) = args.iter().position(|a| names.contains(&a.as_str())) {
        args.remove(i);
        true
    } else {
        false
    }
}

/// `<base>.overlay.mpedb` — the default delta overlay's file (mirrors
/// `mpedb::sqlite_overlay::overlay_path`).
fn overlay_file(base: &Path) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(".overlay.mpedb");
    PathBuf::from(s)
}

/// `mpedb data.db --direct ['SQL' ...]` — read-only, zero-import attach.
/// One-shot with a statement; a minimal line repl without one (read-only, so
/// no BEGIN/COMMIT — just statements and .quit).
fn run_direct(p: &Path, rest: &[String]) -> CliResult {
    let at = mpedb::SqliteAttach::open(p)?;
    for (t, why) in at.skipped() {
        eprintln!("note: table `{t}` not attached: {why}");
    }
    match rest {
        [sql, params @ ..] => {
            let vals: Vec<mpedb::Value> = params.iter().map(|s| parse_param(s)).collect();
            print_result(&at.query(sql, &vals)?);
            Ok(())
        }
        [] => {
            use std::io::BufRead as _;
            let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                if interactive {
                    // The prompt says what this session IS.
                    eprint!("mpedb(ro)> ");
                }
                let line = line?;
                let stmt = line.trim().trim_end_matches(';').trim();
                if stmt.is_empty() {
                    continue;
                }
                if stmt == ".quit" || stmt == ".exit" {
                    break;
                }
                match at.query(stmt, &[]) {
                    Ok(r) => print_result(&r),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Ok(())
        }
    }
}
