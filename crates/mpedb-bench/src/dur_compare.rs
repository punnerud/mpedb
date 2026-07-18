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
use crate::workloads::{measure_batched_insert, measure_single_insert, RunCfg};

/// Rows per batch in the "batched" (amortized-fsync) rows.
pub const BATCH: i64 = 100;

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
    only.as_ref().is_none_or(|f| key.contains(f.as_str()))
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

/// Run the whole class comparison. `disk_base` MUST be real disk (ext4);
/// `tmpfs_base` only holds the PostgreSQL unix socket (a short path). Progress
/// goes to stderr; the caller renders the returned rows.
pub fn run(
    disk_base: &Path,
    tmpfs_base: &Path,
    cfg: &RunCfg,
    only: &Option<String>,
    labels: &Labels,
) -> Vec<DurRow> {
    let mut rows = Vec::new();

    // -------------------------------------------------------------- mpedb
    if wanted("mpedb", only) {
        eprintln!("  [dur] mpedb wal (durable-on-ack)");
        match MpedbEngine::new(disk_base.join("dur-mpedb-wal"), "wal") {
            Ok(mut e) => {
                rows.push(row(DURABLE_ON_ACK, &labels.mpedb, "disk, durability=wal",
                    "single client", measure(&mut e, cfg, None)));
                rows.push(row(DURABLE_ON_ACK, &labels.mpedb, "disk, durability=wal",
                    &format!("batched WriteSession, {BATCH}/commit"),
                    measure(&mut e, cfg, Some(BATCH))));
            }
            Err(e) => rows.push(row(DURABLE_ON_ACK, &labels.mpedb, "disk, durability=wal",
                "single client", Err(e))),
        }
        eprintln!("  [dur] mpedb async (deferred)");
        match MpedbEngine::new(disk_base.join("dur-mpedb-async"), "async") {
            Ok(mut e) => rows.push(row(DEFERRED, &labels.mpedb, "disk, durability=async",
                "single client", measure(&mut e, cfg, None))),
            Err(e) => rows.push(row(DEFERRED, &labels.mpedb, "disk, durability=async",
                "single client", Err(e))),
        }
    }

    // ------------------------------------------------------------- sqlite
    if wanted("sqlite", only) {
        eprintln!("  [dur] sqlite FULL+WAL (durable-on-ack)");
        match SqliteEngine::new(disk_base.join("dur-sqlite-full"), SqliteMode::CommitClass) {
            Ok(mut e) => {
                rows.push(row(DURABLE_ON_ACK, &labels.sqlite, "disk, sync=FULL+WAL",
                    "single client", measure(&mut e, cfg, None)));
                rows.push(row(DURABLE_ON_ACK, &labels.sqlite, "disk, sync=FULL+WAL",
                    &format!("batched txn, {BATCH}/commit"),
                    measure(&mut e, cfg, Some(BATCH))));
            }
            Err(e) => rows.push(row(DURABLE_ON_ACK, &labels.sqlite, "disk, sync=FULL+WAL",
                "single client", Err(e))),
        }
        eprintln!("  [dur] sqlite NORMAL+WAL (deferred)");
        match SqliteEngine::new(disk_base.join("dur-sqlite-normal"), SqliteMode::NormalClass) {
            Ok(mut e) => rows.push(row(DEFERRED, &labels.sqlite, "disk, sync=NORMAL+WAL",
                "single client", measure(&mut e, cfg, None))),
            Err(e) => rows.push(row(DEFERRED, &labels.sqlite, "disk, sync=NORMAL+WAL",
                "single client", Err(e))),
        }
    }

    // ------------------------------------------------------------- postgres
    if wanted("postgres", only) {
        eprintln!("  [dur] postgres synchronous_commit=on (durable-on-ack)");
        let sock = tmpfs_base.join("dur-pgsock-on");
        match PgServer::start_general(disk_base.join("dur-pgdata-on"), sock, "on", "on")
            .and_then(PgEngine::new)
        {
            Ok(mut e) => {
                rows.push(row(DURABLE_ON_ACK, &labels.pg, "disk, fsync=on+sc=on",
                    "single client", measure(&mut e, cfg, None)));
                rows.push(row(DURABLE_ON_ACK, &labels.pg, "disk, fsync=on+sc=on",
                    &format!("batched txn, {BATCH}/commit"),
                    measure(&mut e, cfg, Some(BATCH))));
            }
            Err(e) => rows.push(row(DURABLE_ON_ACK, &labels.pg, "disk, fsync=on+sc=on",
                "single client", Err(e))),
        }
        eprintln!("  [dur] postgres synchronous_commit=off (deferred)");
        let sock = tmpfs_base.join("dur-pgsock-off");
        match PgServer::start_general(disk_base.join("dur-pgdata-off"), sock, "on", "off")
            .and_then(PgEngine::new)
        {
            Ok(mut e) => rows.push(row(DEFERRED, &labels.pg, "disk, fsync=on+sc=off",
                "single client", measure(&mut e, cfg, None))),
            Err(e) => rows.push(row(DEFERRED, &labels.pg, "disk, fsync=on+sc=off",
                "single client", Err(e))),
        }
    }

    rows
}
