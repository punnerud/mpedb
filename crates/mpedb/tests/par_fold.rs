//! The partitioned parallel fold's differential battery — every parallelized
//! shape against the BUNDLED sqlite (3.45.0) oracle AND against serial mpedb,
//! with the engagement counter proving the parallel path actually ran (by
//! design nothing else observable distinguishes it).
//!
//! `MPEDB_PAR_MIN_ROWS=1` (set process-wide below, the `MPEDB_FOLD_BATCH`
//! precedent) collapses the ~100k-row engagement threshold so a 4 000-row
//! fixture — big enough for a branch-root B+tree, i.e. real partition cuts —
//! exercises the workers.
//!
//! The overflow probes are the semantic heart: sqlite's integer `sum` raises
//! on INTERMEDIATE overflow even when the total fits (probed: `[MAX, 1, -2]`
//! errors, the same multiset as `[1, -2, MAX]` completes), mpedb's serial
//! fold has the same raise-iff-some-prefix-escapes rule, and the parallel
//! i128 prefix monoid must reproduce it EXACTLY — including the case that
//! refutes per-partition i64 accumulation: a suffix partition whose LOCAL
//! sum overflows i64 while every TRUE prefix stays in range must complete.

use mpedb::{Config, Database, Error, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);
static ENV: Once = Once::new();

/// The engagement counter is process-global, and several tests assert a
/// DELTA on it (including "did not move") — so the tests in this binary
/// serialize on one lock rather than racing each other's bumps.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Collapse the engagement threshold once, before any Database in this
/// process runs a query (the executor caches it in a `OnceLock`).
fn setup_env() {
    ENV.call_once(|| std::env::set_var("MPEDB_PAR_MIN_ROWS", "1"));
}

const SCHEMA: &str = r#"
[[table]]
name = "t"
primary_key = ["pk"]
  [[table.column]]
  name = "pk"
  type = "int64"
  [[table.column]]
  name = "a"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "f"
  type = "float64"
  nullable = true
  [[table.column]]
  name = "s"
  type = "text"
  nullable = true
  [[table.column]]
  name = "g"
  type = "int64"
"#;

