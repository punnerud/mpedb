//! Common Table Expressions (`WITH cte AS (SELECT …) SELECT …`, #CTE). A
//! non-recursive CTE is a statement-scoped named source: its body is flattened
//! onto its base table at bind time, reusing the derived-table keep-alias splice
//! (no planner/plan-bytes/executor change). Because the reference name is kept as
//! the base's alias, both unqualified refs and qualified `cte.col` / `FROM cte
//! AS x` (`x.col`) refs resolve. Only simple projection/filter bodies flatten;
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
fn qualified_refs_resolve() {
    let (db, path) = open();
    setup(&db);
    // `c.col` resolves in both the projection and the outer WHERE — the CTE name
    // is kept as the spliced base's alias. (sqlite 3.45: 3,4,5.)
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 2) SELECT c.a FROM c WHERE c.a < 6 ORDER BY c.a",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(5)]]);
    // A `SELECT *`-bodied CTE addressed by qualifier, projecting two base columns
    // (incl. column `c`, which the alias `c` must NOT shadow). (sqlite: 3,4,5.)
    let got = rows(db.query(
        "WITH c AS (SELECT * FROM t WHERE a > 2) SELECT c.id, c.c FROM c WHERE c.a < 6 ORDER BY c.id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(3), Value::Int(30)],
        vec![Value::Int(4), Value::Int(40)],
        vec![Value::Int(5), Value::Int(50)],
    ]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn reference_alias_resolves() {
    let (db, path) = open();
    setup(&db);
    // `FROM c AS x`: the reference alias `x` qualifies the columns. (sqlite: 3,4,5.)
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 2) SELECT x.a FROM c AS x WHERE x.a < 6 ORDER BY x.a",
        &[],
    ).unwrap());
    assert_eq!(got, vec![vec![Value::Int(3)], vec![Value::Int(4)], vec![Value::Int(5)]]);
    // `SELECT *` over an aliased CTE exposes exactly the body's columns (id,a).
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 4) SELECT * FROM c AS x ORDER BY x.id",
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
fn cte_joined_with_base_table() {
    let (db, path) = open();
    setup(&db);
    db.query("CREATE TABLE u (uid INTEGER PRIMARY KEY, oid INT, x TEXT)", &[]).unwrap();
    for uid in 1..=6 {
        db.query(&format!("INSERT INTO u (uid, oid, x) VALUES ({uid}, {uid}, 'u{uid}')"), &[]).unwrap();
    }
    // A CTE in the main FROM joined with a base table, addressed by qualified
    // refs on both sides. t rows a>4 = id 5,6,7; u.oid 1..6 → matches 5,6.
    // (Cross-checked vs sqlite 3.45.)
    let got = rows(db.query(
        "WITH c AS (SELECT id, a FROM t WHERE a > 4) SELECT c.id, u.x FROM c JOIN u ON u.oid = c.id ORDER BY c.id",
        &[],
    ).unwrap());
    assert_eq!(got, vec![
        vec![Value::Int(5), Value::Text("u5".into())],
        vec![Value::Int(6), Value::Text("u6".into())],
    ]);
    // A CTE in JOIN position is refused cleanly, never answered wrongly.
    assert!(db.query(
        "WITH c AS (SELECT id FROM t) SELECT u.x FROM u JOIN c ON c.id = u.oid",
        &[],
    ).is_err());
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
