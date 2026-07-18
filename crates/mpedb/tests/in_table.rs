//! sqlite's `x IN <table>` shorthand — `x IN t` means `x IN (SELECT * FROM t)`
//! (t must be single-column). Cross-checked against sqlite 3.45.

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
        "mpedb-intab-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 8\nmax_readers = 8\n\n[[table]]\nname = \"nums\"\nprimary_key = [\"x\"]\n  [[table.column]]\n  name = \"x\"\n  type = \"int64\"\n",
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn one(db: &Database, sql: &str) -> Value {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows.into_iter().next().unwrap().into_iter().next().unwrap(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn in_bare_table_shorthand() {
    let (db, path) = open();
    for x in [4, 6, 8] {
        db.query(&format!("INSERT INTO nums (x) VALUES ({x})"), &[]).unwrap();
    }
    // FROM-less SELECT with the `IN <table>` shorthand. mpedb has a first-class
    // bool (sqlite would render these as 1/0; the value is the same).
    assert_eq!(one(&db, "SELECT 6 IN nums"), Value::Bool(true));
    assert_eq!(one(&db, "SELECT 7 IN nums"), Value::Bool(false));
    assert_eq!(one(&db, "SELECT 7 NOT IN nums"), Value::Bool(true));
    assert_eq!(one(&db, "SELECT 4 NOT IN nums"), Value::Bool(false));

    // As a WHERE predicate over a table (still a single-column membership test).
    db.query("CREATE TABLE q (id INTEGER PRIMARY KEY, v INT)", &[]).unwrap();
    for (id, v) in [(1, 4), (2, 5), (3, 6)] {
        db.query(&format!("INSERT INTO q (id, v) VALUES ({id}, {v})"), &[]).unwrap();
    }
    let res = db.query("SELECT id FROM q WHERE v IN nums ORDER BY id", &[]).unwrap();
    match res {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(3)]]),
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn null_in_empty_set_is_false_not_null() {
    // SQL 3VL: `NULL IN (empty)` is FALSE (nothing is in the empty set), NOT
    // NULL. sqlite/PostgreSQL agree; this was a genuine wrong-answer bug.
    let (db, path) = open();
    db.query("CREATE TABLE empt (id INTEGER PRIMARY KEY)", &[]).unwrap(); // no rows
    // Empty subquery.
    assert_eq!(one(&db, "SELECT NULL IN (SELECT id FROM empt)"), Value::Bool(false));
    assert_eq!(one(&db, "SELECT NULL NOT IN (SELECT id FROM empt)"), Value::Bool(true));
    // The `IN <table>` shorthand over an empty table, likewise.
    assert_eq!(one(&db, "SELECT NULL IN empt"), Value::Bool(false));
    // A NON-empty set with no match and a null probe stays NULL (unknown).
    for x in [1, 2] {
        db.query(&format!("INSERT INTO nums (x) VALUES ({x})"), &[]).unwrap();
    }
    assert_eq!(one(&db, "SELECT NULL IN nums"), Value::Null);
    let _ = std::fs::remove_file(&path);
}
