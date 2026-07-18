//! #73 §3 stage 3 — a nested subquery CORRELATED to a MIDDLE or the OUTERMOST
//! scope, skipping the level(s) in between. The innermost subquery may reference
//! a column of a GRANDPARENT (not only its immediate parent): the scope stack in
//! `Correlate` resolves the name against the nearest enclosing scope that has it,
//! and the intervening level(s) carry the value down as a TRANSIT correlation arg
//! they forward but do not themselves consume. At exec, that ancestor value is
//! pulled into the intervening subplan's correlation region per parent row and
//! inherited by the nested subplan's param buffer, so the innermost reads it as a
//! plain (already-filled) param.
//!
//! No PLAN_FORMAT bump: the recursive `SubPlan` (format 20+) already carries
//! per-level `outer_args`/`sub_base` and the inherited param-buffer prefix, so a
//! transit correlation is representable as an ordinary `outer_arg` on the
//! ancestor's DIRECT child plus a plain inherited `Param` in the descendant.
//!
//! Every expected value is cross-checked against the `sqlite3` CLI (3.45), which
//! has the same correlated semantics. The task's canonical shape is the first
//! query below:
//!   `SELECT a.id FROM a
//!    WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE c.y = a.k))`
//! — the innermost `c` correlates to the OUTERMOST `a`, skipping `b`.
//!
//! What STAYS refused: a correlated `IN (SELECT …)` (an intervening IN-list that
//! would have to become correlated to transit) — see the last test.

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

/// Four integer tables so mpedb's rigid typing and sqlite's loose typing agree
/// cell-for-cell. Every correlation column is nullable to exercise the 3VL
/// `= NULL` path through a transit slot.
const SCHEMA: &str = r#"[[table]]
name = "a"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "k"
  type = "int64"
  nullable = true

[[table]]
name = "b"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "x"
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
  name = "y"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "z"
  type = "int64"
  nullable = true

[[table]]
name = "d"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "m"
  type = "int64"
  nullable = true
"#;

const SQLITE_SCHEMA: &str = "CREATE TABLE a (id INTEGER PRIMARY KEY, k INTEGER);\n\
     CREATE TABLE b (id INTEGER PRIMARY KEY, x INTEGER, w INTEGER);\n\
     CREATE TABLE c (id INTEGER PRIMARY KEY, y INTEGER, z INTEGER);\n\
     CREATE TABLE d (id INTEGER PRIMARY KEY, m INTEGER);\n";

fn insert_statements() -> Vec<&'static str> {
    vec![
        "INSERT INTO a (id, k) VALUES (1,10),(2,20),(3,30),(4,NULL)",
        "INSERT INTO b (id, x, w) VALUES (1,10,100),(2,20,200),(3,30,300),(4,10,999)",
        "INSERT INTO c (id, y, z) VALUES (1,10,100),(2,20,200),(3,30,999),(4,NULL,NULL)",
        "INSERT INTO d (id, m) VALUES (1,10),(2,20),(3,10),(4,NULL)",
    ]
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-midscope-{}-{}.mpedb",
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
        other => panic!("unexpected value in mid-scope test: {other:?}"),
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
    let mut script = String::from(SQLITE_SCHEMA);
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

