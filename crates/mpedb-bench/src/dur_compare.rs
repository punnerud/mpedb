//! Single-client durable point-insert, compared BY DURABILITY CLASS
//! (design/DESIGN.md §5.4). This is the focused answer to "make single-client durable
//! INSERTs competitive with SQLite and PostgreSQL": one sequential writer, on
//! the real ext4 disk, in each honest class.
//!
//! Two classes, never cross-compared:
//! - **durable-on-ack**: a commit is power-loss-durable the instant it returns.
//!   mpedb `wal` vs SQLite `synchronous=FULL`+WAL vs PostgreSQL
//!   `synchronous_commit=on`. The floor here is one fsync per commit.
//! - **crash-consistent-deferred**: a commit is crash-consistent immediately
//!   but power loss may lose a bounded recent window (fsync is coalesced, not
//!   per commit). mpedb `async` vs SQLite `synchronous=NORMAL`+WAL vs
//!   PostgreSQL `synchronous_commit=off`.
//!
//! Plus, for the durable-on-ack class, a BATCHED row per engine (N inserts in
//! one commit) — the honest advice for clients that need both durability and
//! speed: amortize the fsync.

use std::path::Path;

use crate::eng_mpedb::MpedbEngine;
use crate::eng_pg::{PgEngine, PgServer};
use crate::eng_sqlite::{SqliteEngine, SqliteMode};
use crate::engines::Engine;
use crate::util::{BResult, LatStats};
use crate::workloads::{
    measure_batched_insert, measure_batched_insert_from, measure_single_insert, RunCfg,
};

/// Rows per batch in the "batched" (amortized-fsync) rows.
pub const BATCH: i64 = 100;

/// How many interleaved reps for durable-on-ack **batched** cells. One-shot
/// measurements on a shared laptop disk sit inside host drift (M3 showed
/// mpedb 27.1k vs sqlite 27.4k, then 25k vs 31k on the next full suite);
/// BENCHMARKS.md's control-group method requires ratios formed inside a
/// rep and summarized across reps. Single-client rows stay one-shot.
const BATCH_REPS: usize = 7;

pub const DURABLE_ON_ACK: &str = "durable-on-ack";
pub const DEFERRED: &str = "crash-consistent-deferred";

pub struct DurRow {
    pub class: &'static str,
    pub engine: String,
    pub config: String,
    pub note: String,
    pub outcome: Result<LatStats, String>,
}

/// Engine version labels, so the section matches the main matrix's naming.
pub struct Labels {
    pub mpedb: String,
    pub sqlite: String,
    pub pg: String,
}

fn wanted(key: &str, only: &Option<String>) -> bool {
    crate::util::only_matches(key, only)
}

fn measure(engine: &mut dyn Engine, cfg: &RunCfg, batch: Option<i64>) -> BResult<LatStats> {
    engine.reset_and_seed(cfg.seed_rows)?;
    let mut conn = engine.conn()?;
    match batch {
        None => measure_single_insert(&mut *conn, cfg),
        Some(b) => measure_batched_insert(&mut *conn, cfg, b),
    }
}

fn row(
    class: &'static str,
    engine: &str,
    config: &str,
    note: &str,
    r: BResult<LatStats>,
) -> DurRow {
    DurRow {
        class,
        engine: engine.to_string(),
        config: config.to_string(),
        note: note.to_string(),
        outcome: r.map_err(|e| e.to_string()),
    }
}

