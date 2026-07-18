//! #74 — per-statement runtime budget (design/DESIGN-RUNTIME-BUDGET.md).
//!
//! The budget counts deterministic "work rows" (scan yields, nested-loop-join
//! candidates, correlated-subquery re-evaluations), so an abort is reproducible:
//! the same query over the same data trips at the exact same `used` count. These
//! tests assert (a) a runaway aborts with `RuntimeBudget` at a repeatable count,
//! (b) the error attributes the work and tells the user how to raise the cap,
//! (c) the prepare-time risk estimate flags a cartesian bomb before it runs, and
//! (d) a normal small query is untouched.

use mpedb::{Config, Database, Error, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Two integer tables, each `(id PK, val)`.
const SCHEMA: &str = r#"[[table]]
name = "a"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "val"
  type = "int64"

[[table]]
name = "b"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "val"
  type = "int64"
"#;

/// Open a fresh database with an explicit `[runtime] max_work_rows` budget.
fn open(max_work_rows: u64) -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-rtbudget-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 64\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = {max_work_rows}\n\n{SCHEMA}"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

/// Insert ids `1..=n` (each with `val = id % 100`) into `table`, in batches so
/// the statement text stays bounded. Inserts are point writes — they never
/// charge the work budget, so this is safe under a tiny `max_work_rows`.
fn fill(db: &Database, table: &str, n: u64) {
    let mut i = 1u64;
    while i <= n {
        let end = (i + 499).min(n);
        let mut sql = format!("INSERT INTO {table} (id, val) VALUES ");
        for id in i..=end {
            if id != i {
                sql.push(',');
            }
            sql.push_str(&format!("({id},{})", id % 100));
        }
        db.query(&sql, &[]).unwrap();
        i = end + 1;
    }
}

/// Run `sql`, assert it aborted with `RuntimeBudget`, and return `(used, which)`.
fn expect_budget(db: &Database, sql: &str) -> (u64, String) {
    match db.query(sql, &[]) {
        Err(Error::RuntimeBudget { used, limit, which }) => {
            assert!(used > limit, "used {used} must exceed limit {limit}");
            (used, which)
        }
        other => panic!("expected RuntimeBudget from `{sql}`, got {other:?}"),
    }
}

/// (a) A cartesian cross join blows a deliberately tiny budget, and — the point
/// of a count over a clock — aborts at the SAME `used` count on every run.
#[test]
fn cross_join_bomb_aborts_deterministically() {
    let db = open(500);
    fill(&db, "a", 100);
    fill(&db, "b", 100);
    let sql = "SELECT a.id, b.id FROM a, b";

    let (used1, which1) = expect_budget(&db, sql);
    let (used2, which2) = expect_budget(&db, sql);

    assert_eq!(used1, used2, "a work counter must abort at the same count every run");
    assert!(used1 > 500, "used {used1} must have crossed the 500-row limit");
    assert!(
        which1.contains("nested-loop join with \"b\""),
        "attribution should name the join: {which1}"
    );
    assert_eq!(which1, which2, "attribution must be stable too");
}

/// (b) The error attributes WHERE the work went (a correlated subquery, whose
/// inner is a PK point so the correlated driver — not a scan — crosses the
/// budget) and its Display carries the adjust-hint.
#[test]
fn error_attributes_correlated_subquery_and_hints_the_fix() {
    // 200 outer rows; the outer scan charges 200, then the per-outer-row
    // correlated bump crosses a 250 budget ~50 rows in.
    let db = open(250);
    fill(&db, "a", 200);
    fill(&db, "b", 200);
    // Inner probes b BY PRIMARY KEY (get_by_pk, no scan charge), so the trip is
    // the exec-layer correlated re-evaluation counter, not a scan counter.
    let sql = "SELECT a.id, (SELECT b.val FROM b WHERE b.id = a.id) FROM a";

    let err = match db.query(sql, &[]) {
        Err(e @ Error::RuntimeBudget { .. }) => e,
        other => panic!("expected RuntimeBudget, got {other:?}"),
    };
    let (used, which) = match &err {
        Error::RuntimeBudget { used, which, .. } => (*used, which.clone()),
        _ => unreachable!(),
    };
    assert!(used > 250, "used {used} must exceed the limit");
    assert!(
        which.contains("correlated subquery over \"b\""),
        "attribution should name the correlated subquery: {which}"
    );

    let msg = err.to_string();
    assert!(msg.contains("runtime budget exceeded"), "msg: {msg}");
    assert!(msg.contains("work-rows"), "msg should state the unit: {msg}");
    assert!(
        msg.contains("max_work_rows"),
        "msg should tell the user how to adjust: {msg}"
    );
}

/// (c) Layer 1: the prepare-time risk estimate flags a known cartesian bomb from
/// the catalog's exact row counts and names the dominant node — no execution.
#[test]
fn risk_estimate_flags_cartesian_bomb_and_names_dominant() {
    let db = open(0); // unlimited at runtime; we only estimate here
    fill(&db, "a", 1000);
    fill(&db, "b", 1000);

    let est = db
        .estimate_risk_sql("SELECT a.id, b.id FROM a, b")
        .unwrap();
    assert_eq!(est.work_rows, 1_000_000, "1000 x 1000 worst case");
    assert_eq!(est.dominant_rows, 1_000_000);
    assert!(
        est.dominant.contains("nested-loop join with \"b\""),
        "dominant node should be the join: {}",
        est.dominant
    );
    assert!(est.exceeds(500_000), "should flag against a 500k ceiling");

    // A single-table point read is not a cardinality risk.
    let safe = db.estimate_risk_sql("SELECT val FROM a WHERE id = 7").unwrap();
    assert_eq!(safe.work_rows, 1);
    assert!(!safe.exceeds(500_000));
    // ...and a plain full scan is bounded by the one table, not multiplied.
    let scan = db.estimate_risk_sql("SELECT id FROM a").unwrap();
    assert_eq!(scan.work_rows, 1000);
    assert!(scan.dominant.contains("scan of table \"a\""), "{}", scan.dominant);
}

/// (d) A normal small query never trips the budget.
#[test]
fn small_query_does_not_trip() {
    let db = open(500);
    fill(&db, "a", 100);

    match db.query("SELECT id FROM a", &[]) {
        Ok(ExecResult::Rows { rows, .. }) => assert_eq!(rows.len(), 100),
        other => panic!("full scan of 100 under budget 500 should succeed: {other:?}"),
    }
    match db.query("SELECT val FROM a WHERE id = 42", &[]) {
        Ok(ExecResult::Rows { rows, .. }) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Value::Int(42));
        }
        other => panic!("PK point read should succeed: {other:?}"),
    }
}

/// The `0` sentinel means unlimited: a cross join that would blow any finite
/// budget completes.
#[test]
fn unlimited_budget_never_trips() {
    let db = open(0);
    fill(&db, "a", 60);
    fill(&db, "b", 60);
    match db.query("SELECT a.id FROM a, b", &[]) {
        Ok(ExecResult::Rows { rows, .. }) => assert_eq!(rows.len(), 3600),
        other => panic!("unlimited budget should run the 3600-row join: {other:?}"),
    }
}
