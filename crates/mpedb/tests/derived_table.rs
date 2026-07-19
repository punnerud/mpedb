//! Derived tables — a subquery used as a FROM source, `SELECT … FROM (SELECT …)
//! t …` (#74, design/DESIGN-DERIVED-TABLES.md, Stage B). A simple projection/filter
//! body is flattened onto its base at bind time (like a view), keeping the
//! derived alias so `t.col` refs resolve; complex bodies are refused (never
//! answered wrongly). Cross-checked against sqlite 3.45.

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
        "mpedb-derived-{name}-{}-{}.mpedb",
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
    for id in 1..=10 {
        db.query(
            &format!("INSERT INTO t (id, a, b, c) VALUES ({id}, {}, 'r{id}', {})", id, id * 10),
            &[],
        )
        .unwrap();
    }
}

#[test]
fn star_body_star_outer() {
    let (db, path) = open("star");
    setup(&db);
    // `SELECT * FROM (SELECT * FROM t WHERE a > 5) d` — every base column, a>5.
    let got = rows(db.query("SELECT * FROM (SELECT * FROM t WHERE a > 5) d ORDER BY id", &[]).unwrap());
    assert_eq!(got.len(), 5); // a = 6..10
    assert_eq!(got[0][0], Value::Int(6));
    assert_eq!(got[0][2], Value::Text("r6".into())); // b passed through
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn outer_projection_and_filter_merge() {
    let (db, path) = open("merge");
    setup(&db);
    // Outer projects a subset and adds a filter that AND-merges with the body's.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM (SELECT * FROM t WHERE a > 3) d WHERE a < 8"), 4); // a=4,5,6,7
    assert_eq!(scalar_i64(&db, "SELECT id FROM (SELECT * FROM t) d WHERE a = 7"), 7);
    // Qualified refs to the derived alias resolve.
    assert_eq!(scalar_i64(&db, "SELECT d.a FROM (SELECT * FROM t) d WHERE d.a = 4"), 4);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn bare_column_body_and_qualified_body_where() {
    let (db, path) = open("bare");
    setup(&db);
    // A bare-column projection body: outer `*` exposes exactly id,a.
    let got = rows(db.query("SELECT * FROM (SELECT id, a FROM t WHERE a > 8) d ORDER BY id", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(9), Value::Int(9)], vec![Value::Int(10), Value::Int(10)]]);
    // The body's WHERE qualifies with the base's real name — remapped onto the
    // derived alias so it still resolves after flattening.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM (SELECT id, a FROM t WHERE t.a >= 5) d"), 6); // a=5..10
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn aggregate_over_derived_table() {
    let (db, path) = open("agg");
    setup(&db);
    // The OUTER query may aggregate/group over a simple derived table — only the
    // derived BODY is constrained.
    assert_eq!(scalar_i64(&db, "SELECT sum(c) FROM (SELECT c FROM t WHERE a >= 3) d"), (3..=10).map(|i| i * 10).sum());
    let got = rows(db.query(
        "SELECT count(*) FROM (SELECT a, c FROM t WHERE a >= 3) d WHERE c > 50",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(5)]]); // c=60..100
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn derived_table_joined_with_base() {
    let (db, path) = open("join");
    setup(&db);
    db.query("CREATE TABLE u (uid INTEGER PRIMARY KEY, oid INT, x TEXT)", &[]).unwrap();
    for uid in 1..=6 {
        db.query(&format!("INSERT INTO u (uid, oid, x) VALUES ({uid}, {uid}, 'u{uid}')"), &[]).unwrap();
    }
    // A derived table joined with a base table: the derived alias names its side.
    let got = rows(db.query(
        "SELECT d.id, u.x FROM (SELECT * FROM t WHERE a > 4) d JOIN u ON u.oid = d.id ORDER BY d.id",
        &[],
    ).unwrap());
    // t rows a>4 = id 5,6,7,8,9,10; u has oid 1..6 → matches id 5,6.
    assert_eq!(got, vec![
        vec![Value::Int(5), Value::Text("u5".into())],
        vec![Value::Int(6), Value::Text("u6".into())],
    ]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn stage_a_materializes_former_refusals() {
    let (db, path) = open("refuse");
    setup(&db);
    // Every Stage-B refusal is now MATERIALIZED (design/DESIGN-DERIVED-TABLES.md
    // §5, `PlanStmt::Derived`) — cross-checked against sqlite in
    // derived_materialize.rs; here just the smoke that each shape answers.
    assert_eq!(scalar_i64(&db, "SELECT * FROM (SELECT count(*) FROM t) d"), 10); // aggregate body
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM (SELECT DISTINCT a FROM t) d"), 10); // DISTINCT
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM (SELECT a FROM t LIMIT 3) d"), 3); // LIMIT
    assert_eq!(scalar_i64(&db, "SELECT z FROM (SELECT a AS z FROM t) d WHERE z = 7"), 7); // renamed
    // Alias-less derived tables are legal too (sqlite allows them).
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM (SELECT DISTINCT a FROM t)"), 10);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
