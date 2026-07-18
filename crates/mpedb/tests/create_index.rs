//! `CREATE [UNIQUE] INDEX … ON t (cols)` — builds a secondary index over the
//! existing rows. It never changes query ANSWERS (an index is an optimization);
//! it must build cleanly, enforce UNIQUE going forward, reject a build that
//! finds a duplicate, accept composite / ASC-DESC / IF NOT EXISTS forms, and be
//! idempotent by shape.

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
        "mpedb-createidx-{name}-{}-{}.mpedb",
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

#[test]
fn create_index_builds_over_existing_rows_and_queries_stay_correct() {
    let (db, path) = open("build");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT)", &[]).unwrap();
    for id in 1..=20 {
        db.query(&format!("INSERT INTO t (id, a, b) VALUES ({id}, {}, 'r{id}')", id % 5), &[])
            .unwrap();
    }
    // Build a non-unique index AFTER the data exists.
    db.query("CREATE INDEX idx_a ON t (a)", &[]).unwrap();
    // Equality and range over the indexed column return the right rows.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE a = 3"), 4);
    let got = rows(db.query("SELECT id FROM t WHERE a = 0 ORDER BY id", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(5)], vec![Value::Int(10)], vec![Value::Int(15)], vec![Value::Int(20)]]);
    // A composite index with per-column ASC/DESC (direction ignored) also builds.
    db.query("CREATE INDEX idx_ba ON t (b, a DESC)", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT id FROM t WHERE b = 'r7'", &[]).unwrap()),
        vec![vec![Value::Int(7)]]
    );
    // New inserts are indexed too.
    db.query("INSERT INTO t (id, a, b) VALUES (21, 3, 'r21')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE a = 3"), 5);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn unique_index_build_and_enforcement() {
    let (db, path) = open("unique");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT)", &[]).unwrap();
    db.query("INSERT INTO t (id, email) VALUES (1, 'a@x')", &[]).unwrap();
    db.query("INSERT INTO t (id, email) VALUES (2, 'b@x')", &[]).unwrap();
    // Building a UNIQUE index over distinct values succeeds and enforces going
    // forward.
    db.query("CREATE UNIQUE INDEX idx_email ON t (email)", &[]).unwrap();
    assert!(db.query("INSERT INTO t (id, email) VALUES (3, 'a@x')", &[]).is_err());
    db.query("INSERT INTO t (id, email) VALUES (3, 'c@x')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 3);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn unique_index_build_rejects_existing_duplicate() {
    let (db, path) = open("dupbuild");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)", &[]).unwrap();
    db.query("INSERT INTO t (id, v) VALUES (1, 7)", &[]).unwrap();
    db.query("INSERT INTO t (id, v) VALUES (2, 7)", &[]).unwrap(); // duplicate v
    // A UNIQUE index cannot be built over data that already violates it.
    assert!(db.query("CREATE UNIQUE INDEX idx_v ON t (v)", &[]).is_err());
    // The failed build left the table usable (both rows still there, no index).
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 2);
    db.query("INSERT INTO t (id, v) VALUES (3, 7)", &[]).unwrap(); // still allowed
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn create_index_if_not_exists_is_idempotent() {
    let (db, path) = open("idem");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT)", &[]).unwrap();
    db.query("INSERT INTO t (id, a) VALUES (1, 1)", &[]).unwrap();
    db.query("CREATE INDEX idx_a ON t (a)", &[]).unwrap();
    // A second identical index (same columns) is a no-op, not an error.
    db.query("CREATE INDEX IF NOT EXISTS idx_a2 ON t (a)", &[]).unwrap();
    db.query("CREATE INDEX also_a ON t (a)", &[]).unwrap();
    // Unknown table / column errors.
    assert!(db.query("CREATE INDEX x ON nope (a)", &[]).is_err());
    assert!(db.query("CREATE INDEX x ON t (nope)", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
