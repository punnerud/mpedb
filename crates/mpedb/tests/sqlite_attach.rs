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

    // Exclusive lo + inclusive hi — the 0xFF-suffixed bound forms, where the
    // effective inclusivity flips in the decoder (regression: the flag used
    // to carry through unflipped, turning `> 5` into `>= 5` and `<= 10`
    // into `< 10`).
    let got = rows(at.query("SELECT id FROM users WHERE id > 5 AND id <= 10 ORDER BY id", &[]).unwrap());
    assert_eq!(
        got.iter().map(|r| match &r[0] { Value::Int(i) => *i, _ => panic!() }).collect::<Vec<_>>(),
        vec![6, 7, 8, 9, 10]
    );

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

/// A base table with a CHECK constraint ATTACHES (task #102 — it used to be a
/// blanket named refusal); only a CHECK mpedb genuinely cannot compile skips
/// its table — PER TABLE, naming the function — while the rest stay attached.
#[test]
fn check_constraints_compile_or_skip_per_table() {
    let p = std::env::temp_dir()
        .join("mpedb-attach-tests")
        .join(format!("at-chk-{}.db", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&p);
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = DELETE;
         CREATE TABLE ok (id INTEGER PRIMARY KEY, age INTEGER CHECK (age >= 0),
                          CONSTRAINT named CHECK (length('x') = 1));
         CREATE TABLE bad (id INTEGER PRIMARY KEY, v TEXT CHECK (glob('a*', v)));
         INSERT INTO ok VALUES (1, 30);",
    )
    .unwrap();
    drop(c);

    let at = SqliteAttach::open(&p).unwrap();
    // `bad` is the ONLY skip, and the reason names the missing function.
    assert_eq!(at.skipped().len(), 1, "{:?}", at.skipped());
    assert_eq!(at.skipped()[0].0, "bad");
    assert!(at.skipped()[0].1.contains("glob"), "{:?}", at.skipped()[0]);
    assert!(at.skipped()[0].1.contains("CHECK"), "{:?}", at.skipped()[0]);
    // `ok` is attached and queryable, its CHECK sources carried on the schema.
    let got = rows(at.query("SELECT age FROM ok WHERE id = 1", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(30)]]);

    let _ = std::fs::remove_file(&p);
}

/// Plan §6: a `CREATE VIRTUAL TABLE` row is CATALOG, never data. Before this,
/// rootpage 0 (every vtab) aborted the whole scan as "corrupt" — refusing
/// files sqlite opens — and a crafted POSITIVE rootpage was read as a rowid
/// tree, a wrong answer (sqlite ignores a vtab's root entirely, measured).
/// The vtab is a NAMED skip; its shadow tables are ordinary tables and
/// attach readable.
#[test]
fn a_virtual_table_is_catalog_only_and_never_aborts_the_scan() {
    let p = std::env::temp_dir()
        .join("mpedb-attach-tests")
        .join(format!("at-vtab-{}.db", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&p);
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = DELETE;
         CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT);
         INSERT INTO a VALUES (1, 'x'), (2, 'y');
         CREATE VIRTUAL TABLE ft USING fts4(example);
         INSERT INTO ft VALUES ('hello world');",
    )
    .unwrap();
    drop(c);

    let at = SqliteAttach::open(&p).unwrap();
    assert!(
        at.skipped().iter().any(|(n, why)| n == "ft" && why.contains("virtual table")),
        "{:?}",
        at.skipped()
    );
    let got = rows(at.query("SELECT v FROM a WHERE id = 2", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("y".into())]]);
    // The vtab's CONTENT is reachable the way fts4 itself stores it: the
    // `<t>_content` shadow table (single-quoted, typeless DDL — the parser
    // must take both).
    let got = rows(at.query("SELECT c0example FROM ft_content", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("hello world".into())]]);

    let _ = std::fs::remove_file(&p);
}

/// Plan §6 twin rules, measured on stock and scoped identically here:
/// an ordinary table with rootpage 0 refuses ALONE ("database disk image is
/// malformed") while its siblings stay readable; a VIEW with a non-zero
/// rootpage poisons the WHOLE database ("malformed database schema (name)").
#[test]
fn rootpage_damage_is_scoped_exactly_like_sqlite() {
    let base = std::env::temp_dir().join("mpedb-attach-tests");
    std::fs::create_dir_all(&base).unwrap();

    // Table with rootpage 0: only that table refuses.
    let p = base.join(format!("at-root0-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = DELETE;
         CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT);
         INSERT INTO a VALUES (1, 'x');
         CREATE TABLE b (id INTEGER PRIMARY KEY, w TEXT);
         INSERT INTO b VALUES (7, 'z');
         PRAGMA writable_schema = ON;
         UPDATE sqlite_master SET rootpage = 0 WHERE name = 'b';",
    )
    .unwrap();
    drop(c);
    let at = SqliteAttach::open(&p).unwrap();
    let got = rows(at.query("SELECT v FROM a WHERE id = 1", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("x".into())]]);
    let err = at.query("SELECT w FROM b", &[]).unwrap_err();
    assert!(format!("{err}").contains("malformed"), "{err}");
    let _ = std::fs::remove_file(&p);

    // View with rootpage != 0: the whole file refuses, sqlite's words.
    let p = base.join(format!("at-viewroot-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = DELETE;
         CREATE TABLE a (id INTEGER PRIMARY KEY, v TEXT);
         CREATE VIEW vw AS SELECT v FROM a;
         PRAGMA writable_schema = ON;
         UPDATE sqlite_master SET rootpage = 2 WHERE name = 'vw';",
    )
    .unwrap();
    drop(c);
    let err = match SqliteAttach::open(&p) {
        Err(e) => e,
        Ok(_) => panic!("a view with a non-zero rootpage must poison the open"),
    };
    assert!(
        format!("{err}").contains("malformed database schema (vw)"),
        "{err}"
    );
    let _ = std::fs::remove_file(&p);
}
