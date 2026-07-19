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

use std::path::{Path, PathBuf};

use crate::render::print_result;
use crate::util::{open_target, parse_param, runtime, usage, CliResult};

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

fn is_sqlite(path: &Path) -> bool {
    use std::io::Read as _;
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

/// The bare-path entry: dispatch on what the file actually is.
pub fn run(path: &str, rest: &[String]) -> CliResult {
    let p = Path::new(path);
    if !p.exists() {
        return runtime(format!(
            "no such file: {path} — `mpedb <config.toml|db.mpedb|sqlite.db>` opens an \
             existing database (create an mpedb one from a config, or a sqlite one \
             with sqlite3)"
        ));
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

/// `mpedb data.db --overlay ['SQL' ...]` — the v2 delta overlay: read-write,
/// zero-copy, deltas + tombstones beside the base, checkpoint on demand.
fn run_overlay(
    p: &Path,
    mode: mpedb::LockMode,
    reconcile: Option<mpedb::ReconcilePolicy>,
    rest: &[String],
) -> CliResult {
    let mut ovl = mpedb::SqliteOverlay::open_with_options(p, mode, reconcile)?;
    match rest {
        [sql, params @ ..] => {
            let vals: Vec<mpedb::Value> = params.iter().map(|s| parse_param(s)).collect();
            print_result(&ovl.query(sql, &vals)?);
            Ok(())
        }
        [] => {
            use std::io::BufRead as _;
            let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                if interactive {
                    eprint!("mpedb(ovl)> ");
                }
                let line = line?;
                let stmt = line.trim().trim_end_matches(';').trim();
                if stmt.is_empty() {
                    continue;
                }
                if stmt == ".quit" || stmt == ".exit" {
                    break;
                }
                if stmt == ".checkpoint" {
                    match ovl.checkpoint() {
                        Ok(r) => println!(
                            "checkpoint: epoch {} pushed ({} upserts, {} deletes), overlay emptied",
                            r.epoch, r.upserts, r.deletes
                        ),
                        Err(e) => eprintln!("error: {e}"),
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
                    match ovl.reconcile(pol) {
                        Ok(r) => println!(
                            "reconcile: {} unchanged, {} ours-kept, {} theirs-dropped",
                            r.unchanged, r.ours, r.theirs
                        ),
                        Err(e) => eprintln!("error: {e}"),
                    }
                    continue;
                }
                match ovl.query(stmt, &[]) {
                    Ok(r) => print_result(&r),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Ok(())
        }
    }
}

/// `mpedb checkpoint <sqlite.db>` — push local writes back into the base.
/// Default: the v0 sidecar via mirror push (one sqlite transaction; conflicts
/// park per DESIGN-MIRROR §8 and are reported, never silently dropped).
/// `--overlay`: the v2 delta overlay's checkpoint (design §5).
pub fn checkpoint(args: &[String]) -> CliResult {
    let mut args: Vec<String> = args.to_vec();
    let overlay = if let Some(i) = args.iter().position(|a| a == "--overlay") {
        args.remove(i);
        true
    } else {
        false
    };
    let [path] = &args[..] else {
        return usage("checkpoint needs <sqlite.db> (the base file, not the sidecar)");
    };
    if overlay {
        let p = Path::new(path);
        if !is_sqlite(p) {
            return runtime(format!("{path} is not a sqlite database"));
        }
        let mut ovl = mpedb::SqliteOverlay::open(p)?;
        let r = ovl.checkpoint()?;
        println!(
            "checkpoint: epoch {} pushed ({} upserts, {} deletes), overlay emptied",
            r.epoch, r.upserts, r.deletes
        );
        return Ok(());
    }
    let p = Path::new(path);
    if !is_sqlite(p) {
        return runtime(format!("{path} is not a sqlite database"));
    }
    let side = sidecar(p);
    if !side.exists() {
        return runtime(format!(
            "no sidecar {} — open the database first (`mpedb {path}`)",
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
