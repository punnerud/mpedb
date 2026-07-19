//! `INSERT INTO t DEFAULT VALUES` — insert one row where every column takes its
//! default (a rowid alias auto-assigns; a column DEFAULT fills; a nullable
//! column becomes NULL; a NOT NULL column with no default is a clean error).
//! Differentially verified against sqlite 3.45 in the shared battery; here the
//! semantics are pinned directly.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn db() -> (Database, String) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!("{dir}/mpedb-defvals-{}-{}.mpedb", std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n");
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn default_values_rowid_and_null_and_defaults() {
    let (db, path) = db();
    struct G(String);
    impl Drop for G { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
    let _g = G(path);

    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    // rowid alias auto-assigns (1, 2), nullable v becomes NULL.
    db.query("INSERT INTO t DEFAULT VALUES", &[]).unwrap();
    db.query("INSERT INTO t DEFAULT VALUES", &[]).unwrap();
    let r = rows(&db, "SELECT id, v FROM t ORDER BY id");
    assert_eq!(r, vec![
        vec![Value::Int(1), Value::Null],
        vec![Value::Int(2), Value::Null],
    ]);

    // A column DEFAULT is applied (added via ALTER, which supports defaults).
    db.query("ALTER TABLE t ADD COLUMN n INTEGER DEFAULT 7", &[]).unwrap();
    db.query("INSERT INTO t DEFAULT VALUES", &[]).unwrap();
    let r = rows(&db, "SELECT id, v, n FROM t WHERE id = 3");
    assert_eq!(r, vec![vec![Value::Int(3), Value::Null, Value::Int(7)]]);
}

#[test]
fn default_values_not_null_no_default_is_error() {
    let (db, path) = db();
    struct G(String);
    impl Drop for G { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
    let _g = G(path);

    db.query("CREATE TABLE w (id INTEGER PRIMARY KEY, req TEXT NOT NULL)", &[]).unwrap();
    // A NOT NULL column with no default cannot be defaulted — clean error (sqlite
    // rejects it too, at runtime; mpedb at bind).
    let e = db.query("INSERT INTO w DEFAULT VALUES", &[]).unwrap_err().to_string();
    assert!(e.to_uppercase().contains("NOT NULL"), "{e}");
    // A column list cannot be combined with DEFAULT VALUES.
    let e = db.query("INSERT INTO w (id) DEFAULT VALUES", &[]).unwrap_err().to_string();
    assert!(e.to_lowercase().contains("default values"), "{e}");
}
