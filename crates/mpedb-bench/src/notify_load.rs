//! **Arm E — acting on a notification under load (#142).**
//!
//! Arms A–D measure what a *notification* costs. This measures what **acting on
//! one** costs, which is the half that decides whether the feature is usable:
//! a listener wakes, reads, thinks, and writes — and while it thinks, everyone
//! else is either blocked or not.
//!
//! Both engines run **real processes**, not threads: this binary re-invokes
//! itself as a worker. For mpedb that is the honest model (its whole premise is
//! separate processes attaching to shared memory); for PostgreSQL a connection
//! is a backend process anyway, so it makes the two comparable rather than
//! flattering either.
//!
//! ## The shape of one action
//!
//! ```text
//! read a row  ->  think for DELAY_MS  ->  write that row + an audit row
//! ```
//!
//! The think time is the entire point. It stands for whatever a real handler
//! does between deciding and committing — call an API, render, compute — and it
//! is exactly the window a lock has to be held across, or not.
//!
//! * **PostgreSQL** takes `SELECT … FOR UPDATE` before thinking, so the row is
//!   locked for the whole delay. That is the natural, correct way to write it
//!   there, and it is what `SKIP LOCKED`/advisory locks exist to manage.
//! * **mpedb** holds nothing. It captures a snapshot, thinks, then commits
//!   through a guard declaring the statements the action may run
//!   (`begin_guarded_for`). A collision is refused at commit and retried; it is
//!   never waited on.
//!
//! ## Two sub-arms, because the interesting answer is a comparison of them
//!
//! * **E-shard — one shard per user.** Worker `w` only ever touches rows where
//!   `user_id % workers == w`. Nothing collides, by construction. This is the
//!   arm that says what the machinery costs when it is *not* contended, and it
//!   is the shape a real per-user workload has.
//! * **E-hot — every worker on the same small set.** Deliberate collision. Here
//!   PostgreSQL serializes on the row lock and each waiter pays the full think
//!   time; mpedb refuses immediately and retries. This is where the two models
//!   stop being equivalent.
//!
//! Reporting both is the honest framing: if E-shard showed a large gap the
//! feature would be paying for itself in the common case, and it does not.
//! The gap is in E-hot, and it is a property of holding versus not holding.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::eng_pg::PgServer;
use crate::util::BResult;

/// Actions each worker attempts.
const ACTIONS: usize = 60;
/// Think time between the read and the write. Long enough that a held lock is
/// unmistakably visible against process startup noise, short enough that the
/// whole cell finishes in a couple of minutes.
const DELAY_MS: u64 = 20;
/// Distinct users. In E-shard each worker owns a disjoint slice of these; in
/// E-hot every worker fights over `HOT` of them.
const USERS: i64 = 512;
const HOT: i64 = 4;
/// Worker counts to scale across.
const WORKERS: [usize; 3] = [2, 4, 8];