/// A battery of mid-scope correlated nestings — inner→outermost (skipping the
/// middle), inner→both middle and outer, a 4-level skip-two, NOT EXISTS, and
/// scalar variants (in the WHERE and in the SELECT list) — each matching
/// sqlite 3.45 cell-for-cell.
#[test]
fn mid_scope_correlation_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // Canonical: innermost `c` correlates to the OUTERMOST `a`, skipping the
        // middle `b`, which becomes a PURE transit (it forwards `a.k` to `c`
        // without using it).
        "SELECT id FROM a \
         WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE c.y = a.k)) \
         ORDER BY id",
        // Innermost references BOTH the outermost (`a.k`, transit through `b`)
        // AND its immediate parent (`b.w`) — the transit and the direct-parent
        // correlation must not get crossed. `b` is a pure transit for `a.k`.
        "SELECT id FROM a \
         WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS \
                       (SELECT 1 FROM c WHERE c.y = a.k AND c.z = b.w)) \
         ORDER BY id",
        // The design-doc shape: the middle correlates to the outer for its OWN
        // filter (`b.x = a.k`) AND transits `a.k` to the innermost, which also
        // uses `b.w`. The shared `a.k` must collapse to one correlation arg.
        "SELECT id FROM a \
         WHERE EXISTS (SELECT 1 FROM b WHERE b.x = a.k AND EXISTS \
                       (SELECT 1 FROM c WHERE c.y = a.k AND c.z = b.w)) \
         ORDER BY id",
        // 4-level: innermost `d` correlates to the OUTERMOST `a`, skipping BOTH
        // `b` and `c` — the transit is registered at `b` (a's direct child) and
        // inherited all the way down.
        "SELECT id FROM a \
         WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE EXISTS \
                       (SELECT 1 FROM d WHERE d.m = a.k))) \
         ORDER BY id",
        // NOT EXISTS over a mid-scope body.
        "SELECT id FROM a \
         WHERE NOT EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE c.y = a.k)) \
         ORDER BY id",
        // Scalar AGGREGATE (count) whose body has a mid-scope correlated child,
        // used as a WHERE predicate.
        "SELECT id FROM a \
         WHERE (SELECT count(*) FROM b WHERE EXISTS (SELECT 1 FROM c WHERE c.y = a.k)) >= 2 \
         ORDER BY id",
        // Correlated scalar in the SELECT LIST whose body correlates mid-scope.
        "SELECT a.id, (SELECT count(*) FROM b \
                       WHERE EXISTS (SELECT 1 FROM c WHERE c.y = a.k)) AS n \
         FROM a ORDER BY a.id",
        // A scalar innermost (max) that correlates to the outermost, combined
        // with an immediate-parent correlation on the middle.
        "SELECT id FROM a \
         WHERE EXISTS (SELECT 1 FROM b WHERE b.x = a.k \
                       AND (SELECT max(c.z) FROM c WHERE c.y = a.k) = b.w) \
         ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
    d.verify().unwrap();
}

/// Direct `Value` assertions on two canonical shapes, so the behavior is pinned
/// independently of the string cross-check.
#[test]
fn mid_scope_correlation_direct() {
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
    // `b` is non-empty, so `EXISTS(b WHERE EXISTS(c WHERE c.y = a.k))` reduces to
    // `∃c: c.y = a.k`. c.y ∈ {10,20,30,NULL}; a.k ∈ {10,20,30,NULL}. a4's k is
    // NULL (`c.y = NULL` is never true) ⇒ {1,2,3}.
    assert_eq!(
        ids("SELECT id FROM a \
             WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE c.y = a.k)) \
             ORDER BY id"),
        vec![1, 2, 3]
    );
    // 4-level skip-two: d.m ∈ {10,20,10,NULL}. `∃d: d.m = a.k` for a.k ∈
    // {10,20,30,NULL}: a1(10)✓, a2(20)✓, a3(30)✗, a4(NULL)✗ ⇒ {1,2}.
    assert_eq!(
        ids("SELECT id FROM a \
             WHERE EXISTS (SELECT 1 FROM b WHERE EXISTS (SELECT 1 FROM c WHERE EXISTS \
                           (SELECT 1 FROM d WHERE d.m = a.k))) \
             ORDER BY id"),
        vec![1, 2]
    );
    d.verify().unwrap();
}

/// The refusal boundary that STAYS: an intervening `IN (SELECT …)` that would
/// have to become correlated in order to transit an ancestor value is still
/// refused (a correlated IN-list is unsupported) — a clean refusal, never a
/// wrong answer.
#[test]
fn transit_through_in_list_is_refused() {
    let d = db();
    let err = d
        .query(
            "SELECT id FROM a \
             WHERE a.k IN (SELECT x FROM b WHERE EXISTS (SELECT 1 FROM c WHERE c.y = a.k))",
            &[],
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(!msg.is_empty(), "correlated IN transit must refuse");
    assert!(
        msg.contains("correlated IN") || msg.contains("EXISTS") || msg.contains("IN subquery"),
        "expected a correlated-IN refusal, got: {msg}"
    );
    d.verify().unwrap();
}
