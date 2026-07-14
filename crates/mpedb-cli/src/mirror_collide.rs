//! `mpedb mirror-collide` — SIGKILL crash fuzz for the mirror daemon
//! (DESIGN-MIRROR §10.4/§10.5). N source-writer processes churn a WAL sqlite
//! source while a mirror-daemon process loops pull-apply; the parent SIGKILLs and
//! respawns the daemon on a tight cadence so kills land at EVERY instant —
//! mid-apply, mid-fsync, mid-attach.
//!
//! The promise under test: a daemon killed at any instant leaves the `.mpedb`
//! recoverable (atomic meta double-buffer flip + robust-mutex EOWNERDEAD
//! recovery on re-attach), and once the writers stop, a final drain converges
//! mpedb EXACTLY to the source. The source is the model — writers only ever touch
//! it — so `verify` at the end is a total no-lost / no-dup check across all the
//! kills. Pull-only (source→mpedb); the push-side collide is a later refinement.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mpedb::Database;
use mpedb_mirror::switch::drain_pull;
use mpedb_mirror::{import_sqlite, verify, ImportOptions, SqliteAdapter};
use rusqlite::Connection;

use crate::args;
use crate::util::{CliResult, Failure, Watchdog};

fn runtime<T>(msg: impl Into<String>) -> Result<T, Failure> {
    Err(Failure::Runtime(msg.into()))
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

/// Open the sqlite source with the settings every actor shares: WAL (readers
/// never block the single writer) and a generous busy timeout.
fn open_source(path: &Path) -> Result<Connection, Failure> {
    let c = Connection::open(path)
        .map_err(|e| Failure::Runtime(format!("open sqlite `{}`: {e}", path.display())))?;
    c.busy_timeout(Duration::from_secs(15))
        .map_err(|e| Failure::Runtime(format!("busy_timeout: {e}")))?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .map_err(|e| Failure::Runtime(format!("pragma: {e}")))?;
    Ok(c)
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

// ------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["dir", "writers", "secs", "kill-ms", "keyspace"],
        &[],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let writers = p.u64_or("writers", 3)?.max(1);
    let secs = p.u64_or("secs", 5)?.max(1);
    let kill_ms = p.u64_or("kill-ms", 40)?.max(1);
    let keyspace = p.u64_or("keyspace", 16)?.max(1);

    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let source = dir.join("source.db");
    let mirror = dir.join("mirror.mpedb");
    for f in [&source, &mirror] {
        let _ = std::fs::remove_file(f);
    }
    for sidecar in ["source.db-wal", "source.db-shm"] {
        let _ = std::fs::remove_file(dir.join(sidecar));
    }

    // seed the source, then import (installs the changelog + triggers).
    {
        let c = open_source(&source)?;
        c.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
             INSERT INTO t(id,v) VALUES (0,0);",
        )
        .map_err(|e| Failure::Runtime(format!("seed source: {e}")))?;
    }
    {
        let mut c = open_source(&source)?;
        import_sqlite(&mut c, &mirror, &ImportOptions::default())?;
    }

    let _wd = Watchdog::arm(secs + 60, "mirror-collide");
    let exe = std::env::current_exe()?;
    let deadline = now_ms() + secs as u128 * 1000;
    let deadline_s = deadline.to_string();

    // source-writer processes (run to the deadline, then exit cleanly)
    let mut writer_procs: Vec<Child> = Vec::new();
    for id in 0..writers {
        let child = Command::new(&exe)
            .arg("mirror-collide-writer")
            .args(["--source", source.to_str().unwrap()])
            .args(["--deadline", &deadline_s])
            .args(["--keyspace", &keyspace.to_string()])
            .args(["--id", &id.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        writer_procs.push(child);
    }

    let spawn_daemon = || -> std::io::Result<Child> {
        Command::new(&exe)
            .arg("mirror-collide-daemon")
            .args(["--db", mirror.to_str().unwrap()])
            .args(["--source", source.to_str().unwrap()])
            .args(["--deadline", &deadline_s])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
    };

    // SIGKILL + respawn the daemon until the deadline. Kills land at every
    // instant of its pull-apply loop.
    let mut daemon = spawn_daemon()?;
    let mut kills = 0u64;
    let mut daemon_self_exit_bad = 0u64;
    while now_ms() < deadline {
        std::thread::sleep(Duration::from_millis(kill_ms));
        // did it die on its own (a bug) before we could kill it?
        if let Ok(Some(status)) = daemon.try_wait() {
            use std::os::unix::process::ExitStatusExt;
            if !status.success() && status.signal().is_none() {
                daemon_self_exit_bad += 1;
            }
            daemon = spawn_daemon()?;
            continue;
        }
        let _ = daemon.kill();
        let _ = daemon.wait();
        kills += 1;
        daemon = spawn_daemon()?;
    }

    // wind down: writers self-exit at the deadline; the daemon is killed for good
    // so the parent is the sole accessor for the final drain.
    for (id, mut w) in writer_procs.into_iter().enumerate() {
        let status = w.wait()?;
        if !status.success() {
            return runtime(format!("writer {id} exited abnormally: {status}"));
        }
    }
    let _ = daemon.kill();
    let _ = daemon.wait();

    // final drain in the parent: must succeed (a corrupt/unrecoverable .mpedb
    // would fail to open or fail here) and then converge exactly to the source.
    let db = Database::open_from_file(&mirror)?;
    let mut adapter = SqliteAdapter::new(open_source(&source)?, None, &[])?;
    let applied = drain_pull(&db, &mut adapter)?;

    let src_rows = source_count(&source)?;
    let mp_rows = mpedb_count(&db)?;
    if !verify(&db, &mut adapter)? {
        return runtime(format!(
            "MIRROR-COLLIDE DIVERGENCE after {kills} kill(s): mpedb ({mp_rows} rows) \
             != source ({src_rows} rows) — a kill lost or duplicated data"
        ));
    }
    if daemon_self_exit_bad > 0 {
        return runtime(format!(
            "the daemon self-exited with an error {daemon_self_exit_bad} time(s) — a real bug, \
             not a kill (see stderr above)"
        ));
    }

    println!(
        "mirror-collide: writers={writers} secs={secs} daemon-kills={kills} \
         final-drain={applied} rows={mp_rows} verify=ok — no data lost or duplicated across kills"
    );
    Ok(())
}

fn source_count(path: &Path) -> Result<i64, Failure> {
    let c = open_source(path)?;
    c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
        .map_err(|e| Failure::Runtime(format!("source count: {e}")))
}

fn mpedb_count(db: &Database) -> Result<i64, Failure> {
    match db.query("SELECT id FROM t", &[])? {
        mpedb::ExecResult::Rows { rows, .. } => Ok(rows.len() as i64),
        other => runtime(format!("unexpected read result: {other:?}")),
    }
}

// ------------------------------------------------------------------- writer

/// Hidden subcommand: churn random put/delete on the source until the deadline.
pub fn run_writer(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["source", "deadline", "keyspace", "id"], &[])?;
    let source = PathBuf::from(p.require("source")?);
    let deadline: u128 = p.require("deadline")?.parse().map_err(|_| bad("deadline"))?;
    let keyspace = p.require_u64("keyspace")?.max(1);
    let id = p.require_u64("id")?;

    let conn = open_source(&source)?;
    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ id.wrapping_mul(0x1000_0001b3);
    while now_ms() < deadline {
        // a small burst per loop, then re-check the clock
        for _ in 0..8 {
            let key = (xorshift(&mut state) % keyspace) as i64;
            if xorshift(&mut state).is_multiple_of(6) {
                conn.execute("DELETE FROM t WHERE id=?1", rusqlite::params![key])
                    .map_err(|e| Failure::Runtime(format!("writer delete: {e}")))?;
            } else {
                let v = (xorshift(&mut state) % 1_000_000) as i64;
                conn.execute(
                    "INSERT INTO t(id,v) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET v=excluded.v",
                    rusqlite::params![key, v],
                )
                .map_err(|e| Failure::Runtime(format!("writer upsert: {e}")))?;
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- daemon

/// Hidden subcommand: loop pull-apply until the deadline (or until SIGKILLed by
/// the parent). Each `drain_pull` catches mpedb up to the source; a kill mid-flight
/// is recovered on the next process's attach.
pub fn run_daemon(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["db", "source", "deadline"], &[])?;
    let db_path = PathBuf::from(p.require("db")?);
    let source = PathBuf::from(p.require("source")?);
    let deadline: u128 = p.require("deadline")?.parse().map_err(|_| bad("deadline"))?;

    let db = Database::open_from_file(&db_path)?;
    let mut adapter = SqliteAdapter::new(open_source(&source)?, None, &[])?;
    while now_ms() < deadline {
        drain_pull(&db, &mut adapter)?;
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn bad(flag: &str) -> Failure {
    Failure::Usage(format!("--{flag} must be an integer"))
}
