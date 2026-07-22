//! The ADAPTIVE half of design/DESIGN-PARALLEL-READ.md §8, at its default
//! settings: **the data decides at run time, with no estimate.**
//!
//! This binary deliberately does NOT collapse `MPEDB_PAR_PROBE_ROWS` (that is
//! `par_fold.rs`'s job, so a small fixture can exercise the merge). Here the
//! probe stands at its shipped value, and the claims are the ones the design
//! is built on:
//!
//! - a scan SHORTER than the probe engages nothing at all — no thread, no
//!   structural cut, no reader census — which is why parallelism can be on by
//!   default without a row-estimate gate;
//! - a scan LONGER than the probe hands its tail over, on evidence, without
//!   anyone having predicted anything;
//! - either way the answers are identical, so the only difference is time.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// The engagement counter is process-global and these tests assert deltas on
/// it, so they serialize.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
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
  name = "g"
  type = "int64"
"#;

/// The shipped probe, mirrored from `exec/parallel.rs`. A scan below it must
/// never engage; the "long" fixture must exceed it.
const PROBE_ROWS: u64 = 32_768;

fn open(path: &str, threads: u32) -> Database {
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 128\nmax_readers = 8\n\n\
         [runtime]\nmax_query_threads = {threads}\n{SCHEMA}"
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

struct Fixture {
    db: Database,
    path: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

fn fixture(rows: u64, threads: u32) -> Fixture {
    let path = format!(
        "/dev/shm/mpedb-paradapt-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let db = open(&path, threads);
    for chunk in (1..=rows).collect::<Vec<_>>().chunks(500) {
        let vals: Vec<String> = chunk
            .iter()
            .map(|&pk| format!("({pk},{},{})", (pk as i64 * 37) % 1000 - 300, pk % 7))
            .collect();
        db.query(&format!("INSERT INTO t (pk,a,g) VALUES {}", vals.join(",")), &[])
            .unwrap();
    }
    Fixture { db, path }
}

fn scalar(db: &Database, q: &str) -> Vec<Value> {
    match db.query(q, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows.into_iter().next().unwrap_or_default(),
        other => panic!("expected rows from `{q}`, got {other:?}"),
    }
}

/// The shapes that WOULD parallelize, over both bodies (fused and residual).
const SHAPES: [&str; 6] = [
    "SELECT sum(a) FROM t",
    "SELECT min(a), max(a) FROM t",
    "SELECT count(a) FROM t",
    "SELECT sum(a) FROM t WHERE g <> 3",
    "SELECT count(*) FROM t WHERE a > 0",
    "SELECT min(a), max(a), count(*) FROM t WHERE pk > 10",
];

/// **The no-regression claim.** A scan far below the probe is eligible by
/// shape and still engages NOTHING: the leader answers it at serial speed, and
/// no thread is ever spawned. This is what makes the compile-time row-estimate
/// gate unnecessary — the small query never pays for the big query's machinery.
#[test]
fn a_short_scan_engages_nothing() {
    let _g = lock();
    let fx = fixture(4_000, 4);
    for q in SHAPES {
        let before = mpedb::parallel_folds_engaged();
        let _ = scalar(&fx.db, q);
        assert_eq!(
            mpedb::parallel_folds_engaged(),
            before,
            "a 4 000-row scan is below the {PROBE_ROWS}-row probe and must stay serial: `{q}`"
        );
    }
}

/// **The adaptive claim.** The same shapes over a scan LONGER than the probe
/// hand their tail to workers — decided by the rows this statement actually
/// read, not by any estimate — and answer exactly what the serial handle does.
#[test]
fn a_long_scan_hands_its_tail_over_and_answers_identically() {
    let _g = lock();
    let fx = fixture(PROBE_ROWS + 20_000, 4);
    let serial = open(&fx.path, 1);
    let mut engaged_any = false;
    for q in SHAPES {
        let before = mpedb::parallel_folds_engaged();
        let par = scalar(&fx.db, q);
        engaged_any |= mpedb::parallel_folds_engaged() > before;
        assert_eq!(par, scalar(&serial, q), "parallel vs serial on `{q}`");
    }
    assert!(
        engaged_any,
        "a {}-row scan must hand its tail to workers on at least one shape",
        PROBE_ROWS + 20_000
    );
}

/// A bounded PK range that is itself short must not engage even when the TABLE
/// is long: the probe counts the rows this statement reads, which is the whole
/// point of measuring instead of estimating.
#[test]
fn a_short_range_over_a_long_table_engages_nothing() {
    let _g = lock();
    let fx = fixture(PROBE_ROWS + 20_000, 4);
    for q in [
        "SELECT sum(a) FROM t WHERE pk < 1000",
        "SELECT count(*) FROM t WHERE pk >= 500 AND pk < 900",
    ] {
        let before = mpedb::parallel_folds_engaged();
        let _ = scalar(&fx.db, q);
        assert_eq!(
            mpedb::parallel_folds_engaged(),
            before,
            "a short range must stay serial however long the table is: `{q}`"
        );
    }
}

/// `max_query_threads = 1` is the off switch, and it is honoured however long
/// the scan proves to be.
#[test]
fn the_serial_knob_never_engages() {
    let _g = lock();
    let fx = fixture(PROBE_ROWS + 20_000, 1);
    for q in SHAPES {
        let before = mpedb::parallel_folds_engaged();
        let _ = scalar(&fx.db, q);
        assert_eq!(
            mpedb::parallel_folds_engaged(),
            before,
            "max_query_threads = 1 must never engage: `{q}`"
        );
    }
}
