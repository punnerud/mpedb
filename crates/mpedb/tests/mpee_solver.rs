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

/// **The memory claim, as an exact and machine-independent number.**
///
/// `max_join_cells` counts the `Value` cells a join HOLDS, and `charge` trips
/// on `live > budget` — so the SMALLEST budget under which a statement
/// completes is exactly its peak. That makes the peak assertable: it is a pure
/// function of data and plan, not of the machine, the allocator or the timer.
///
/// Walked in path order the chain holds ONE STEP's worth at a time, so the peak
/// is `40n - 60` — **linear in the width**. In the user's textual order the same
/// query holds the PRODUCT of every table placed so far; measured on an M3 Pro
/// with `MPEDB_NO_MPEE=1` (`examples/mpee_memory`, design/DESIGN-MPEE-SOLVER.md
/// §10.2) that peak is 460 / 6,800 / 90,000 / 1,120,000 / 13,400,000 cells at
/// n = 4 / 6 / 8 / 10 / 12 — a factor of ten per table added, against a constant
/// 40 here. Linear versus exponential is the whole result, and this test pins
/// the linear side of it so it cannot regress silently.
#[test]
fn the_solved_order_holds_a_linear_number_of_cells() {
    for n in [4usize, 6, 8, 10, 12] {
        let peak = (40 * n - 60) as u64;
        // Exactly `peak` succeeds…
        let db = open_chain(n, peak);
        assert_eq!(
            rows(&db, &scrambled_sql(n, 4)).len(),
            1,
            "n={n}: the solved order should fit in {peak} cells"
        );
        drop(db);
        // …and one cell less does not, which is what makes `peak` the PEAK and
        // not merely an upper bound that happens to hold.
        let db = open_chain(n, peak - 1);
        assert!(
            matches!(
                db.query(&scrambled_sql(n, 4), &[]),
                Err(mpedb::Error::RuntimeBudget { kind: mpedb::BudgetKind::JoinCells, .. })
            ),
            "n={n}: {} cells must NOT be enough — otherwise the peak is lower \
             than claimed and the formula is stale",
            peak - 1
        );
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
         (0 cartesian steps)",
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

/// A chain that is ALL barriers has no free run wider than one table, so the
/// written order survives — not because outer joins are refused (#116 makes
/// them a constraint), but because there is nothing left to permute.
#[test]
fn an_all_barrier_chain_has_nothing_to_reorder() {
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

// ===================== the real `select5.test` shape =====================

/// The 17 tables `select5.test`'s `join-17-4` names, and the exact join graph
/// it builds: a PATH, whose 16 equi-join conjuncts each pin one side's PK, plus
/// a single constant anchor `a38 = 9` written as the 16th of 17 conjuncts.
const J17: [u32; 17] = [1, 4, 6, 9, 10, 14, 24, 25, 27, 38, 47, 53, 54, 56, 58, 61, 63];

/// The failing variant verbatim (`select5.test` line ~5418): the FROM list
/// order that made mpedb materialize ~11 GB and die on an allocation failure.
const J17_SQL: &str = "SELECT x24,x6,x53,x1,x54,x61,x58,x63,x56,x47,x27,x38,x4,x25,x9,x14,x10 \
     FROM t9,t56,t53,t61,t54,t1,t27,t4,t38,t14,t63,t10,t25,t24,t47,t58,t6 \
     WHERE b61=a38 AND a54=b6 AND a9=b14 AND b53=a14 AND a1=b4 AND b10=a25 \
     AND a53=b63 AND a10=b9 AND b25=a6 AND b27=a47 AND b1=a58 AND a24=b54 \
     AND a63=b58 AND a61=b24 AND b47=a56 AND a38=9 AND b56=a4";

fn open_j17() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-j17-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    // The DEFAULT join-cells budget, deliberately: this shape is exactly what
    // that budget exists to catch, and the point is that it no longer has to.
    let mut toml =
        format!("[database]\npath = \"{path}\"\nsize_mb = 64\nmax_readers = 8\n\n[runtime]\n");
    for t in J17 {
        toml.push_str(&format!(
            "\n[[table]]\nname = \"t{t}\"\nprimary_key = [\"a{t}\"]\n\
             \x20 [[table.column]]\n  name = \"a{t}\"\n  type = \"int64\"\n\
             \x20 [[table.column]]\n  name = \"b{t}\"\n  type = \"int64\"\n\
             \x20 [[table.column]]\n  name = \"x{t}\"\n  type = \"text\"\n"
        ));
    }
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for t in J17 {
        let vals: Vec<String> =
            (1..=10).map(|i| format!("({i}, {}, 'table t{t} row {i}')", i % 10 + 1)).collect();
        db.query(
            &format!("INSERT INTO t{t} (a{t}, b{t}, x{t}) VALUES {}", vals.join(", ")),
            &[],
        )
        .unwrap();
    }
    Tmp { db, path }
}

/// `join-17-4` itself. Written order: six steps have no predicate linking them
/// to anything already read, so the intermediate reaches 10^7 rows of 51
/// columns and the statement dies. Solved: the path is walked end to end, every
/// step is a PK probe, and it ANSWERS.
#[test]
fn join_17_4_answers() {
    let db = open_j17();
    let plan = explain(&db, J17_SQL);
    let line = plan
        .lines()
        .find(|l| l.trim_start().starts_with("join order:"))
        .unwrap_or_else(|| panic!("no join-order line:\n{plan}"));
    assert!(
        line.ends_with("(0 cartesian steps)"),
        "every one of the 16 steps must be linked: {line}"
    );
    assert_eq!(
        line.matches("[pk]").count(),
        16,
        "16 of the 17 positions are PK probes: {line}"
    );
    let r = rows(&db, J17_SQL);
    assert_eq!(r.len(), 1, "the anchored path pins one tuple: {} rows", r.len());
}

// ===================== #116: constraints, not refusals =====================
//
// v1 REFUSED to reorder on a correlated lifted subplan, on any LEFT join, and
// left every residual conjunct wherever the textual order happened to put it.
// Each of those is a CONSTRAINT a solver can price, not a reason to give up
// (design/DESIGN-MPEE-SOLVER.md §7). Every case below therefore proves TWO
// things: that the order actually moved, and that the answer did not.

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

/// The v2 fixture: three tables whose TEXTUAL order is the bad one.
///
/// - `a(id PK, av)` — the big one, scanned if it is placed first;
/// - `b(id PK, aref, y)` — pinned outright by `b.id = <const>`;
/// - `c(id PK, cv)` — the LEFT-join target, deliberately missing rows so the
///   NULL extension is exercised and not merely present.
const V2_DDL: &[&str] = &[
    "CREATE TABLE a (id INTEGER PRIMARY KEY, av INTEGER)",
    "CREATE TABLE b (id INTEGER PRIMARY KEY, aref INTEGER, y INTEGER)",
    "CREATE TABLE c (id INTEGER PRIMARY KEY, cv INTEGER)",
    "CREATE TABLE k (kid INTEGER PRIMARY KEY, ref INTEGER)",
];

fn v2_inserts() -> Vec<String> {
    let mut out = Vec::new();
    for i in 1..=8 {
        out.push(format!("INSERT INTO a (id, av) VALUES ({i}, {})", i * 10));
        // b.y points at c ids 1..4 only, so half the LEFT joins miss.
        out.push(format!("INSERT INTO b (id, aref, y) VALUES ({i}, {i}, {i})"));
    }
    for i in 1..=4 {
        out.push(format!("INSERT INTO c (id, cv) VALUES ({i}, {})", i * 100));
    }
    // `k.ref` hits a.id ∈ {1,2,4} — and 4 twice, so a correlated EXISTS is not
    // accidentally a one-to-one join.
    for (kid, r) in [(1, 1), (2, 2), (3, 4), (4, 4), (5, 99)] {
        out.push(format!("INSERT INTO k (kid, ref) VALUES ({kid}, {r})"));
    }
    out
}

fn v2_db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-mpee-v2-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for d in V2_DDL {
        db.query(d, &[]).unwrap();
    }
    for s in v2_inserts() {
        db.query(&s, &[]).unwrap();
    }
    Tmp { db, path }
}

/// The same schema, the same rows and the same query through the BUNDLED
/// sqlite 3.45 (`sqlite_oracle`), rendered `|`-separated with `NULL` for null.
fn sqlite_out(query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
    for d in V2_DDL {
        script.push_str(d);
        script.push_str(";\n");
    }
    for s in v2_inserts() {
        script.push_str(&s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    sqlite_oracle::script_stdout(&script, "NULL")
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

fn cell_matches(m: &Value, s: &str) -> bool {
    match m {
        Value::Null => s == "NULL",
        Value::Int(i) => s.parse::<i64>().map(|y| y == *i).unwrap_or(false),
        Value::Float(x) => s.parse::<f64>().map(|y| (x - y).abs() <= 1e-9).unwrap_or(false),
        Value::Bool(b) => s == if *b { "1" } else { "0" },
        Value::Text(t) => s == t,
        other => panic!("unexpected value type: {other:?}"),
    }
}

/// mpedb and sqlite must return the same rows, in the same order, for `query`.
fn agree(db: &Database, query: &str) {
    let m = rows(db, query);
    let s = sqlite_out(query);
    assert_eq!(m.len(), s.len(), "row count differs for `{query}`:\n mpedb {m:?}\n sqlite {s:?}");
    for (mr, sr) in m.iter().zip(&s) {
        assert_eq!(mr.len(), sr.len(), "arity differs for `{query}`: {mr:?} vs {sr:?}");
        for (mv, sv) in mr.iter().zip(sr) {
            assert!(cell_matches(mv, sv), "cell differs for `{query}`: {mv:?} vs {sv:?}");
        }
    }
}

fn join_order_line(db: &Database, sql: &str) -> String {
    let plan = explain(db, sql);
    plan.lines()
        .find(|l| l.trim_start().starts_with("join order:"))
        .unwrap_or_else(|| panic!("EXPLAIN has no join-order line for `{sql}`:\n{plan}"))
        .trim()
        .to_string()
}

// ---- refusal 1: the CORRELATED lifted subplan ----

/// **The shape v1 got wrong.** `agg_filter.rs` caught a `count(*) FILTER
/// (WHERE EXISTS (… k.ref = a.id))` over a join returning the wrong number,
/// because a lifted correlated subplan's `outer_args` are base-row slots of the
/// joined tuple in the TEXTUAL order and the reorder moved the columns out from
/// under them. v1 refused to reorder whenever any correlated subplan existed;
/// v2 remaps the args through the permutation instead.
///
/// The query is written `FROM a, b` so the solver DOES move it: `b.id = 3` pins
/// `b` outright, and entering `a` from `b` is a PK probe, so the reversed order
/// costs nothing while the written one scans `a`.
#[test]
fn a_correlated_filter_over_a_reordered_join_matches_sqlite() {
    let db = v2_db();
    const Q: &str = "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM k WHERE k.ref = a.id)) \
                     FROM a, b WHERE b.aref = a.id AND b.id = 3";
    let line = join_order_line(&db, Q);
    assert!(
        line.starts_with("join order: b "),
        "the solver must actually reorder this, or the test proves nothing: {line}"
    );
    agree(&db, Q);
    // …and the whole family, reordered or not: a correlated EXISTS, a
    // correlated IN, a correlated scalar, and the un-anchored form where every
    // `a` row participates.
    for q in [
        "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM k WHERE k.ref = a.id)) \
         FROM a, b WHERE b.aref = a.id",
        "SELECT count(*) FILTER (WHERE NOT EXISTS (SELECT 1 FROM k WHERE k.ref = a.id)) \
         FROM a, b WHERE b.aref = a.id AND b.id = 3",
        "SELECT count(*) FILTER (WHERE a.id IN (SELECT ref FROM k WHERE k.ref > b.y)) \
         FROM a, b WHERE b.aref = a.id",
        "SELECT a.id, b.y, (SELECT count(*) FROM k WHERE k.ref = a.id) \
         FROM a, b WHERE b.aref = a.id ORDER BY a.id",
        "SELECT a.id FROM a, b WHERE b.aref = a.id AND EXISTS \
         (SELECT 1 FROM k WHERE k.ref = a.id AND k.kid < b.y) ORDER BY a.id",
    ] {
        agree(&db, q);
    }
}

/// The remap has to survive the plan REGISTRY too — `prepare` → encode →
/// decode → `validate` → `execute` re-checks every `outer_arg` against the
/// (now reordered) base row, so a stale slot would surface as `Corrupt`.
#[test]
fn the_remapped_correlation_survives_the_plan_registry() {
    let db = v2_db();
    const Q: &str = "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM k WHERE k.ref = a.id)) \
                     FROM a, b WHERE b.aref = a.id AND b.id = 3";
    let h = db.prepare(Q).expect("a reordered correlated plan must validate");
    let prepared = match db.execute(&h, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(prepared, rows(&db, Q), "prepare/execute differs from query");
}

// ---- refusal 2: LEFT JOIN as a precedence constraint ----

/// A LEFT join is a BARRIER: its inner side stays exactly where it was written,
/// and the INNER run in front of it is reordered freely. `(A ⋈ B) ⟕ C ≡
/// (B ⋈ A) ⟕ C` — the run's row set is identical either way, so what the outer
/// join preserves and NULL-extends cannot move.
#[test]
fn a_left_join_is_a_barrier_and_the_run_in_front_of_it_reorders() {
    let db = v2_db();
    const Q: &str = "SELECT a.id, b.y, c.cv FROM a, b LEFT JOIN c ON c.id = b.y \
                     WHERE b.aref = a.id AND b.id = 3 ORDER BY a.id";
    let line = join_order_line(&db, Q);
    assert!(
        line.starts_with("join order: b ") && line.contains("-> a ") && line.contains("-> c "),
        "the preserved run must reorder to b, a while c stays last: {line}"
    );
    agree(&db, Q);
}

/// And the NULL extension itself is unchanged — including the rows where `c`
/// has no match at all, which is the half a wrong barrier would silently drop.
#[test]
fn left_join_null_extension_survives_the_reorder() {
    let db = v2_db();
    for q in [
        // every `b` participates, half of them missing their `c`
        "SELECT a.id, c.cv FROM a, b LEFT JOIN c ON c.id = b.y WHERE b.aref = a.id ORDER BY a.id",
        // the WHERE mentions the NULL-extended side: #65 keeps it in the
        // joined filter, so it must not become a pushed-down restriction
        "SELECT a.id, c.cv FROM a, b LEFT JOIN c ON c.id = b.y \
         WHERE b.aref = a.id AND (c.cv IS NULL OR c.cv > 150) ORDER BY a.id",
        // two barriers with a free run between them
        "SELECT a.id, c.cv FROM b LEFT JOIN c ON c.id = b.y, a \
         WHERE b.aref = a.id ORDER BY a.id",
        // an ON that references the preserved side only
        "SELECT a.id, b.y, c.cv FROM a, b LEFT JOIN c ON c.id = b.y AND b.y < 3 \
         WHERE b.aref = a.id ORDER BY a.id",
        // a barrier first, an inner run after it
        "SELECT b.id, c.cv, a.av FROM b LEFT JOIN c ON c.id = b.y \
         INNER JOIN a ON a.id = b.aref WHERE b.id > 2 ORDER BY b.id",
    ] {
        agree(&db, q);
    }
}

/// FULL stays refused, and it is a NAMED refusal rather than an oversight: #65
/// disables WHERE pushdown entirely when any FULL is in the chain, so the
/// `INNER JOIN … ON p` ≡ `CROSS JOIN … WHERE p` move this rewrite is built on
/// has no way back to a per-step ON there.
#[test]
fn a_full_join_still_keeps_the_written_order() {
    let db = v2_db();
    const Q: &str = "SELECT a.id, c.cv FROM a FULL JOIN c ON c.id = a.id ORDER BY a.id";
    let line = join_order_line(&db, Q);
    assert!(line.starts_with("join order: a "), "FULL must not be reordered: {line}");
    agree(&db, Q);
}

// ---- refusal 3: residual placement is a cost, not a default ----

/// #65 evaluates a conjunct at the step that places its LAST table, so *when* a
/// filter runs is a consequence of the order. v1 had no term for it and fell
/// back to the textual order whenever the first three terms tied; v2 charges
/// each conjunct the position at which it becomes evaluable, so the table
/// carrying the most restrictions goes first.
///
/// Here `a` carries three single-table conjuncts and `c` one, every join is on
/// a NON-key column (so no step can ever be KNOWN and the worst-case term ties
/// across every connected order), and the user wrote the bad end first.
#[test]
fn residual_placement_is_priced_and_moves_the_order() {
    let db = v2_db();
    const Q: &str = "SELECT count(*) FROM c, b, a \
                     WHERE a.av = b.y AND b.aref = c.cv \
                       AND a.av > 0 AND a.av < 1000 AND a.id <> 99 AND c.cv > 50";
    let line = join_order_line(&db, Q);
    assert!(
        line.starts_with("join order: a "),
        "the most-restricted table should be entered first: {line}"
    );
    agree(&db, Q);
}

/// **The barrier's measured win.** `join-17-4`'s shape with a LEFT JOIN hung
/// off the end: ten chain tables written scrambled, then `LEFT JOIN t11`. v1
/// saw a non-INNER join anywhere in the chain and refused the whole scope, so
/// it kept the scrambled order. v2 treats `t11` as a barrier, reorders the
/// INNER run in front of it, and the same query under the same budget ANSWERS.
///
/// Measured on this exact query, same database, same 200 k-cell budget:
///
/// ```text
/// v1: join order: t1 [scan] -> t3 [cartesian] -> t5 [cartesian] -> t7 [cartesian]
///                 -> t9 [cartesian] -> t2 [pk] -> … -> t11 [pk]  (4 cartesian steps)
///     => runtime budget exceeded: 200010 live joined cells > limit 200000
///        while evaluating nested-loop join with "t9"
/// v2: join order: t1 [scan] -> t2 [pk] -> … -> t10 [pk] -> t11 [pk] (0 cartesian steps)
///     => 1 row
/// ```
#[test]
fn a_left_join_no_longer_costs_the_whole_scope_its_ordering() {
    let db = open_chain(11, 200_000);
    let mut from: Vec<String> = (1..=10).step_by(2).map(|k| format!("t{k}")).collect();
    from.extend((2..=10).step_by(2).map(|k| format!("t{k}")));
    let sql = format!(
        "SELECT t1.a, t11.b FROM {} LEFT JOIN t11 ON t11.a = t10.b WHERE {}",
        from.join(", "),
        chain_where(10, 4)
    );
    let plan = explain(&db, &sql);
    let line = plan
        .lines()
        .find(|l| l.trim_start().starts_with("join order:"))
        .unwrap_or_else(|| panic!("no join-order line:\n{plan}"));
    assert!(
        line.ends_with("(0 cartesian steps)"),
        "the run in front of the barrier must be walked, not crossed: {line}"
    );
    assert!(
        line.trim().starts_with("join order: t1 ") && line.contains("t11 [pk] ("),
        "the barrier must stay LAST while the run reorders: {line}"
    );
    let r = rows(&db, &sql);
    assert_eq!(r.len(), 1, "the anchored chain pins one tuple: {r:?}");
    // b = a % 10 + 1, so walking back from a10 = 4 gives a1 = 5.
    assert_eq!(r[0][0], Value::Int(5), "t1.a for the anchored chain");
}

/// The A/B switch must actually select the pre-#114 arm, or every paired
/// measurement taken with it is worthless. Asserted on the process's own env,
/// so it runs in whichever arm the harness set.
///
/// This is the same discipline `MPEDB_MSYNC_PER_RUN` and `MPEDB_NO_SUBPLAN_MEMO`
/// carry: a falsifiability switch nobody checks is a switch that can rot into a
/// no-op, and then the A/B silently compares an arm against itself.
#[test]
fn the_mpee_kill_switch_selects_the_textual_order() {
    let db = open_chain(6, 0);
    let sql = scrambled_sql(6, 2);
    let plan = explain(&db, &sql);
    let line = plan
        .lines()
        .find(|l| l.trim_start().starts_with("join order:"))
        .unwrap_or_else(|| panic!("EXPLAIN has no join-order line:\n{plan}"));
    if std::env::var("MPEDB_NO_MPEE").as_deref() == Ok("1") {
        // FROM is odds-then-evens, so the textual order cross-joins: t1,t3,...
        assert!(
            line.contains("t3 [cartesian]"),
            "with the solver OFF the chain must stay in the user's textual \
             order, cartesian steps and all: {line}"
        );
    } else {
        assert!(
            line.ends_with("(0 cartesian steps)"),
            "with the solver ON the path must be walked: {line}"
        );
    }
    // Either way the ANSWER is the same — that is the whole licence for
    // reordering, and an A/B switch that changed it would be a bug in the
    // switch, not a measurement knob.
    assert_eq!(rows(&db, &sql).len(), 1, "the anchored chain pins one tuple");
}
