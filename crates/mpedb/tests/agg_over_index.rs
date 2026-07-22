//! Aggregate access paths over index trees (plan format 59).
//!
//! The semantic key: mpedb's index membership rule — **a row with ANY NULL
//! indexed column has no index entry** — coincides with SQL's aggregate
//! NULL-skip, so `sum(a)`/`avg(a)`/`min(a)`/`max(a)`/`count(a)` over the index
//! on `a` are correct BY CONSTRUCTION: the rows the tree omits are exactly the
//! rows the aggregate ignores. `count(*)` counts NULL rows too, so it may only
//! ride an index whose every column is schema-NOT-NULL.
//!
//! Everything here is DIFFERENTIAL against the bundled sqlite 3.45.0 oracle
//! (`sqlite_oracle/mod.rs`) on value AND typeof(), with the SAME indexes
//! created on both sides — so where sqlite itself rides a covering index (its
//! own version of this optimization), the fold orders agree too. The shapes
//! the planner must REFUSE (residual WHERE, GROUP BY, DISTINCT, FILTER,
//! NOCASE min/max, nullable trailing composite columns) are pinned twice:
//! EXPLAIN must not claim the index, and the fallback fold must still answer
//! exactly what sqlite answers — agree or refuse, never differ.

use mpedb::{Config, Database, Error, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

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

/// A fresh database with an empty seed schema (`seed` — one throwaway table:
/// a config must declare at least one) and an optional work budget; every test
/// table is then created at RUNTIME with the same DDL the oracle runs.
fn open(tag: &str, max_work_rows: u64) -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-aggidx-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = {max_work_rows}\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

/// The shared DDL — run VERBATIM on both engines. `t1` is the single-column
/// battery: nullable indexed `a`/`f`/`s`, a NOT NULL indexed `nn` (the only
/// `count(*)`-eligible tree), a NOCASE `nc` (min/max must refuse), a UNIQUE
/// index on `u` (the `values → pk` key layout), and an unindexed `b` for
/// residual/grouping shapes.
const DDL: &[&str] = &[
    "CREATE TABLE t1 (id INTEGER PRIMARY KEY, a INTEGER, f REAL, s TEXT, \
     nn INTEGER NOT NULL, nc TEXT COLLATE NOCASE, u INTEGER, b INTEGER)",
    "CREATE INDEX i1_a ON t1 (a)",
    "CREATE INDEX i1_f ON t1 (f)",
    "CREATE INDEX i1_s ON t1 (s)",
    "CREATE INDEX i1_nn ON t1 (nn)",
    "CREATE INDEX i1_nc ON t1 (nc)",
    "CREATE UNIQUE INDEX i1_u ON t1 (u)",
];

/// Seed rows: NULL holes in `a`/`f`/`s`, negative floats, min/max ties in `a`
/// (two rows with 7), int64 extremes in `u` (also UNIQUE — still one entry per
/// row), mixed-case text for the NOCASE lane.
const SEED: &[&str] = &[
    "INSERT INTO t1 VALUES (1, 5, -2.5, 'pear', 10, 'Alpha', 9223372036854775806, 1)",
    "INSERT INTO t1 VALUES (2, NULL, 0.25, 'Apple', 20, 'beta', -9223372036854775807, 2)",
    "INSERT INTO t1 VALUES (3, 7, NULL, 'apple', 30, 'ALPHA', 3, 3)",
    "INSERT INTO t1 VALUES (4, -13, -0.5, NULL, 40, NULL, 4, 4)",
    "INSERT INTO t1 VALUES (5, 7, 1.75, 'Pear', 50, 'gamma', 5, 5)",
    "INSERT INTO t1 VALUES (6, NULL, -2.5, 'az', 60, 'Beta', 6, 6)",
];

fn run_ddl(db: &Database, stmts: &[&str]) {
    for s in stmts {
        db.query(s, &[]).unwrap_or_else(|e| panic!("{s}: {e:?}"));
    }
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn explain(db: &Database, sql: &str) -> String {
    match db.query(&format!("EXPLAIN {sql}"), &[]).unwrap() {
        ExecResult::Explain(text) => text,
        other => panic!("expected explain, got {other:?}"),
    }
}

/// One mpedb cell vs one oracle-rendered cell: exact for NULL/int/text,
/// numeric (parse) for floats — sqlite renders ~15 digits, and `0.0`/`-0.0`
/// compare equal on purpose (sqlite normalizes the sign at STORAGE time, so
/// the oracle can never answer `-0.0` from a table; bit-level float identity
/// is pinned separately in `neg_zero_is_internally_consistent`).
fn cell_matches(m: &Value, s: &str) -> bool {
    match m {
        Value::Null => s == "NULL",
        Value::Int(i) => s.parse::<i64>().map(|y| y == *i).unwrap_or(false),
        Value::Float(x) => match s.parse::<f64>() {
            Ok(y) => (*x == y) || (x - y).abs() <= 1e-12 * x.abs().max(1.0),
            Err(_) => false,
        },
        Value::Text(t) => t == s,
        other => panic!("unexpected value type: {other:?}"),
    }
}

/// Run `queries` on BOTH engines (oracle: `ddl` + `seed` + query, one fresh
/// in-memory db per call) and require cell-for-cell agreement.
fn cross_check(db: &Database, ddl: &[&str], seed: &[&str], queries: &[&str]) {
    for q in queries {
        let got = rows(db.query(q, &[]).unwrap_or_else(|e| panic!("mpedb `{q}`: {e:?}")));
        let mut script = String::new();
        for s in ddl.iter().chain(seed) {
            script.push_str(s);
            script.push_str(";\n");
        }
        script.push_str(q);
        script.push_str(";\n");
        let oracle = sqlite_oracle::script_stdout(&script, "NULL");
        let want: Vec<Vec<String>> = oracle
            .lines()
            .map(|l| l.split('|').map(|c| c.to_string()).collect())
            .collect();
        assert_eq!(
            got.len(),
            want.len(),
            "row count differs for `{q}`: mpedb {got:?} vs sqlite {want:?}"
        );
        for (mr, sr) in got.iter().zip(&want) {
            assert_eq!(mr.len(), sr.len(), "arity differs for `{q}`");
            for (mc, sc) in mr.iter().zip(sr) {
                assert!(
                    cell_matches(mc, sc),
                    "mismatch on `{q}`: mpedb {mc:?} vs sqlite `{sc}` (rows {got:?} vs {want:?})"
                );
            }
        }
    }
}

/// EXPLAIN claims the index-tree aggregate — or is asserted NOT to.
fn assert_via_index(db: &Database, sql: &str, wanted: bool, needle: &str) {
    let text = explain(db, sql);
    assert_eq!(
        text.contains("aggregate via index"),
        wanted,
        "EXPLAIN honesty for `{sql}`:\n{text}"
    );
    if wanted {
        assert!(text.contains(needle), "expected `{needle}` in:\n{text}");
    }
}

// ---------------------------------------------------------------- admission

/// Which shapes ride the tree, and what EXPLAIN says about it — the honesty
/// half of the access decision (post-36155ae: never claim an attribution that
/// did not happen).
#[test]
fn admission_and_explain_honesty() {
    let d = open("adm", 0);
    run_ddl(&d, DDL);
    run_ddl(&d, SEED);

    // min/max only → boundary probes.
    assert_via_index(&d, "SELECT min(a) FROM t1", true, "boundary probe");
    assert_via_index(&d, "SELECT min(a), max(a) FROM t1", true, "boundary probe");
    // A value fold → the index-tree scan.
    assert_via_index(&d, "SELECT sum(a) FROM t1", true, "index-tree scan");
    assert_via_index(&d, "SELECT avg(f) FROM t1", true, "index 2 (f)");
    assert_via_index(&d, "SELECT min(a), count(a), sum(a), avg(a), total(a) FROM t1", true, "index-tree scan");
    // count(*) → the narrowest all-NOT-NULL tree (only i1_nn qualifies).
    assert_via_index(&d, "SELECT count(*) FROM t1", true, "index 4 (nn)");
    // count(a) → membership only; the nullable index on `a` serves it.
    assert_via_index(&d, "SELECT count(a) FROM t1", true, "index 1 (a)");
    // Binary-collated text min/max probes the tree (value re-fetched from the row).
    assert_via_index(&d, "SELECT min(s), max(s) FROM t1", true, "index 3 (s)");
    // UNIQUE index (`values → pk` layout) serves the same paths.
    assert_via_index(&d, "SELECT min(u), max(u) FROM t1", true, "index 6 (u)");

    // Refusals — each falls back to the row fold and says nothing false.
    for refused in [
        "SELECT min(nc) FROM t1",                    // NOCASE: tree orders folded text
        "SELECT sum(a) FROM t1 WHERE b > 0",         // residual filter
        "SELECT sum(a) FROM t1 WHERE a > 0",         // consumed by IndexRange access, not FullScan
        "SELECT b, sum(a) FROM t1 GROUP BY b",       // grouping needs row values
        "SELECT count(DISTINCT a) FROM t1",          // dedup needs more than a count
        "SELECT sum(a) FILTER (WHERE b > 0) FROM t1", // filter reads other columns
        "SELECT min(a), max(f) FROM t1",             // mixed columns: no single tree
        "SELECT sum(b) FROM t1",                     // no index on b
        "SELECT group_concat(s) FROM t1",            // concat order = scan order
    ] {
        assert_via_index(&d, refused, false, "");
    }
    // NOCASE count(a) is membership-only, so the collated tree MAY serve it.
    assert_via_index(&d, "SELECT count(nc) FROM t1", true, "index 5 (nc)");
}

// ------------------------------------------------------------- differential

/// The main battery: every admitted shape, cross-checked against the oracle
/// on value and typeof, over the NULL-holed seed.
#[test]
fn differential_battery_matches_sqlite() {
    let d = open("diff", 0);
    run_ddl(&d, DDL);
    run_ddl(&d, SEED);
    cross_check(
        &d,
        DDL,
        SEED,
        &[
            "SELECT min(a), max(a), count(a), sum(a), avg(a), total(a) FROM t1",
            "SELECT typeof(min(a)), typeof(max(a)), typeof(sum(a)), typeof(avg(a)), typeof(total(a)) FROM t1",
            "SELECT min(f), max(f), count(f), sum(f), avg(f) FROM t1",
            "SELECT typeof(min(f)), typeof(sum(f)), typeof(avg(f)) FROM t1",
            "SELECT min(s), max(s), count(s), typeof(min(s)) FROM t1",
            "SELECT min(u), max(u), count(u) FROM t1", // int64 extremes via the UNIQUE tree
            "SELECT count(*), count(nn), min(nn), max(nn), sum(nn) FROM t1",
            "SELECT count(nc) FROM t1",
            // Shared finish machinery over the injected group.
            "SELECT sum(a) + 1, min(a) * 2 FROM t1",
            "SELECT max(a) FROM t1 HAVING max(a) > 0",
            "SELECT min(a) FROM t1 HAVING min(a) > 0",
            "SELECT sum(a) FROM t1 LIMIT 1",
            // Refused shapes must agree through the fold as well. (NOT here:
            // `min(nc)`/`max(nc)` — the PRE-EXISTING row fold compares min/max
            // under BINARY where sqlite uses the argument's NOCASE collation,
            // a table-path gap this task neither created nor widened; the
            // refusal keeps the index path from inheriting it, and
            // `nocase_minmax_is_refused_not_worsened` pins fold self-consistency.)
            "SELECT min(a), max(f) FROM t1",
            "SELECT sum(a) FROM t1 WHERE b > 0",
            "SELECT count(DISTINCT a) FROM t1",
        ],
    );
}

/// Empty table and all-NULL column: `min`/`max`/`sum`/`avg` are NULL, the
/// counts are 0 — the empty boundary probe must answer NULL, never error.
#[test]
fn empty_and_all_null_match_sqlite() {
    let d = open("empty", 0);
    run_ddl(&d, DDL);
    let queries: &[&str] = &[
        "SELECT min(a), max(a), sum(a), avg(a), count(a), count(*) FROM t1",
        "SELECT typeof(min(a)), typeof(sum(a)), typeof(avg(a)), typeof(count(*)) FROM t1",
        "SELECT total(a) FROM t1",
        "SELECT min(s), max(s) FROM t1",
    ];
    // Entirely empty.
    cross_check(&d, DDL, &[], queries);
    // Non-empty table, all-NULL indexed column: the tree is EMPTY while the
    // table is not — the membership rule's sharpest edge. count(*) still 3.
    let nulls: &[&str] = &[
        "INSERT INTO t1 VALUES (1, NULL, NULL, NULL, 1, NULL, NULL, 1)",
        "INSERT INTO t1 VALUES (2, NULL, NULL, NULL, 2, NULL, NULL, 2)",
        "INSERT INTO t1 VALUES (3, NULL, NULL, NULL, 3, NULL, NULL, 3)",
    ];
    run_ddl(&d, nulls);
    cross_check(&d, DDL, nulls, queries);
}

/// `sum()` int64 overflow raises in BOTH engines — through the index-tree
/// scan exactly as through the fold. (The scan folds in KEY order, the same
/// order sqlite's covering-index scan uses with the identical index present.)
#[test]
fn sum_overflow_raises_on_both_engines() {
    let d = open("ovf", 0);
    run_ddl(&d, DDL);
    let seed: &[&str] = &[
        "INSERT INTO t1 VALUES (1, 9223372036854775806, NULL, NULL, 1, NULL, NULL, 1)",
        "INSERT INTO t1 VALUES (2, 2, NULL, NULL, 2, NULL, NULL, 2)",
    ];
    run_ddl(&d, seed);
    let q = "SELECT sum(a) FROM t1";
    assert_via_index(&d, q, true, "index-tree scan");
    let err = d.query(q, &[]).unwrap_err();
    assert!(
        matches!(err, Error::ArithmeticOverflow),
        "mpedb must raise on int64 sum overflow: {err:?}"
    );
    let mut script = String::new();
    for s in DDL.iter().chain(seed) {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(q);
    script.push_str(";\n");
    let oracle = sqlite_oracle::try_script_stdout(&script, "NULL");
    assert!(
        oracle.as_ref().is_err_and(|e| e.contains("overflow")),
        "sqlite must also raise: {oracle:?}"
    );
}

/// Composite indexes: admissible for the LEADING column only when every
/// TRAILING column is schema-NOT-NULL — otherwise a row with `a` set but the
/// trailing column NULL has NO entry, and the tree would silently omit a row
/// the aggregate must see. Pinned with exactly that trap row.
#[test]
fn composite_trailing_null_membership() {
    let d = open("comp", 0);
    let ddl: &[&str] = &[
        "CREATE TABLE t2 (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, nn INTEGER NOT NULL)",
        "CREATE INDEX i2_ab ON t2 (a, b)",   // trailing nullable: NOT admissible
        "CREATE INDEX i2_ann ON t2 (a, nn)", // trailing NOT NULL: admissible
    ];
    let seed: &[&str] = &[
        "INSERT INTO t2 VALUES (1, 1, NULL, 10)", // in i2_ann, NOT in i2_ab
        "INSERT INTO t2 VALUES (2, 5, 2, 20)",
        "INSERT INTO t2 VALUES (3, NULL, 3, 30)",
    ];
    run_ddl(&d, ddl);
    run_ddl(&d, seed);
    // The planner must pick i2_ann (index 2), never i2_ab: through i2_ab,
    // min(a) would be 5 — a wrong answer, not a slow one.
    assert_via_index(&d, "SELECT min(a) FROM t2", true, "index 2 (a, nn)");
    assert_via_index(&d, "SELECT count(a) FROM t2", true, "index 2 (a, nn)");
    // count(*) may ride i2_ann too (both its columns NOT NULL)? No — `a` is
    // nullable, so row 3 has no entry; only an ALL-NOT-NULL index counts rows.
    assert_via_index(&d, "SELECT count(*) FROM t2", false, "");
    cross_check(
        &d,
        ddl,
        seed,
        &["SELECT min(a), max(a), count(a), sum(a) FROM t2", "SELECT count(*) FROM t2"],
    );

    // Only the nullable-trailing composite exists → full refusal + fold.
    let d2 = open("comp2", 0);
    let ddl2: &[&str] = &[
        "CREATE TABLE t2 (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
        "CREATE INDEX i2_ab ON t2 (a, b)",
    ];
    run_ddl(&d2, ddl2);
    run_ddl(&d2, seed.iter().map(|s| {
        // Same trap rows, minus the nn column.
        s.replace(", 10)", ")").replace(", 20)", ")").replace(", 30)", ")")
    }).collect::<Vec<_>>().iter().map(|s| s.as_str()).collect::<Vec<_>>().as_slice());
    assert_via_index(&d2, "SELECT min(a) FROM t2", false, "");
    let got = rows(d2.query("SELECT min(a), count(a) FROM t2", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(1), Value::Int(2)]]);
}

/// min/max ties resolve to the same VALUE the fold keeps (first strict beat ⇒
/// lowest-pk witness): pinned bit-exactly by comparing the read path (probe)
/// with a write-session run of the SAME statement (the fold — a write context
/// declines the index path by construction).
#[test]
fn probe_agrees_with_fold_bit_exactly() {
    let d = open("tie", 0);
    run_ddl(&d, DDL);
    run_ddl(&d, SEED);
    for q in [
        "SELECT min(a), max(a) FROM t1",
        "SELECT min(f), max(f) FROM t1",
        "SELECT min(s), max(s) FROM t1",
        "SELECT min(u), max(u) FROM t1",
        "SELECT sum(a), avg(a), count(a), count(*) FROM t1",
    ] {
        let via_index = rows(d.query(q, &[]).unwrap());
        let mut w = d.begin().unwrap();
        let via_fold = rows(w.query(q, &[]).unwrap());
        w.rollback();
        assert_eq!(via_index, via_fold, "read path vs write-session fold for `{q}`");
    }
}

/// `-0.0`: keycode canonicalizes the key image, so the PROBE re-fetches the
/// row (bit-exact, sign preserved) while the SCAN decodes the canonical
/// member. sqlite stores integral reals as integers and thereby LOSES the
/// sign at rest — its answer is always `0.0` — so the scan's canonical decode
/// agrees with the oracle, and the probe agrees with mpedb's own fold. Both
/// pinned; neither is a wrong answer under any SQL comparison.
#[test]
fn neg_zero_is_internally_consistent() {
    let d = open("nz", 0);
    run_ddl(&d, DDL);
    let ins = d.prepare("INSERT INTO t1 (id, f, nn) VALUES ($1, $2, $3)").unwrap();
    d.execute(&ins, &[Value::Int(1), Value::Float(0.0), Value::Int(1)]).unwrap();
    d.execute(&ins, &[Value::Int(2), Value::Float(-0.0), Value::Int(2)]).unwrap();

    // Ties between 0.0 and -0.0 (equal keys, equal per every SQL comparison):
    // the probe takes the run's FIRST entry = lowest pk = the fold's
    // first-strict-beat pick. Bit-exact parity with the fold, both directions.
    let probe = rows(d.query("SELECT min(f), max(f) FROM t1", &[]).unwrap());
    let mut w = d.begin().unwrap();
    let fold = rows(w.query("SELECT min(f), max(f) FROM t1", &[]).unwrap());
    w.rollback();
    for (p, f) in probe[0].iter().zip(&fold[0]) {
        let (Value::Float(p), Value::Float(f)) = (p, f) else { panic!("floats") };
        assert_eq!(p.to_bits(), f.to_bits(), "probe/fold float identity (sign included)");
        assert_eq!(p.to_bits(), 0.0f64.to_bits(), "lowest-pk witness holds +0.0");
    }
    // The scan path: sum over {+0.0, -0.0} is +0.0 — same as sqlite (which
    // cannot even store the sign) and as IEEE addition in either order.
    let sum = rows(d.query("SELECT sum(f), avg(f) FROM t1", &[]).unwrap());
    let Value::Float(sv) = sum[0][0] else { panic!("float sum") };
    assert_eq!(sv.to_bits(), 0.0f64.to_bits());
}

/// NOCASE min/max: the index is REFUSED (the tree orders folded text; the
/// fold's comparison is binary — riding it could return a different row's
/// text). The pre-existing fold gap vs sqlite (binary vs argument-collation
/// comparison) is NOT this task's to fix; what this pins is that the read
/// path stays the FOLD (self-consistent with a write-session run) and the
/// membership-only `count(nc)` still rides the tree correctly.
#[test]
fn nocase_minmax_is_refused_not_worsened() {
    let d = open("nocase", 0);
    run_ddl(&d, DDL);
    run_ddl(&d, SEED);
    assert_via_index(&d, "SELECT min(nc), max(nc) FROM t1", false, "");
    let read = rows(d.query("SELECT min(nc), max(nc), count(nc) FROM t1", &[]).unwrap());
    let mut w = d.begin().unwrap();
    let fold = rows(w.query("SELECT min(nc), max(nc), count(nc) FROM t1", &[]).unwrap());
    w.rollback();
    assert_eq!(read, fold, "read path must be the same fold, not a collated tree");
    // count over the collated tree IS admissible (membership only) and agrees.
    cross_check(&d, DDL, SEED, &["SELECT count(nc) FROM t1"]);
}

// -------------------------------------------------------------- work meter

/// #74 parity. `count(*)` via the index charges EXACTLY the table-drain
/// total (entry count == row count on an all-NOT-NULL tree): the budget trips
/// at n-1 and passes at n, both paths. A boundary probe charges 1 per probed
/// row — `min(a), max(a)` is two probes, so it answers under budget 2 where
/// the fold would charge n; that difference is the documented, deterministic
/// charge of the access path actually taken.
#[test]
fn work_meter_charges_are_deterministic() {
    let n = 40u64;
    for (budget, count_ok, minmax_ok) in [(n - 1, false, true), (n, true, true), (2, false, true)] {
        let d = open(&format!("wm{budget}"), budget);
        run_ddl(&d, DDL);
        let mut w = d.begin().unwrap();
        for i in 1..=n {
            w.query(
                &format!("INSERT INTO t1 (id, a, nn) VALUES ({i}, {}, {i})", i * 3),
                &[],
            )
            .unwrap();
        }
        w.commit().unwrap();

        let count = d.query("SELECT count(*) FROM t1", &[]);
        assert_eq!(
            count.is_ok(),
            count_ok,
            "count(*) under budget {budget}: {count:?}"
        );
        if !count_ok {
            assert!(matches!(count, Err(Error::RuntimeBudget { .. })));
        }
        let mm = d.query("SELECT min(a), max(a) FROM t1", &[]);
        assert_eq!(mm.is_ok(), minmax_ok, "min/max under budget {budget}: {mm:?}");
    }
}

/// The scan path (sum) charges one work-row per ENTRY — over a NULL-holed
/// column that is FEWER than the table's rows (it visits fewer), and the
/// refusal point is deterministic: trips at entries-1, passes at entries.
#[test]
fn scan_charge_is_the_entry_count() {
    let entries = 30u64; // 30 non-NULL out of 60 rows
    for (budget, ok) in [(entries - 1, false), (entries, true)] {
        let d = open(&format!("sc{budget}"), budget);
        run_ddl(&d, DDL);
        let mut w = d.begin().unwrap();
        for i in 1..=60u64 {
            let a = if i % 2 == 0 { format!("{}", i) } else { "NULL".into() };
            w.query(&format!("INSERT INTO t1 (id, a, nn) VALUES ({i}, {a}, {i})"), &[])
                .unwrap();
        }
        w.commit().unwrap();
        let r = d.query("SELECT sum(a) FROM t1", &[]);
        assert_eq!(r.is_ok(), ok, "sum under budget {budget}: {r:?}");
    }
}

// ------------------------------------------------------------ change safety

/// The tree answers must track WRITES — insert/delete/update through the
/// same session API, re-checked against the oracle after each mutation (the
/// index maintenance is the engine's, but the aggregate path must read the
/// LIVE tree of its snapshot, never a stale count).
#[test]
fn tracks_mutations() {
    let d = open("mut", 0);
    run_ddl(&d, DDL);
    run_ddl(&d, SEED);
    let mutations: &[&str] = &[
        "DELETE FROM t1 WHERE id = 4",                  // removes min(a) = -13
        "UPDATE t1 SET a = -99 WHERE id = 1",           // new minimum
        "INSERT INTO t1 VALUES (7, 100, 9.5, 'zz', 70, 'x', 7, 7)", // new max
        "UPDATE t1 SET a = NULL WHERE id = 3",          // shrinks count(a), drops a 7-tie
    ];
    let mut applied: Vec<&str> = SEED.to_vec();
    for m in mutations {
        d.query(m, &[]).unwrap();
        applied.push(m);
        cross_check(
            &d,
            DDL,
            &applied,
            &[
                "SELECT min(a), max(a), count(a), sum(a), avg(a) FROM t1",
                "SELECT count(*) FROM t1",
            ],
        );
    }
}
