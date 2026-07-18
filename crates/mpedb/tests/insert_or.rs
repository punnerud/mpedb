//! sqlite's `INSERT OR {IGNORE|ABORT|FAIL|ROLLBACK|REPLACE}` conflict prefix.
//! IGNORE keeps the existing row on a PK conflict (= ON CONFLICT DO NOTHING);
//! ABORT/FAIL/ROLLBACK are the default error; REPLACE deletes every existing
//! row the new row conflicts with — on the PK AND on any secondary UNIQUE index
//! — then inserts (sqlite's real delete-on-any-unique semantics). Cross-checked
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
fn insert_or_replace_replaces_on_pk_conflict() {
    // On a PK-only table, OR REPLACE is a PK-keyed upsert-all (sqlite semantics),
    // desugared to ON CONFLICT (pk) DO UPDATE SET <non-pk cols> = excluded.
    let (db, path) = open();
    db.query("INSERT INTO t (id, v) VALUES (1, 10)", &[]).unwrap();
    db.query("INSERT OR REPLACE INTO t (id, v) VALUES (1, 99)", &[]).unwrap(); // replace
    assert_eq!(scalar_i64(&db, "SELECT v FROM t WHERE id = 1"), 99);
    db.query("INSERT OR REPLACE INTO t (id, v) VALUES (2, 20)", &[]).unwrap(); // insert
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 2);
    // A garbage OR-action is a clean parse error, not a silent accept.
    assert!(db.query("INSERT OR BOGUS INTO t (id, v) VALUES (1, 10)", &[]).is_err());
    let _ = std::fs::remove_file(&path);
}

/// Read one text column of the single row a query returns.
fn scalar_text(db: &Database, sql: &str) -> String {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Text(s) => s.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn insert_or_replace_deletes_secondary_unique_conflict() {
    // REPLACE that collides on a secondary UNIQUE (email), with a NEW pk, must
    // delete the old email-owner and insert the new row. sqlite 3.45 ground
    // truth: after `INSERT OR REPLACE (3,'a@x','Carol')` the table is
    // {(2,b@x,Bob),(3,a@x,Carol)} — row 1 (the old 'a@x') is gone.
    let (db, path) = open();
    db.query(
        "CREATE TABLE t2 (id INTEGER PRIMARY KEY, email TEXT, name TEXT, UNIQUE (email))",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO t2 VALUES (1,'a@x','Alice')", &[]).unwrap();
    db.query("INSERT INTO t2 VALUES (2,'b@x','Bob')", &[]).unwrap();
    db.query("INSERT OR REPLACE INTO t2 VALUES (3,'a@x','Carol')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t2"), 2);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t2 WHERE id=1"), 0);
    assert_eq!(scalar_text(&db, "SELECT name FROM t2 WHERE email='a@x'"), "Carol");
    assert_eq!(scalar_i64(&db, "SELECT id FROM t2 WHERE email='a@x'"), 3);

    // A single REPLACE that collides on BOTH the PK and a *different* email
    // deletes TWO rows. sqlite: `INSERT OR REPLACE (5,'f@x','Zoe')` over
    // {(5,e@x,Eve),(6,f@x,Fred)} leaves just (5,f@x,Zoe).
    db.query("INSERT INTO t2 VALUES (5,'e@x','Eve')", &[]).unwrap();
    db.query("INSERT INTO t2 VALUES (6,'f@x','Fred')", &[]).unwrap();
    db.query("INSERT OR REPLACE INTO t2 VALUES (5,'f@x','Zoe')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t2"), 3);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t2 WHERE id=6"), 0);
    assert_eq!(scalar_text(&db, "SELECT name FROM t2 WHERE id=5"), "Zoe");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn bare_replace_into_is_insert_or_replace() {
    // sqlite's `REPLACE INTO t …` is an alias for `INSERT OR REPLACE INTO t …`.
    // Verified against sqlite 3.45: the second REPLACE overwrites the PK-1 row.
    let (db, path) = open();
    db.query("INSERT INTO t (id, v) VALUES (1, 10)", &[]).unwrap();
    db.query("REPLACE INTO t (id, v) VALUES (1, 99)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT v FROM t WHERE id = 1"), 99);
    db.query("REPLACE INTO t (id, v) VALUES (2, 20)", &[]).unwrap(); // insert
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 2);
    // The `replace()` scalar is unaffected — only `REPLACE INTO` at statement
    // start is the alias (a SELECT never reaches that positional check).
    assert_eq!(scalar_text(&db, "SELECT replace('aXa','X','1')"), "a1a");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_or_replace_null_unique_does_not_conflict() {
    // UNIQUE permits many NULLs, so a NULL in the unique column is no conflict:
    // both rows coexist. sqlite ground truth: {(1,NULL,a),(2,NULL,b)}.
    let (db, path) = open();
    db.query(
        "CREATE TABLE u (id INTEGER PRIMARY KEY, e TEXT, n TEXT, UNIQUE (e))",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO u VALUES (1,NULL,'a')", &[]).unwrap();
    db.query("INSERT OR REPLACE INTO u VALUES (2,NULL,'b')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM u"), 2);
    let _ = std::fs::remove_file(&path);
}
