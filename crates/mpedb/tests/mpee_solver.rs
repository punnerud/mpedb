//! #114 — the MPEE join-order solver (design/DESIGN-MPEE-SOLVER.md).
//!
//! The shape under test is `select5.test`'s `join-17-4`, scaled down: N tables
//! chained by PK equalities into a path, the FROM list SCRAMBLED so consecutive
//! entries are not adjacent in that path, and the only constant anchor written
//! LAST. In the user's textual order most steps are cartesian products; there
//! exists an order in which every step is a PK probe. The solver must find it
//! without being told, and without changing any answer.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

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

/// `n` tables `tK(a int64 PK, b int64)`, 10 rows each — the `select5` shape.
fn open_chain(n: usize, max_join_cells: u64) -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-mpee-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let mut toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 64\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = 0\nmax_join_cells = {max_join_cells}\n"
    );
    for k in 1..=n {
        toml.push_str(&format!(
            "\n[[table]]\nname = \"t{k}\"\nprimary_key = [\"a\"]\n\
             \x20 [[table.column]]\n  name = \"a\"\n  type = \"int64\"\n\
             \x20 [[table.column]]\n  name = \"b\"\n  type = \"int64\"\n"
        ));
    }
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    // `b` is a rotation of `a`, so every `t{k}.b = t{k+1}.a` equality matches
    // exactly one row — the chain pins a single tuple once the anchor lands.
    for k in 1..=n {
        let vals: Vec<String> = (1..=10).map(|i| format!("({i}, {})", i % 10 + 1)).collect();
        db.query(&format!("INSERT INTO t{k} (a, b) VALUES {}", vals.join(", ")), &[])
            .unwrap();
    }
    Tmp { db, path }
}

/// The chain's conjuncts: `t1.b = t2.a AND … AND t{n-1}.b = t{n}.a`, then the
/// CONSTANT anchor on the LAST table as the LAST conjunct — `join-17-4`'s
/// defining property.
fn chain_where(n: usize, anchor: i64) -> String {
    let mut conj: Vec<String> = (1..n).map(|k| format!("t{k}.b = t{}.a", k + 1)).collect();
    conj.push(format!("t{n}.a = {anchor}"));
    conj.join(" AND ")
}

/// FROM in the order the user wrote it, scrambled so consecutive entries are
/// NOT adjacent in the join path: odds first, then evens. Every step but the
/// first two is a cartesian product in this order.
fn scrambled_sql(n: usize, anchor: i64) -> String {
    let mut from: Vec<String> = (1..=n).step_by(2).map(|k| format!("t{k}")).collect();
    from.extend((2..=n).step_by(2).map(|k| format!("t{k}")));
    format!("SELECT t1.a FROM {} WHERE {}", from.join(", "), chain_where(n, anchor))
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows,
        other => panic!("expected rows for `{sql}`, got {other:?}"),
    }
}

fn explain(db: &Database, sql: &str) -> String {
    match db.query(&format!("EXPLAIN {sql}"), &[]) {
        Ok(ExecResult::Explain(t)) => t,
        other => panic!("expected an EXPLAIN rendering, got {other:?}"),
    }
}

/// The acceptance proof, scaled down from `select5.test`'s `join-17-4`.
///
/// 10 tables, FROM scrambled, the only anchor last. In the written order the
/// five odd tables cross-join before any predicate can apply — 10^5 rows of 20
/// columns = 2 M live cells — so a 200 k-cell budget refuses it. With the chain
/// walked in path order every step is a PK probe and the peak is ~200 cells, so
/// the same query under the same budget ANSWERS.
#[test]
fn scrambled_late_anchor_chain_answers_instead_of_exploding() {
    let db = open_chain(10, 200_000);
    let r = rows(&db, &scrambled_sql(10, 4));
    assert_eq!(r.len(), 1, "the chain pins exactly one tuple: {r:?}");
    // b = a % 10 + 1, so a_k = a_{k+1} - 1 walking back from a10 = 4.
    assert_eq!(r[0][0], Value::Int(5), "t1.a for the anchored chain");
}

