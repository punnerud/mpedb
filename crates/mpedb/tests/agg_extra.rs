//! `total` and `group_concat` aggregates (COMPAT), each checked against the
//! rules sqlite 3.45 uses: `total` is always a float and 0.0 over an empty or
//! all-NULL group (never NULL, the deliberate contrast with `sum`);
//! `group_concat` joins the non-NULL values' text with `,` in scan order and is
//! NULL over an empty group.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open(name: &str) -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-aggx-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 16

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"

  [[table.column]]
  name = "s"
  type = "text"
"#,
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn row0(res: ExecResult) -> Vec<Value> {
    match res {
        ExecResult::Rows { rows, .. } => rows.into_iter().next().unwrap(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar(db: &Database, sql: &str) -> Value {
    row0(db.query(sql, &[]).unwrap()).into_iter().next().unwrap()
}

#[test]
fn total_is_float_and_zero_over_empty() {
    let (db, path) = open("total");
    // Empty table: total is 0.0 (never NULL), sum is NULL.
    assert_eq!(scalar(&db, "SELECT total(v) FROM t"), Value::Float(0.0));
    assert_eq!(scalar(&db, "SELECT sum(v) FROM t"), Value::Null);

    for (id, v) in [(1, 10), (2, 20), (3, 30)] {
        db.query(&format!("INSERT INTO t (id, v, s) VALUES ({id}, {v}, 'x')"), &[]).unwrap();
    }
    // total sums as a float.
    assert_eq!(scalar(&db, "SELECT total(v) FROM t"), Value::Float(60.0));
    // A NULL row is skipped; total stays a float and never NULL.
    db.query("INSERT INTO t (id, v, s) VALUES (4, NULL, 'y')", &[]).unwrap();
    assert_eq!(scalar(&db, "SELECT total(v) FROM t"), Value::Float(60.0));
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn group_concat_joins_text_and_is_null_over_empty() {
    let (db, path) = open("gc");
    // Empty group → NULL (not an empty string).
    assert_eq!(scalar(&db, "SELECT group_concat(s) FROM t"), Value::Null);

    for (id, s) in [(1, "a"), (2, "b"), (3, "c")] {
        db.query(&format!("INSERT INTO t (id, v, s) VALUES ({id}, {id}, '{s}')"), &[]).unwrap();
    }
    // Scan order is PK order here, so the join is deterministic.
    assert_eq!(scalar(&db, "SELECT group_concat(s) FROM t"), Value::Text("a,b,c".into()));
    // NULLs are skipped, not rendered as the string "NULL".
    db.query("INSERT INTO t (id, v, s) VALUES (4, 4, NULL)", &[]).unwrap();
    assert_eq!(scalar(&db, "SELECT group_concat(s) FROM t"), Value::Text("a,b,c".into()));
    // A two-argument custom separator is refused (v1).
    assert!(db.query("SELECT group_concat(s, '|') FROM t", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn total_and_group_concat_per_group() {
    let (db, path) = open("grouped");
    // Two groups keyed by v%... use s as the group key: 'even'/'odd'.
    for (id, v, s) in [(1, 10, "g1"), (2, 20, "g1"), (3, 100, "g2"), (4, 200, "g2")] {
        db.query(&format!("INSERT INTO t (id, v, s) VALUES ({id}, {v}, '{s}')"), &[]).unwrap();
    }
    let res = db
        .query("SELECT s, total(v), group_concat(s) FROM t GROUP BY s ORDER BY s", &[])
        .unwrap();
    let rows = match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("g1".into()), Value::Float(30.0), Value::Text("g1,g1".into())],
            vec![Value::Text("g2".into()), Value::Float(300.0), Value::Text("g2,g2".into())],
        ]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
