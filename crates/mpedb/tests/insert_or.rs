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
    // ABORT is mpedb's default: the statement errors and undoes itself.
    // FAIL means the same thing for a SINGLE-row source (there is no earlier
    // row of this statement to keep), which is the only shape it is accepted in.
    for kw in ["ABORT", "FAIL"] {
        assert!(
            db.query(&format!("INSERT OR {kw} INTO t (id, v) VALUES (1, 99)"), &[]).is_err(),
            "OR {kw} must error on a PK conflict"
        );
    }
    assert_eq!(scalar_i64(&db, "SELECT v FROM t WHERE id = 1"), 10);
    let _ = std::fs::remove_file(&path);
}

/// The two conflict actions mpedb cannot express refuse BY NAME rather than
/// being accepted as "error on conflict", which is what they used to do:
///
/// * `OR ROLLBACK` must abort the enclosing TRANSACTION, which a statement
///   cannot reach (the C-API shim, which owns the connection, implements it).
/// * `OR FAIL` over a MULTI-row source must keep the rows inserted before the
///   conflicting one. mpedb statements are atomic, so it would silently undo
///   them — a wrong answer, not a missing feature.
#[test]
fn unexpressible_conflict_actions_refuse_by_name() {
    let (db, path) = open();
    db.query("INSERT INTO t (id, v) VALUES (1, 10)", &[]).unwrap();

    let e = db.query("INSERT OR ROLLBACK INTO t (id, v) VALUES (2, 20)", &[]).unwrap_err();
    let msg = e.to_string();
    assert!(msg.contains("OR ROLLBACK"), "{msg}");
    assert!(msg.contains("cannot abort the transaction"), "{msg}");

    // Multi-row VALUES and INSERT … SELECT are both multi-row sources.
    for sql in [
        "INSERT OR FAIL INTO t (id, v) VALUES (2, 20), (1, 99)",
        "INSERT OR FAIL INTO t (id, v) SELECT id + 5, v FROM t",
    ] {
        let msg = db.query(sql, &[]).unwrap_err().to_string();
        assert!(msg.contains("multi-row source"), "{sql}: {msg}");
    }
    // …and neither refusal wrote anything.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 1);

    // A bare typo still names the four actions that ARE accepted.
    let msg = db.query("INSERT OR NOPE INTO t (id, v) VALUES (2, 20)", &[]).unwrap_err().to_string();
    assert!(msg.contains("expected IGNORE, REPLACE, ABORT, or FAIL"), "{msg}");
    let _ = std::fs::remove_file(&path);
}

/// sqlite's per-constraint `ON CONFLICT <action>` clause. `ABORT` names
/// mpedb's own behaviour and is accepted and dropped; the other four would
/// change how a conflicting statement resolves and mpedb's schema carries no
/// per-constraint action, so they refuse by name at CREATE TABLE instead of
/// being parsed and ignored.
#[test]
fn constraint_conflict_clause_accepts_abort_and_refuses_the_rest() {
    let (db, path) = open();
    db.query("CREATE TABLE ok1 (x INTEGER, UNIQUE (x) ON CONFLICT ABORT)", &[]).unwrap();
    db.query("CREATE TABLE ok2 (x INTEGER NOT NULL ON CONFLICT ABORT)", &[]).unwrap();
    db.query("CREATE TABLE ok3 (x INTEGER PRIMARY KEY ON CONFLICT ABORT)", &[]).unwrap();
    // The accepted clause really is inert: the constraint still fires.
    db.query("INSERT INTO ok1 (x) VALUES (1)", &[]).unwrap();
    assert!(db.query("INSERT INTO ok1 (x) VALUES (1)", &[]).is_err());

    for (n, ddl) in [
        "CREATE TABLE bad1 (x INTEGER, UNIQUE (x) ON CONFLICT ROLLBACK)",
        "CREATE TABLE bad2 (x INTEGER UNIQUE ON CONFLICT IGNORE)",
        "CREATE TABLE bad3 (x INTEGER NOT NULL ON CONFLICT REPLACE)",
        "CREATE TABLE bad4 (x INTEGER PRIMARY KEY ON CONFLICT FAIL)",
    ]
    .iter()
    .enumerate()
    {
        let msg = db.query(ddl, &[]).unwrap_err().to_string();
        assert!(msg.contains("ON CONFLICT"), "{n}: {msg}");
        assert!(msg.contains("is not supported"), "{n}: {msg}");
    }
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
