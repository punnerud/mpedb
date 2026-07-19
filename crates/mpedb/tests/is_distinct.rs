//! General `x IS y` / `x IS NOT y` — NULL-safe "(not) distinct from", the
//! 2-valued cousin of `=`/`<>`. `a IS b` is TRUE when both are NULL, FALSE when
//! exactly one is, else `a = b`; `a IS NOT b` is its negation. It NEVER yields
//! NULL. Every case here is cross-checked against the `sqlite3` CLI (3.45),
//! whose `IS`/`IS NOT` operator has the same semantics.
//!
//! The `IS NULL` / `IS NOT NULL` forms are a separate node and are covered
//! elsewhere; this file exercises the general operand form over int, text and
//! NULL-bearing columns.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

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

/// `id` is the PK; `a`/`b` are nullable ints and `s`/`u` are nullable text —
/// the two column types the operator has to compare, both able to be NULL.
const SCHEMA: &str = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "a"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "b"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "s"
  type = "text"
  nullable = true
  [[table.column]]
  name = "u"
  type = "text"
  nullable = true
"#;

/// One seed row: `(id, a, b, s, u)`.
type Row = (i64, Option<i64>, Option<i64>, Option<&'static str>, Option<&'static str>);

/// Covers every NULL/value pairing that matters: equal, unequal, both-NULL,
/// and one-NULL-each-way on both an int and a text column.
const ROWS: &[Row] = &[
    (1, Some(1), Some(1), Some("x"), Some("x")), // equal / equal
    (2, Some(1), Some(2), Some("x"), Some("y")), // unequal / unequal
    (3, None, None, None, None),                 // both NULL / both NULL
    (4, Some(1), None, Some("x"), None),         // right NULL / right NULL
    (5, None, Some(1), None, Some("x")),         // left NULL / left NULL
];

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, a, b, s, u)| {
            let i = |v: &Option<i64>| v.map_or("NULL".to_string(), |x| x.to_string());
            let t = |v: &Option<&str>| v.map_or("NULL".to_string(), |x| format!("'{x}'"));
            format!(
                "INSERT INTO t (id, a, b, s, u) VALUES ({id}, {}, {}, {}, {})",
                i(a),
                i(b),
                t(s),
                t(u)
            )
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
        "{dir}/mpedb-isdistinct-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml =
        format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

/// Canonical, engine-agnostic cell rendering: it must match how the `sqlite3`
/// CLI's default "list" mode prints the same value — NULL as empty, a boolean
/// (which is all `IS` ever produces) as sqlite's 1/0.
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        other => panic!("unexpected value in IS test: {other:?}"),
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
    let mut script =
        String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, s TEXT, u TEXT);\n");
    for stmt in insert_statements() {
        script.push_str(&stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    let stdout = sqlite_oracle::script_stdout(&script, "");
    // Default list mode: one row per line, columns joined by '|', NULL empty.
    // Every query below selects `id` first (never NULL), so no wanted row is a
    // blank line.
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// The heart of the test: for a battery of queries mixing `IS` / `IS NOT` over
/// int and text columns and literals, mpedb must produce exactly what sqlite
/// 3.45 does.
#[test]
fn is_distinct_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // Column vs column, both types, both directions.
        "SELECT id, a IS b, a IS NOT b, s IS u, s IS NOT u FROM t ORDER BY id",
        // Column vs a non-NULL literal — exercises the NULL-safe path when the
        // column side is NULL (rows 3 and 5).
        "SELECT id, a IS 1, a IS NOT 1, s IS 'x', s IS NOT 'x' FROM t ORDER BY id",
        // As a WHERE predicate: `IS` never yields NULL, so unlike `=` it decides
        // every row — including the all-NULL one (row 3 passes `a IS b`).
        "SELECT id FROM t WHERE a IS b ORDER BY id",
        "SELECT id FROM t WHERE a IS NOT b ORDER BY id",
        "SELECT id FROM t WHERE s IS u ORDER BY id",
        "SELECT id FROM t WHERE s IS NOT u ORDER BY id",
        // Combined with other logic.
        "SELECT id FROM t WHERE a IS NOT b AND id < 5 ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// The property that distinguishes `IS` from `=`: it is TWO-valued and never
/// returns NULL. Asserted directly on the `Value`, not just via the string
/// cross-check, and contrasted with `=` yielding NULL on the same NULL inputs.
#[test]
fn is_is_two_valued_never_null() {
    let d = db();

    let one = |sql: &str| -> Value {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };

    // Row 3: a and b are both NULL. `a IS b` is TRUE (a real Bool), while the
    // 3VL `a = b` is NULL — that gap is the whole reason `IS` exists.
    assert_eq!(one("SELECT a IS b FROM t WHERE id = 3"), Value::Bool(true));
    assert_eq!(one("SELECT a IS NOT b FROM t WHERE id = 3"), Value::Bool(false));
    assert_eq!(one("SELECT a = b FROM t WHERE id = 3"), Value::Null);

    // Row 4: exactly one side NULL. `IS` is FALSE (never NULL); `=` is NULL.
    assert_eq!(one("SELECT a IS b FROM t WHERE id = 4"), Value::Bool(false));
    assert_eq!(one("SELECT a IS NOT b FROM t WHERE id = 4"), Value::Bool(true));
    assert_eq!(one("SELECT a = b FROM t WHERE id = 4"), Value::Null);

    // Row 1: both non-NULL and equal — agrees with `=`.
    assert_eq!(one("SELECT a IS b FROM t WHERE id = 1"), Value::Bool(true));
    assert_eq!(one("SELECT s IS u FROM t WHERE id = 1"), Value::Bool(true));

    // Constant folding still applies to the general form.
    assert_eq!(one("SELECT 1 IS 1 FROM t WHERE id = 1"), Value::Bool(true));
    assert_eq!(one("SELECT 1 IS NOT 2 FROM t WHERE id = 1"), Value::Bool(true));
}
