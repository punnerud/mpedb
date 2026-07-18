//! `INSERT INTO t [(cols)] SELECT …` (COMPAT) — the source query is read fully
//! first (so a self-insert reads the pre-insert state), its output tuple fills
//! the listed columns, omitted columns take their default/NULL, a WHERE filters
//! the copied rows, and a column subset/reorder is honored. Cross-checked
//! against sqlite 3.45.

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
        "mpedb-inssel-{name}-{}-{}.mpedb",
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
name = "src"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "a"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "text"
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
fn insert_select_star_copies_all_rows() {
    let (db, path) = open("star");
    db.query("CREATE TABLE dst (id INTEGER PRIMARY KEY, a INT, b TEXT)", &[]).unwrap();
    for id in 1..=5 {
        db.query(&format!("INSERT INTO src (id, a, b) VALUES ({id}, {}, 'r{id}')", id * 10), &[])
            .unwrap();
    }
    // The corpus shape: copy every column of every row.
    let res = db.query("INSERT INTO dst SELECT * FROM src", &[]).unwrap();
    assert!(matches!(res, ExecResult::Affected(5)), "{res:?}");
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM dst"), 5);
    assert_eq!(scalar_i64(&db, "SELECT a FROM dst WHERE id = 3"), 30);
    assert_eq!(
        rows(db.query("SELECT b FROM dst WHERE id = 5", &[]).unwrap()),
        vec![vec![Value::Text("r5".into())]]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_select_with_where_and_column_subset() {
    let (db, path) = open("subset");
    db.query("CREATE TABLE dst (id INTEGER PRIMARY KEY, label TEXT, amt INT)", &[]).unwrap();
    for id in 1..=6 {
        db.query(&format!("INSERT INTO src (id, a, b) VALUES ({id}, {}, 'x')", id), &[]).unwrap();
    }
    // Copy only some rows, into a subset of columns in a different order; the
    // omitted column (`amt`) defaults to NULL.
    db.query(
        "INSERT INTO dst (id, label) SELECT id, b FROM src WHERE a > 3",
        &[],
    )
    .unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM dst"), 3); // ids 4,5,6
    assert_eq!(
        rows(db.query("SELECT label, amt FROM dst WHERE id = 5", &[]).unwrap()),
        vec![vec![Value::Text("x".into()), Value::Null]]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_select_self_reads_pre_insert_state() {
    // `INSERT INTO src SELECT ... FROM src` must read the source fully BEFORE
    // inserting — otherwise the freshly inserted rows would feed back and the
    // insert would not terminate / would double.
    let (db, path) = open("self");
    for id in 1..=3 {
        db.query(&format!("INSERT INTO src (id, a, b) VALUES ({id}, {id}, 'r')"), &[]).unwrap();
    }
    // Copy every row to a new id (id + 100), reading only the original 3.
    db.query("INSERT INTO src SELECT id + 100, a, b FROM src", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM src"), 6);
    assert_eq!(scalar_i64(&db, "SELECT a FROM src WHERE id = 102"), 2);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_select_refusals() {
    let (db, path) = open("refuse");
    db.query("CREATE TABLE dst (id INTEGER PRIMARY KEY, a INT)", &[]).unwrap();
    // Source column count must match the target column list.
    assert!(db.query("INSERT INTO dst (id) SELECT id, a FROM src", &[]).is_err());
    assert!(db.query("INSERT INTO dst SELECT id FROM src", &[]).is_err());
    // A duplicate PK from the source is a PK violation (nothing half-applied
    // beyond sqlite's own behavior — the error surfaces).
    db.query("INSERT INTO src (id, a, b) VALUES (1, 1, 'x')", &[]).unwrap();
    db.query("INSERT INTO dst (id, a) SELECT id, a FROM src", &[]).unwrap();
    assert!(db.query("INSERT INTO dst (id, a) SELECT id, a FROM src", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
