//! `WITH RECURSIVE` (recursive CTEs, design/DESIGN-CTE-RECURSIVE.md stage 1) —
//! differential against the `sqlite3` CLI (3.45).
//!
//! Covers: the counting generator bounded by an outer LIMIT (an infinite
//! `UNION ALL` generator made finite), a Fibonacci / number sequence with a WHERE
//! termination, tree/graph transitive closure (`edges` → reachable set),
//! `UNION` dedup vs `UNION ALL` multiplicity, insertion-order (breadth-first
//! FIFO) output, and a deliberately-unbounded `UNION ALL` under a tiny
//! `max_work_rows` that must abort with `Error::RuntimeBudget` attributed to the
//! `recursive CTE "<name>"` at the SAME `used` count on repeat runs.
//!
//! Output ORDER is compared, not just the set — the fixpoint is FIFO and a plain
//! `SELECT * FROM cte` returns rows in insertion order, which is sqlite's default.
//!
//! (The plan-bytes truncation sweep for the new `RecursiveCte` node lives with
//! the other decoder truncation tests, in `mpedb-sql`'s `plan::tests`.)

use mpedb::{Config, Database, Error, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database, so a panicking test does not leak a `/dev/shm` file.
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

fn shm_path(tag: &str) -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let p = format!(
        "{dir}/mpedb-rcte-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&p);
    p
}

// ---- the graph fixture -----------------------------------------------------
//
// A small DAG with a diamond (both 2→4 and 3→4 reach 4, so `UNION ALL` counts 4
// twice) and a tail (4→5). Reachable from 1: {1,2,3,4,5}. Acyclic, so a
// `UNION ALL` closure still terminates.
const EDGES: &[(i64, i64)] = &[(1, 2), (1, 3), (2, 4), (3, 4), (4, 5)];

fn insert_statements() -> Vec<String> {
    EDGES
        .iter()
        .map(|(s, d)| format!("INSERT INTO edges (src, dst) VALUES ({s}, {d})"))
        .collect()
}

const SCHEMA: &str = r#"[[table]]
name = "edges"
primary_key = ["src", "dst"]
  [[table.column]]
  name = "src"
  type = "int64"
  [[table.column]]
  name = "dst"
  type = "int64"
"#;

const CREATE_SQLITE: &str = "CREATE TABLE edges (src INTEGER, dst INTEGER, PRIMARY KEY (src, dst));";

fn db_with_budget(max_work_rows: u64) -> Tmp {
    let path = shm_path("g");
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = {max_work_rows}\n\n{SCHEMA}"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

fn db() -> Tmp {
    db_with_budget(0) // unlimited
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("unexpected value in recursive-CTE test: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Run schema + data + one query through the `sqlite3` CLI and parse its default
/// list-mode output (rows preserved in order, cells `|`-separated).
fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from(CREATE_SQLITE);
    script.push('\n');
    for stmt in insert_statements() {
        script.push_str(&stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    let text = sqlite_oracle::script_stdout(&script, "");
    if text.trim().is_empty() {
        return Vec::new();
    }
    text.lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Assert mpedb and sqlite agree, cell for cell AND row for row (order included).
fn assert_same(d: &Database, query: &str) {
    let got = mpedb_rows(d, query);
    let want = sqlite_rows(query);
    assert_eq!(got, want, "mismatch on `{query}`");
}

// ---- 1. the counting generator, bounded by the outer LIMIT ------------------

#[test]
fn counting_generator_with_limit_matches_sqlite() {
    let d = db();
    for q in [
        // The canonical infinite generator made finite by the outer LIMIT.
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x FROM c LIMIT 10",
        // OFFSET + LIMIT still bounds the iteration.
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x FROM c LIMIT 5 OFFSET 3",
        // A projected expression over the generator (row count unchanged).
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x*x FROM c LIMIT 6",
        // Starting value other than 1.
        "WITH RECURSIVE c(x) AS (SELECT 100 UNION ALL SELECT x+7 FROM c) SELECT x FROM c LIMIT 4",
    ] {
        assert_same(&d, q);
    }
}

// ---- 2. number sequence / Fibonacci with a WHERE termination ---------------

#[test]
fn bounded_sequences_match_sqlite() {
    let d = db();
    for q in [
        // A finite range via a WHERE inside the recursive term (natural fixpoint).
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 20) SELECT x FROM c",
        // Fibonacci: two carried columns, project one.
        "WITH RECURSIVE fib(a, b) AS (SELECT 0, 1 UNION ALL SELECT b, a+b FROM fib WHERE b < 200) SELECT a FROM fib",
        // The same, projecting the second column.
        "WITH RECURSIVE fib(a, b) AS (SELECT 0, 1 UNION ALL SELECT b, a+b FROM fib WHERE b < 200) SELECT b FROM fib",
        // Countdown.
        "WITH RECURSIVE c(x) AS (SELECT 10 UNION ALL SELECT x-1 FROM c WHERE x > 0) SELECT x FROM c",
        // Outer WHERE / ORDER BY over the finished CTE.
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 15) SELECT x FROM c WHERE x % 2 = 0 ORDER BY x DESC",
    ] {
        assert_same(&d, q);
    }
}

// ---- 3. tree / graph transitive closure ------------------------------------

#[test]
fn transitive_closure_matches_sqlite() {
    let d = db();
    for q in [
        // Reachable set from node 1 (UNION dedups the diamond); ORDER BY makes the
        // comparison order-stable regardless of join iteration order.
        "WITH RECURSIVE reach(node) AS (SELECT 1 UNION SELECT edges.dst FROM edges JOIN reach ON edges.src = reach.node) SELECT node FROM reach ORDER BY node",
        // The CTE as the join DRIVER (outer) rather than the inner operand.
        "WITH RECURSIVE reach(node) AS (SELECT 1 UNION SELECT edges.dst FROM reach JOIN edges ON edges.src = reach.node) SELECT node FROM reach ORDER BY node",
        // Reachable from a mid-graph node.
        "WITH RECURSIVE reach(node) AS (SELECT 2 UNION SELECT edges.dst FROM edges JOIN reach ON edges.src = reach.node) SELECT node FROM reach ORDER BY node",
        // Count of reachable nodes (aggregate over the finished CTE).
        "WITH RECURSIVE reach(node) AS (SELECT 1 UNION SELECT edges.dst FROM edges JOIN reach ON edges.src = reach.node) SELECT count(*) FROM reach",
    ] {
        assert_same(&d, q);
    }
}

// ---- 4. UNION dedup vs UNION ALL multiplicity ------------------------------

#[test]
fn union_dedup_vs_union_all_multiplicity_match_sqlite() {
    let d = db();
    for q in [
        // UNION: the diamond node 4 (reached via 2 and via 3) appears ONCE.
        "WITH RECURSIVE reach(node) AS (SELECT 1 UNION SELECT edges.dst FROM edges JOIN reach ON edges.src = reach.node) SELECT node FROM reach ORDER BY node",
        // UNION ALL: node 4 appears TWICE (and 5, reached via each 4). Order-stable
        // via ORDER BY so the multiset is compared without join-order sensitivity.
        "WITH RECURSIVE reach(node) AS (SELECT 1 UNION ALL SELECT edges.dst FROM edges JOIN reach ON edges.src = reach.node) SELECT node FROM reach ORDER BY node",
        // UNION ALL multiplicity as a grouped count per node.
        "WITH RECURSIVE reach(node) AS (SELECT 1 UNION ALL SELECT edges.dst FROM edges JOIN reach ON edges.src = reach.node) SELECT node, count(*) FROM reach GROUP BY node ORDER BY node",
    ] {
        assert_same(&d, q);
    }
}

// ---- 5. insertion-order (breadth-first FIFO) output ------------------------

#[test]
fn insertion_order_matches_sqlite() {
    let d = db();
    for q in [
        // A strictly-ordered generator: each step adds exactly one row, so the
        // output is 1,2,…,12 in order (no ORDER BY).
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 12) SELECT x FROM c",
        // Fibonacci in insertion order (no ORDER BY): 0,1,1,2,3,5,8,….
        "WITH RECURSIVE fib(a, b) AS (SELECT 0, 1 UNION ALL SELECT b, a+b FROM fib WHERE a < 50) SELECT a FROM fib",
    ] {
        assert_same(&d, q);
    }
}

// ---- 6. the #74 termination backstop ---------------------------------------

/// A deliberately-unbounded `UNION ALL` recursion under a tiny `max_work_rows`
/// must abort with `Error::RuntimeBudget`, attributed to the recursive CTE, at
/// the SAME `used` count on every run (a count, not a clock — this repo's ethos).
#[test]
fn unbounded_union_all_trips_the_budget_deterministically() {
    let d = db_with_budget(500);
    // No outer LIMIT and no WHERE termination: the generator never closes.
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x FROM c";

    let run = || match d.query(sql, &[]) {
        Err(Error::RuntimeBudget { used, limit, which }) => {
            assert!(used > limit, "used {used} must exceed the limit {limit}");
            (used, which)
        }
        other => panic!("expected RuntimeBudget from the unbounded recursion, got {other:?}"),
    };

    let (used1, which1) = run();
    let (used2, which2) = run();

    assert_eq!(used1, used2, "a work counter must abort at the SAME count every run");
    assert!(used1 > 500, "used {used1} must have crossed the 500-row limit");
    assert!(
        which1.contains(r#"recursive CTE "c""#),
        "attribution should name the recursive CTE: {which1}"
    );
    assert_eq!(which1, which2, "attribution must be stable across runs too");

    // The Display carries the unit and the adjust-hint.
    let msg = match d.query(sql, &[]) {
        Err(e @ Error::RuntimeBudget { .. }) => e.to_string(),
        other => panic!("expected RuntimeBudget, got {other:?}"),
    };
    assert!(msg.contains("work-rows"), "msg should state the unit: {msg}");
    assert!(msg.contains("max_work_rows"), "msg should hint the fix: {msg}");
}

/// A bounded generator with the SAME body but an outer LIMIT completes fine under
/// the same tiny budget — the outer LIMIT bounds the iteration (§2), so the
/// backstop is never reached.
#[test]
fn outer_limit_bounds_the_iteration_under_a_tiny_budget() {
    let d = db_with_budget(100);
    match d.query(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x FROM c LIMIT 10",
        &[],
    ) {
        Ok(ExecResult::Rows { rows, .. }) => {
            let got: Vec<i64> = rows
                .iter()
                .map(|r| match r[0] {
                    Value::Int(i) => i,
                    ref v => panic!("expected int, got {v:?}"),
                })
                .collect();
            assert_eq!(got, (1..=10).collect::<Vec<_>>());
        }
        other => panic!("LIMIT 10 should bound the generator under a tiny budget: {other:?}"),
    }
}

// ---- 7. clean refusals (§3), never a wrong answer --------------------------

#[test]
fn illegal_recursive_ctes_are_refused_cleanly() {
    let d = db();
    for (sql, needle) in [
        // A column list is required.
        (
            "WITH RECURSIVE c AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT * FROM c LIMIT 3",
            "required",
        ),
        // Aggregate in the recursive term.
        (
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT count(*) FROM c) SELECT x FROM c LIMIT 3",
            "recursive term",
        ),
        // DISTINCT in the recursive term.
        (
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT DISTINCT x+1 FROM c) SELECT x FROM c LIMIT 3",
            "recursive term",
        ),
        // The recursive term references the CTE twice (self-join of the CTE).
        (
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT a.x+1 FROM c a JOIN c b ON a.x = b.x) SELECT x FROM c LIMIT 3",
            "once",
        ),
    ] {
        let err = d.query(sql, &[]).expect_err(&format!("`{sql}` must be refused"));
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains(needle),
            "refusal for `{sql}` should mention `{needle}`, got: {msg}"
        );
    }
}