/// The answer must not depend on the order the tables were written in: the
/// scrambled form, the path-order form and the reversed form are the same query.
#[test]
fn reordering_never_changes_the_answer() {
    let db = open_chain(6, 0);
    let w = chain_where(6, 3);
    let path: Vec<String> = (1..=6).map(|k| format!("t{k}")).collect();
    let rev: Vec<String> = (1..=6).rev().map(|k| format!("t{k}")).collect();
    let base = rows(&db, &format!("SELECT t1.a FROM {} WHERE {w}", path.join(", ")));
    assert_eq!(base.len(), 1);
    assert_eq!(base, rows(&db, &scrambled_sql(6, 3)), "scrambled FROM changed the answer");
    assert_eq!(
        base,
        rows(&db, &format!("SELECT t1.a FROM {} WHERE {w}", rev.join(", "))),
        "reversed FROM changed the answer"
    );
}

/// EXPLAIN names the chosen order and why each table sits where it does. The
/// solver walks the path, so every step after the first is a PK probe and no
/// step is a cartesian product — even though the user wrote them interleaved.
#[test]
fn explain_shows_the_chosen_order_and_the_reason() {
    let db = open_chain(6, 0);
    let plan = explain(&db, &scrambled_sql(6, 2));
    let line = plan
        .lines()
        .find(|l| l.trim_start().starts_with("join order:"))
        .unwrap_or_else(|| panic!("EXPLAIN has no join-order line:\n{plan}"));
    assert_eq!(
        line,
        "  join order: t1 [scan] -> t2 [pk] -> t3 [pk] -> t4 [pk] -> t5 [pk] -> t6 [pk] \
         (MPEE: 0 cartesian steps)",
        "the solver should walk the path:\n{plan}"
    );
    assert!(
        !plan.contains("[cartesian]"),
        "no step should be labelled cartesian:\n{plan}"
    );
}

/// The order the user wrote is kept when the solver has nothing to add: mpedb
/// never reorders without a reason. Path order is already optimal here.
#[test]
fn an_already_optimal_order_is_left_alone() {
    let db = open_chain(5, 0);
    let from: Vec<String> = (1..=5).map(|k| format!("t{k}")).collect();
    let sql = format!("SELECT t1.a FROM {} WHERE {}", from.join(", "), chain_where(5, 2));
    let plan = explain(&db, &sql);
    assert!(
        plan.contains("join order: t1 [scan] -> t2 [pk] -> t3 [pk] -> t4 [pk] -> t5 [pk]"),
        "path order is already optimal and must survive verbatim:\n{plan}"
    );
}

/// The plan-hash contract (design/DESIGN-MPEE-SOLVER.md §6): row counts enter
/// the solver only as a MAGNITUDE bucket, so writes that do not double a table
/// cannot move the chosen plan. And whatever a bigger write does to the CHOICE,
/// it can never make one hash name two plans — the hash is over the plan bytes,
/// so the old hash keeps naming a plan that still returns the same answer.
#[test]
fn row_counts_move_the_plan_only_by_magnitude() {
    let db = open_chain(4, 0);
    let sql = scrambled_sql(4, 2);
    let h0 = db.prepare(&sql).unwrap();
    for i in 11..=15 {
        // 10 -> 15 rows: still bucket 4 (8..15), so the same plan and hash.
        db.query(&format!("INSERT INTO t2 (a, b) VALUES ({i}, {i})"), &[]).unwrap();
    }
    assert_eq!(db.prepare(&sql).unwrap(), h0, "a sub-doubling write moved the plan");
    for i in 16..=400 {
        db.query(&format!("INSERT INTO t2 (a, b) VALUES ({i}, {i})"), &[]).unwrap();
    }
    let h1 = db.prepare(&sql).unwrap();
    match (db.execute(&h1, &[]).unwrap(), db.execute(&h0, &[]).unwrap()) {
        (ExecResult::Rows { rows: x, .. }, ExecResult::Rows { rows: y, .. }) => {
            assert_eq!(x, y, "the old hash must still name a plan with the same answer")
        }
        other => panic!("both plans should return rows: {other:?}"),
    }
}

/// A LEFT join anywhere makes the whole scope ineligible — outer joins are not
/// commutative — so the written order survives and the query still answers.
#[test]
fn outer_join_chains_keep_the_written_order() {
    let db = open_chain(3, 0);
    let sql = "SELECT t1.a FROM t1 LEFT JOIN t2 ON t1.b = t2.a \
               LEFT JOIN t3 ON t2.b = t3.a WHERE t1.a = 7";
    let r = rows(&db, sql);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(7));
    let plan = explain(&db, sql);
    assert!(
        plan.contains("join order: t1 [pk] -> t2 [pk] -> t3 [pk]"),
        "a LEFT chain keeps the written order:\n{plan}"
    );
}
