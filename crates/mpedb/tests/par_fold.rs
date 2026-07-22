//! The adaptive parallel fold's differential battery — every parallelized
//! shape against the BUNDLED sqlite (3.45) oracle AND against serial mpedb,
//! with the engagement counter proving the parallel path actually ran (by
//! design, nothing else observable distinguishes it).
//!
//! `MPEDB_PAR_PROBE_ROWS=1` (set process-wide below, the `MPEDB_FOLD_BATCH`
//! precedent) collapses the leader's probe so a 4 000-row fixture — big enough
//! for a branch-root B+tree, i.e. real structural cuts — hands its tail to
//! workers. That the probe is NOT collapsed by default, and that a short scan
//! therefore engages nothing at all, is the claim `par_adaptive.rs` makes.
//!
//! The overflow probes are the semantic heart: sqlite's integer `sum` raises
//! on INTERMEDIATE overflow even when the total fits (probed: `[MAX, 1, -2]`
//! errors, the same multiset as `[1, -2, MAX]` completes), mpedb's serial fold
//! has the same raise-iff-some-prefix-escapes rule, and the parallel i128
//! prefix monoid must reproduce it EXACTLY — including the case that refutes
//! per-morsel i64 accumulation: a suffix whose LOCAL sum overflows i64 while
//! every TRUE prefix stays in range must complete.

use mpedb::{Config, Database, Error, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;


/// Scratch directory for a throwaway test database: tmpfs where it exists
/// (Linux `/dev/shm` — the whole suite's convention, and much faster), the
/// platform temp dir otherwise. macOS has no `/dev/shm`, and hardcoding it
/// failed the entire file there with `Io(NotFound)` the first time CI ran on
/// macOS.
fn scratch_dir() -> String {
    if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm".to_string()
    } else {
        std::env::temp_dir().to_string_lossy().trim_end_matches('/').to_string()
    }
}

static UNIQ: AtomicU64 = AtomicU64::new(0);
static ENV: Once = Once::new();

