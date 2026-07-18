//! sqlite's `INSERT OR {IGNORE|ABORT|FAIL|ROLLBACK|REPLACE}` conflict prefix.
//! IGNORE keeps the existing row on a PK conflict (= ON CONFLICT DO NOTHING);
//! ABORT/FAIL/ROLLBACK are the default error; REPLACE is refused (its
//! multi-constraint delete semantics differ from a plain upsert). Cross-checked
//! against sqlite 3.45.

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
        "mpedb-insor-{}-{}.mpedb",
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
  name = "v"
  type = "int64"
"#,
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
fn insert_or_ignore_keeps_existing_row() {
    let (db, path) = open();
    db.query("INSERT INTO t (id, v) VALUES (1, 10)", &[]).unwrap();
    // OR IGNORE on a PK conflict is a no-op — the existing value stays.
    let res = db.query("INSERT OR IGNORE INTO t (id, v) VALUES (1, 99)", &[]).unwrap();
    assert!(matches!(res, ExecResult::Affected(_)), "{res:?}");
    assert_eq!(scalar_i64(&db, "SELECT v FROM t WHERE id = 1"), 10);
    // A non-conflicting OR IGNORE still inserts.
    db.query("INSERT OR IGNORE INTO t (id, v) VALUES (2, 20)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 2);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_or_abort_fail_rollback_error_on_conflict() {
    let (db, path) = open();
    db.query("INSERT INTO t (id, v) VALUES (1, 10)", &[]).unwrap();
    // ABORT/FAIL/ROLLBACK all mean "error on conflict" (mpedb's default).
    for kw in ["ABORT", "FAIL", "ROLLBACK"] {
        assert!(
            db.query(&format!("INSERT OR {kw} INTO t (id, v) VALUES (1, 99)"), &[]).is_err(),
            "OR {kw} must error on a PK conflict"
        );
    }
    assert_eq!(scalar_i64(&db, "SELECT v FROM t WHERE id = 1"), 10);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_or_replace_is_refused() {
    let (db, path) = open();
    let err = db.query("INSERT OR REPLACE INTO t (id, v) VALUES (1, 10)", &[]).unwrap_err();
    assert!(format!("{err}").contains("REPLACE"), "{err}");
    // A garbage OR-action is a clean parse error, not a silent accept.
    assert!(db.query("INSERT OR BOGUS INTO t (id, v) VALUES (1, 10)", &[]).is_err());
    let _ = std::fs::remove_file(&path);
}
