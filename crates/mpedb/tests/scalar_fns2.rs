//! Second batch of scalar functions — `char`, `unicode`, `hex`, `typeof`, the
//! two-argument `trim(x, y)`, and the control-flow `iif(c, a, b)` — each value
//! cross-checked against the real `sqlite3` CLI over a shared table.
//!
//! The comparison is differential: the SAME `SELECT … FROM t ORDER BY id`
//! string runs against mpedb and against an in-memory sqlite loaded with the
//! same rows, and the rendered result vectors must match row for row. sqlite is
//! driven with `-nullvalue NULL` so NULL is unambiguous, which is exactly how
//! `render` prints an mpedb NULL.
//!
//! One deliberate divergence is asserted directly rather than cross-checked:
//! mpedb propagates NULL through `char()` (its uniform scalar rule), whereas
//! sqlite reads a NULL argument as code point 0.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// The rows both engines are loaded with. `s` is text (with an empty and a NULL
/// case), `c` is an integer code point (with a NULL case).
const ROWS: &[&str] = &[
    "INSERT INTO t (id, s, c) VALUES (1, 'Hello', 72)",
    "INSERT INTO t (id, s, c) VALUES (2, 'æøå', 233)",
    "INSERT INTO t (id, s, c) VALUES (3, '', 65)",
    "INSERT INTO t (id, s, c) VALUES (4, NULL, NULL)",
];

fn sqlite_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn mpedb_db() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-scalar2-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
max_readers = 8

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "s"
  type = "text"

  [[table.column]]
  name = "c"
  type = "int64"
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for r in ROWS {
        db.query(r, &[]).unwrap();
    }
    (db, path)
}

/// The sqlite schema + data, as a script for the `sqlite3` CLI.
fn sqlite_setup() -> String {
    let mut s = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, c INTEGER);\n");
    for r in ROWS {
        s.push_str(r);
        s.push_str(";\n");
    }
    s
}

/// Canonical rendering of one mpedb value, aligned with sqlite's
/// `-nullvalue NULL` list output (NULL → "NULL", ints decimal, text verbatim).
fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn mpedb_rows(db: &Database, query: &str) -> Vec<String> {
    match db.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows.iter().map(|r| render(&r[0])).collect(),
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<String> {
    let mut input = sqlite_setup();
    input.push_str(query);
    input.push_str(";\n");
    let mut child = Command::new("sqlite3")
        .args(["-batch", "-noheader", "-nullvalue", "NULL", ":memory:"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sqlite3");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write to sqlite3");
    let out = child.wait_with_output().expect("wait sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 failed for `{query}`: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// The same query must give the same rows in both engines.
fn cross_check(db: &Database, query: &str) {
    let m = mpedb_rows(db, query);
    let s = sqlite_rows(query);
    assert_eq!(m, s, "mpedb vs sqlite disagree on `{query}`");
}

#[test]
fn new_scalar_fns_match_sqlite_over_a_table() {
    if !sqlite_available() {
        eprintln!("skipping: sqlite3 CLI not found");
        return;
    }
    let (db, path) = mpedb_db();

    // hex(x): uppercase hex of the UTF-8 bytes; '' → ''. The NULL row is
    // excluded — mpedb propagates NULL, sqlite returns '' (asserted below).
    cross_check(&db, "SELECT hex(s) FROM t WHERE s IS NOT NULL ORDER BY id");
    // unicode(x): first char's code point; '' → NULL; NULL → NULL.
    cross_check(&db, "SELECT unicode(s) FROM t ORDER BY id");
    // typeof(x): datatype name — over a text column and an integer column,
    // including the NULL row ('null' on both).
    cross_check(&db, "SELECT typeof(s) FROM t ORDER BY id");
    cross_check(&db, "SELECT typeof(c) FROM t ORDER BY id");
    // trim(x): whitespace by default (padded so there is something to strip).
    cross_check(&db, "SELECT trim('  ' || s || '  ') FROM t ORDER BY id");
    // trim(x, set): strip a set of characters from BOTH ends.
    cross_check(&db, "SELECT trim('xx' || s || 'xx', 'x') FROM t ORDER BY id");
    // char(...): code points → string (non-NULL rows only — the NULL case is
    // the documented divergence, asserted separately below).
    cross_check(&db, "SELECT char(c) FROM t WHERE c IS NOT NULL ORDER BY id");
    cross_check(&db, "SELECT char(c, 33) FROM t WHERE c IS NOT NULL ORDER BY id");
    // iif(c, a, b): control flow == CASE WHEN c THEN a ELSE b END.
    cross_check(&db, "SELECT iif(id > 2, 'big', 'small') FROM t ORDER BY id");

    // Documented deviations: mpedb propagates NULL through both char() and
    // hex() (the uniform scalar rule), whereas sqlite reads a NULL char()
    // argument as code point 0 and returns '' for hex(NULL). Asserted directly.
    let null_row = |sql: &str| match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(null_row("SELECT char(c) FROM t WHERE id = 4"), Value::Null);
    assert_eq!(null_row("SELECT hex(s) FROM t WHERE id = 4"), Value::Null);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn iif_condition_is_truthy_tested_and_hex_rejects_numbers() {
    let (db, path) = mpedb_db();
    // iif's condition is a CASE WHEN, so it is truthy-tested exactly as sqlite
    // does (Django gap #5): a bare number, a text value and NULL all follow
    // `sqlite3VdbeBooleanValue`. Diffed against the sqlite3 binary.
    if sqlite_available() {
        for q in [
            "SELECT iif(id, 'a', 'b') FROM t",
            "SELECT iif(c, 'a', 'b') FROM t",
            "SELECT iif(s, 'a', 'b') FROM t",
            "SELECT iif(id - 1, 'a', 'b') FROM t",
        ] {
            cross_check(&db, q);
        }
    }
    // hex accepts text/blob only; an integer argument is a compile error
    // rather than sqlite's render-to-text-then-hex.
    assert!(db.query("SELECT hex(c) FROM t", &[]).is_err());
    // char's arguments must be integer code points.
    assert!(db.query("SELECT char(s) FROM t", &[]).is_err());
    // unicode/typeof arity is checked at compile time.
    assert!(db.query("SELECT unicode('a', 'b') FROM t", &[]).is_err());
    assert!(db.query("SELECT typeof() FROM t", &[]).is_err());
    let _ = std::fs::remove_file(&path);
}
