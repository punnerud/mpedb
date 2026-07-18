//! #73 §3 stage 1 — UNCORRELATED nested subqueries. A subquery may now CONTAIN
//! subqueries: `IN (… IN (…))`, `EXISTS (… EXISTS (…))`, and a scalar whose body
//! holds another scalar. The recursive `SubPlan` fills each inner lift ONCE,
//! bottom-up, before the enclosing subplan runs. Every expected value below is
//! cross-checked against the `sqlite3` CLI (3.45), which has the same semantics.
//!
//! Correlation to the IMMEDIATE parent (stage 2) now WORKS and is cross-checked
//! in `nested_correlated.rs`. What STAYS refused here: a nested subquery that
//! correlates to a MIDDLE/OUTER scope, skipping its parent (stage 3) — it must
//! refuse with a clean message, never a wrong answer.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database so a panicking test does not leak a `/dev/shm` file.
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

/// Three tables of plain integers so mpedb's rigid typing and sqlite's loose
/// typing agree cell-for-cell. `a.x`, `c.w` are nullable to exercise the IN/3VL
/// path through a nested list.
const SCHEMA: &str = r#"[[table]]
name = "a"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "x"
  type = "int64"
  nullable = true

[[table]]
name = "b"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "y"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "z"
  type = "int64"
  nullable = true

[[table]]
name = "c"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "w"
  type = "int64"
  nullable = true
"#;

fn insert_statements() -> Vec<&'static str> {
    vec![
        "INSERT INTO a (id, x) VALUES (1,10),(2,20),(3,30),(4,99),(5,NULL)",
        "INSERT INTO b (id, y, z) VALUES (1,10,5),(2,20,15),(3,30,99),(4,40,25)",
        "INSERT INTO c (id, w) VALUES (1,5),(2,15),(3,25),(4,NULL)",
    ]
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-nested-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

/// Canonical cell rendering, matching the `sqlite3` CLI default "list" mode:
/// NULL as empty, integers verbatim. (Every query below outputs only integers.)
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        other => panic!("unexpected value in nested-subquery test: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.into_iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Run schema + data + one query through the `sqlite3` CLI and parse its
/// default list-mode output into rows.
fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, x INTEGER);\n\
         CREATE TABLE b (id INTEGER PRIMARY KEY, y INTEGER, z INTEGER);\n\
         CREATE TABLE c (id INTEGER PRIMARY KEY, w INTEGER);\n",
    );
    for stmt in insert_statements() {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the sqlite3 CLI (3.45) must be on PATH for this cross-check");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// A battery of uncorrelated nested forms — IN-in-IN, EXISTS-in-EXISTS, a scalar
/// whose body holds a scalar, and a nested scalar consumed as a PK point — each
/// must match sqlite 3.45 cell-for-cell.
#[test]
fn nested_uncorrelated_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // IN inside IN: grandchild list feeds the middle list feeds the outer.
        "SELECT id FROM a WHERE x IN (SELECT y FROM b WHERE z IN (SELECT w FROM c)) ORDER BY id",
        // NOT IN over the nested list, with a filtered grandchild — exercises the
        // IN/3VL path (a NULL outer key and a NULL in the grandchild list).
        "SELECT id FROM a \
         WHERE x NOT IN (SELECT y FROM b WHERE z IN (SELECT w FROM c WHERE w <> 15)) ORDER BY id",
        // EXISTS inside EXISTS — non-empty grandchild ⇒ every outer row.
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c)) ORDER BY id",
        // …and the empty innermost ⇒ no outer rows (the whole chain collapses).
        "SELECT id FROM a \
         WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE w > 1000)) ORDER BY id",
        // Scalar whose body holds a scalar (avg, coerced): the task's canonical
        // `(SELECT max(y) FROM b WHERE y < (SELECT avg(w) FROM c))`.
        "SELECT id FROM a \
         WHERE x = (SELECT max(y) FROM b WHERE y < (SELECT avg(w) FROM c)) ORDER BY id",
        // The same nested scalar, FROM-less, as the sole projection.
        "SELECT (SELECT max(y) FROM b WHERE y < (SELECT avg(w) FROM c))",
        // A nested scalar (count) consumed as the outer PK point — the child's
        // `slot_type` has to type the inner PkPoint's key part.
        "SELECT id FROM a WHERE id = (SELECT count(*) FROM b WHERE z IN (SELECT w FROM c)) ORDER BY id",
        // A nested scalar consumed as an INNER PK point (inside the middle
        // subplan), the middle then feeding the outer list.
        "SELECT id FROM a \
         WHERE x IN (SELECT y FROM b WHERE id = (SELECT count(*) FROM c WHERE w < 20)) ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// Direct `Value` assertions on the two canonical shapes, so the behavior is
/// pinned independently of the string cross-check.
#[test]
fn nested_scalar_and_exists_direct() {
    let d = db();
    let one = |sql: &str| -> Value {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };
    // (SELECT max(y) FROM b WHERE y < avg(w)=15) = max({10}) = 10.
    assert_eq!(
        one("SELECT (SELECT max(y) FROM b WHERE y < (SELECT avg(w) FROM c))"),
        Value::Int(10)
    );
    // count(*) over the nested-list filter = |{5,15,25}| = 3.
    assert_eq!(
        one("SELECT (SELECT count(*) FROM b WHERE z IN (SELECT w FROM c))"),
        Value::Int(3)
    );
}

/// The refusal boundary that STAYS: a nested subquery that CORRELATES to a
/// MIDDLE/OUTER scope, skipping its immediate parent (stage 3), must refuse
/// cleanly — never misexecute. (Immediate-parent correlation, stage 2, now
/// works — see `nested_correlated.rs`.)
#[test]
fn mid_scope_correlated_nested_is_refused() {
    let d = db();

    // The innermost references a MIDDLE/OUTER scope (`a.x`), skipping its parent
    // (`b`). Refused — the scope stack is not built, so `a.x` surfaces as an
    // unresolved name in the innermost bind, and no wrong answer escapes.
    let err = d
        .query(
            "SELECT id FROM a \
             WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE c.w = a.x))",
            &[],
        )
        .unwrap_err();
    // Any refusal is acceptable here; assert only that it is NOT silently run.
    assert!(
        !err.to_string().is_empty(),
        "stage-3 mid-scope correlation must refuse, got empty error"
    );

    d.verify().unwrap();
}
