//! v1 query-attach differential: SQL answers over a real sqlite file, native
//! path vs the sqlite library, plus the named refusals.

use mpedb::{SqliteAttach, Value};
use rusqlite::Connection;

fn setup() -> std::path::PathBuf {
    let p = std::env::temp_dir()
        .join("mpedb-attach-tests")
        .join(format!("at-{}.db", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&p);
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = DELETE;
         CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         CREATE TABLE logs (msg TEXT);  -- no int pk: synthetic rowid
         CREATE TABLE wr (k INTEGER PRIMARY KEY, v TEXT) WITHOUT ROWID;",
    )
    .unwrap();
    for i in 0..500i64 {
        c.execute(
            "INSERT INTO users VALUES (?, ?, ?)",
            rusqlite::params![i, format!("u{i}"), 20 + i % 50],
        )
        .unwrap();
        if i % 3 == 0 {
            c.execute("INSERT INTO logs VALUES (?)", rusqlite::params![format!("m{i}")])
                .unwrap();
        }
        if i % 5 == 0 {
            c.execute("INSERT INTO wr VALUES (?, ?)", rusqlite::params![i, format!("w{i}")])
                .unwrap();
        }
    }
    drop(c);
    p
}

fn rows(r: mpedb::ExecResult) -> Vec<Vec<Value>> {
    match r {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn answers_match_the_library() {
    let p = setup();
    let at = SqliteAttach::open(&p).unwrap();
    assert!(at.skipped().is_empty(), "{:?}", at.skipped());
    let lib = Connection::open(&p).unwrap();

    // Point probe (PkPoint through the planner → seek_rowid underneath).
    let got = rows(at.query("SELECT name FROM users WHERE id = 123", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("u123".into())]]);

    // Range + residual + ORDER BY + LIMIT.
    let got = rows(
        at.query(
            "SELECT id FROM users WHERE id >= 10 AND id < 20 AND CAST(age AS INTEGER) > 21 ORDER BY id DESC LIMIT 3",
            &[],
        )
        .unwrap(),
    );
    let expect: Vec<i64> = {
        let mut s = lib
            .prepare("SELECT id FROM users WHERE id >= 10 AND id < 20 AND age > 21 ORDER BY id DESC LIMIT 3")
            .unwrap();
        let v: Vec<i64> = s.query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
        v
    };
    assert_eq!(got.iter().map(|r| match &r[0] { Value::Int(i) => *i, _ => panic!() }).collect::<Vec<_>>(), expect);

    // Aggregate over the whole table.
    let got = rows(at.query("SELECT count(*), min(id), max(id) FROM users", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(500), Value::Int(0), Value::Int(499)]]);

    // Synthetic-rowid table: count + rowid probe.
    let got = rows(at.query("SELECT count(*) FROM logs", &[]).unwrap());
    let n: i64 = lib.query_row("SELECT count(*) FROM logs", [], |r| r.get(0)).unwrap();
    assert_eq!(got, vec![vec![Value::Int(n)]]);
    let got = rows(at.query("SELECT msg FROM logs WHERE rowid = 1", &[]).unwrap());
    let m: String = lib.query_row("SELECT msg FROM logs WHERE rowid = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(got, vec![vec![Value::Text(m)]]);

    // WITHOUT ROWID with int PK.
    let got = rows(at.query("SELECT v FROM wr WHERE k = 45", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("w45".into())]]);

    // Join between two attached tables.
    let got = rows(
        at.query("SELECT count(*) FROM users JOIN wr ON users.id = wr.k", &[]).unwrap(),
    );
    let n: i64 = lib
        .query_row("SELECT count(*) FROM users JOIN wr ON users.id = wr.k", [], |r| r.get(0))
        .unwrap();
    assert_eq!(got, vec![vec![Value::Int(n)]]);

    // Read-only: writes are refused by name.
    let err = at.query("INSERT INTO logs (msg, rowid) VALUES ('nei', 999)", &[]).unwrap_err();
    assert!(format!("{err}").contains("read-only"), "{err}");

    let _ = std::fs::remove_file(&p);
}
