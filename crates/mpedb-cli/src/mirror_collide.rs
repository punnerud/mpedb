//! `mpedb mirror-collide` — SIGKILL crash fuzz for the mirror daemon
//! (DESIGN-MIRROR §10.4/§10.5). Two directions, `--mode pull|push`:
//!
//! **pull** (source→mpedb): N source-writer processes churn a WAL sqlite source
//! while a mirror-daemon process loops pull-apply; the parent SIGKILLs and
//! respawns the daemon on a tight cadence so kills land at EVERY instant —
//! mid-apply, mid-fsync, mid-attach. The source is the model (writers only ever
//! touch it), so a final drain must converge mpedb EXACTLY to it.
//!
//! **push** (mpedb→source): the mirror is switched to mpedb authority, N mpedb
//! writer processes churn the `.mpedb` (contending on the writer lock, each
//! write also emitting a CDC dirty entry in the SAME commit), and a push daemon
//! loops drain-push while the parent kills it. Here **mpedb is the model** and a
//! final drain must converge the SOURCE exactly to it. This exercises the other
//! half of §6: a kill between the source commit and the dirty-entry clear must
//! re-push idempotently (at-least-once), never lose or double-apply a write.
//!
//! Either way `verify` at the end is a total no-lost / no-dup check across all
//! the kills.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mpedb::{Database, ExecResult};
use mpedb_mirror::switch::{drain_pull, drain_push, switch_to_mpedb};
use mpedb_mirror::{import_sqlite, verify, ImportOptions, SqliteAdapter};
use mpedb_types::Value;
use rusqlite::Connection;

use crate::args;
use crate::util::{usage, CliResult, Failure, Watchdog};

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

struct Setup {
    source: PathBuf,
    mirror: PathBuf,
    exe: PathBuf,
    deadline: u128,
}

/// Fresh source + fresh import (which installs the changelog + triggers).
fn setup(dir: &Path, secs: u64) -> Result<Setup, Failure> {
    std::fs::create_dir_all(dir)?;
    let dir = dir.canonicalize()?;
    let source = dir.join("source.db");
    let mirror = dir.join("mirror.mpedb");
    for f in [&source, &mirror] {
        let _ = std::fs::remove_file(f);
    }
    for sidecar in ["source.db-wal", "source.db-shm"] {
        let _ = std::fs::remove_file(dir.join(sidecar));
    }
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
    Ok(Setup {
        source,
        mirror,
        exe: std::env::current_exe()?,
        deadline: now_ms() + secs as u128 * 1000,
    })
}

