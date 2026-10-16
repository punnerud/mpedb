//! `INSERT INTO t DEFAULT VALUES` — insert one row where every column takes its
//! default (a rowid alias auto-assigns; a column DEFAULT fills; a nullable
//! column becomes NULL; a NOT NULL column with no default is a clean error).
//! Differentially verified against sqlite 3.45 in the shared battery; here the
//! semantics are pinned directly.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn db() -> (Database, String) {
    let dir = mpedb_testkit::scratch_base_str();
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
#[cfg(feature = "pg-dialect")]
fn owned_serial_reservations_survive_reopen_and_share_insert_counter() {
    let (mut db, path) = db();
    db.set_dialect(mpedb_types::Dialect::Postgres);
    db.query("CREATE TABLE serial_keys (id serial PRIMARY KEY)", &[])
        .unwrap();
    assert_eq!(db.next_serial_value("serial_keys_id_seq", None).unwrap(), 1);
    let mut peer = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    peer.set_dialect(mpedb_types::Dialect::Postgres);
    assert_eq!(
        peer.next_serial_value("serial_keys_id_seq", None).unwrap(),
        2
    );
    db.query("INSERT INTO serial_keys DEFAULT VALUES", &[])
        .unwrap();
    assert_eq!(
        rows(&db, "SELECT id FROM serial_keys"),
        vec![vec![Value::Int(3)]]
    );
    drop(peer);
    drop(db);
    let mut reopened = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    reopened.set_dialect(mpedb_types::Dialect::Postgres);
    assert_eq!(
        reopened
            .next_serial_value("serial_keys_id_seq", None)
            .unwrap(),
        4
    );
    reopened
        .query("INSERT INTO serial_keys VALUES (9223372036854775807)", &[])
        .unwrap();
    assert!(reopened
        .next_serial_value("serial_keys_id_seq", None)
        .is_err());
    drop(reopened);
    std::fs::remove_file(path).unwrap();
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

    // PostgreSQL's per-cell DEFAULT is dialect-specific. SQLite still rejects
    // it, and its double-quoted-string fallback remains a literal value.
    assert!(db.query("INSERT INTO t (id) VALUES (DEFAULT)", &[]).is_err());
    db.query("INSERT INTO t (id,v) VALUES (4,\"DEFAULT\")", &[]).unwrap();
    assert_eq!(rows(&db, "SELECT v FROM t WHERE id=4"), vec![vec![Value::Text("DEFAULT".into())]]);
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
