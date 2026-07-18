//! `x REGEXP 'pat'` / `x NOT REGEXP 'pat'` — sqlite's bundled `ext/misc/regexp.c`
//! matcher: case-SENSITIVE, unanchored substring match with `.`, `* + ?`,
//! `{p,q}`, `[...]`, `^`/`$`, `|`, `(...)`, `\d`/`\w`/`\s`/`\b` and escapes.
//! Modeled on the GLOB integration test, and every case here is cross-checked
//! against the `sqlite3` CLI (3.45), whose `REGEXP` operator ships that engine.
//!
//! The pattern is a literal (as with LIKE/GLOB in Phase 1); the left operand is
//! a text column that may be NULL, so the NULL-propagation rule (`NULL REGEXP p`
//! and `NULL NOT REGEXP p` are both NULL) is exercised too.

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

/// `id` is the PK; `s` is a nullable text column the operand REGEXP matches
/// against. The strings deliberately cover case, digits, spaces, a literal `.`
/// and `-` in the DATA, an empty string, and a NULL, so a query can distinguish
/// anchors, classes, escapes, case-sensitivity and the 3VL NULL path.
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

/// One seed row: `(id, s)`.
const ROWS: &[(i64, Option<&'static str>)] = &[
    (1, Some("abc")),
    (2, Some("Abc")),         // case: uppercase A
    (3, Some("aXc")),         // middle char varies
    (4, Some("a1c")),         // an embedded digit
    (5, Some("a.c")),         // a literal '.' in the DATA
    (6, Some("hello world")), // a space + word boundary
    (7, Some("123")),         // all digits
    (8, Some("a-c")),         // a literal '-' in the DATA
    (9, Some("foobar")),      // no boundary before "bar"
    (10, Some("foo bar")),    // a boundary before "bar"
    (11, Some("")),           // empty string
    (12, None),               // NULL → every REGEXP/NOT REGEXP is NULL
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
        "{dir}/mpedb-regexp-{}-{}.mpedb",
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
/// REGEXP ever produces) as sqlite's 1/0, text verbatim.
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        other => panic!("unexpected value in REGEXP test: {other:?}"),
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

/// A battery of REGEXP / NOT REGEXP queries, in the SELECT list and as a WHERE
/// predicate, exercising anchors, `.`, quantifiers, counts, classes,
/// alternation, the Perl escapes and `\b`, escaped metacharacters,
/// case-sensitivity and NULL — each must match sqlite 3.45.
#[test]
fn regexp_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // Unanchored substring, then `^`/`$` anchors in the projection.
        "SELECT id, s REGEXP 'bc' FROM t ORDER BY id",
        "SELECT id, s REGEXP '^a' FROM t ORDER BY id",
        "SELECT id, s REGEXP 'c$' FROM t ORDER BY id",
        // `.` = any one char; a `.*` run; a `{p}` count.
        "SELECT id, s REGEXP '^a.c$' FROM t ORDER BY id",
        "SELECT id, s REGEXP '^a.*c$' FROM t ORDER BY id",
        "SELECT id, s REGEXP '^.{3}$' FROM t ORDER BY id",
        // Character classes: range, and a negated class with a space.
        "SELECT id, s REGEXP '^[a-c]+$' FROM t ORDER BY id",
        "SELECT id, s REGEXP '[^a-z ]' FROM t ORDER BY id",
        // Alternation + grouping.
        "SELECT id, s REGEXP '^(abc|123)$' FROM t ORDER BY id",
        // Perl classes and word boundary.
        "SELECT id, s REGEXP '\\d' FROM t ORDER BY id",
        "SELECT id, s REGEXP '\\bworld\\b' FROM t ORDER BY id",
        "SELECT id, s REGEXP '\\bbar' FROM t ORDER BY id",
        // An escaped metacharacter: literal `.` matches only the "a.c" row.
        "SELECT id, s REGEXP '^a\\.c$' FROM t ORDER BY id",
        // Case-SENSITIVE: '^abc$' must NOT match the "Abc" row.
        "SELECT id, s REGEXP '^abc$' FROM t ORDER BY id",
        // NOT REGEXP in the projection (NULL row stays NULL → empty cell).
        "SELECT id, s NOT REGEXP '^a' FROM t ORDER BY id",
        // As a WHERE predicate: NULL denies, so the NULL row drops out.
        "SELECT id FROM t WHERE s REGEXP '^a.c$' ORDER BY id",
        "SELECT id FROM t WHERE s NOT REGEXP '\\d' ORDER BY id",
        "SELECT id FROM t WHERE s REGEXP '^[a-c]+$' ORDER BY id",
        // Combined with other logic.
        "SELECT id FROM t WHERE s REGEXP 'a' AND id < 6 ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// The properties asserted directly on the `Value` (not only via the string
/// cross-check): case-sensitivity, unanchored substring matching, that a NULL
/// operand propagates through both REGEXP and NOT REGEXP, and that `NOT REGEXP`
/// is the exact negation on non-NULL rows.
#[test]
fn regexp_null_and_case_direct() {
    let d = db();

    let one = |sql: &str| -> Value {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };

    // Case-SENSITIVE: "abc" matches '^abc$'; "Abc" does not.
    assert_eq!(one("SELECT s REGEXP '^abc$' FROM t WHERE id = 1"), Value::Bool(true));
    assert_eq!(one("SELECT s REGEXP '^abc$' FROM t WHERE id = 2"), Value::Bool(false));

    // Unanchored: 'bar' matches anywhere inside "foobar".
    assert_eq!(one("SELECT s REGEXP 'bar' FROM t WHERE id = 9"), Value::Bool(true));
    // `\b` requires a boundary: "foobar" has none before "bar", "foo bar" does.
    assert_eq!(one("SELECT s REGEXP '\\bbar' FROM t WHERE id = 9"), Value::Bool(false));
    assert_eq!(one("SELECT s REGEXP '\\bbar' FROM t WHERE id = 10"), Value::Bool(true));

    // An escaped `.` is a literal dot: matches "a.c", not "aXc".
    assert_eq!(one("SELECT s REGEXP '^a\\.c$' FROM t WHERE id = 5"), Value::Bool(true));
    assert_eq!(one("SELECT s REGEXP '^a\\.c$' FROM t WHERE id = 3"), Value::Bool(false));
    // An UNescaped `.` is any char: matches "aXc".
    assert_eq!(one("SELECT s REGEXP '^a.c$' FROM t WHERE id = 3"), Value::Bool(true));

    // NOT REGEXP is the exact negation on a non-NULL row.
    assert_eq!(one("SELECT s NOT REGEXP '^a' FROM t WHERE id = 1"), Value::Bool(false));
    assert_eq!(one("SELECT s NOT REGEXP '^a' FROM t WHERE id = 6"), Value::Bool(true));

    // Row 12 is NULL: both REGEXP and NOT REGEXP propagate NULL (NOT of NULL is
    // NULL), exactly as GLOB/LIKE do.
    assert_eq!(one("SELECT s REGEXP '^a' FROM t WHERE id = 12"), Value::Null);
    assert_eq!(one("SELECT s NOT REGEXP '^a' FROM t WHERE id = 12"), Value::Null);
}