pub struct LoadResult {
    pub engine: &'static str,
    pub arm: String,
    pub workers: usize,
    pub actions_per_sec: f64,
    pub p50_ms: u64,
    pub p99_ms: u64,
    /// mpedb: guard refusals. postgres: not applicable (it waits instead of
    /// refusing, which is the whole difference — the cost shows up in latency).
    pub retries: u64,
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

// ------------------------------------------------------------------ worker

/// The worker half, run in a child process. Prints one line per completed
/// action: `<elapsed_micros>`; then a final `RETRIES <n>`.
///
/// Argument order: `<engine> <target> <mode> <worker> <workers>`.
pub fn worker_main(argv: &[String]) -> BResult<()> {
    let [engine, target, mode, w, nw] = argv else {
        return Err("notify-worker needs <engine> <target> <mode> <worker> <workers>".into());
    };
    let w: i64 = w.parse().map_err(|_| "bad worker id")?;
    let nw: i64 = nw.parse().map_err(|_| "bad worker count")?;
    let hot = mode.starts_with("hot");
    // The control: identical work, no guard. Without it a flat mpedb line
    // cannot be attributed — it could be the guard's retries or it could be
    // the single writer lock, and those call for opposite fixes.
    let guarded = !mode.ends_with("-noguard");

    // Deterministic per-worker user sequence. In shard mode the worker owns a
    // residue class, so two workers can never pick the same user; in hot mode
    // everyone draws from the same tiny set.
    let user_at = |i: usize| -> i64 {
        if hot {
            (i as i64) % HOT
        } else {
            let per = USERS / nw.max(1);
            w * per + (i as i64) % per.max(1)
        }
    };

    let mut lat = Vec::with_capacity(ACTIONS);
    let mut retries = 0u64;

    match engine.as_str() {
        "mpedb" => {
            use mpedb::{Config, Database, Error, Value};
            let db = Database::open(Path::new(target)).or_else(|_| {
                Database::open_with_config(Config::from_toml_str(&std::fs::read_to_string(
                    target,
                )?)?)
            })?;
            // Declared once: the statements this action MAY run. Compilation is
            // content-hash cached, so re-declaring per action is a lookup.
            let may_run = [
                "SELECT bal FROM acct WHERE id = $1",
                "UPDATE acct SET bal = $1 WHERE id = $2",
                "INSERT INTO audit (id, acct, note) VALUES ($1, $2, $3)",
            ];
            let mut audit_id = w * 1_000_000;
            for i in 0..ACTIONS {
                let user = user_at(i);
                let t0 = Instant::now();
                loop {
                    let snap = db.snapshot_txn();
                    // read
                    let bal = match db.query(may_run[0], &[Value::Int(user)])? {
                        mpedb::ExecResult::Rows { rows, .. } if !rows.is_empty() => {
                            match rows[0][0] {
                                Value::Int(b) => b,
                                _ => 0,
                            }
                        }
                        _ => 0,
                    };
                    // think — holding NOTHING
                    std::thread::sleep(Duration::from_millis(DELAY_MS));
                    // act, guarded
                    audit_id += 1;
                    let mut s = if guarded {
                        db.begin_guarded_for(snap, &may_run)?
                    } else {
                        db.begin()?
                    };
                    s.query(may_run[1], &[Value::Int(bal + 1), Value::Int(user)])?;
                    s.query(
                        may_run[2],
                        &[Value::Int(audit_id), Value::Int(user), Value::Int(bal + 1)],
                    )?;
                    match s.commit() {
                        Ok(()) => break,
                        Err(Error::WriteConflict) => {
                            retries += 1;
                            continue;
                        }
                        Err(e) => return Err(format!("mpedb commit: {e}").into()),
                    }
                }
                lat.push(t0.elapsed().as_micros() as u64);
            }
        }
        "postgres" => {
            let mut c = postgres::Client::connect(target, postgres::NoTls)
                .map_err(|e| format!("pg connect: {e}"))?;
            let mut audit_id = w * 1_000_000;
            for i in 0..ACTIONS {
                let user = user_at(i);
                let t0 = Instant::now();
                let mut txn = c.transaction().map_err(|e| format!("begin: {e}"))?;
                // read AND lock — the row stays locked across the think time.
                // This is the ordinary correct way to write it here, and it is
                // what the comparison is about.
                let row = txn
                    .query_one("SELECT bal FROM acct WHERE id = $1 FOR UPDATE", &[&user])
                    .map_err(|e| format!("select for update: {e}"))?;
                let bal: i64 = row.get(0);
                std::thread::sleep(Duration::from_millis(DELAY_MS));
                audit_id += 1;
                txn.execute("UPDATE acct SET bal = $1 WHERE id = $2", &[&(bal + 1), &user])
                    .map_err(|e| format!("update: {e}"))?;
                txn.execute(
                    "INSERT INTO audit (id, acct, note) VALUES ($1, $2, $3)",
                    &[&audit_id, &user, &(bal + 1)],
                )
                .map_err(|e| format!("insert: {e}"))?;
                txn.batch_execute("NOTIFY acct_ch, 'x'").map_err(|e| format!("notify: {e}"))?;
                txn.commit().map_err(|e| format!("commit: {e}"))?;
                lat.push(t0.elapsed().as_micros() as u64);
            }
        }
        other => return Err(format!("unknown engine {other}").into()),
    }

    for l in &lat {
        println!("{l}");
    }
    println!("RETRIES {retries}");
    Ok(())
}

// ------------------------------------------------------------------ parent

fn spawn_and_collect(
    engine: &'static str,
    target: &str,
    mode: &str,
    workers: usize,
) -> BResult<(f64, u64, u64, u64)> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let t0 = Instant::now();
    let mut kids = Vec::new();
    for w in 0..workers {
        kids.push(
            Command::new(&exe)
                .args([
                    "--notify-worker",
                    engine,
                    target,
                    mode,
                    &w.to_string(),
                    &workers.to_string(),
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("spawn worker: {e}"))?,
        );
    }
    let mut lat = Vec::new();
    let mut retries = 0u64;
    for k in kids {
        let out = k.wait_with_output().map_err(|e| format!("worker wait: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "worker failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )
            .into());
        }
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(n) = line.strip_prefix("RETRIES ") {
                retries += n.trim().parse::<u64>().unwrap_or(0);
            } else if let Ok(v) = line.trim().parse::<u64>() {
                lat.push(v);
            }
        }
    }
    let elapsed = t0.elapsed().as_secs_f64();
    lat.sort_unstable();
    let total = workers * ACTIONS;
    Ok((
        total as f64 / elapsed,
        pct(&lat, 0.50) / 1000,
        pct(&lat, 0.99) / 1000,
        retries,
    ))
}