struct Fixture {
    /// Parallel handle (`max_query_threads = 3`).
    par: Database,
    /// Serial control on the SAME file (`max_query_threads = 1`).
    ser: Database,
    /// The oracle-side seed script (schema + inserts), prepended to queries.
    oracle_seed: String,
    path: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

fn open(path: &str, threads: u32, extra_runtime: &str) -> Database {
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 64\nmax_readers = 8\n\n\
         [runtime]\nmax_query_threads = {threads}\n{extra_runtime}\n{SCHEMA}"
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

/// One deterministic fixture row: `(pk, a, f, s, g)`.
type Row = (u64, Option<i64>, Option<f64>, Option<String>, i64);

/// Deterministic row set: NULL sprinkles in `a`/`f`/`s`, mixed-case text,
/// 7 groups. 4 000 rows ⇒ a branch-root PK tree ⇒ real partition cuts.
fn seed_rows(n: u64) -> Vec<Row> {
    (1..=n)
        .map(|pk| {
            let a = (pk % 7 != 0).then(|| (pk as i64 * 37 % 1000) - 300);
            let f = (pk % 11 != 0).then_some((pk % 97) as f64 * 0.25);
            let s = (pk % 13 != 0).then(|| {
                let case = if pk % 3 == 0 { 'A' } else { 'a' };
                format!("{case}{}", pk % 50)
            });
            (pk, a, f, s, (pk % 7) as i64)
        })
        .collect()
}

fn fixture(n: u64) -> Fixture {
    setup_env();
    let path = format!(
        "/dev/shm/mpedb-parfold-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let par = open(&path, 3, "");
    let ser = open(&path, 1, "");
    let mut oracle_seed = String::from(
        "CREATE TABLE t (pk INTEGER PRIMARY KEY, a INT, f REAL, s TEXT, g INT) STRICT;\n",
    );
    let rows = seed_rows(n);
    let lit = |r: &Row| {
        format!(
            "({},{},{},{},{})",
            r.0,
            r.1.map_or("NULL".into(), |v| v.to_string()),
            r.2.map_or("NULL".into(), |v| format!("{v:?}")),
            r.3.as_ref().map_or("NULL".into(), |v| format!("'{v}'")),
            r.4
        )
    };
    for chunk in rows.chunks(400) {
        let vals: Vec<String> = chunk.iter().map(lit).collect();
        let stmt = format!("INSERT INTO t (pk,a,f,s,g) VALUES {}", vals.join(","));
        par.query(&stmt, &[]).unwrap();
        oracle_seed.push_str(&stmt);
        oracle_seed.push_str(";\n");
    }
    Fixture { par, ser, oracle_seed, path }
}

/// Render one mpedb cell the way the sqlite CLI prints it (list mode).
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        Value::Float(f) => {
            if f == f.trunc() && f.abs() < 1e15 {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        other => panic!("unexpected value: {other:?}"),
    }
}

fn rows_of(db: &Database, q: &str) -> Vec<Vec<String>> {
    match db.query(q, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows
            .into_iter()
            .map(|r| r.into_iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{q}`, got {other:?}"),
    }
}

/// Three-way differential on one query: parallel mpedb ≡ serial mpedb ≡ the
/// bundled oracle, cell for cell. Returns whether the parallel handle's fold
/// ENGAGED while answering.
fn diff3(fx: &Fixture, q: &str) -> bool {
    let before = mpedb::parallel_folds_engaged();
    let par = rows_of(&fx.par, q);
    let engaged = mpedb::parallel_folds_engaged() > before;
    let ser = rows_of(&fx.ser, q);
    assert_eq!(par, ser, "parallel vs serial mpedb on `{q}`");
    let script = format!("{}{q};\n", fx.oracle_seed);
    let oracle: Vec<Vec<String>> = sqlite_oracle::script_stdout(&script, "")
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect();
    assert_eq!(
        par,
        oracle,
        "mpedb vs bundled sqlite {} on `{q}`",
        sqlite_oracle::version()
    );
    engaged
}

/// Every PARALLELIZED shape, differential three ways, and each must have
/// actually engaged the workers.
#[test]
fn parallel_shapes_match_serial_and_the_oracle() {
    let _g = lock();
    let fx = fixture(4000);
    let eligible = [
        // scan-bound counts under a residual WHERE — body B
        "SELECT count(*) FROM t WHERE a > 0",
        "SELECT count(a) FROM t",
        "SELECT count(*), count(a), count(s) FROM t WHERE a > -100",
        // PkRange partitioning
        "SELECT count(*), count(a) FROM t WHERE pk >= 500 AND pk < 3500",
        "SELECT sum(a) FROM t WHERE pk > 1000",
        // the fused single-column folds — body A
        "SELECT min(a), max(a), count(*) FROM t",
        "SELECT sum(a) FROM t",
        "SELECT min(s), max(s) FROM t",
        "SELECT min(f), max(f) FROM t",
        // ParSum under a residual filter — body B
        "SELECT sum(a) FROM t WHERE g <> 3",
        // per-aggregate FILTER clauses
        "SELECT count(*) FILTER (WHERE a > 0), count(*) FILTER (WHERE a < 0) FROM t",
        // GROUP BY: the merge across worker maps, every proven aggregate at once
        "SELECT g, count(*), count(a), sum(a), min(a), max(a), min(s), max(s) \
         FROM t GROUP BY g ORDER BY g",
        "SELECT g, count(*) FROM t GROUP BY g HAVING count(*) > 500 ORDER BY g",
        "SELECT g, min(a) FROM t GROUP BY g ORDER BY g LIMIT 3",
        "SELECT g, max(s) FROM t GROUP BY g ORDER BY g LIMIT 2 OFFSET 2",
        // computed group key
        "SELECT a % 10, count(*) FROM t WHERE a IS NOT NULL GROUP BY a % 10 \
         ORDER BY a % 10",
        // grouped range scan
        "SELECT g, sum(a) FROM t WHERE pk >= 200 AND pk < 3800 GROUP BY g ORDER BY g",
    ];
    for q in eligible {
        assert!(diff3(&fx, q), "expected the parallel fold to engage on `{q}`");
    }
}

/// Shapes the gate REFUSES (order-dependent or unprovable): the counter must
/// not move, and the serial answer must still match the oracle — refusing
/// parallelism must never have become refusing the query.
#[test]
fn refused_shapes_stay_serial_and_correct() {
    let _g = lock();
    let fx = fixture(4000);
    let refused = [
        "SELECT total(a) FROM t",                  // f64 accumulation
        "SELECT sum(f) FROM t",                    // float sum
        "SELECT count(DISTINCT a) FROM t",         // cross-partition dedup
        "SELECT group_concat(s) FROM t WHERE pk < 40", // order IS the answer
        "SELECT g, max(a), s FROM t GROUP BY g ORDER BY g", // bare-column witness
        "SELECT sum(a + 0) FROM t",                // computed sum arg: not schema-pinned
    ];
    for q in refused {
        assert!(!diff3(&fx, q), "the gate must refuse `{q}`");
    }
    // Float avg is refused too, but its value cannot ride the oracle leg
    // (an arbitrary quotient may need more decimal digits than sqlite's
    // 15-significant print carries): parallel-vs-serial equality and the
    // still counter suffice — the refusal itself is the claim.
    let before = mpedb::parallel_folds_engaged();
    assert_eq!(
        rows_of(&fx.par, "SELECT avg(f) FROM t"),
        rows_of(&fx.ser, "SELECT avg(f) FROM t")
    );
    assert_eq!(mpedb::parallel_folds_engaged(), before, "avg(f) must not engage");
}

/// Integer `avg` parallelizes inside the f64-exactness window and must be
/// BIT-identical to serial there — asserted on the raw f64, not on prints.
#[test]
fn integer_avg_matches_serial_bit_for_bit() {
    let _g = lock();
    let fx = fixture(4000);
    for q in [
        "SELECT avg(a) FROM t",
        "SELECT avg(a) FROM t WHERE g <> 2",
        "SELECT g, avg(a), count(*), sum(a) FROM t GROUP BY g ORDER BY g",
    ] {
        let before = mpedb::parallel_folds_engaged();
        let p = match fx.par.query(q, &[]) {
            Ok(ExecResult::Rows { rows, .. }) => rows,
            other => panic!("{q}: {other:?}"),
        };
        assert!(mpedb::parallel_folds_engaged() > before, "expected engagement on `{q}`");
        let s = match fx.ser.query(q, &[]) {
            Ok(ExecResult::Rows { rows, .. }) => rows,
            other => panic!("{q}: {other:?}"),
        };
        // Value equality on f64 is bit equality for non-NaN — and stricter
        // than any print comparison.
        assert_eq!(p, s, "parallel avg must be bit-identical to serial on `{q}`");
    }
}

/// Integer `avg` against the ORACLE, on data whose divisions are exact and
/// short enough to print identically on both sides (group sizes are powers
/// of two, values small): sqlite's compensated sum of exact steps carries
/// zero compensation, so the three answers agree byte for byte.
#[test]
fn integer_avg_matches_the_oracle_on_exact_data() {
    let _g = lock();
    setup_env();
    let path = format!(
        "/dev/shm/mpedb-paravg-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let par = open(&path, 3, "");
    let ser = open(&path, 1, "");
    let mut oracle_seed = String::from(
        "CREATE TABLE t (pk INTEGER PRIMARY KEY, a INT, f REAL, s TEXT, g INT) STRICT;\n",
    );
    // 4096 rows, no NULLs, g = pk % 4 ⇒ every group holds exactly 1024 rows:
    // avg = Σ/2¹⁰ is a dyadic rational with few decimal digits.
    for chunk in (1..=4096u64).collect::<Vec<_>>().chunks(512) {
        let vals: Vec<String> = chunk
            .iter()
            .map(|&pk| format!("({pk},{},NULL,NULL,{})", (pk * 37) % 200, pk % 4))
            .collect();
        let stmt = format!("INSERT INTO t (pk,a,f,s,g) VALUES {}", vals.join(","));
        par.query(&stmt, &[]).unwrap();
        oracle_seed.push_str(&stmt);
        oracle_seed.push_str(";\n");
    }
    let fx = Fixture { par, ser, oracle_seed, path };
    for q in [
        "SELECT avg(a) FROM t",
        "SELECT g, avg(a) FROM t GROUP BY g ORDER BY g",
    ] {
        assert!(diff3(&fx, q), "expected engagement on `{q}`");
    }
}

/// Outside the f64-exactness window (a term beyond ±2⁵³) the merge ABANDONS
/// and the serial fold owns the order-dependent low bits: the parallel
/// handle's answer must equal serial's exactly, engagement notwithstanding.
#[test]
fn integer_avg_escapes_the_window_and_falls_back_serially() {
    let _g = lock();
    let fx = overflow_fixture(&[(1, 1 << 54), (2000, 1 << 54)]);
    let q = "SELECT avg(a) FROM t";
    let before = mpedb::parallel_folds_engaged();
    let p = match fx.par.query(q, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows,
        other => panic!("{q}: {other:?}"),
    };
    assert!(
        mpedb::parallel_folds_engaged() > before,
        "the fold engages, then abandons at the merge"
    );
    let s = match fx.ser.query(q, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows,
        other => panic!("{q}: {other:?}"),
    };
    assert_eq!(p, s, "the abandoned fold must hand the statement to serial verbatim");
}

/// Serial integer `avg` RAISES on an intermediate i64 overflow (mpedb's
/// documented divergence from sqlite's float-degrading avg, unreachable in
/// the generator's value range) — and the parallel path, which abandons on
/// the escaped window long before, surfaces the identical serial raise.
#[test]
fn integer_avg_i64_overflow_raises_like_serial() {
    let _g = lock();
    let fx = overflow_fixture(&[(1, i64::MAX), (2, 1)]);
    let q = "SELECT avg(a) FROM t";
    expect_overflow(&fx.par, q);
    expect_overflow(&fx.ser, q);
}

/// Ten runs of the same statements at threads ∈ {1,2,3,5,8} — the knob must
/// be observable as wall time ONLY.
#[test]
fn thread_count_is_unobservable() {
    let _g = lock();
    let fx = fixture(4000);
    let queries = [
        "SELECT g, count(*), sum(a), min(s) FROM t GROUP BY g ORDER BY g",
        "SELECT sum(a), count(a), min(a), max(a) FROM t",
        "SELECT count(*) FROM t WHERE a BETWEEN -50 AND 200",
    ];
    let baseline: Vec<_> = queries.iter().map(|q| rows_of(&fx.ser, q)).collect();
    for threads in [2u32, 3, 5, 8] {
        let db = open(&fx.path, threads, "");
        for (q, want) in queries.iter().zip(&baseline) {
            assert_eq!(&rows_of(&db, q), want, "threads={threads} on `{q}`");
        }
    }
}

// ---------------------------------------------------------------- overflow

/// A big fixture whose `a` column is mostly zeros with `spikes` planted at
/// chosen PKs — the intermediate-overflow probes.
fn overflow_fixture(spikes: &[(u64, i64)]) -> Fixture {
    setup_env();
    let path = format!(
        "/dev/shm/mpedb-parofl-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let par = open(&path, 3, "");
    let ser = open(&path, 1, "");
    let mut oracle_seed = String::from(
        "CREATE TABLE t (pk INTEGER PRIMARY KEY, a INT, f REAL, s TEXT, g INT) STRICT;\n",
    );
    let spike = |pk: u64| spikes.iter().find(|(p, _)| *p == pk).map(|(_, v)| *v);
    for chunk in (1..=4000u64).collect::<Vec<_>>().chunks(400) {
        let vals: Vec<String> = chunk
            .iter()
            .map(|&pk| format!("({pk},{},NULL,NULL,0)", spike(pk).unwrap_or(0)))
            .collect();
        let stmt = format!("INSERT INTO t (pk,a,f,s,g) VALUES {}", vals.join(","));
        par.query(&stmt, &[]).unwrap();
        oracle_seed.push_str(&stmt);
        oracle_seed.push_str(";\n");
    }
    Fixture { par, ser, oracle_seed, path }
}

fn expect_overflow(db: &Database, q: &str) {
    match db.query(q, &[]) {
        Err(Error::ArithmeticOverflow) => {}
        other => panic!("expected ArithmeticOverflow from `{q}`, got {other:?}"),
    }
}

/// sqlite raises on INTERMEDIATE overflow even when the total fits — the
/// oracle probe that decided integer sum's design — and the parallel monoid
/// reproduces the raise. `[MAX, 1, -2, 0, …]`: total = MAX − 1, prefix 2
/// escapes; everyone must error.
#[test]
fn sum_intermediate_overflow_raises_even_when_the_total_fits() {
    let _g = lock();
    let fx = overflow_fixture(&[(1, i64::MAX), (2, 1), (3, -2)]);
    let q = "SELECT sum(a) FROM t";
    let before = mpedb::parallel_folds_engaged();
    expect_overflow(&fx.par, q);
    assert!(mpedb::parallel_folds_engaged() > before, "the raise must come from the engaged fold");
    expect_overflow(&fx.ser, q);
    let script = format!("{}{q};\n", fx.oracle_seed);
    let err = sqlite_oracle::try_script_stdout(&script, "")
        .expect_err("the bundled oracle must raise too");
    assert!(err.contains("integer overflow"), "oracle said: {err}");
}

/// The SAME multiset in an order whose prefixes all fit completes — for the
/// oracle, for serial mpedb, and for the engaged parallel fold, with the
/// same total.
#[test]
fn sum_same_multiset_in_a_safe_order_completes() {
    let _g = lock();
    let fx = overflow_fixture(&[(1, 1), (2, -2), (3, i64::MAX)]);
    assert!(
        diff3(&fx, "SELECT sum(a) FROM t"),
        "expected engagement on the completing probe"
    );
    assert_eq!(
        rows_of(&fx.par, "SELECT sum(a) FROM t"),
        vec![vec![(i64::MAX - 1).to_string()]]
    );
}

/// The probe that REFUTES per-partition i64 accumulation: a late suffix
/// whose LOCAL sum overflows i64 (`MAX + MAX`) while every TRUE prefix stays
/// in range (a leading `-MAX` offsets it). Serial completes with `MAX`; the
/// i128 prefix monoid must too; a per-partition i64 fold would have raised.
#[test]
fn sum_local_partition_overflow_must_not_raise() {
    let _g = lock();
    let fx = overflow_fixture(&[(1, -i64::MAX), (3998, i64::MAX), (3999, i64::MAX)]);
    assert!(
        diff3(&fx, "SELECT sum(a) FROM t"),
        "expected engagement on the local-overflow probe"
    );
    assert_eq!(
        rows_of(&fx.par, "SELECT sum(a) FROM t"),
        vec![vec![i64::MAX.to_string()]]
    );
}

/// The negative-side raise: `MIN − 1` escapes below i64.
#[test]
fn sum_negative_intermediate_overflow_raises() {
    let _g = lock();
    // (i64::MIN itself is not writable as a SQL literal; -MAX − 2 escapes
    // below i64 just the same.)
    let fx = overflow_fixture(&[(1, -i64::MAX), (2, -2), (3, 2)]);
    let q = "SELECT sum(a) FROM t";
    expect_overflow(&fx.par, q);
    expect_overflow(&fx.ser, q);
    let script = format!("{}{q};\n", fx.oracle_seed);
    let err = sqlite_oracle::try_script_stdout(&script, "")
        .expect_err("the bundled oracle must raise too");
    assert!(err.contains("integer overflow"), "oracle said: {err}");
}

/// Overflow inside ONE GROUP of a grouped fold raises statement-wide,
/// parallel and serial alike (the per-group monoid, same argument).
#[test]
fn grouped_sum_overflow_raises_like_serial() {
    let _g = lock();
    // pk 1 and 3 land in group g=0's scan order back-to-back… both spikes in
    // the same group: prefix MAX+1 escapes within the group.
    let fx = overflow_fixture(&[(7, i64::MAX), (14, 1)]);
    let q = "SELECT g, sum(a) FROM t GROUP BY g ORDER BY g";
    expect_overflow(&fx.par, q);
    expect_overflow(&fx.ser, q);
}

// ---------------------------------------------------------------- budgets

/// A work-budget refusal through the parallel path must be byte-identical to
/// the serial one — same kind, used, limit, attribution — and stable across
/// runs (the runtime_budget.rs contract, now under threads).
#[test]
fn budget_refusal_is_deterministic_and_serial_identical() {
    let _g = lock();
    setup_env();
    let path = format!(
        "/dev/shm/mpedb-parbudget-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let seed = open(&path, 1, "");
    for chunk in seed_rows(4000).chunks(400) {
        let vals: Vec<String> = chunk
            .iter()
            .map(|r| {
                format!(
                    "({},{},NULL,NULL,{})",
                    r.0,
                    r.1.map_or("NULL".into(), |v| v.to_string()),
                    r.4
                )
            })
            .collect();
        seed.query(
            &format!("INSERT INTO t (pk,a,f,s,g) VALUES {}", vals.join(",")),
            &[],
        )
        .unwrap();
    }
    drop(seed);
    let refusal = |db: &Database, q: &str| match db.query(q, &[]) {
        Err(Error::RuntimeBudget { kind, limit, used, which }) => (kind, limit, used, which),
        other => panic!("expected RuntimeBudget from `{q}`, got {other:?}"),
    };
    let par = open(&path, 3, "max_work_rows = 1000");
    let ser = open(&path, 1, "max_work_rows = 1000");
    for q in [
        "SELECT sum(a) FROM t",
        "SELECT g, count(*), sum(a) FROM t GROUP BY g",
        "SELECT count(*) FROM t WHERE a > 0",
    ] {
        let a = refusal(&par, q);
        let b = refusal(&par, q);
        assert_eq!(a, b, "parallel refusal must be stable across runs on `{q}`");
        let s = refusal(&ser, q);
        assert_eq!(a, s, "parallel refusal must equal serial's on `{q}`");
    }
    // The cells budget: N worker maps must refuse like ONE serial map does.
    let par = open(&path, 3, "max_work_rows = 0\nmax_join_cells = 8");
    let ser = open(&path, 1, "max_work_rows = 0\nmax_join_cells = 8");
    let q = "SELECT g, count(*), sum(a) FROM t GROUP BY g";
    let a = refusal(&par, q);
    assert_eq!(a, refusal(&par, q), "cells refusal must be stable across runs");
    assert_eq!(a, refusal(&ser, q), "cells refusal must equal serial's");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

// ---------------------------------------------------------------- collation

/// NOCASE grouping and min/max ties across partition boundaries: the merged
/// answer must keep the serial FIRST-row spelling, which is also sqlite's.
#[test]
fn nocase_group_keys_and_ties_keep_the_first_spelling() {
    let _g = lock();
    setup_env();
    let path = format!(
        "/dev/shm/mpedb-parnc-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    // The config needs SOME table; nc is created live so COLLATE applies.
    let par = open(&path, 3, "");
    let ser = open(&path, 1, "");
    let mut script = String::new();
    let ddl = "CREATE TABLE nc (id INTEGER PRIMARY KEY, x TEXT COLLATE NOCASE, g INT)";
    par.query(ddl, &[]).unwrap();
    script.push_str(ddl);
    script.push_str(";\n");
    // Mixed-case duplicates spread across the whole PK range, so collation
    // classes span partitions and the tie rule is exercised at the merge.
    for chunk in (1..=3000u64).collect::<Vec<_>>().chunks(300) {
        let vals: Vec<String> = chunk
            .iter()
            .map(|&id| {
                let case = if id % 2 == 0 { 'V' } else { 'v' };
                format!("({id},'{case}{}',{})", id % 40, id % 5)
            })
            .collect();
        let stmt = format!("INSERT INTO nc (id,x,g) VALUES {}", vals.join(","));
        par.query(&stmt, &[]).unwrap();
        script.push_str(&stmt);
        script.push_str(";\n");
    }
    for q in [
        "SELECT min(x), max(x), count(*) FROM nc",
        "SELECT g, min(x), max(x) FROM nc GROUP BY g ORDER BY g",
        "SELECT count(*) FROM nc GROUP BY x ORDER BY count(*) LIMIT 5",
    ] {
        let before = mpedb::parallel_folds_engaged();
        let p = rows_of(&par, q);
        assert!(
            mpedb::parallel_folds_engaged() > before,
            "expected engagement on `{q}`"
        );
        assert_eq!(p, rows_of(&ser, q), "parallel vs serial on `{q}`");
        let s = format!("{script}{q};\n");
        let oracle: Vec<Vec<String>> = sqlite_oracle::script_stdout(&s, "")
            .lines()
            .map(|l| l.split('|').map(str::to_string).collect())
            .collect();
        assert_eq!(p, oracle, "vs bundled sqlite on `{q}`");
    }
    drop(par);
    drop(ser);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

// ---------------------------------------------------------------- explain

/// EXPLAIN states the gate honestly: eligible shapes carry the line, refused
/// shapes do not.
#[test]
fn explain_states_the_parallel_choice() {
    let _g = lock();
    let fx = fixture(100);
    let text = |q: &str| match fx.par.query(q, &[]) {
        Ok(ExecResult::Explain(s)) => s,
        other => panic!("expected EXPLAIN text from `{q}`, got {other:?}"),
    };
    let yes = text("EXPLAIN SELECT g, count(*), sum(a) FROM t GROUP BY g");
    assert!(
        yes.contains("parallel fold: eligible"),
        "eligible shape must say so:\n{yes}"
    );
    let avg = text("EXPLAIN SELECT avg(a) FROM t");
    assert!(
        avg.contains("parallel fold: eligible"),
        "integer avg is eligible:\n{avg}"
    );
    let no = text("EXPLAIN SELECT total(a) FROM t");
    assert!(
        !no.contains("parallel fold"),
        "total is refused and must not claim eligibility:\n{no}"
    );
    let no = text("EXPLAIN SELECT avg(f) FROM t");
    assert!(
        !no.contains("parallel fold"),
        "float avg is refused and must not claim eligibility:\n{no}"
    );
}

/// Snapshot identity, adversarially: with parallel folds engaged on one
/// handle, a writer moving every group's values between statements must
/// never produce a torn (mixed-snapshot) answer — each statement's count and
/// sum reflect exactly one commit.
#[test]
fn parallel_fold_reads_one_snapshot_under_writes() {
    let _g = lock();
    let fx = fixture(4000);
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let path = fx.path.clone();
        let writer = s.spawn({
            let stop = &stop;
            move || {
                let db = open(&path, 1, "");
                let mut bump = 0i64;
                while !stop.load(Ordering::Relaxed) {
                    bump += 1;
                    // One statement = one commit: every row of group 3 moves
                    // together, so any mix of pre/post rows in ONE read would
                    // corrupt the (count, sum) pairing checked below.
                    db.query("UPDATE t SET a = a + 1000000 WHERE g = 3", &[]).unwrap();
                    if bump > 200 {
                        break;
                    }
                }
            }
        });
        let baseline = rows_of(&fx.ser, "SELECT count(a) FROM t WHERE g = 3");
        let n: i64 = baseline[0][0].parse().unwrap();
        for _ in 0..100 {
            let got = rows_of(&fx.par, "SELECT count(a), sum(a) FROM t WHERE g = 3");
            assert_eq!(
                got[0][0].parse::<i64>().unwrap(),
                n,
                "the count never changes; only the sum moves, by whole commits"
            );
            let sum: i64 = got[0][1].parse().unwrap();
            // sum ≡ baseline_sum (mod 1_000_000 · n) exactly when the rows
            // seen are all from one commit.
            assert_eq!(
                (sum.rem_euclid(1_000_000 * n)),
                {
                    let base: i64 = rows_of(&fx.ser, "SELECT sum(a) FROM t WHERE g = 3")[0][0]
                        .parse::<i64>()
                        .unwrap();
                    base.rem_euclid(1_000_000 * n)
                },
                "a parallel fold mixed rows from two commits"
            );
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    });
}
