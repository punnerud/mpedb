//! Standalone `VALUES (…), (…), …` as a top-level row-returning statement, the
//! way sqlite treats it: the listed tuples become the result rows, in order,
//! and the columns are named `column1`, `column2`, … . mpedb desugars it at
//! parse time into the equivalent compound `SELECT … UNION ALL SELECT …` of
//! FROM-less SELECTs, so there is no new plan format — but the OBSERVABLE
//! behaviour (column names, row values, row order) must match sqlite 3.45,
//! which is what this file cross-checks.
//!
//! The subquery-source form (`SELECT * FROM (VALUES …)`) is deliberately out of
//! scope: a multi-row VALUES is a compound, which a derived-table body cannot
//! hold. `not_yet_a_subquery_source` pins that as a clean parse refusal rather
//! than a silent wrong answer.

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

/// VALUES reads no table, but a Database still needs a valid schema to open;
/// this one dummy table is never referenced by any query here.
const SCHEMA: &str = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
"#;

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-values-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml =
        format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

/// Canonical, engine-agnostic cell rendering, matched to how the `sqlite3` CLI
/// prints the same value in list mode: NULL empty, booleans as 1/0, floats the
/// way sqlite renders them (an integral float still prints `1.0`).
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        Value::Float(f) => {
            if f == f.trunc() && f.is_finite() {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        other => panic!("unexpected value in VALUES test: {other:?}"),
    }
}

/// mpedb: `(column names, rows-as-strings)` for one query.
fn mpedb_out(db: &Database, sql: &str) -> (Vec<String>, Vec<Vec<String>>) {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { columns, rows } => (
            columns,
            rows.into_iter()
                .map(|r| r.into_iter().map(render).collect())
                .collect(),
        ),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// sqlite 3.45: `(column names, rows-as-strings)` for one query, via the CLI in
/// list mode with headers on. First output line is the header row.
fn sqlite_out(query: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let script = format!("{query};\n");
    let stdout = sqlite_oracle::script_stdout_headers(&script, "");
    let mut lines = stdout.lines();
    let header = lines
        .next()
        .expect("sqlite3 emitted no header line")
        .split('|')
        .map(str::to_string)
        .collect();
    let rows = lines
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect();
    (header, rows)
}

/// The core cross-check: for a battery of standalone VALUES statements, mpedb
/// must produce the same column names, the same rows, and the same order as
/// sqlite 3.45.
#[test]
fn values_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // The canonical two-column, two-row form.
        "VALUES (1, 2), (3, 4)",
        // A single row — a bare SELECT, not a compound.
        "VALUES (1)",
        // A single row, several columns of mixed type.
        "VALUES (1, 'x', 2.5)",
        // Expressions in the tuples, not just literals.
        "VALUES (1 + 1, 'x')",
        // Text and NULL, several rows — order and NULL rendering.
        "VALUES ('a'), ('b'), (NULL), ('c')",
        // Wider tuple to be sure the naming runs past column2.
        "VALUES (1, 2, 3, 4, 5)",
    ];
    for q in queries {
        let (mc, mr) = mpedb_out(&d, q);
        let (sc, sr) = sqlite_out(q);
        assert_eq!(mc, sc, "column names differ on `{q}`");
        assert_eq!(mr, sr, "rows differ on `{q}`");
    }
}

/// The default column names are sqlite's `column1..columnN`, asserted directly
/// rather than only through the CLI cross-check.
#[test]
fn columns_are_named_column1_upwards() {
    let d = db();
    let (cols, rows) = mpedb_out(&d, "VALUES (10, 20, 30)");
    assert_eq!(cols, vec!["column1", "column2", "column3"]);
    assert_eq!(rows, vec![vec!["10", "20", "30"]]);
}

/// A single-row VALUES is planned as a plain SELECT and returns exactly one row.
#[test]
fn single_row_is_one_row() {
    let d = db();
    let (_cols, rows) = mpedb_out(&d, "VALUES (42)");
    assert_eq!(rows, vec![vec!["42"]]);
}

/// Ragged tuples are a parse error, as in sqlite/PG — never silently NULL-padded.
#[test]
fn mismatched_arity_is_rejected() {
    let d = db();
    let err = d.query("VALUES (1, 2), (3)", &[]);
    assert!(err.is_err(), "ragged VALUES should be refused, got {err:?}");
}

/// `VALUES` as a derived-table / subquery source is not supported yet; it must
/// fail cleanly (a parse/compile error), not mis-execute.
#[test]
fn not_yet_a_subquery_source() {
    let d = db();
    let err = d.query("SELECT * FROM (VALUES (1), (2))", &[]);
    assert!(
        err.is_err(),
        "FROM (VALUES …) is out of scope and must be refused, got {err:?}"
    );
}
