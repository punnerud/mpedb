//! `mpedb repl <config.toml>` — line-oriented session, friendly to piped
//! stdin (no prompt unless stdin is a tty). On a tty the line editor is
//! rustyline, so Tab completes dot-commands, SQL keywords, table names and
//! `<table>.<column>` (see [`crate::line`]); piped stdin takes the same plain
//! reader it always did.
//!
//! `BEGIN` opens a [`mpedb::WriteSession`]; statements then run inside it via
//! `session.query` (compiled locally, never published — the facade's
//! self-lock rule). `COMMIT`/`ROLLBACK` close it. `.hash`/`.verify` are
//! refused while a session is open: both may need the writer lock this
//! thread already holds (ERRORCHECK would error out, but the refusal message
//! is clearer).
//!
//! [`run_path`] additionally takes a database that does not exist YET (a
//! [`PendingCreate`] from `mpedb new.mpedb`): it is created by the first SQL
//! statement, not by opening the repl.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mpedb::{Config, Database, WriteSession};
use mpedb_core::Engine;

use crate::line::{LineSource, Names};
use crate::openpath::{is_read_only, scratch_ref, PendingCreate, Scratch};
use crate::render::{print_result, schema_toml};
use crate::util::{usage, CliResult};

/// The dot-commands this repl answers to — also the Tab-completion set.
const DOTS: &[&str] = &[".tables", ".schema", ".hash", ".verify", ".help", ".quit", ".exit"];

pub fn run(argv: &[String]) -> CliResult {
    let [config_path] = argv else {
        return usage("repl needs <config.toml|db.mpedb>");
    };
    run_path(config_path, None)
}