/// The engagement counter is process-global, and several tests assert a DELTA
/// on it (including "did not move") — so the tests in this binary serialize on
/// one lock rather than racing each other's bumps. It also keeps the helper
/// budget (which is per process) from being spent by a neighbouring test.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Collapse the probe once, before any Database in this process runs a query
/// (the executor caches it in a `OnceLock`).
fn setup_env() {
    ENV.call_once(|| std::env::set_var("MPEDB_PAR_PROBE_ROWS", "1"));
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

fn tmp(tag: &str) -> String {
    format!(
        "{}/mpedb-{tag}-{}-{}.mpedb",
        scratch_dir(),
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// One deterministic fixture row: `(pk, a, f, s, g)`.
type Row = (u64, Option<i64>, Option<f64>, Option<String>, i64);

/// Deterministic row set: NULL sprinkles in `a`/`f`/`s`, mixed-case text,
/// 7 groups. 4 000 rows ⇒ a branch-root PK tree ⇒ real structural cuts.
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

const ORACLE_DDL: &str =
    "CREATE TABLE t (pk INTEGER PRIMARY KEY, a INT, f REAL, s TEXT, g INT) STRICT;\n";

fn fixture(n: u64) -> Fixture {
    setup_env();
    let path = tmp("parfold");
    let _ = std::fs::remove_file(&path);
    let par = open(&path, 3, "");
    let ser = open(&path, 1, "");
    let mut oracle_seed = String::from(ORACLE_DDL);
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

fn oracle_rows(seed: &str, q: &str) -> Vec<Vec<String>> {
    sqlite_oracle::script_stdout(&format!("{seed}{q};\n"), "")
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
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
    assert_eq!(
        par,
        oracle_rows(&fx.oracle_seed, q),
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
        // the fused single-column folds — one bare column, no residual
        "SELECT min(a), max(a), count(*) FROM t",
        "SELECT sum(a) FROM t",
        "SELECT min(s), max(s) FROM t",
        "SELECT min(f), max(f) FROM t",
        "SELECT count(a) FROM t",
        // scan-bound folds under a residual WHERE — the general row body
        "SELECT count(*) FROM t WHERE a > 0",
        "SELECT count(*), count(a), count(s) FROM t WHERE a > -100",
        "SELECT sum(a) FROM t WHERE g <> 3",
        "SELECT min(a), max(a) FROM t WHERE s IS NOT NULL",
        // PkRange partitioning: the morsels are cut inside a bounded range
        "SELECT count(*), count(a) FROM t WHERE pk >= 500 AND pk < 3500",
        "SELECT sum(a) FROM t WHERE pk > 1000",
        "SELECT max(a) FROM t WHERE pk < 2000",
        // per-aggregate FILTER clauses
        "SELECT count(*) FILTER (WHERE a > 0), count(*) FILTER (WHERE a < 0) FROM t",
        // a computed count argument (count observes only non-NULL-ness)
        "SELECT count(a + 1) FROM t",
        // HAVING and LIMIT run at finish, over the merged answer
        "SELECT count(a) FROM t HAVING count(a) > 10",
        "SELECT sum(a), count(*) FROM t LIMIT 1",
    ];
    for q in eligible {
        assert!(diff3(&fx, q), "expected the parallel fold to engage on `{q}`");
    }
}

/// Shapes the gate REFUSES (order-dependent, unprovable, or out of v1 scope):
/// the counter must not move, and the serial answer must still match the
/// oracle — refusing parallelism must never have become refusing the query.
#[test]
fn refused_shapes_stay_serial_and_correct() {
    let _g = lock();
    let fx = fixture(4000);
    let refused = [
        "SELECT total(a) FROM t",                      // f64 accumulation
        "SELECT sum(f) FROM t",                        // float sum
        "SELECT count(DISTINCT a) FROM t",             // cross-morsel dedup
        "SELECT group_concat(s) FROM t WHERE pk < 40", // order IS the answer
        "SELECT g, count(*) FROM t GROUP BY g ORDER BY g", // v1 scope: ungrouped only
        "SELECT g, max(a), s FROM t GROUP BY g ORDER BY g", // bare-column witness
        "SELECT sum(a + 0) FROM t",                    // computed sum arg: not schema-pinned
        "SELECT count(*) FROM t",                      // leaf-wholesale count: already ~free
    ];
    for q in refused {
        assert!(!diff3(&fx, q), "the gate must refuse `{q}`");
    }
    // `avg` is refused too (its f64 running total is accumulated in scan
    // order), but its value cannot ride the oracle leg: an arbitrary quotient
    // may need more decimal digits than sqlite's 15-significant print carries.
    // Parallel-vs-serial equality and the still counter are the claim.
    for q in ["SELECT avg(a) FROM t", "SELECT avg(f) FROM t"] {
        let before = mpedb::parallel_folds_engaged();
        assert_eq!(rows_of(&fx.par, q), rows_of(&fx.ser, q), "on `{q}`");
        assert_eq!(mpedb::parallel_folds_engaged(), before, "`{q}` must not engage");
    }
}

/// Ten runs of the same statements at threads ∈ {1,2,3,5,8} — the knob must be
/// observable as wall time ONLY.
#[test]
fn thread_count_is_unobservable() {
    let _g = lock();
    let fx = fixture(4000);
    let queries = [
        "SELECT sum(a), count(a), min(a), max(a) FROM t",
        "SELECT count(*) FROM t WHERE a BETWEEN -50 AND 200",
        "SELECT min(s), max(s), count(s) FROM t WHERE g > 1",
    ];
    let baseline: Vec<_> = queries.iter().map(|q| rows_of(&fx.ser, q)).collect();
    for threads in [2u32, 3, 5, 8] {
        let db = open(&fx.path, threads, "");
        for _ in 0..3 {
            for (q, want) in queries.iter().zip(&baseline) {
                assert_eq!(&rows_of(&db, q), want, "threads={threads} on `{q}`");
            }
        }
    }
}

// ---------------------------------------------------------------- overflow

/// A big fixture whose `a` column is mostly zeros with `spikes` planted at
/// chosen PKs — the intermediate-overflow probes.
fn overflow_fixture(spikes: &[(u64, i64)]) -> Fixture {
    setup_env();
    let path = tmp("parofl");
    let _ = std::fs::remove_file(&path);
    let par = open(&path, 3, "");
    let ser = open(&path, 1, "");
    let mut oracle_seed = String::from(ORACLE_DDL);
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
    assert!(
        mpedb::parallel_folds_engaged() > before,
        "the raise must come from the engaged fold"
    );
    expect_overflow(&fx.ser, q);
    let err = sqlite_oracle::try_script_stdout(&format!("{}{q};\n", fx.oracle_seed), "")
        .expect_err("the bundled oracle must raise too");
    assert!(err.contains("integer overflow"), "oracle said: {err}");
}

/// The SAME multiset in an order whose prefixes all fit completes — for the
/// oracle, for serial mpedb, and for the engaged parallel fold, with the same
/// total. (Order-dependence of the raise is real, and reproduced.)
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

/// The probe that REFUTES per-morsel i64 accumulation: a late suffix whose
/// LOCAL sum overflows i64 (`MAX + MAX`) while every TRUE prefix stays in
/// range (a leading `-MAX` offsets it). Serial completes with `MAX`; the i128
/// prefix monoid must too; a per-morsel i64 fold would have raised.
#[test]
fn sum_local_morsel_overflow_must_not_raise() {
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
    let err = sqlite_oracle::try_script_stdout(&format!("{}{q};\n", fx.oracle_seed), "")
        .expect_err("the bundled oracle must raise too");
    assert!(err.contains("integer overflow"), "oracle said: {err}");
}

/// The overflow raise must also survive the residual-filter body (the general
/// row fold), not just the fused one.
#[test]
fn sum_overflow_under_a_residual_raises_like_serial() {
    let _g = lock();
    let fx = overflow_fixture(&[(1, i64::MAX), (2, 1), (3, -2)]);
    let q = "SELECT sum(a) FROM t WHERE g = 0";
    expect_overflow(&fx.par, q);
    expect_overflow(&fx.ser, q);
}

// ---------------------------------------------------------------- budgets

/// A work-budget refusal through the parallel path must be byte-identical to
/// the serial one — same kind, used, limit, attribution — and stable across
/// runs (the `runtime_budget.rs` contract, now under threads). This is the
/// abandon-and-continue-serially path: the workers trip, the meter rewinds,
/// and the leader's own scan produces the authentic refusal.
#[test]
fn budget_refusal_is_deterministic_and_serial_identical() {
    let _g = lock();
    setup_env();
    let path = tmp("parbudget");
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
        seed.query(&format!("INSERT INTO t (pk,a,f,s,g) VALUES {}", vals.join(",")), &[])
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
        "SELECT count(*) FROM t WHERE a > 0",
        "SELECT min(a), max(a) FROM t",
    ] {
        let a = refusal(&par, q);
        let b = refusal(&par, q);
        assert_eq!(a, b, "parallel refusal must be stable across runs on `{q}`");
        assert_eq!(a, refusal(&ser, q), "parallel refusal must equal serial's on `{q}`");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

// ---------------------------------------------------------------- collation

/// NOCASE min/max ties across morsel boundaries: the merged answer must keep
/// the serial FIRST-row spelling, which is also sqlite's.
#[test]
fn nocase_ties_keep_the_first_spelling() {
    let _g = lock();
    setup_env();
    let path = tmp("parnc");
    let _ = std::fs::remove_file(&path);
    let par = open(&path, 3, "");
    let ser = open(&path, 1, "");
    let mut script = String::new();
    let ddl = "CREATE TABLE nc (id INTEGER PRIMARY KEY, x TEXT COLLATE NOCASE, g INT)";
    par.query(ddl, &[]).unwrap();
    script.push_str(ddl);
    script.push_str(";\n");
    // Mixed-case duplicates spread across the whole PK range, so collation
    // classes span morsels and the tie rule is exercised at the merge.
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
        "SELECT min(x), max(x) FROM nc WHERE g > 0",
    ] {
        let before = mpedb::parallel_folds_engaged();
        let p = rows_of(&par, q);
        assert!(mpedb::parallel_folds_engaged() > before, "expected engagement on `{q}`");
        assert_eq!(p, rows_of(&ser, q), "parallel vs serial on `{q}`");
        assert_eq!(p, oracle_rows(&script, q), "vs bundled sqlite on `{q}`");
    }
    drop(par);
    drop(ser);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

// ---------------------------------------------------------------- explain

/// EXPLAIN states the gate honestly: eligible shapes say ELIGIBLE (never a
/// worker count — engagement is a runtime decision), refused shapes say
/// nothing at all.
#[test]
fn explain_states_the_parallel_choice() {
    let _g = lock();
    let fx = fixture(100);
    let text = |q: &str| match fx.par.query(q, &[]) {
        Ok(ExecResult::Explain(s)) => s,
        other => panic!("expected EXPLAIN text from `{q}`, got {other:?}"),
    };
    let yes = text("EXPLAIN SELECT sum(a), min(a) FROM t WHERE g > 1");
    assert!(yes.contains("parallel fold: eligible"), "eligible shape must say so:\n{yes}");
    for (q, why) in [
        ("EXPLAIN SELECT total(a) FROM t", "total is refused"),
        ("EXPLAIN SELECT avg(a) FROM t", "avg is refused"),
        ("EXPLAIN SELECT sum(f) FROM t", "float sum is refused"),
        ("EXPLAIN SELECT g, count(*) FROM t GROUP BY g", "GROUP BY is out of v1 scope"),
    ] {
        let no = text(q);
        assert!(!no.contains("parallel fold"), "{why} and must not claim eligibility:\n{no}");
    }
}

/// Snapshot identity, adversarially: with parallel folds engaged on one
/// handle, a writer moving a whole group's values between statements must
/// never produce a torn (mixed-snapshot) answer — every worker reads the
/// leader's pin, so each statement reflects exactly one commit.
#[test]
fn parallel_fold_reads_one_snapshot_under_writes() {
    let _g = lock();
    let fx = fixture(4000);
    let n: i64 = rows_of(&fx.ser, "SELECT count(a) FROM t WHERE g = 3")[0][0]
        .parse()
        .unwrap();
    let base: i64 = rows_of(&fx.ser, "SELECT sum(a) FROM t WHERE g = 3")[0][0]
        .parse()
        .unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let path = fx.path.clone();
        let writer = s.spawn({
            let stop = &stop;
            move || {
                let db = open(&path, 1, "");
                for _ in 0..200 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // One statement = one commit: every row of group 3 moves
                    // together, so any mix of pre/post rows in ONE read would
                    // break the congruence checked below.
                    db.query("UPDATE t SET a = a + 1000000 WHERE g = 3", &[]).unwrap();
                }
            }
        });
        for _ in 0..100 {
            let got = rows_of(&fx.par, "SELECT count(a), sum(a) FROM t WHERE g = 3");
            assert_eq!(
                got[0][0].parse::<i64>().unwrap(),
                n,
                "the count never changes; only the sum moves, by whole commits"
            );
            let sum: i64 = got[0][1].parse().unwrap();
            assert_eq!(
                (sum - base) % (1_000_000 * n),
                0,
                "a parallel fold mixed rows from two commits"
            );
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    });
}
