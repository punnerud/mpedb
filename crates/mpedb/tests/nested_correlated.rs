//! #73 §3 stage 2 — a nested subquery CORRELATED to its IMMEDIATE parent. The
//! innermost subquery may reference the row of the subquery that directly
//! encloses it (one level out); the executor fills that correlated slot per
//! PARENT row, exactly as the top level fills its correlated subplans per outer
//! row. No PLAN_FORMAT bump — the recursive `SubPlan` (format 20) already carries
//! per-level `outer_args`/`sub_base` and a parent `post_filter`.
//!
//! Every expected value is cross-checked against the `sqlite3` CLI (3.45), which
//! has the same correlated semantics. The task's canonical shape is here:
//!   `… WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g
//!                    AND EXISTS (SELECT 1 FROM c WHERE c.m = b.k))`
//! — the inner correlates to `b` (its immediate parent), which itself correlates
//! to the outer `a`.
//!
//! Correlation to a MIDDLE/OUTER scope (stage 3) now also works — see the last
//! test here and the dedicated `nested_midscope.rs`.

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

/// Three integer tables so mpedb's rigid typing and sqlite's loose typing agree
/// cell-for-cell. `a.g`, `b.k`, `b.w`, `c.m` are nullable to exercise the 3VL
/// `= NULL` path through correlation.
const SCHEMA: &str = r#"[[table]]
name = "a"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "g"
  type = "int64"
  nullable = true

[[table]]
name = "b"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "k"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "w"
  type = "int64"
  nullable = true

[[table]]
name = "c"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "m"
  type = "int64"
  nullable = true
"#;

fn insert_statements() -> Vec<&'static str> {
    vec![
        "INSERT INTO a (id, g) VALUES (1,10),(2,20),(3,30),(4,NULL)",
        "INSERT INTO b (id, k, w) VALUES (1,10,100),(2,20,200),(3,30,100),(4,10,999)",
        "INSERT INTO c (id, m) VALUES (1,10),(2,200),(3,100),(4,NULL)",
    ]
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-nestcorr-{}-{}.mpedb",
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
        other => panic!("unexpected value in nested-correlated test: {other:?}"),
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
        "CREATE TABLE a (id INTEGER PRIMARY KEY, g INTEGER);\n\
         CREATE TABLE b (id INTEGER PRIMARY KEY, k INTEGER, w INTEGER);\n\
         CREATE TABLE c (id INTEGER PRIMARY KEY, m INTEGER);\n",
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

/// A battery of nested subqueries whose innermost level correlates to its
/// IMMEDIATE parent — EXISTS-in-EXISTS, NOT EXISTS, correlated scalar aggregates
/// (count/max), and a correlated scalar in the SELECT list — each matching
/// sqlite 3.45 cell-for-cell.
#[test]
fn nested_correlated_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // The canonical shape: inner→b (immediate parent), b→a (outer).
        "SELECT id FROM a \
         WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g \
                       AND EXISTS (SELECT 1 FROM c WHERE c.m = b.k)) ORDER BY id",
        // The inner correlates to a DIFFERENT parent column (b.w) than the one
        // the middle uses for its own correlation (b.k) — the per-level outer_args
        // must not get crossed.
        "SELECT id FROM a \
         WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g \
                       AND EXISTS (SELECT 1 FROM c WHERE c.m = b.w)) ORDER BY id",
        // NOT EXISTS over the same nested-correlated body.
        "SELECT id FROM a \
         WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.g \
                           AND EXISTS (SELECT 1 FROM c WHERE c.m = b.k)) ORDER BY id",
        // Correlated scalar AGGREGATE (count) whose body has a correlated child:
        // the §1 correlated-aggregate path, one level down.
        "SELECT id FROM a \
         WHERE (SELECT count(*) FROM b WHERE b.k = a.g \
                AND EXISTS (SELECT 1 FROM c WHERE c.m = b.w)) > 0 ORDER BY id",
        // Correlated scalar aggregate (max) compared to a constant.
        "SELECT id FROM a \
         WHERE (SELECT max(b.w) FROM b WHERE b.k = a.g \
                AND EXISTS (SELECT 1 FROM c WHERE c.m = b.k)) >= 100 ORDER BY id",
        // Correlated scalar in the SELECT LIST (no outer WHERE), body correlated:
        // the top projection reads a correlated slot whose subplan itself has a
        // correlated child.
        "SELECT a.id, (SELECT count(*) FROM b WHERE b.k = a.g \
                       AND EXISTS (SELECT 1 FROM c WHERE c.m = b.w)) AS n \
         FROM a ORDER BY a.id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// Direct `Value` assertions on the two canonical shapes, so the behavior is
/// pinned independently of the string cross-check.
#[test]
fn nested_correlated_direct() {
    let d = db();
    let ids = |sql: &str| -> Vec<i64> {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| match r.into_iter().next().unwrap() {
                    Value::Int(i) => i,
                    other => panic!("{other:?}"),
                })
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    // a1(g=10): b with k=10 exists (b1,b4) and c has m=10 ⇒ a1. a2/a3 have no
    // c matching their b.k; a4's g is NULL. Only {1}.
    assert_eq!(
        ids("SELECT id FROM a \
             WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g \
                           AND EXISTS (SELECT 1 FROM c WHERE c.m = b.k)) ORDER BY id"),
        vec![1]
    );
    // c.m = b.w: every non-NULL a finds a b whose w is in c ({100,200,100}) ⇒
    // {1,2,3}.
    assert_eq!(
        ids("SELECT id FROM a \
             WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g \
                           AND EXISTS (SELECT 1 FROM c WHERE c.m = b.w)) ORDER BY id"),
        vec![1, 2, 3]
    );
    d.verify().unwrap();
}

/// Stage 3, now SUPPORTED: a nested subquery that correlates to a MIDDLE/OUTER
/// scope (`a.g`), skipping its immediate parent (`b`). The innermost `c.m = a.g`
/// references the outermost `a` (a transit through `b`, which itself also uses
/// `a.g`). Cross-checked against sqlite 3.45 cell-for-cell.
#[test]
fn mid_scope_correlation_matches_sqlite() {
    let d = db();
    let q = "SELECT id FROM a \
             WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g \
                           AND EXISTS (SELECT 1 FROM c WHERE c.m = a.g)) ORDER BY id";
    assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    d.verify().unwrap();
}
