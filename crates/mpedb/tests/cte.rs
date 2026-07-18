//! Common Table Expressions (`WITH cte AS (SELECT …) SELECT …`, #CTE). A
//! non-recursive CTE is a statement-scoped named view: it is folded into the
//! view catalog and flattened onto its base at bind time, reusing the view /
//! derived-table machinery (no planner/plan-bytes/executor change). Only simple
//! projection/filter bodies with unqualified outer refs (the view-path limit);
//! RECURSIVE, column-lists and complex bodies are refused. Cross-checked vs
//! sqlite 3.45.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-cte-{}-{}.mpedb",
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
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar_i64(db: &Database, sql: &str) -> i64 {
    match &rows(db.query(sql, &[]).unwrap())[0][0] {
        Value::Int(i) => *i,
        other => panic!("{other:?}"),
    }
}

fn setup(db: &Database) {
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT, c INT)", &[]).unwrap();
    for id in 1..=7 {
        db.query(
            &format!("INSERT INTO t (id, a, b, c) VALUES ({id}, {}, 'r{id}', {})", id, id * 10),
            &[],
        )
        .unwrap();
    }
}

#[test]
fn basic_cte_flattens() {
    let (db, path) = open();
    setup(&db);
    // `WITH c AS (SELECT * FROM t WHERE a>4) SELECT id, a FROM c` → rows a>4.
    let got = rows(db.query(
        "WITH c AS (SELECT * FROM t WHERE a > 4) SELECT id, a FROM c ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(5), Value::Int(5)],
        vec![Value::Int(6), Value::Int(6)],
        vec![Value::Int(7), Value::Int(7)],
    ]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn projection_body_and_outer_filter_merge() {
    let (db, path) = open();
    setup(&db);
    // Bare-column body + an unqualified outer filter that AND-merges.
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 2) SELECT id FROM c WHERE a < 6 ORDER BY id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(5)]]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aggregate_over_cte_and_multiple_ctes() {
    let (db, path) = open();
    setup(&db);
    // The outer may aggregate over the CTE (only the CTE body is constrained).
    let got = rows(db.query(
        "WITH c AS (SELECT * FROM t WHERE a >= 3) SELECT count(*), sum(c) FROM c",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(5), Value::Int(250)]]);

    // Multiple CTEs; only one referenced (unused CTEs are a safe leniency).
    assert_eq!(scalar_i64(
        &db,
        "WITH lo AS (SELECT * FROM t WHERE a < 3), hi AS (SELECT * FROM t WHERE a > 5) SELECT count(*) FROM hi",
    ), 2);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn refusals() {
    let (db, path) = open();
    setup(&db);
    // RECURSIVE is refused.
    assert!(db.query("WITH RECURSIVE c AS (SELECT 1) SELECT * FROM c", &[]).is_err());
    // An explicit column list is refused.
    assert!(db.query("WITH c(x) AS (SELECT a FROM t) SELECT x FROM c", &[]).is_err());
    // A complex (aggregate) body is refused at reference time.
    assert!(db.query("WITH c AS (SELECT count(*) AS n FROM t) SELECT * FROM c", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
