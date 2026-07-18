//! `x IN ()` / `x NOT IN ()` — membership in the EMPTY set. sqlite accepts the
//! empty RHS (PostgreSQL rejects it, but accepting it rejects nothing PG
//! accepts). The empty set is FALSE for every probe, NULL included, so
//! `NOT IN ()` is TRUE for every probe. Cross-checked against sqlite 3.45.

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
        "mpedb-inempty-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 8\nmax_readers = 8\n\n[[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  [[table.column]]\n  name = \"v\"\n  type = \"int64\"\n",
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn empty_in_set_is_false_and_not_in_is_true() {
    let (db, path) = open();
    db.query("INSERT INTO t (id, v) VALUES (1, 10), (2, 20)", &[]).unwrap();

    // `IN ()` is FALSE for every row → no rows pass the filter.
    assert_eq!(rows(&db, "SELECT id FROM t WHERE v IN ()").len(), 0);
    // `NOT IN ()` is TRUE for every row → all rows pass.
    assert_eq!(rows(&db, "SELECT id FROM t WHERE v NOT IN () ORDER BY id").len(), 2);

    // Constant probes over a FROM-less SELECT: 1 IN () → FALSE, so the WHERE
    // eliminates the synthetic row; NOT IN () keeps it.
    assert_eq!(rows(&db, "SELECT 1 WHERE 1 IN ()").len(), 0);
    assert_eq!(rows(&db, "SELECT 1 WHERE 1 NOT IN ()"), vec![vec![Value::Int(1)]]);

    // NULL IN () is FALSE (empty set), NOT NULL — so a bare `SELECT NULL IN ()`
    // yields Bool(false), and NULL NOT IN () yields Bool(true).
    assert_eq!(rows(&db, "SELECT NULL IN ()"), vec![vec![Value::Bool(false)]]);
    assert_eq!(rows(&db, "SELECT NULL NOT IN ()"), vec![vec![Value::Bool(true)]]);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
