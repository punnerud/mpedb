//! `ANALYZE` and `REINDEX` are accepted no-ops. sqlite's `ANALYZE [name]`
//! gathers optimizer statistics and `REINDEX [name]` rebuilds indexes; mpedb's
//! planner is rule-based (PK > unique > non-unique index > scan — no statistics)
//! and maintains every index eagerly on each write, so both have nothing to do
//! but must SUCCEED so tools/migrations that emit them don't break. Each form is
//! cross-checked against sqlite 3.45 (via the bundled `rusqlite`), which also
//! accepts them. Routed through the facade `query()` path — never a plan.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open(name: &str) -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-analyze-{name}-{}-{}.mpedb",
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
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

/// A no-op DDL returns `Affected(0)` (no rows, success).
fn assert_noop(res: ExecResult) {
    assert_eq!(res, ExecResult::Affected(0), "expected a no-op success");
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn analyze_and_reindex_are_accepted_noops_and_table_stays_queryable() {
    let (db, path) = open("noop");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT)", &[]).unwrap();
    for id in 1..=10 {
        db.query(&format!("INSERT INTO t (id, a, b) VALUES ({id}, {}, 'r{id}')", id % 3), &[])
            .unwrap();
    }
    db.query("CREATE INDEX idx_a ON t (a)", &[]).unwrap();

    // All four forms succeed as no-ops.
    assert_noop(db.query("ANALYZE", &[]).unwrap());
    assert_noop(db.query("ANALYZE t", &[]).unwrap());
    assert_noop(db.query("REINDEX", &[]).unwrap());
    assert_noop(db.query("REINDEX t", &[]).unwrap());
    // Lenient: a name that does not exist is still accepted (never a wrong
    // answer), as is a bare index name and a trailing semicolon.
    assert_noop(db.query("ANALYZE no_such_thing", &[]).unwrap());
    assert_noop(db.query("REINDEX idx_a", &[]).unwrap());
    assert_noop(db.query("analyze;", &[]).unwrap());

    // The table (and its index) are completely unaffected — still queryable and
    // still returning the same correct answers after every ANALYZE/REINDEX.
    let got = rows(db.query("SELECT id FROM t WHERE a = 1 ORDER BY id", &[]).unwrap());
    assert_eq!(
        got,
        vec![vec![Value::Int(1)], vec![Value::Int(4)], vec![Value::Int(7)], vec![Value::Int(10)]]
    );
    assert_eq!(
        rows(db.query("SELECT count(*) FROM t", &[]).unwrap()),
        vec![vec![Value::Int(10)]]
    );
    // Writes still work afterwards (the index keeps being maintained eagerly).
    db.query("INSERT INTO t (id, a, b) VALUES (11, 1, 'r11')", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT count(*) FROM t WHERE a = 1", &[]).unwrap()),
        vec![vec![Value::Int(5)]]
    );
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// A column/identifier literally named `analyze`/`reindex` is unaffected — the
/// DDL words are positional identifiers, not reserved keywords, so a query that
/// merely mentions them routes to the ordinary plan path, not to DDL.
#[test]
fn analyze_reindex_are_not_reserved_words() {
    let (db, path) = open("ident");
    // Used as an alias — proves `analyze` is a plain identifier.
    let got = rows(db.query("SELECT 7 AS analyze", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(7)]]);
    let got = rows(db.query("SELECT 9 AS reindex", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(9)]]);
    let _ = std::fs::remove_file(&path);
}

/// Cross-check: sqlite 3.45 also accepts `ANALYZE`, `ANALYZE <name>`, `REINDEX`,
/// and `REINDEX <table>` as successful statements — the behavior mpedb mirrors.
#[test]
fn sqlite_also_accepts_analyze_and_reindex() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT);
         CREATE INDEX idx_a ON t(a);
         INSERT INTO t (id, a, b) VALUES (1, 1, 'x'), (2, 2, 'y');",
    )
    .unwrap();
    // Each of these must succeed in sqlite (no error), matching mpedb.
    conn.execute_batch("ANALYZE;").unwrap();
    conn.execute_batch("ANALYZE t;").unwrap();
    conn.execute_batch("REINDEX;").unwrap();
    conn.execute_batch("REINDEX t;").unwrap();
    // sqlite keeps the table queryable afterwards, exactly like mpedb.
    let n: i64 = conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
}
