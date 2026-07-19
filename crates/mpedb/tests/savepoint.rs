//! `SAVEPOINT` / `RELEASE` / `ROLLBACK TO` SQL surface, differential-tested
//! against the `sqlite3` CLI (3.45).
//!
//! mpedb already had the engine mechanism: `WriteTxn::savepoint()` captures a
//! cheap COW snapshot and `rollback_to()` restores it (used by the mirror
//! importer for per-row rollback). This suite exercises the SQL surface built on
//! top of it — a full named savepoint STACK on the `WriteSession`, matching
//! sqlite's nesting, shadowing (innermost matching name wins), case-insensitive
//! name matching, and the "no such savepoint" error.
//!
//! Method: every scenario runs the SAME statement sequence inside a transaction
//! through both engines and compares the rows a final SELECT returns. Both sides
//! CONTINUE past a per-statement error (the sqlite3 CLI's default; the mpedb
//! harness ignores the per-statement `Result`), so scenarios can include an
//! intentional failure and assert that `ROLLBACK TO` recovers exactly as sqlite
//! does. Savepoints only exist inside an explicit `WriteSession` (mpedb has no
//! autocommit implicit transaction), so the transaction body is what differs.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database so a panicking test never leaks a `/dev/shm` file.
struct Tmp {
    db: Database,
    path: String,
}
impl Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

const MPEDB_SCHEMA: &str = r#"[[table]]
name = "t"
primary_key = ["x"]
  [[table.column]]
  name = "x"
  type = "int64"
  [[table.column]]
  name = "y"
  type = "int64"
  nullable = true
"#;

const SQLITE_DDL: &str = "CREATE TABLE t(x INTEGER PRIMARY KEY, y INTEGER);\n";

fn dbt() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-savepoint-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{MPEDB_SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

/// Canonical cell rendering matching the `sqlite3` CLI default "list" mode:
/// NULL as empty, integers verbatim.
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("unexpected value in savepoint test: {other:?}"),
    }
}

