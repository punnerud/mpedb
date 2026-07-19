//! The BUNDLED-sqlite differential oracle.
//!
//! Every differential test in this directory used to shell out to whatever
//! `sqlite3` binary the machine happened to have — so the same commit could
//! pass on one box (3.45.1) and fail on another (3.51.0, different `%f`
//! rounding), and every such failure cost a human judgement ("real bug or
//! version wobble?"). This module replaces the subprocess with the sqlite
//! that is COMPILED IN via the `rusqlite`/`libsqlite3-sys` dev-dependency
//! pinned in Cargo.toml (`features = ["bundled"]` → SQLite 3.45.0): identical
//! on every machine, versioned with the repo, and upgraded only by a
//! deliberate Cargo.toml bump whose behavioural diff is reviewable.
//!
//! The functions here reproduce the `sqlite3 -batch :memory:` list-mode
//! stdout BYTE FOR BYTE, so converted call sites keep their existing parsing
//! (`.lines()`, `split('|')`, empty-line filters) untouched:
//!
//! - one output line per row, columns joined by `|`;
//! - NULL rendered as `nullvalue` (the CLI default is the empty string;
//!   `-nullvalue NULL` style sentinels are the parameter);
//! - INTEGER as decimal; REAL through sqlite's OWN value-to-text conversion
//!   (a `CAST(? AS TEXT)` on the same connection — the code path
//!   `sqlite3_column_text` itself uses, so `1.5` → `1.5`, `1e20` → `1.0e+20`,
//!   `-0.0` → `0.0` exactly as the CLI prints them);
//! - TEXT verbatim; BLOB as its raw bytes (lossily UTF-8, like piping the
//!   CLI's stdout through `String::from_utf8`).
//!
//! Two semantic corrections to match the CLI environment the tests were
//! written against:
//! - `PRAGMA foreign_keys = OFF` on every connection: libsqlite3-sys builds
//!   the bundled library with `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`, but stock
//!   sqlite — and therefore the CLI, and therefore mpedb's dialect contract
//!   (see django_parse_gaps.rs: REFERENCES is parsed, not enforced) — defaults
//!   it OFF.
//! - the math functions (sin/log2/…) exist because `.cargo/config.toml` sets
//!   `LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_MATH_FUNCTIONS`, which the CLI build
//!   has by default and the bare bundled build lacks.
//!
//! NOT covered: the `regexp()` function, which lives in the sqlite SHELL
//! (ext/misc/regexp.c compiled into the CLI), not in the library —
//! `regexp.rs` therefore still drives the real CLI and is the one deliberate
//! exemption.

#![allow(dead_code)] // each test binary uses the subset it needs

use rusqlite::types::ValueRef;
use rusqlite::{Connection, Statement};

/// The version of the compiled-in oracle, e.g. `"3.45.0"`. Changes only with
/// a deliberate rusqlite/libsqlite3-sys bump in Cargo.toml.
pub fn version() -> &'static str {
    rusqlite::version()
}

/// Run a whole `;`-separated script against a fresh in-memory bundled-sqlite
/// connection and return what the `sqlite3 -batch :memory:` CLI would have
/// printed on stdout (list mode, headers off). Panics on the first statement
/// that errors — the moral equivalent of the old
/// `assert!(out.status.success(), …)` after a CLI run.
pub fn script_stdout(script: &str, nullvalue: &str) -> String {
    match run_script(script, nullvalue, false) {
        Ok(out) => out,
        Err(e) => panic!(
            "bundled sqlite ({}) failed: {e}\nscript:\n{script}",
            version()
        ),
    }
}

/// Fail-fast variant (the CLI's `.bail on`): `Ok(stdout)` if every statement
/// succeeded, otherwise `Err(message)` of the FIRST failing statement, with
/// sqlite's own error text (`no such savepoint: nope`, `UNIQUE constraint
/// failed: …`) so callers can assert on it or just on failure itself.
pub fn try_script_stdout(script: &str, nullvalue: &str) -> Result<String, String> {
    run_script(script, nullvalue, false)
}

/// Continue-past-errors variant (the CLI's DEFAULT batch behaviour: a failed
/// statement prints to stderr and the script keeps going). Returns the stdout
/// of the statements that did succeed. Statements that fail to PREPARE
/// (syntax errors) still panic — a harness bug, not a comparable outcome.
pub fn script_stdout_lenient(script: &str, nullvalue: &str) -> String {
    match run_script(script, nullvalue, true) {
        Ok(out) => out,
        Err(e) => panic!(
            "bundled sqlite ({}) could not prepare a statement: {e}\nscript:\n{script}",
            version()
        ),
    }
}

fn run_script(script: &str, nullvalue: &str, lenient: bool) -> Result<String, String> {
    let conn = Connection::open_in_memory().expect("open in-memory bundled sqlite");
    // Stock-sqlite default (see module docs); the bundled build flips it.
    conn.pragma_update(None, "foreign_keys", false)
        .expect("PRAGMA foreign_keys = OFF");
    // sqlite's own REAL→TEXT conversion — the same code path the CLI's
    // sqlite3_column_text output goes through.
    let mut caster = conn
        .prepare("SELECT CAST(?1 AS TEXT)")
        .expect("prepare the REAL→TEXT caster");

    let mut out = String::new();
    let mut batch = rusqlite::Batch::new(&conn, script);
    loop {
        let mut stmt = match batch.next() {
            Ok(Some(stmt)) => stmt,
            Ok(None) => break,
            // A prepare error. Batch cannot advance past it, so this is never
            // continuable — lenient callers get the panic in their wrapper.
            Err(e) => return Err(e.to_string()),
        };
        if let Err(e) = run_stmt(&mut stmt, &mut caster, nullvalue, &mut out) {
            if lenient {
                continue;
            }
            return Err(e.to_string());
        }
    }
    Ok(out)
}

fn run_stmt(
    stmt: &mut Statement,
    caster: &mut Statement,
    nullvalue: &str,
    out: &mut String,
) -> rusqlite::Result<()> {
    if stmt.column_count() == 0 {
        stmt.raw_execute()?;
        return Ok(());
    }
    let ncol = stmt.column_count();
    let mut rows = stmt.raw_query();
    while let Some(row) = rows.next()? {
        for i in 0..ncol {
            if i > 0 {
                out.push('|');
            }
            match row.get_ref(i)? {
                ValueRef::Null => out.push_str(nullvalue),
                ValueRef::Integer(v) => out.push_str(&v.to_string()),
                ValueRef::Real(f) => {
                    let text: String = caster.query_row([f], |r| r.get(0))?;
                    out.push_str(&text);
                }
                ValueRef::Text(t) => out.push_str(&String::from_utf8_lossy(t)),
                ValueRef::Blob(b) => out.push_str(&String::from_utf8_lossy(b)),
            }
        }
        out.push('\n');
    }
    Ok(())
}
