//! `mpedb repl <config.toml>` — line-oriented session, friendly to piped
//! stdin (no prompt unless stdin is a tty).
//!
//! `BEGIN` opens a [`mpedb::WriteSession`]; statements then run inside it via
//! `session.query` (compiled locally, never published — the facade's
//! self-lock rule). `COMMIT`/`ROLLBACK` close it. `.hash`/`.verify` are
//! refused while a session is open: both may need the writer lock this
//! thread already holds (ERRORCHECK would error out, but the refusal message
//! is clearer).

use std::io::{BufRead, Write as _};
use std::path::Path;

use mpedb::{Config, Database, WriteSession};
use mpedb_core::Engine;

use crate::render::{print_result, schema_toml};
use crate::util::{usage, CliResult};

pub fn run(argv: &[String]) -> CliResult {
    let [config_path] = argv else {
        return usage("repl needs <config.toml>");
    };
    let config = Config::from_file(Path::new(config_path))?;
    let db_path = config.options.path.clone();
    let db = Database::open_with_config(config)?;

    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut session: Option<WriteSession<'_>> = None;

    loop {
        if interactive {
            print!("mpedb> ");
            let _ = std::io::stdout().flush();
        }
        let Some(line) = lines.next() else { break };
        let line = line?;
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
                Ok(r) => print_result(&r),
                Err(e) => eprintln!("error: {e}"),
            }
        }
    }

    if session.is_some() {
        eprintln!("warning: open transaction rolled back at end of input");
    }
    Ok(())
}

fn dot_command(cmd: &str, db: &Database, db_path: &Path, in_session: bool) {
    let (name, arg) = match cmd.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (cmd, ""),
    };
    match name {
        "schema" => print!("{}", schema_toml(db.schema())),
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
        other => eprintln!("error: unknown command .{other} (try .tables .schema .hash .verify .quit)"),
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
