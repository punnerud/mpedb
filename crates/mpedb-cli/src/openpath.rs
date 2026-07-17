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
//! the stamp machinery are DESIGN-SQLITE-BACKED.md v2; this is the
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

/// `mpedb checkpoint <sqlite.db>` — push the sidecar's local writes back into
/// the base through mirror push (one sqlite transaction; conflicts park per
/// DESIGN-MIRROR §8 and are reported, never silently dropped).
pub fn checkpoint(args: &[String]) -> CliResult {
    let [path] = args else {
        return usage("checkpoint needs <sqlite.db> (the base file, not the sidecar)");
    };
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