/// SIGKILL + respawn `spawn()` every `kill_ms` until the deadline. Returns
/// (kills, bad self-exits). Kills land at every instant of the daemon's loop.
fn kill_loop(
    deadline: u128,
    kill_ms: u64,
    spawn: &dyn Fn() -> std::io::Result<Child>,
) -> Result<(u64, u64), Failure> {
    let mut daemon = spawn()?;
    let mut kills = 0u64;
    let mut bad_exits = 0u64;
    while now_ms() < deadline {
        std::thread::sleep(Duration::from_millis(kill_ms));
        // did it die on its own (a bug) before we could kill it?
        if let Ok(Some(status)) = daemon.try_wait() {
            use std::os::unix::process::ExitStatusExt;
            if !status.success() && status.signal().is_none() {
                bad_exits += 1;
            }
            daemon = spawn()?;
            continue;
        }
        let _ = daemon.kill();
        let _ = daemon.wait();
        kills += 1;
        daemon = spawn()?;
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    Ok((kills, bad_exits))
}

fn reap(writers: Vec<Child>) -> Result<(), Failure> {
    for (id, mut w) in writers.into_iter().enumerate() {
        let status = w.wait()?;
        if !status.success() {
            return runtime(format!("writer {id} exited abnormally: {status}"));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["dir", "writers", "secs", "kill-ms", "keyspace", "mode"],
        &[],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let writers = p.u64_or("writers", 3)?.max(1);
    let secs = p.u64_or("secs", 5)?.max(1);
    let kill_ms = p.u64_or("kill-ms", 40)?.max(1);
    let keyspace = p.u64_or("keyspace", 16)?.max(1);
    match p.value("mode").unwrap_or("pull") {
        "pull" => run_pull(&dir, writers, secs, kill_ms, keyspace),
        "push" => run_push(&dir, writers, secs, kill_ms, keyspace),
        other => usage(format!("--mode must be pull or push, got `{other}`")),
    }
}

/// source→mpedb: source writers vs. a killed pull daemon. Source is the model.
fn run_pull(dir: &Path, writers: u64, secs: u64, kill_ms: u64, keyspace: u64) -> CliResult {
    let s = setup(dir, secs)?;
    let _wd = Watchdog::arm(secs + 60, "mirror-collide");
    let deadline_s = s.deadline.to_string();

    let mut writer_procs: Vec<Child> = Vec::new();
    for id in 0..writers {
        writer_procs.push(
            Command::new(&s.exe)
                .arg("mirror-collide-writer")
                .args(["--source", s.source.to_str().unwrap()])
                .args(["--deadline", &deadline_s])
                .args(["--keyspace", &keyspace.to_string()])
                .args(["--id", &id.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
    }

    let spawn = || -> std::io::Result<Child> {
        Command::new(&s.exe)
            .arg("mirror-collide-daemon")
            .args(["--db", s.mirror.to_str().unwrap()])
            .args(["--source", s.source.to_str().unwrap()])
            .args(["--deadline", &deadline_s])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
    };
    let (kills, bad_exits) = kill_loop(s.deadline, kill_ms, &spawn)?;
    reap(writer_procs)?;

    // final drain in the parent: must succeed (a corrupt/unrecoverable .mpedb
    // would fail to open or fail here) and then converge exactly to the source.
    let db = Database::open_from_file(&s.mirror)?;
    let mut adapter = SqliteAdapter::new(open_source(&s.source)?, None, &[])?;
    let applied = drain_pull(&db, &mut adapter)?;

    let src_rows = source_count(&s.source)?;
    let mp_rows = mpedb_count(&db)?;
    if !verify(&db, &mut adapter)? {
        return runtime(format!(
            "MIRROR-COLLIDE DIVERGENCE after {kills} kill(s): mpedb ({mp_rows} rows) \
             != source ({src_rows} rows) — a kill lost or duplicated data"
        ));
    }
    check_bad_exits(bad_exits)?;
    println!(
        "mirror-collide(pull): writers={writers} secs={secs} daemon-kills={kills} \
         final-drain={applied} rows={mp_rows} verify=ok — no data lost or duplicated across kills"
    );
    Ok(())
}

/// mpedb→source: mpedb writers vs. a killed push daemon. **mpedb is the model.**
/// Exercises §6's at-least-once write-back: a kill between the source commit and
/// the dirty-entry clear must re-push idempotently.
fn run_push(dir: &Path, writers: u64, secs: u64, kill_ms: u64, keyspace: u64) -> CliResult {
    let s = setup(dir, secs)?;
    let _wd = Watchdog::arm(secs + 60, "mirror-collide");

    // hand authority to mpedb: local writes now accumulate as the truth and
    // push is unconditional (local-wins), which is the drain this fuzz targets.
    {
        let db = Database::open_from_file(&s.mirror)?;
        let mut adapter = SqliteAdapter::new(open_source(&s.source)?, None, &[])?;
        switch_to_mpedb(&db, &mut adapter)?;
    }
    let deadline_s = s.deadline.to_string();

    let mut writer_procs: Vec<Child> = Vec::new();
    for id in 0..writers {
        writer_procs.push(
            Command::new(&s.exe)
                .arg("mirror-collide-mwriter")
                .args(["--db", s.mirror.to_str().unwrap()])
                .args(["--deadline", &deadline_s])
                .args(["--keyspace", &keyspace.to_string()])
                .args(["--id", &id.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
    }

    let spawn = || -> std::io::Result<Child> {
        Command::new(&s.exe)
            .arg("mirror-collide-pdaemon")
            .args(["--db", s.mirror.to_str().unwrap()])
            .args(["--source", s.source.to_str().unwrap()])
            .args(["--deadline", &deadline_s])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
    };
    let (kills, bad_exits) = kill_loop(s.deadline, kill_ms, &spawn)?;
    reap(writer_procs)?;

    // final drain in the parent: every surviving dirty entry must land, and the
    // source must then be byte-identical to mpedb.
    let db = Database::open_from_file(&s.mirror)?;
    let mut adapter = SqliteAdapter::new(open_source(&s.source)?, None, &[])?;
    let stats = drain_push(&db, &mut adapter)?;

    let src_rows = source_count(&s.source)?;
    let mp_rows = mpedb_count(&db)?;
    if !verify(&db, &mut adapter)? {
        return runtime(format!(
            "MIRROR-COLLIDE(push) DIVERGENCE after {kills} kill(s): source ({src_rows} rows) \
             != mpedb ({mp_rows} rows) — a kill lost or duplicated a write-back"
        ));
    }
    check_bad_exits(bad_exits)?;
    println!(
        "mirror-collide(push): writers={writers} secs={secs} daemon-kills={kills} \
         final-drain={}u/{}d rows={mp_rows} verify=ok — every local write reached the source \
         exactly once across kills",
        stats.upserts, stats.deletes
    );
    Ok(())
}

fn check_bad_exits(bad_exits: u64) -> Result<(), Failure> {
    if bad_exits > 0 {
        return runtime(format!(
            "the daemon self-exited with an error {bad_exits} time(s) — a real bug, \
             not a kill (see stderr above)"
        ));
    }
    Ok(())
}

fn source_count(path: &Path) -> Result<i64, Failure> {
    let c = open_source(path)?;
    c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
        .map_err(|e| Failure::Runtime(format!("source count: {e}")))
}

fn mpedb_count(db: &Database) -> Result<i64, Failure> {
    match db.query("SELECT id FROM t", &[])? {
        ExecResult::Rows { rows, .. } => Ok(rows.len() as i64),
        other => runtime(format!("unexpected read result: {other:?}")),
    }
}

// ------------------------------------------------------------------- writers

/// Hidden subcommand: churn random put/delete on the sqlite source until the
/// deadline (pull mode).
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

/// Hidden subcommand: churn random put/delete on the **mpedb** side until the
/// deadline (push mode). Several of these contend on the single writer lock;
/// each write commits its row and its CDC dirty entry in one COW commit.
pub fn run_mwriter(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["db", "deadline", "keyspace", "id"], &[])?;
    let db_path = PathBuf::from(p.require("db")?);
    let deadline: u128 = p.require("deadline")?.parse().map_err(|_| bad("deadline"))?;
    let keyspace = p.require_u64("keyspace")?.max(1);
    let id = p.require_u64("id")?;

    let db = Database::open_from_file(&db_path)?;
    // mpedb has no ON CONFLICT yet (task #21): update, and insert if it missed.
    let upd = db.prepare("UPDATE t SET v = $1 WHERE id = $2")?;
    let ins = db.prepare("INSERT INTO t (id, v) VALUES ($1, $2)")?;
    let del = db.prepare("DELETE FROM t WHERE id = $1")?;

    let mut state = 0x2545_f491_4f6c_dd1du64 ^ id.wrapping_mul(0x9e37_79b1);
    while now_ms() < deadline {
        for _ in 0..8 {
            let key = (xorshift(&mut state) % keyspace) as i64;
            if xorshift(&mut state).is_multiple_of(6) {
                db.execute(&del, &[Value::Int(key)])?;
            } else {
                let v = (xorshift(&mut state) % 1_000_000) as i64;
                let hit = matches!(
                    db.execute(&upd, &[Value::Int(v), Value::Int(key)])?,
                    ExecResult::Affected(n) if n > 0
                );
                if !hit {
                    // A racing writer may have inserted the key between our
                    // UPDATE and this INSERT (each statement is its own txn):
                    // losing that race is a legitimate outcome, not a bug.
                    match db.execute(&ins, &[Value::Int(key), Value::Int(v)]) {
                        Ok(_) => {}
                        Err(mpedb_types::Error::PrimaryKeyViolation { .. })
                        | Err(mpedb_types::Error::UniqueViolation { .. }) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- daemons

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

/// Hidden subcommand: loop drain-push until the deadline (or until SIGKILLed).
/// A kill between the source commit and the dirty-entry clear leaves the entry
/// behind; the next process re-pushes it idempotently (§6 at-least-once).
pub fn run_push_daemon(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["db", "source", "deadline"], &[])?;
    let db_path = PathBuf::from(p.require("db")?);
    let source = PathBuf::from(p.require("source")?);
    let deadline: u128 = p.require("deadline")?.parse().map_err(|_| bad("deadline"))?;

    let db = Database::open_from_file(&db_path)?;
    let mut adapter = SqliteAdapter::new(open_source(&source)?, None, &[])?;
    while now_ms() < deadline {
        drain_push(&db, &mut adapter)?;
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn bad(flag: &str) -> Failure {
    Failure::Usage(format!("--{flag} must be an integer"))
}
