//! Division / modulo by zero, cross-checked against the real `sqlite3` CLI
//! 3.45. sqlite yields NULL (not an error) for both integer and real division
//! by zero and for modulo by zero; mpedb matches it. Normal division is
//! unaffected: integer `/` truncates toward zero, `%` is the integer
//! remainder, and a real operand makes the whole expression real.
//!
//! The check is differential: the SAME SQL runs against mpedb and against an
//! in-memory sqlite, cell-by-cell. Direct `assert_eq!`s pin the exact enumerated
//! cases too, so the intent is explicit even if the CLI is absent.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// One row with a non-NULL and a NULL for both an int and a float column, so a
/// zero divisor can be built from a literal or from a row value.
const ROWS: &[&str] = &[
    "INSERT INTO t (id, a, f) VALUES (1, 5, 5.5)",
    "INSERT INTO t (id, a, f) VALUES (2, NULL, NULL)",
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
        "mpedb-divzero-{}-{}.mpedb",
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
  name = "a"
  type = "int64"
  nullable = true

  [[table.column]]
  name = "f"
  type = "float64"
  nullable = true
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
    let mut s = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, f REAL);\n");
    for r in ROWS {
        s.push_str(r);
        s.push_str(";\n");
    }
    s
}

/// Every result cell (row-major) mpedb returns for `query`.
fn mpedb_cells(db: &Database, query: &str) -> Vec<Value> {
    match db.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows.into_iter().flatten().collect(),
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

/// Every result cell (row-major) sqlite returns for `query`, as rendered text.
/// `with_table` prepends the shared table so table queries have data.
fn sqlite_cells(query: &str, with_table: bool) -> Vec<String> {
    let mut input = if with_table { sqlite_setup() } else { String::new() };
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
    // Each row prints its columns pipe-separated (only single-column queries
    // are used here, so a line is a cell).
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .flat_map(|l| l.split('|').map(str::to_string).collect::<Vec<_>>())
        .collect()
}

/// Does one mpedb value agree with sqlite's rendered cell?
fn value_matches(m: &Value, s: &str) -> bool {
    match m {
        Value::Null => s == "NULL",
        // sqlite renders booleans as 1 / 0 integers.
        Value::Bool(b) => s == if *b { "1" } else { "0" },
        Value::Int(i) => s
            .parse::<i64>()
            .map(|y| y == *i)
            .unwrap_or_else(|_| s.parse::<f64>().map(|y| y == *i as f64).unwrap_or(false)),
        Value::Float(x) => match s.parse::<f64>() {
            Ok(y) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
            Err(_) => false,
        },
        other => panic!("unexpected value type: {other:?}"),
    }
}

/// The same query must give the same cells in both engines.
fn cross_check(db: &Database, query: &str, with_table: bool) {
    let m = mpedb_cells(db, query);
    let s = sqlite_cells(query, with_table);
    assert_eq!(
        m.len(),
        s.len(),
        "cell count differs for `{query}`: mpedb {m:?} vs sqlite {s:?}"
    );
    for (mv, sv) in m.iter().zip(&s) {
        assert!(
            value_matches(mv, sv),
            "mismatch on `{query}`: mpedb {mv:?} vs sqlite `{sv}`"
        );
    }
}

/// A single FROM-less scalar result.
fn scalar(db: &Database, sql: &str) -> Value {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { mut rows, .. } => {
            assert_eq!(rows.len(), 1, "expected one row for `{sql}`");
            rows.swap_remove(0).swap_remove(0)
        }
        other => panic!("expected rows for `{sql}`, got {other:?}"),
    }
}

