//! Mixed-class `IN` lists (Django injection probe / sqlite affinity).
//! `name IN (num_chairs + 0)` is a clean FALSE for text vs int, never a bind error.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open() -> (Database, PathBuf) {
    let dir = if Path::new("/mnt/ext4").is_dir() {
        PathBuf::from("/mnt/ext4")
    } else if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-inmix-{}-{}.mpedb",
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
name = "seed"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        path,
    )
}

#[test]
fn text_probe_in_int_expression_is_false_not_a_type_error() {
    let (db, path) = open();
    db.query(
        "CREATE TABLE c (id INTEGER PRIMARY KEY, name TEXT, num_chairs INT)",
        &[],
    )
    .unwrap();
    db.query(
        "INSERT INTO c (id, name, num_chairs) VALUES (1, 'Acme', 5)",
        &[],
    )
    .unwrap();
    // Django shape: name__in=[F('num_chairs') + '…'] — text vs int, empty set.
    let res = db
        .query("SELECT id FROM c WHERE name IN (num_chairs + 0)", &[])
        .expect("mixed-class IN must bind");
    match res {
        ExecResult::Rows { rows, .. } => assert!(rows.is_empty(), "{rows:?}"),
        other => panic!("{other:?}"),
    }
    // Same class still matches.
    let res = db
        .query("SELECT id FROM c WHERE num_chairs IN (1, 5, 9)", &[])
        .unwrap();
    match res {
        ExecResult::Rows { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(1)]]);
        }
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_file(&path);
}