/// Fresh database per measurement. Not only to keep audit ids from colliding
/// across sub-arms — a run must not inherit the previous one's table size, or
/// the later combinations are measured against a bigger tree than the earlier
/// ones and the scaling column stops meaning what it says.
fn mpedb_setup(dir: &Path) -> BResult<String> {
    let cfg = dir.join("notify-load.toml");
    let db = dir.join("notify-load.mpedb");
    let _ = std::fs::remove_file(&db);
    std::fs::write(
        &cfg,
        format!(
            r#"
[database]
path = "{}"
size_mb = 256
max_readers = 64
durability = "wal"

[[table]]
name = "acct"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "bal"
  type = "int64"

[[table]]
name = "audit"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "acct"
  type = "int64"

  [[table.column]]
  name = "note"
  type = "int64"
"#,
            db.display()
        ),
    )?;
    let s = cfg.to_string_lossy().into_owned();
    {
        use mpedb::{Config, Database, Value};
        let d = Database::open_with_config(Config::from_toml_str(&std::fs::read_to_string(&s)?)?)?;
        for i in 0..USERS {
            d.query("INSERT INTO acct (id, bal) VALUES ($1, 0)", &[Value::Int(i)])?;
        }
    }
    Ok(s)
}

fn pg_setup(pg: &PgServer) -> BResult<String> {
    let conn = pg.conn_str();
    let mut c = postgres::Client::connect(&conn, postgres::NoTls)
        .map_err(|e| format!("pg setup: {e}"))?;
    c.batch_execute(
        "DROP TABLE IF EXISTS acct; DROP TABLE IF EXISTS audit;
         CREATE TABLE acct (id bigint primary key, bal bigint not null);
         CREATE TABLE audit (id bigint primary key, acct bigint, note bigint);",
    )
    .map_err(|e| format!("pg schema: {e}"))?;
    for i in 0..USERS {
        c.execute("INSERT INTO acct (id, bal) VALUES ($1, 0)", &[&i])
            .map_err(|e| format!("pg seed: {e}"))?;
    }
    Ok(conn)
}

pub fn run(scratch: PathBuf) -> BResult<()> {
    std::fs::create_dir_all(&scratch)?;
    println!(
        "arm E: acting on a notification. {ACTIONS} actions/worker, {DELAY_MS} ms think time \
         between read and write, REAL PROCESSES on both engines."
    );
    println!(
        "  E-shard = one shard per user (worker w owns a disjoint slice) — nothing collides.\n  \
         E-hot   = every worker on {HOT} users — deliberate collision.\n  \
         postgres holds SELECT ... FOR UPDATE across the think time; mpedb holds nothing and \
         guards at commit."
    );
    println!();

    let mut rows: Vec<LoadResult> = Vec::new();

    for mode in ["shard-noguard", "shard", "hot"] {
        for &w in &WORKERS {
            let target = mpedb_setup(&scratch)?;
            let (aps, p50, p99, retries) = spawn_and_collect("mpedb", &target, mode, w)?;
            rows.push(LoadResult {
                engine: "mpedb",
                arm: format!("E-{mode}"),
                workers: w,
                actions_per_sec: aps,
                p50_ms: p50,
                p99_ms: p99,
                retries,
            });
        }
    }

    let datadir = scratch.join("pgdata-load");
    let sockdir = scratch.join("pgsock-load");
    let _ = std::fs::remove_dir_all(&datadir);
    std::fs::create_dir_all(&sockdir)?;
    match PgServer::start_general_conn(datadir, sockdir, "on", "on", 256) {
        Ok(pg) => {
            for mode in ["shard", "hot"] {
                for &w in &WORKERS {
                    let conn = pg_setup(&pg)?;
                    let (aps, p50, p99, _) = spawn_and_collect("postgres", &conn, mode, w)?;
                    rows.push(LoadResult {
                        engine: "postgres",
                        arm: format!("E-{mode}"),
                        workers: w,
                        actions_per_sec: aps,
                        p50_ms: p50,
                        p99_ms: p99,
                        retries: 0,
                    });
                }
            }
        }
        Err(e) => println!("postgres unavailable, mpedb-only run: {e}"),
    }

    println!(
        "{:<10} {:<9} {:>8} {:>14} {:>9} {:>9} {:>9}",
        "engine", "arm", "workers", "actions/s", "p50 ms", "p99 ms", "retries"
    );
    for r in &rows {
        println!(
            "{:<10} {:<9} {:>8} {:>14.0} {:>9} {:>9} {:>9}",
            r.engine, r.arm, r.workers, r.actions_per_sec, r.p50_ms, r.p99_ms, r.retries
        );
    }
    println!();
    println!(
        "The floor for one action is the {DELAY_MS} ms think time, so p50 at or near it means \
         the worker never waited on anyone."
    );
    Ok(())
}
