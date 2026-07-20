//! `ALTER TABLE ADD COLUMN <name> <type> [NOT NULL] DEFAULT <const>`, closing
//! the last ADD COLUMN gap (#47). Differential vs the real sqlite 3.45 library
//! (bundled through rusqlite): the same statements run against both engines and
//! the resulting rows must agree value-for-value.
//!
//! sqlite's ADD COLUMN rules (verified against sqlite3 3.45.1) that we match:
//! `ADD COLUMN c INT NOT NULL DEFAULT 5` → OK, existing rows get 5 and a later
//! INSERT omitting `c` gets 5 too (the default is persisted); `ADD COLUMN c
//! TEXT DEFAULT 'x'` → OK, existing rows get 'x'; `ADD COLUMN c INT NOT NULL`
//! with no default → ERROR; `ADD COLUMN c INT UNIQUE` / `PRIMARY KEY` → ERROR;
//! a non-constant default (`(1+2)`, a function, `CURRENT_*`) → ERROR.
//!
//! The one DELIBERATE divergence is the rigid schema: a type-mismatched default
//! (`c INT DEFAULT 'nope'`) is a clean error in mpedb, whereas sqlite's loose
//! typing stores it — so that case is asserted on mpedb alone.

use mpedb::{Config, Database, ExecResult, Value};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn config(name: &str) -> (Config, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-addcoldflt-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 32

[[table]]
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
}

/// A normalized cell, comparable across engines: mpedb's Bool/Timestamp collapse
/// to the integer sqlite would store, and floats compare by bit pattern.
#[derive(Debug, PartialEq)]
enum Cell {
    Null,
    Int(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

fn from_mpedb(v: &Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Int(i) => Cell::Int(*i),
        Value::Bool(b) => Cell::Int(*b as i64),
        Value::Timestamp(t) => Cell::Int(*t),
        Value::Float(f) => Cell::Real(f.to_bits()),
        Value::Text(s) => Cell::Text(s.clone()),
        Value::Blob(b) => Cell::Blob(b.clone()),
        Value::List(_) => panic!("list value is not storable"),
    }
}

fn from_sqlite(v: rusqlite::types::ValueRef<'_>) -> Cell {
    use rusqlite::types::ValueRef as V;
    match v {
        V::Null => Cell::Null,
        V::Integer(i) => Cell::Int(i),
        V::Real(f) => Cell::Real(f.to_bits()),
        V::Text(t) => Cell::Text(std::str::from_utf8(t).unwrap().to_string()),
        V::Blob(b) => Cell::Blob(b.to_vec()),
    }
}

fn mpedb_select(db: &Database, sql: &str) -> Vec<Vec<Cell>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            rows.iter().map(|r| r.iter().map(from_mpedb).collect()).collect()
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

fn sqlite_select(conn: &Connection, sql: &str) -> Vec<Vec<Cell>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let n = stmt.column_count();
    let rows = stmt
        .query_map([], |r| {
            Ok((0..n).map(|i| from_sqlite(r.get_ref(i).unwrap())).collect::<Vec<_>>())
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// Fresh mpedb `t(id INTEGER PRIMARY KEY, a TEXT)` seeded with two rows.
fn fresh_mpedb(name: &str) -> (Database, PathBuf) {
    let (cfg, path) = config(name);
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)", &[]).unwrap();
    db.query("INSERT INTO t (id, a) VALUES (1, 'x')", &[]).unwrap();
    db.query("INSERT INTO t (id, a) VALUES (2, 'y')", &[]).unwrap();
    (db, path)
}

/// The same table through the real sqlite library, in memory.
fn fresh_sqlite() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT); \
         INSERT INTO t VALUES (1, 'x'), (2, 'y');",
    )
    .unwrap();
    conn
}

