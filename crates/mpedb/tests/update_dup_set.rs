//! UPDATE with a column assigned more than once. sqlite (R-34751-18293): "all
//! but the rightmost occurrence is ignored" — so `SET x=3, x=4, x=5` sets x=5,
//! and the ignored occurrences are NOT evaluated (a would-be error in one is
//! never raised). Cross-checked against sqlite 3.45.

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
        "mpedb-updup-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 8\nmax_readers = 8\n\n[[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  [[table.column]]\n  name = \"x\"\n  type = \"int64\"\n",
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn scalar_i64(db: &Database, sql: &str) -> i64 {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Int(i) => *i,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn rightmost_assignment_wins_and_others_are_ignored() {
    let (db, path) = open();
    db.query("INSERT INTO t (id, x) VALUES (1, 0)", &[]).unwrap();

    // Rightmost wins: x becomes 5, not 3 or 4.
    db.query("UPDATE t SET x = 3, x = 4, x = 5 WHERE id = 1", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT x FROM t WHERE id = 1"), 5);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE x = 3"), 0);

    // The ignored occurrences are not even evaluated: a division-by-zero in a
    // non-rightmost assignment (which mpedb would otherwise raise on) is silently
    // dropped, exactly as sqlite ignores it.
    db.query("UPDATE t SET x = 1 / 0, x = 7 WHERE id = 1", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT x FROM t WHERE id = 1"), 7);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