#[test]
fn div_zero_matches_sqlite_differentially() {
    if !sqlite_available() {
        eprintln!("skipping: sqlite3 CLI not found");
        return;
    }
    let (db, path) = mpedb_db();

    // FROM-less scalars: every division / modulo by zero is NULL, in integer
    // and in real forms, and normal arithmetic is unaffected.
    for sql in [
        "SELECT 1 / 0",
        "SELECT 1.0 / 0",
        "SELECT 1 / 0.0",
        "SELECT 0 / 0",
        "SELECT -5 / 0",
        "SELECT 5 % 0",
        "SELECT 5.5 % 0",
        "SELECT 1 / NULL",
        "SELECT NULL / 0",
        "SELECT NULL % 0",
        // normal division / modulo, unaffected:
        "SELECT 7 / 2",
        "SELECT -7 / 2",
        "SELECT 7.0 / 2",
        "SELECT 7 % 3",
        "SELECT -7 % 3",
        "SELECT 1 / 0 IS NULL",
        "SELECT 7 / 2 IS NULL",
    ] {
        cross_check(&db, sql, false);
    }

    // Over a table: a zero divisor in a projection, from a literal and from a
    // row value; and normal division of a row value.
    cross_check(&db, "SELECT a / 0 FROM t ORDER BY id", true);
    cross_check(&db, "SELECT a % 0 FROM t ORDER BY id", true);
    cross_check(&db, "SELECT f / 0 FROM t ORDER BY id", true);
    cross_check(&db, "SELECT a / (a - a) FROM t ORDER BY id", true);
    cross_check(&db, "SELECT a / 2 FROM t ORDER BY id", true);

    // In a WHERE clause: `1/0 IS NULL` is always TRUE (every row survives);
    // `1/0 = 1` is UNKNOWN (no row survives).
    cross_check(&db, "SELECT id FROM t WHERE 1 / 0 IS NULL ORDER BY id", true);
    cross_check(&db, "SELECT id FROM t WHERE 1 / 0 = 1 ORDER BY id", true);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn div_zero_is_null_not_an_error() {
    let (db, path) = mpedb_db();

    // Integer division / modulo by zero -> NULL (the enumerated cases).
    assert_eq!(scalar(&db, "SELECT 1 / 0"), Value::Null);
    assert_eq!(scalar(&db, "SELECT 0 / 0"), Value::Null);
    assert_eq!(scalar(&db, "SELECT -5 / 0"), Value::Null);
    assert_eq!(scalar(&db, "SELECT 5 % 0"), Value::Null);
    // Real division / modulo by zero -> NULL.
    assert_eq!(scalar(&db, "SELECT 1.0 / 0"), Value::Null);
    assert_eq!(scalar(&db, "SELECT 1 / 0.0"), Value::Null);
    assert_eq!(scalar(&db, "SELECT 5.5 % 0"), Value::Null);
    // NULL operands are already NULL via 3VL.
    assert_eq!(scalar(&db, "SELECT 1 / NULL"), Value::Null);
    assert_eq!(scalar(&db, "SELECT NULL / 0"), Value::Null);

    // Normal arithmetic is unchanged: integer `/` truncates toward zero, `%`
    // is the integer remainder, a real operand makes the result real.
    assert_eq!(scalar(&db, "SELECT 7 / 2"), Value::Int(3));
    assert_eq!(scalar(&db, "SELECT -7 / 2"), Value::Int(-3));
    assert_eq!(scalar(&db, "SELECT 7 % 3"), Value::Int(1));
    assert_eq!(scalar(&db, "SELECT -7 % 3"), Value::Int(-1));
    assert_eq!(scalar(&db, "SELECT 7.0 / 2"), Value::Float(3.5));

    // A zero divisor built from a row value at runtime is still NULL, never an
    // error, so the whole statement succeeds.
    assert_eq!(
        mpedb_cells(&db, "SELECT a / (a - a) FROM t ORDER BY id"),
        vec![Value::Null, Value::Null]
    );

    // In a WHERE clause: IS NULL keeps every row; `= 1` (UNKNOWN) keeps none.
    assert_eq!(
        mpedb_cells(&db, "SELECT id FROM t WHERE 1 / 0 IS NULL ORDER BY id"),
        vec![Value::Int(1), Value::Int(2)]
    );
    assert_eq!(
        mpedb_cells(&db, "SELECT id FROM t WHERE 1 / 0 = 1 ORDER BY id"),
        Vec::<Value>::new()
    );

    // Overflow, by contrast, still raises — the change is specific to a zero
    // divisor, not "arithmetic never errors".
    assert!(db.query("SELECT 9223372036854775807 + 1", &[]).is_err());

    let _ = std::fs::remove_file(&path);
}