/// Feed a full script to the bundled sqlite and capture its rows, CONTINUING
/// past per-statement errors (the CLI's default batch behaviour, whose
/// non-zero exit the old subprocess version ignored).
fn sqlite_capture(script: &str) -> Vec<Vec<String>> {
    sqlite_oracle::script_stdout_lenient(script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// sqlite side of a scenario: CREATE, BEGIN, the body (continue on error), then
/// the SELECT. The body must contain no SELECT of its own (only the final SELECT
/// produces rows to compare).
fn sqlite_diff(body: &[&str], select: &str) -> Vec<Vec<String>> {
    let mut s = String::from(SQLITE_DDL);
    s.push_str("BEGIN;\n");
    for b in body {
        s.push_str(b);
        s.push_str(";\n");
    }
    s.push_str(select);
    s.push_str(";\n");
    sqlite_capture(&s)
}

/// mpedb side: open a `WriteSession`, replay the body (continue on error, like
/// the sqlite CLI), run the SELECT, then discard the transaction. Each scenario
/// starts from the empty committed state, so nothing persists between scenarios.
fn mpedb_diff(db: &Database, body: &[&str], select: &str) -> Vec<Vec<String>> {
    let mut s = db.begin().unwrap();
    for b in body {
        let _ = s.query(b, &[]);
    }
    let rows = match s.query(select, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{select}`, got {other:?}"),
    };
    s.rollback();
    rows
}

/// Run a scenario through both engines and assert the SELECT agrees cell-for-cell.
fn assert_diff(body: &[&str], select: &str) -> Vec<Vec<String>> {
    let db = dbt();
    let got = mpedb_diff(&db, body, select);
    let want = sqlite_diff(body, select);
    assert_eq!(got, want, "\nbody = {body:?}\nselect = {select}");
    got
}

const SEL: &str = "SELECT x, y FROM t ORDER BY x";

// ------------------------------------------------------------------- basics

#[test]
fn rollback_to_undoes_since_savepoint_but_keeps_it() {
    // Insert 1 before `a`; 2 after `a`; 3 after `b`. ROLLBACK TO a undoes 2 and
    // 3 but keeps `a` (and everything before it).
    let rows = assert_diff(
        &[
            "INSERT INTO t VALUES(1,10)",
            "SAVEPOINT a",
            "INSERT INTO t VALUES(2,20)",
            "SAVEPOINT b",
            "INSERT INTO t VALUES(3,30)",
            "ROLLBACK TO a",
        ],
        SEL,
    );
    assert_eq!(rows, vec![vec!["1".to_string(), "10".to_string()]]);
}

#[test]
fn rollback_to_the_same_savepoint_twice() {
    // `a` survives the first ROLLBACK TO, so a second one (after more work) still
    // resolves and restores the same point.
    let rows = assert_diff(
        &[
            "INSERT INTO t VALUES(1,10)",
            "SAVEPOINT a",
            "INSERT INTO t VALUES(2,20)",
            "ROLLBACK TO a",
            "INSERT INTO t VALUES(9,90)",
            "ROLLBACK TO a",
        ],
        SEL,
    );
    assert_eq!(rows, vec![vec!["1".to_string(), "10".to_string()]]);
}

#[test]
fn release_merges_changes_into_the_enclosing_scope() {
    // RELEASE keeps the changes (they merge up) and removes the savepoint.
    let rows = assert_diff(
        &[
            "INSERT INTO t VALUES(1,1)",
            "SAVEPOINT a",
            "INSERT INTO t VALUES(2,2)",
            "RELEASE a",
        ],
        SEL,
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), "1".to_string()],
            vec!["2".to_string(), "2".to_string()],
        ]
    );
}

#[test]
fn syntax_variants_release_savepoint_and_rollback_transaction_to_savepoint() {
    assert_diff(
        &[
            "INSERT INTO t VALUES(1,1)",
            "SAVEPOINT a",
            "INSERT INTO t VALUES(2,2)",
            "RELEASE SAVEPOINT a",
            "SAVEPOINT b",
            "INSERT INTO t VALUES(3,3)",
            "ROLLBACK TRANSACTION TO SAVEPOINT b",
        ],
        SEL,
    );
}

// ----------------------------------------------------------------- nesting

#[test]
fn shadowed_name_resolves_innermost_then_the_outer() {
    // Two savepoints both named `a`. ROLLBACK TO a hits the INNER one.
    assert_diff(
        &[
            "SAVEPOINT a",
            "INSERT INTO t VALUES(1,1)",
            "SAVEPOINT a",
            "INSERT INTO t VALUES(2,2)",
            "ROLLBACK TO a",
        ],
        SEL,
    );
    // After RELEASE of the inner `a`, ROLLBACK TO a reaches the OUTER `a` and
    // undoes the first insert too.
    let rows = assert_diff(
        &[
            "SAVEPOINT a",
            "INSERT INTO t VALUES(1,1)",
            "SAVEPOINT a",
            "INSERT INTO t VALUES(2,2)",
            "ROLLBACK TO a",
            "RELEASE a",
            "ROLLBACK TO a",
        ],
        SEL,
    );
    assert!(rows.is_empty(), "outer rollback should empty the table, got {rows:?}");
}

#[test]
fn deep_nesting_partial_unwind() {
    assert_diff(
        &[
            "SAVEPOINT s1",
            "INSERT INTO t VALUES(1,1)",
            "SAVEPOINT s2",
            "INSERT INTO t VALUES(2,2)",
            "SAVEPOINT s3",
            "INSERT INTO t VALUES(3,3)",
            "SAVEPOINT s4",
            "INSERT INTO t VALUES(4,4)",
            "ROLLBACK TO s2",
            "INSERT INTO t VALUES(5,5)",
        ],
        SEL,
    );
}

// ------------------------------------------------------------ error recovery

#[test]
fn rollback_to_recovers_from_a_failed_statement() {
    // A duplicate-key INSERT fails; mpedb detects it before mutating, so the
    // session is not poisoned and ROLLBACK TO recovers — exactly as sqlite does
    // (both undo the successful insert since `a`, then insert 2).
    let rows = assert_diff(
        &[
            "SAVEPOINT a",
            "INSERT INTO t VALUES(1,1)",
            "INSERT INTO t VALUES(1,2)", // PK violation — error, no effect
            "ROLLBACK TO a",
            "INSERT INTO t VALUES(2,2)",
        ],
        SEL,
    );
    assert_eq!(rows, vec![vec!["2".to_string(), "2".to_string()]]);
}

#[test]
fn case_insensitive_and_null_values_round_trip() {
    // `RELEASE foo` matches `SAVEPOINT Foo`; NULL y renders as empty on both.
    let rows = assert_diff(
        &[
            "SAVEPOINT Foo",
            "INSERT INTO t (x) VALUES(1)",
            "INSERT INTO t VALUES(2, NULL)",
            "RELEASE foo",
        ],
        SEL,
    );
    assert_eq!(
        rows,
        vec![
            vec!["1".to_string(), String::new()],
            vec!["2".to_string(), String::new()],
        ]
    );
}

// -------------------------------------------------------- unknown-name errors

#[test]
fn unknown_savepoint_is_an_error_matching_sqlite() {
    let db = dbt();
    let mut s = db.begin().unwrap();
    s.query("SAVEPOINT a", &[]).unwrap();

    for stmt in ["ROLLBACK TO nope", "RELEASE nope"] {
        let e = s.query(stmt, &[]).expect_err("unknown savepoint must error");
        let msg = e.to_string();
        assert!(
            msg.contains("no such savepoint: nope"),
            "expected sqlite's message, got `{msg}` for `{stmt}`"
        );
    }
    // sqlite emits the same text.
    let script = format!(
        "{SQLITE_DDL}BEGIN;\nSAVEPOINT a;\nROLLBACK TO nope;\n"
    );
    let err = sqlite_oracle::try_script_stdout(&script, "")
        .expect_err("sqlite must reject the unknown savepoint name");
    assert!(
        err.contains("no such savepoint: nope"),
        "sqlite should also reject the unknown name, got `{err}`"
    );
    s.rollback();
}

// ------------------------------------------------ BEGIN/COMMIT/ROLLBACK interplay

#[test]
fn commit_persists_the_state_after_release() {
    let db = dbt();
    {
        let mut s = db.begin().unwrap();
        s.query("INSERT INTO t VALUES(1,1)", &[]).unwrap();
        s.query("SAVEPOINT a", &[]).unwrap();
        s.query("INSERT INTO t VALUES(2,2)", &[]).unwrap();
        s.query("SAVEPOINT b", &[]).unwrap();
        s.query("INSERT INTO t VALUES(3,3)", &[]).unwrap();
        s.query("ROLLBACK TO b", &[]).unwrap(); // drop 3
        s.query("RELEASE a", &[]).unwrap(); // keep 1,2
        s.commit().unwrap();
    }
    // Committed state: 1 and 2 survive, 3 was rolled back.
    match db.query(SEL, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            let got: Vec<Vec<String>> = rows
                .iter()
                .map(|r| r.iter().map(render).collect())
                .collect();
            assert_eq!(
                got,
                vec![
                    vec!["1".to_string(), "1".to_string()],
                    vec!["2".to_string(), "2".to_string()],
                ]
            );
        }
        other => panic!("expected rows, got {other:?}"),
    }
    // Page accounting must stay exact through savepoint churn + commit.
    db.verify().unwrap();
}

#[test]
fn full_rollback_discards_all_savepoints() {
    let db = dbt();
    {
        let mut s = db.begin().unwrap();
        s.query("SAVEPOINT a", &[]).unwrap();
        s.query("INSERT INTO t VALUES(1,1)", &[]).unwrap();
        s.query("SAVEPOINT b", &[]).unwrap();
        s.query("INSERT INTO t VALUES(2,2)", &[]).unwrap();
        s.rollback(); // whole txn gone, savepoints with it
    }
    match db.query(SEL, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert!(rows.is_empty(), "rollback should discard all"),
        other => panic!("expected rows, got {other:?}"),
    }
    db.verify().unwrap();
}

// ----------------------------------------------------- refusals (documented)

#[test]
fn nested_begin_is_refused() {
    let db = dbt();
    let mut s = db.begin().unwrap();
    assert!(
        s.query("BEGIN", &[]).is_err(),
        "BEGIN inside a session is refused (already a transaction)"
    );
    s.rollback();
}

#[test]
fn savepoint_without_a_transaction_is_a_clean_error() {
    // mpedb has no autocommit implicit transaction, so a bare SAVEPOINT through
    // the autocommit facade is refused (never silently mis-handled). Inside a
    // WriteSession it works — that is what every other test uses.
    let db = dbt();
    for stmt in ["SAVEPOINT a", "RELEASE a", "ROLLBACK TO a"] {
        assert!(
            db.query(stmt, &[]).is_err(),
            "`{stmt}` through autocommit must be a clean error"
        );
    }
}

// ------------------------------------------------------ page-accounting churn

#[test]
fn heavy_allocation_inside_savepoints_keeps_accounting_exact() {
    // Allocate many pages inside savepoints, roll back to reclaim them, do it
    // repeatedly, then commit — the engine verifier proves the freelist/high-
    // water accounting survived the repeated snapshot/restore.
    let db = dbt();
    {
        let mut s = db.begin().unwrap();
        s.query("SAVEPOINT base", &[]).unwrap();
        for round in 0..3 {
            s.query("SAVEPOINT churn", &[]).unwrap();
            for i in 0..200 {
                let x = round * 1000 + i;
                s.query(&format!("INSERT INTO t VALUES({x},{x})"), &[]).unwrap();
            }
            // Undo the whole round; `churn` stays for the next round.
            s.query("ROLLBACK TO churn", &[]).unwrap();
        }
        // Nothing from the rounds survives; keep one committed row.
        s.query("INSERT INTO t VALUES(7,7)", &[]).unwrap();
        s.query("RELEASE base", &[]).unwrap();
        s.commit().unwrap();
    }
    match db.query("SELECT count(*) FROM t", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(render(&rows[0][0]), "1"),
        other => panic!("expected rows, got {other:?}"),
    }
    db.verify().unwrap();
}
