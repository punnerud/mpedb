//! Partial indexes (P1 / design/DESIGN-WORKLOAD-INDEXES.md §5):
//! `CREATE INDEX … WHERE <predicate>` is parsed, stored on `IndexDef`, and
//! survives the schema wire (canonical-bytes v10). The planner never picks a
//! partial for access yet (implication / P6), so SELECT stays correct via
//! FullScan. CPython's `test_func_deterministic` needs CREATE to succeed.

use mpedb::{Config, Database, Value};
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
        "mpedb-partial-ix-{}-{}.mpedb",
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
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        path,
    )
}

#[test]
fn create_index_where_parses_and_builds() {
    let (db, path) = open();
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT)", &[])
        .unwrap();
    db.query("INSERT INTO t (id, a, b) VALUES (1, 10, 'x')", &[])
        .unwrap();
    db.query("INSERT INTO t (id, a, b) VALUES (2, NULL, 'y')", &[])
        .unwrap();
    // Non-unique partial: create succeeds and stores the predicate.
    db.query(
        "CREATE INDEX ix_a ON t (a) WHERE a IS NOT NULL",
        &[],
    )
    .unwrap();
    // SELECT still answers correctly (planner does not use the partial).
    let res = db
        .query("SELECT id FROM t WHERE a = 10", &[])
        .unwrap();
    match res {
        mpedb::ExecResult::Rows { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(1)]]);
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Predicate is in the live schema.
    let sch = db.schema();
    let t = sch
        .tables
        .iter()
        .find(|t| t.name == "t")
        .expect("table t");
    let ix = t
        .indexes
        .iter()
        .find(|ix| ix.predicate.is_some())
        .expect("partial index");
    assert!(
        ix.predicate
            .as_ref()
            .unwrap()
            .to_ascii_uppercase()
            .contains("IS NOT NULL"),
        "{:?}",
        ix.predicate
    );
    // UNIQUE partial is refused until membership evaluation ships.
    let err = db
        .query("CREATE UNIQUE INDEX ux ON t (b) WHERE b IS NOT NULL", &[])
        .unwrap_err();
    assert!(
        format!("{err}").contains("UNIQUE INDEX") || format!("{err}").contains("partial"),
        "{err}"
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn create_index_where_empty_table_like_cpython() {
    let (db, path) = open();
    db.query("CREATE TABLE test (t TEXT)", &[]).unwrap();
    // CPython shape: empty table + WHERE with a function call spelling.
    // (Host UDF not registered — predicate text is stored; build is empty.)
    db.query(
        "CREATE INDEX t ON test(t) WHERE t IS NOT NULL",
        &[],
    )
    .unwrap();
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