#[test]
fn not_null_default_fills_existing_and_new_rows_like_sqlite() {
    let (db, path) = fresh_mpedb("nn-default");
    let conn = fresh_sqlite();

    let alter = "ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT 5";
    db.query(alter, &[]).unwrap();
    conn.execute(alter, []).unwrap();

    // Existing rows were rewritten with the constant 5 — identical both sides.
    let sel = "SELECT id, c FROM t ORDER BY id";
    assert_eq!(mpedb_select(&db, sel), sqlite_select(&conn, sel));

    // A later INSERT omitting `c` takes the persisted default (both get 5).
    let ins = "INSERT INTO t (id, a) VALUES (3, 'z')";
    db.query(ins, &[]).unwrap();
    conn.execute(ins, []).unwrap();
    let sel3 = "SELECT id, c FROM t WHERE id = 3";
    assert_eq!(mpedb_select(&db, sel3), sqlite_select(&conn, sel3));
    // And explicitly setting it still works.
    db.query("INSERT INTO t (id, a, c) VALUES (4, 'w', 9)", &[]).unwrap();
    conn.execute("INSERT INTO t (id, a, c) VALUES (4, 'w', 9)", []).unwrap();
    let all = "SELECT id, c FROM t ORDER BY id";
    assert_eq!(mpedb_select(&db, all), sqlite_select(&conn, all));

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn text_default_matches_sqlite() {
    let (db, path) = fresh_mpedb("text-default");
    let conn = fresh_sqlite();

    let alter = "ALTER TABLE t ADD COLUMN d TEXT DEFAULT 'hi'";
    db.query(alter, &[]).unwrap();
    conn.execute(alter, []).unwrap();

    let sel = "SELECT id, d FROM t ORDER BY id";
    assert_eq!(mpedb_select(&db, sel), sqlite_select(&conn, sel));

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn signed_and_real_and_bool_defaults() {
    // Negative-int, float, and boolean literal defaults all fold and fill.
    let (db, path) = fresh_mpedb("misc-defaults");
    let conn = fresh_sqlite();

    for stmt in [
        "ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT -7",
        "ALTER TABLE t ADD COLUMN r REAL NOT NULL DEFAULT 1.5",
    ] {
        db.query(stmt, &[]).unwrap();
        conn.execute(stmt, []).unwrap();
    }
    let sel = "SELECT id, c, r FROM t ORDER BY id";
    assert_eq!(mpedb_select(&db, sel), sqlite_select(&conn, sel));

    // A BOOL column (sqlite stores the boolean literal as integer 1) — compared
    // through the Bool→Int normalization.
    db.query("ALTER TABLE t ADD COLUMN b BOOL NOT NULL DEFAULT true", &[]).unwrap();
    conn.execute("ALTER TABLE t ADD COLUMN b BOOL NOT NULL DEFAULT 1", []).unwrap();
    let selb = "SELECT id, b FROM t ORDER BY id";
    assert_eq!(mpedb_select(&db, selb), sqlite_select(&conn, selb));

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn refusals_match_sqlite() {
    let (db, path) = fresh_mpedb("refusals");
    let conn = fresh_sqlite();

    // NOT NULL with no default: both engines refuse (existing rows have no fill).
    let nn = "ALTER TABLE t ADD COLUMN c INT NOT NULL";
    assert!(db.query(nn, &[]).is_err());
    assert!(conn.execute(nn, []).is_err());

    // NOT NULL DEFAULT NULL is still "no non-NULL default": both refuse.
    let nn_null = "ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT NULL";
    assert!(db.query(nn_null, &[]).is_err());
    assert!(conn.execute(nn_null, []).is_err());

    // UNIQUE / PRIMARY KEY on ADD: both refuse.
    for stmt in [
        "ALTER TABLE t ADD COLUMN c INT UNIQUE",
        "ALTER TABLE t ADD COLUMN c INT PRIMARY KEY",
    ] {
        assert!(db.query(stmt, &[]).is_err(), "mpedb should refuse: {stmt}");
        assert!(conn.execute(stmt, []).is_err(), "sqlite should refuse: {stmt}");
    }

    // Non-constant default: sqlite refuses "Cannot add a column with non-constant
    // default" / "not constant"; mpedb refuses at parse time.
    for stmt in [
        "ALTER TABLE t ADD COLUMN c INT DEFAULT (1+2)",
        "ALTER TABLE t ADD COLUMN c INT DEFAULT abs(-5)",
        "ALTER TABLE t ADD COLUMN c TEXT DEFAULT current_timestamp",
    ] {
        assert!(db.query(stmt, &[]).is_err(), "mpedb should refuse: {stmt}");
        assert!(conn.execute(stmt, []).is_err(), "sqlite should refuse: {stmt}");
    }

    // After every refusal a valid ADD still works — no half-applied state.
    db.query("ALTER TABLE t ADD COLUMN note TEXT NOT NULL DEFAULT 'z'", &[]).unwrap();
    assert_eq!(
        mpedb_select(&db, "SELECT note FROM t WHERE id = 1"),
        vec![vec![Cell::Text("z".into())]]
    );

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn type_mismatched_default_is_a_clean_error() {
    // A default whose type does not match the column, after the column's OWN
    // store-time affinity has had its go (task #113): a DDL-declared column
    // carries sqlite's affinity, so `TEXT DEFAULT 5` really does store the
    // text `'5'` here, exactly as sqlite does — the refusals below are the
    // ones where sqlite's conversion does not land inside the rigid type and
    // sqlite would have kept the original class.
    let (db, path) = fresh_mpedb("type-mismatch");
    // `'nope'` numerifies to nothing, so it stays text and an int column
    // refuses it; sqlite stores the text. Narrower, never different.
    assert!(db.query("ALTER TABLE t ADD COLUMN c INT DEFAULT 'nope'", &[]).is_err());
    // `bool` is mpedb's own type, not one of sqlite's affinities, and never
    // converts.
    assert!(db.query("ALTER TABLE t ADD COLUMN c BOOL DEFAULT 'x'", &[]).is_err());
    // The table is untouched: the column was never added.
    assert!(db.query("SELECT c FROM t", &[]).is_err());
    // A well-typed default still works right afterwards.
    db.query("ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT 3", &[]).unwrap();
    assert_eq!(
        mpedb_select(&db, "SELECT c FROM t ORDER BY id"),
        vec![vec![Cell::Int(3)], vec![Cell::Int(3)]]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// Task #113, the other half: where sqlite's store affinity DOES land inside
/// the rigid type, a DDL-declared column converts exactly as sqlite does — the
/// value AND its `typeof()`, both sides, on the same statements.
#[test]
fn a_ddl_declared_column_applies_sqlites_store_affinity_to_its_default() {
    let (db, path) = fresh_mpedb("aff-default");
    let conn = fresh_sqlite();
    for alter in [
        // TEXT affinity renders a number.
        "ALTER TABLE t ADD COLUMN s TEXT DEFAULT 5",
        "ALTER TABLE t ADD COLUMN s2 VARCHAR(10) DEFAULT 1.5",
        // INTEGER affinity parses a fully-numeric string, and takes a real
        // only when the round trip is lossless.
        "ALTER TABLE t ADD COLUMN i INT DEFAULT '12'",
        "ALTER TABLE t ADD COLUMN i2 BIGINT DEFAULT 9.0",
        // REAL affinity floats an integer.
        "ALTER TABLE t ADD COLUMN r REAL DEFAULT 7",
    ] {
        db.query(alter, &[]).unwrap();
        conn.execute(alter, []).unwrap();
    }
    let sel = "SELECT s, s2, i, i2, r FROM t ORDER BY id";
    assert_eq!(mpedb_select(&db, sel), sqlite_select(&conn, sel));
    let tys = "SELECT typeof(s), typeof(s2), typeof(i), typeof(i2), typeof(r) FROM t ORDER BY id";
    assert_eq!(mpedb_select(&db, tys), sqlite_select(&conn, tys));
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn default_persists_across_reopen() {
    let (cfg, path) = config("persist");
    {
        let db = Database::open_with_config(cfg.clone()).unwrap();
        db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)", &[]).unwrap();
        db.query("INSERT INTO t (id, a) VALUES (1, 'x')", &[]).unwrap();
        db.query("ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT 42", &[]).unwrap();
        db.verify().unwrap();
    }
    // Reopen: the added column, its filled value, and its default all survive.
    {
        let db = Database::open_with_config(cfg).unwrap();
        assert_eq!(
            mpedb_select(&db, "SELECT c FROM t WHERE id = 1"),
            vec![vec![Cell::Int(42)]]
        );
        // The persisted default still applies to a fresh INSERT omitting `c`.
        db.query("INSERT INTO t (id, a) VALUES (2, 'y')", &[]).unwrap();
        assert_eq!(
            mpedb_select(&db, "SELECT c FROM t WHERE id = 2"),
            vec![vec![Cell::Int(42)]]
        );
        db.verify().unwrap();
    }
    let _ = std::fs::remove_file(&path);
}