/// The repl on a target that may not EXIST yet (`mpedb new.mpedb` with no
/// statement). `pending` is the database that would be created: sqlite3
/// materializes the file on the first STATEMENT, so this reads lines until one
/// arrives, creates the database then, and only then opens it. The `Database`
/// value literally cannot exist before the create — the session below is
/// unreachable without it, so there is no path that skips materializing.
pub fn run_path(config_path: &str, pending: Option<PendingCreate>) -> CliResult {
    let names = Rc::new(RefCell::new(Names::new(DOTS)));
    let mut input = LineSource::new("mpedb> ", names.clone());

    // Nothing on disk yet: blank lines, dot-commands (`.exit`, `.tables`) and
    // READS create nothing at all — `mpedb new.mpedb` then `SELECT 1` then
    // `.exit` must leave the directory exactly as it was. The first WRITE is the
    // trigger, and it is then run as the session's first statement.
    let mut queued: Option<String> = None;
    if let Some(create) = pending {
        match pending_prelude(&mut input, &create)? {
            None => return Ok(()),
            Some(stmt) => {
                create.materialize()?;
                queued = Some(stmt);
            }
        }
    }

    // Same rule as `open_target`: a .toml is a config, anything else is the
    // database file itself (`mpedb data.db` lands here via its sidecar).
    let (db, db_path) = if Path::new(config_path).extension().is_some_and(|e| e == "toml") {
        let config = Config::from_file(Path::new(config_path))?;
        let db_path = config.options.path.clone();
        (Database::open_with_config(config)?, db_path)
    } else {
        (
            Database::open_from_file(Path::new(config_path))?,
            PathBuf::from(config_path),
        )
    };

    names.borrow_mut().set_schema(&db.schema());
    if input.prompts() {
        println!("mpedb {} — .help for commands, .quit to exit", env!("CARGO_PKG_VERSION"));
    }
    let mut session: Option<WriteSession<'_>> = None;

    loop {
        // The statement that triggered the create, if any, runs first.
        let line = match queued.take() {
            Some(l) => l,
            None => match input.next_line() {
                Some(l) => l?,
                None => break,
            },
        };
        let stmt = line.trim().trim_end_matches(';').trim();
        if stmt.is_empty() {
            continue;
        }

        if let Some(rest) = stmt.strip_prefix('.') {
            // Break (not exit()) so an open session is dropped → rolled back
            // → the writer lock is released cleanly.
            if matches!(rest.trim(), "quit" | "exit") {
                break;
            }
            dot_command(rest, &db, &db_path, session.is_some());
        } else if stmt.eq_ignore_ascii_case("begin") {
            if session.is_some() {
                eprintln!("error: a transaction is already open");
            } else {
                match db.begin() {
                    Ok(s) => {
                        session = Some(s);
                        println!("begin");
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        } else if stmt.eq_ignore_ascii_case("commit") {
            match session.take() {
                None => eprintln!("error: no open transaction"),
                Some(s) => match s.commit() {
                    Ok(()) => println!("commit"),
                    Err(e) => eprintln!("error: {e}"),
                },
            }
        } else if stmt.eq_ignore_ascii_case("rollback") {
            match session.take() {
                None => eprintln!("error: no open transaction"),
                Some(s) => {
                    s.rollback();
                    println!("rollback");
                }
            }
        } else {
            let res = match session.as_mut() {
                Some(s) => s.query(stmt, &[]),
                None => db.query(stmt, &[]),
            };
            match res {
                Ok(r) => {
                    print_result(&r);
                    // Recommend the backtest right where a trigger goes live:
                    // it replays the trigger over the existing rows in an
                    // always-rolled-back txn and reports what it would do.
                    let head = stmt.trim_start().get(..14).unwrap_or("");
                    if head.eq_ignore_ascii_case("create trigger") && input.prompts() {
                        eprintln!(
                            "tip: `mpedb trigger backtest <db> <name>` replays this \
                             trigger over the current rows (always rolled back) and \
                             reports what it would have done"
                        );
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
            // DDL moves the schema under the completer; refresh the snapshot
            // (only when something is actually completing against it).
            if input.prompts() {
                names.borrow_mut().set_schema(&db.schema());
            }
        }
    }

    if session.is_some() {
        eprintln!("warning: open transaction rolled back at end of input");
    }
    Ok(())
}

/// Read lines until one of them is a statement that must CREATE the database.
///
/// Returns that statement (the caller materializes, then runs it as the
/// session's first), or `None` when the input ended without ever needing a
/// database — in which case nothing was written to disk at all. Reads along the
/// way are answered from an empty [`Scratch`], which is indistinguishable from
/// the empty database that would have been created.
fn pending_prelude(
    input: &mut LineSource,
    create: &PendingCreate,
) -> Result<Option<String>, crate::util::Failure> {
    let mut scratch: Option<Scratch> = None;
    loop {
        let Some(line) = input.next_line() else {
            return Ok(None);
        };
        let line = line?;
        let stmt = line.trim().trim_end_matches(';').trim();
        if stmt.is_empty() {
            continue;
        }
        if let Some(dot) = stmt.strip_prefix('.') {
            if matches!(dot.trim(), "quit" | "exit") {
                return Ok(None);
            }
            if dot.trim() == "help" {
                println!(
                    "{} does not exist yet — the first WRITE statement creates it.\n\
                     Until then: read-only SQL runs on a scratch database,\n\
                     .help shows this text, .quit / .exit leave without creating."
                , create.path().display());
                continue;
            }
            eprintln!(
                "error: {} does not exist yet — run a statement to create it",
                create.path().display()
            );
            continue;
        }
        if is_read_only(stmt) {
            if let Err(e) = scratch_ref(&mut scratch)?.run(stmt, &[]) {
                eprintln!("error: {e}");
            }
            continue;
        }
        return Ok(Some(stmt.to_owned()));
    }
}

fn dot_command(cmd: &str, db: &Database, db_path: &Path, in_session: bool) {
    let (name, arg) = match cmd.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (cmd, ""),
    };
    match name {
        "schema" => print!("{}", schema_toml(&db.schema())),
        "tables" => match tables(db_path) {
            Ok(lines) => print!("{lines}"),
            Err(e) => eprintln!("error: {e}"),
        },
        "hash" => {
            if arg.is_empty() {
                eprintln!("usage: .hash <SQL>");
            } else if in_session {
                eprintln!("error: .hash publishes to the registry; not allowed inside a transaction");
            } else {
                match db.prepare(arg) {
                    Ok(h) => println!("{h}"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        "help" => {
            println!(
                "SQL statements run directly; begin/commit/rollback open a transaction.\n\
                 .tables          table names + committed row counts\n\
                 .schema          the live schema as config TOML\n\
                 .hash <SQL>      compile <SQL>, publish, print the plan hash\n\
                 .verify          page-accounting verifier (takes the writer lock)\n\
                 .help            this list\n\
                 .quit / .exit    leave"
            );
        }
        "verify" => {
            if in_session {
                eprintln!("error: .verify needs the writer lock; not allowed inside a transaction");
            } else {
                match db.verify() {
                    Ok(()) => println!("verify: ok"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
        other => eprintln!("error: unknown command .{other} (.help lists commands)"),
    }
}

/// Table names + committed row counts. Uses a second, file-authoritative
/// attach (same as `mpedb dump`): row counts live in the catalog, so this is
/// O(tables), not O(rows) — and reads never contend with the writer lock.
fn tables(db_path: &Path) -> Result<String, mpedb::Error> {
    let eng = Engine::open_from_file(db_path)?;
    let r = eng.begin_read()?;
    let mut out = String::new();
    for (tid, table) in eng.schema().tables.iter().enumerate() {
        out.push_str(&format!("{}\t{}\n", table.name, r.row_count(tid as u32)?));
    }
    r.finish()?;
    Ok(out)
}