/// Median of successful samples; if none, the last error (or a static note).
fn median_lat(samples: Vec<BResult<LatStats>>) -> BResult<LatStats> {
    let mut ok: Vec<LatStats> = Vec::new();
    let mut last_err = None;
    for s in samples {
        match s {
            Ok(v) => ok.push(v),
            Err(e) => last_err = Some(e),
        }
    }
    if ok.is_empty() {
        return Err(last_err.unwrap_or_else(|| "no samples".into()));
    }
    ok.sort_by(|a, b| {
        a.ops_per_s()
            .partial_cmp(&b.ops_per_s())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(ok[ok.len() / 2].clone())
}

/// Run the whole class comparison. `disk_base` MUST be real disk (ext4);
/// `tmpfs_base` only holds the PostgreSQL unix socket (a short path). Progress
/// goes to stderr; the caller renders the returned rows.
///
/// Durable-on-ack **batched** rows are measured with [`BATCH_REPS`] interleaved
/// round-robin reps (mpedb → sqlite → postgres each rep) and reported as the
/// median ops/s. That is the control-group method required by BENCHMARKS.md
/// for a published ratio on a drifting host.
pub fn run(
    disk_base: &Path,
    tmpfs_base: &Path,
    cfg: &RunCfg,
    only: &Option<String>,
    labels: &Labels,
) -> Vec<DurRow> {
    let mut rows = Vec::new();

    // -------------------------------------------------------------- mpedb
    let mut mpedb_wal = if wanted("mpedb", only) {
        eprintln!("  [dur] mpedb wal (durable-on-ack)");
        match MpedbEngine::new(disk_base.join("dur-mpedb-wal"), "wal") {
            Ok(mut e) => {
                rows.push(row(
                    DURABLE_ON_ACK,
                    &labels.mpedb,
                    "disk, durability=wal",
                    "single client",
                    measure(&mut e, cfg, None),
                ));
                Some(e)
            }
            Err(e) => {
                rows.push(row(
                    DURABLE_ON_ACK,
                    &labels.mpedb,
                    "disk, durability=wal",
                    "single client",
                    Err(e),
                ));
                None
            }
        }
    } else {
        None
    };

    if wanted("mpedb", only) {
        eprintln!("  [dur] mpedb async (deferred)");
        match MpedbEngine::new(disk_base.join("dur-mpedb-async"), "async") {
            Ok(mut e) => rows.push(row(
                DEFERRED,
                &labels.mpedb,
                "disk, durability=async",
                "single client",
                measure(&mut e, cfg, None),
            )),
            Err(e) => rows.push(row(
                DEFERRED,
                &labels.mpedb,
                "disk, durability=async",
                "single client",
                Err(e),
            )),
        }
    }

    // ------------------------------------------------------------- sqlite
    let mut sqlite_full = if wanted("sqlite", only) {
        eprintln!("  [dur] sqlite FULL+WAL (durable-on-ack)");
        match SqliteEngine::new(disk_base.join("dur-sqlite-full"), SqliteMode::CommitClass) {
            Ok(mut e) => {
                rows.push(row(
                    DURABLE_ON_ACK,
                    &labels.sqlite,
                    "disk, sync=FULL+WAL",
                    "single client",
                    measure(&mut e, cfg, None),
                ));
                Some(e)
            }
            Err(e) => {
                rows.push(row(
                    DURABLE_ON_ACK,
                    &labels.sqlite,
                    "disk, sync=FULL+WAL",
                    "single client",
                    Err(e),
                ));
                None
            }
        }
    } else {
        None
    };

    if wanted("sqlite", only) {
        eprintln!("  [dur] sqlite NORMAL+WAL (deferred)");
        match SqliteEngine::new(disk_base.join("dur-sqlite-normal"), SqliteMode::NormalClass) {
            Ok(mut e) => rows.push(row(
                DEFERRED,
                &labels.sqlite,
                "disk, sync=NORMAL+WAL",
                "single client",
                measure(&mut e, cfg, None),
            )),
            Err(e) => rows.push(row(
                DEFERRED,
                &labels.sqlite,
                "disk, sync=NORMAL+WAL",
                "single client",
                Err(e),
            )),
        }
    }

    // ------------------------------------------------------------- postgres
    let mut pg_on = if wanted("postgres", only) {
        eprintln!("  [dur] postgres synchronous_commit=on (durable-on-ack)");
        let sock = tmpfs_base.join("dur-pgsock-on");
        match PgServer::start_general(disk_base.join("dur-pgdata-on"), sock, "on", "on")
            .and_then(PgEngine::new)
        {
            Ok(mut e) => {
                rows.push(row(
                    DURABLE_ON_ACK,
                    &labels.pg,
                    "disk, fsync=on+sc=on",
                    "single client",
                    measure(&mut e, cfg, None),
                ));
                Some(e)
            }
            Err(e) => {
                rows.push(row(
                    DURABLE_ON_ACK,
                    &labels.pg,
                    "disk, fsync=on+sc=on",
                    "single client",
                    Err(e),
                ));
                None
            }
        }
    } else {
        None
    };

    if wanted("postgres", only) {
        eprintln!("  [dur] postgres synchronous_commit=off (deferred)");
        let sock = tmpfs_base.join("dur-pgsock-off");
        match PgServer::start_general(disk_base.join("dur-pgdata-off"), sock, "on", "off")
            .and_then(PgEngine::new)
        {
            Ok(mut e) => rows.push(row(
                DEFERRED,
                &labels.pg,
                "disk, fsync=on+sc=off",
                "single client",
                measure(&mut e, cfg, None),
            )),
            Err(e) => rows.push(row(
                DEFERRED,
                &labels.pg,
                "disk, fsync=on+sc=off",
                "single client",
                Err(e),
            )),
        }
    }

    // ---- interleaved multi-rep batched durable-on-ack (control-group) ----
    //
    // Seed ONCE per engine, then interleave only the timed batch loops with
    // disjoint id ranges. Re-seeding 50k rows between every arm (the old path)
    // spent most of the wall time in unmeasured setup and left the second arm
    // on a different disk-cache / F_FULLFSYNC thermal state every rep — which
    // on M3 systematically inflated the lucky arm. Control-group fairness is
    // "same host state for both timed loops", not "full file recreate".
    eprintln!(
        "  [dur] batched durable-on-ack: {BATCH_REPS} interleaved reps (median ops/s, seed-once)"
    );
    // Id bands: 1e9 + rep*1e7 + arm*1e6 — never collides with seed 0..seed_rows.
    const ID_BASE: i64 = 1_000_000_000;
    const REP_STRIDE: i64 = 10_000_000;
    const ARM_STRIDE: i64 = 1_000_000;

    let mut mpedb_conn = None;
    let mut sqlite_conn = None;
    let mut pg_conn = None;
    let mut mpedb_seed_err: Option<String> = None;
    let mut sqlite_seed_err: Option<String> = None;
    let mut pg_seed_err: Option<String> = None;
    if let Some(e) = mpedb_wal.as_mut() {
        match e.reset_and_seed(cfg.seed_rows).and_then(|_| e.conn()) {
            Ok(c) => mpedb_conn = Some(c),
            Err(err) => mpedb_seed_err = Some(err.to_string()),
        }
    }
    if let Some(e) = sqlite_full.as_mut() {
        match e.reset_and_seed(cfg.seed_rows).and_then(|_| e.conn()) {
            Ok(c) => sqlite_conn = Some(c),
            Err(err) => sqlite_seed_err = Some(err.to_string()),
        }
    }
    if let Some(e) = pg_on.as_mut() {
        match e.reset_and_seed(cfg.seed_rows).and_then(|_| e.conn()) {
            Ok(c) => pg_conn = Some(c),
            Err(err) => pg_seed_err = Some(err.to_string()),
        }
    }

    let mut mpedb_samples = Vec::new();
    let mut sqlite_samples = Vec::new();
    let mut pg_samples = Vec::new();
    for rep in 0..BATCH_REPS {
        let order_mpedb_first = rep % 2 == 0;
        let base_m = ID_BASE + rep as i64 * REP_STRIDE;
        let base_s = ID_BASE + rep as i64 * REP_STRIDE + ARM_STRIDE;
        let base_p = ID_BASE + rep as i64 * REP_STRIDE + 2 * ARM_STRIDE;
        if order_mpedb_first {
            if let Some(c) = mpedb_conn.as_mut() {
                mpedb_samples.push(measure_batched_insert_from(c.as_mut(), cfg, BATCH, base_m));
            }
            if let Some(c) = sqlite_conn.as_mut() {
                sqlite_samples.push(measure_batched_insert_from(c.as_mut(), cfg, BATCH, base_s));
            }
            if let Some(c) = pg_conn.as_mut() {
                pg_samples.push(measure_batched_insert_from(c.as_mut(), cfg, BATCH, base_p));
            }
        } else {
            if let Some(c) = sqlite_conn.as_mut() {
                sqlite_samples.push(measure_batched_insert_from(c.as_mut(), cfg, BATCH, base_s));
            }
            if let Some(c) = mpedb_conn.as_mut() {
                mpedb_samples.push(measure_batched_insert_from(c.as_mut(), cfg, BATCH, base_m));
            }
            if let Some(c) = pg_conn.as_mut() {
                pg_samples.push(measure_batched_insert_from(c.as_mut(), cfg, BATCH, base_p));
            }
        }
    }
    if wanted("mpedb", only) {
        let outcome = match mpedb_seed_err {
            Some(e) => Err(e.into()),
            None => median_lat(mpedb_samples),
        };
        rows.push(row(
            DURABLE_ON_ACK,
            &labels.mpedb,
            "disk, durability=wal",
            &format!("batched WriteSession, {BATCH}/commit"),
            outcome,
        ));
    }
    if wanted("sqlite", only) {
        let outcome = match sqlite_seed_err {
            Some(e) => Err(e.into()),
            None => median_lat(sqlite_samples),
        };
        rows.push(row(
            DURABLE_ON_ACK,
            &labels.sqlite,
            "disk, sync=FULL+WAL",
            &format!("batched txn, {BATCH}/commit"),
            outcome,
        ));
    }
    if wanted("postgres", only) {
        let outcome = match pg_seed_err {
            Some(e) => Err(e.into()),
            None => median_lat(pg_samples),
        };
        rows.push(row(
            DURABLE_ON_ACK,
            &labels.pg,
            "disk, fsync=on+sc=on",
            &format!("batched txn, {BATCH}/commit"),
            outcome,
        ));
    }

    rows
}
