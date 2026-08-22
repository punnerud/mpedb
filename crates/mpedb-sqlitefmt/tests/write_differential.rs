//! The writer's fence, pointing the OTHER way from `differential.rs`: every
//! image `write_image` emits is handed to the real sqlite — the CLI must
//! pass `PRAGMA integrity_check`, `.dump` must carry the CREATE text and the
//! rows, and (via rusqlite, the always-present bundled library) every value
//! must read back identically. The CLI half self-gates on `sqlite3` being on
//! PATH; the library half always runs.

use mpedb_sqlitefmt::{write_image, ImageTable, Value};
use rusqlite::Connection;
use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str, img: &[u8]) -> PathBuf {
    let p = mpedb_testkit::scratch_base()
        .join("mpedb-sqlitefmt-tests")
        .join(format!("{name}-{}.db", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, img).unwrap();
    p
}

fn have_cli() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run one statement (or dot-command) through the sqlite3 CLI, panicking on
/// a non-zero exit — a refused image must fail the test, not vanish.
fn cli(path: &PathBuf, arg: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(path)
        .arg(arg)
        .output()
        .expect("run sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 {arg} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 CLI output")
}

fn lib_value(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef as V;
    match v {
        V::Null => Value::Null,
        V::Integer(i) => Value::Int(i),
        V::Real(f) => Value::Float(f),
        V::Text(t) => Value::Text(std::str::from_utf8(t).unwrap().to_string()),
        V::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// Every row of `t`, read back through the LIBRARY in rowid order, must be
/// value-for-value what the writer was given.
fn assert_lib_reads_back(conn: &Connection, t: &ImageTable) {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM \"{}\" ORDER BY rowid", t.name))
        .unwrap();
    let n = stmt.column_count();
    let rows: Vec<Vec<Value>> = stmt
        .query_map([], |r| {
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(lib_value(r.get_ref(i).unwrap()));
            }
            Ok(out)
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows, t.rows, "table `{}` through the library", t.name);
}

fn verify(name: &str, tables: &[ImageTable]) -> PathBuf {
    let img = write_image(tables, 4096).unwrap();
    let path = scratch(name, &img);

    // Library half — always runs.
    let conn = Connection::open(&path).unwrap();
    for t in tables {
        assert_lib_reads_back(&conn, t);
    }
    drop(conn);

    // CLI half — the stock binary's own verdict.
    if !have_cli() {
        eprintln!("sqlite3 CLI not on PATH — skipping the CLI half of {name}");
        return path;
    }
    assert_eq!(cli(&path, "PRAGMA integrity_check").trim(), "ok", "{name}");
    let dump = cli(&path, ".dump");
    for t in tables {
        assert!(dump.contains(&t.sql), "{name}: .dump lost CREATE text of `{}`", t.name);
    }
    path
}

/// The byte-exact facit table also has to satisfy stock end-to-end: 2 pages,
/// integrity ok, both rows in the dump.
#[test]
fn foo_facit_through_stock() {
    let foo = ImageTable {
        name: "foo".into(),
        sql: "CREATE TABLE foo (a, b)".into(),
        rows: vec![
            vec![Value::Int(1), Value::Text("x".into())],
            vec![Value::Int(2), Value::Text("y".into())],
        ],
        indexes: Vec::new(),
    };
    let path = verify("wfoo", &[foo]);
    if !have_cli() {
        return;
    }
    assert_eq!(cli(&path, "PRAGMA page_count").trim(), "2");
    let dump = cli(&path, ".dump");
    assert!(dump.contains("INSERT INTO foo VALUES(1,'x');"), "{dump}");
    assert!(dump.contains("INSERT INTO foo VALUES(2,'y');"), "{dump}");
}

/// The shapes v1 claims: an empty table, several tables in one image, every
/// value class (NULL / float / blob / empty string / negative and 8-byte
/// ints), a payload at exactly the 4061-byte X ceiling, and a quoted
/// `'PRIMARY KEY'` default that must survive the DDL sniff AND stock's
/// parser.
#[test]
fn shapes_through_stock() {
    let tables = vec![
        ImageTable {
            name: "empty".into(),
            sql: "CREATE TABLE empty (a, b, c)".into(),
            rows: vec![],
            indexes: Vec::new(),
        },
        ImageTable {
            name: "vals".into(),
            sql: "CREATE TABLE vals (v)".into(),
            rows: vec![
                vec![Value::Null],
                vec![Value::Float(1.5)],
                vec![Value::Float(-2.75e300)],
                vec![Value::Blob(vec![0, 1, 2, 0xff])],
                vec![Value::Blob(Vec::new())],
                vec![Value::Text(String::new())],
                vec![Value::Int(-1)],
                vec![Value::Int(-129)],
                vec![Value::Int(1 << 40)],
                vec![Value::Int(i64::MIN)],
                vec![Value::Int(i64::MAX)],
            ],
            indexes: Vec::new(),
        },
        ImageTable {
            // Alone in its tree: the near-X cell fills the whole leaf.
            name: "big".into(),
            sql: "CREATE TABLE big (t)".into(),
            rows: vec![vec![Value::Text("x".repeat(4058))]],
            indexes: Vec::new(),
        },
        ImageTable {
            name: "wide".into(),
            sql: r#"CREATE TABLE wide ("key" INTEGER, txt TEXT DEFAULT 'PRIMARY KEY')"#.into(),
            rows: vec![
                vec![Value::Int(7), Value::Text("a".into())],
                vec![Value::Int(0), Value::Null],
            ],
            indexes: Vec::new(),
        },
    ];
    let path = verify("wshapes", &tables);
    if !have_cli() {
        return;
    }
    assert_eq!(cli(&path, "PRAGMA page_count").trim(), "5");
    let dump = cli(&path, ".dump");
    assert!(dump.contains("INSERT INTO vals VALUES(NULL);"), "{dump}");
    assert!(dump.contains("INSERT INTO vals VALUES(1.5);"), "{dump}");
    assert!(dump.contains("INSERT INTO vals VALUES(-9223372036854775808);"), "{dump}");
    assert!(dump.contains("INSERT INTO wide VALUES(7,'a');"), "{dump}");
    // The near-X text arrives whole through the CLI too.
    assert_eq!(
        cli(&path, "SELECT length(t) FROM big").trim(),
        "4058"
    );
}

/// Zero tables is a legal image: stock opens it, sees no schema, calls it ok.
#[test]
fn zero_tables_through_stock() {
    let img = write_image(&[], 4096).unwrap();
    let path = scratch("wzero", &img);
    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    drop(conn);
    if have_cli() {
        assert_eq!(cli(&path, "PRAGMA integrity_check").trim(), "ok");
        assert_eq!(cli(&path, "PRAGMA page_count").trim(), "1");
    }
}
