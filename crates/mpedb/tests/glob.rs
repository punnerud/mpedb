//! `x GLOB 'pat'` / `x NOT GLOB 'pat'` — sqlite's case-SENSITIVE pattern match
//! with `*` (any run), `?` (one char) and `[...]` character classes. Modeled on
//! the LIKE implementation, and every case here is cross-checked against the
//! `sqlite3` CLI (3.45), whose `GLOB` operator has the same semantics.
//!
//! The pattern is a literal (as with LIKE in Phase 1); the left operand is a
//! text column that may be NULL, so the NULL-propagation rule (`NULL GLOB p` and
//! `NULL NOT GLOB p` are both NULL) is exercised too.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database, so a panicking test does not leak a `/dev/shm` file.
struct Tmp {
    db: Database,
    path: String,
}
impl Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

/// `id` is the PK; `s` is a nullable text column — the operand GLOB matches
/// against, able to be NULL to exercise the 3VL path.
const SCHEMA: &str = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "s"
  type = "text"
  nullable = true
"#;

/// One seed row: `(id, s)`. The strings deliberately cover case differences,
/// the metacharacters `*`/`?`/`[`/`]`/`-`, spaces, and a NULL, so a query can
/// distinguish literal-vs-wildcard and case-sensitive behavior.
const ROWS: &[(i64, Option<&'static str>)] = &[
    (1, Some("abc")),
    (2, Some("Abc")),         // case: uppercase A
    (3, Some("aXc")),         // middle char varies
    (4, Some("a")),
    (5, Some("abcdef")),
    (6, Some("a*c")),         // a literal '*' in the DATA
    (7, Some("a-c")),         // a literal '-' in the DATA
    (8, Some("]")),           // a lone bracket
    (9, Some("hello world")), // a space
    (10, None),               // NULL → every GLOB/NOT GLOB is NULL
];

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, s)| {
            let t = s.map_or("NULL".to_string(), |x| format!("'{x}'"));
            format!("INSERT INTO t (id, s) VALUES ({id}, {t})")
        })
        .collect()
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-glob-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

/// Canonical, engine-agnostic cell rendering: must match how the `sqlite3` CLI's
/// default "list" mode prints the same value — NULL as empty, a boolean (all
/// GLOB ever produces) as sqlite's 1/0, text verbatim.
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        other => panic!("unexpected value in GLOB test: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.into_iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Run a full script (schema + data + one query) through the `sqlite3` CLI and
/// parse its default list-mode output into rows.
fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT);\n");
    for stmt in insert_statements() {
        script.push_str(&stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the sqlite3 CLI (3.45) must be on PATH for this cross-check");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Default list mode: one row per line, columns joined by '|', NULL empty.
    // Every query below selects `id` first (never NULL), so no wanted row is a
    // blank line.
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// A battery of GLOB / NOT GLOB queries, in the SELECT list and as a WHERE
/// predicate, exercising wildcards, character classes (ranges, negation,
/// literal metachars), case-sensitivity and NULL — each must match sqlite 3.45.
#[test]
fn glob_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // `*` prefix / infix and `?` single-char, in the projection.
        "SELECT id, s GLOB 'a*' FROM t ORDER BY id",
        "SELECT id, s GLOB 'a*c' FROM t ORDER BY id",
        "SELECT id, s GLOB 'a?c' FROM t ORDER BY id",
        // Case-SENSITIVE: 'A*' must NOT match the lowercase rows.
        "SELECT id, s GLOB 'A*' FROM t ORDER BY id",
        // Character classes: range, negation, and a literal `*` via `[*]`.
        "SELECT id, s GLOB '[a-c]*' FROM t ORDER BY id",
        "SELECT id, s GLOB '[^a]*' FROM t ORDER BY id",
        "SELECT id, s GLOB 'a[*]c' FROM t ORDER BY id",
        // A class with a leading literal `-` and a space.
        "SELECT id, s GLOB '*[- ]*' FROM t ORDER BY id",
        // NOT GLOB in the projection (NULL row stays NULL → empty cell).
        "SELECT id, s NOT GLOB 'a*' FROM t ORDER BY id",
        // As a WHERE predicate: NULL denies, so the NULL row drops out.
        "SELECT id FROM t WHERE s GLOB 'a*c' ORDER BY id",
        "SELECT id FROM t WHERE s NOT GLOB 'a*' ORDER BY id",
        "SELECT id FROM t WHERE s GLOB '[a-c]*' ORDER BY id",
        // Combined with other logic.
        "SELECT id FROM t WHERE s GLOB 'a*' AND id < 5 ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// The properties asserted directly on the `Value` (not only via the string
/// cross-check): case-sensitivity, that a NULL operand propagates through both
/// GLOB and NOT GLOB, and that `NOT GLOB` is the exact negation on non-NULL rows.
#[test]
fn glob_null_and_case_direct() {
    let d = db();

    let one = |sql: &str| -> Value {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };

    // Row 1 ("abc") matches 'a*'; row 2 ("Abc") does NOT — GLOB is case-sensitive.
    assert_eq!(one("SELECT s GLOB 'a*' FROM t WHERE id = 1"), Value::Bool(true));
    assert_eq!(one("SELECT s GLOB 'a*' FROM t WHERE id = 2"), Value::Bool(false));
    assert_eq!(one("SELECT s GLOB 'A*' FROM t WHERE id = 2"), Value::Bool(true));

    // `?` needs exactly one char: 'a?c' matches "aXc" but not "a".
    assert_eq!(one("SELECT s GLOB 'a?c' FROM t WHERE id = 3"), Value::Bool(true));
    assert_eq!(one("SELECT s GLOB 'a?c' FROM t WHERE id = 4"), Value::Bool(false));

    // A literal '*' in the data is matched by the class `[*]`, not by `*`.
    assert_eq!(one("SELECT s GLOB 'a[*]c' FROM t WHERE id = 6"), Value::Bool(true));

    // NOT GLOB is the exact negation on a non-NULL row.
    assert_eq!(one("SELECT s NOT GLOB 'a*' FROM t WHERE id = 1"), Value::Bool(false));
    assert_eq!(one("SELECT s NOT GLOB 'a*' FROM t WHERE id = 2"), Value::Bool(true));

    // Row 10 is NULL: both GLOB and NOT GLOB propagate NULL (NOT of NULL is NULL).
    assert_eq!(one("SELECT s GLOB 'a*' FROM t WHERE id = 10"), Value::Null);
    assert_eq!(one("SELECT s NOT GLOB 'a*' FROM t WHERE id = 10"), Value::Null);
}
