//! `mpedb queue-collide` — SIGKILL crash fuzz for the queue claim protocol
//! (design/DESIGN-SERVICE.md §2/§7 stage 1, the CLI crash conventions).
//!
//! N runner processes — literal `mpedb queue run` invocations, the public
//! drain-and-exit command — race over one `.mpedb` while the parent SIGKILLs
//! and respawns each of them on a tight random cadence, so kills land at
//! EVERY instant: mid-claim, between claim and proc, mid-proc-txn,
//! mid-complete. The parent keeps enqueueing during the storm (contending on
//! the same writer lock, including right after a runner died holding it).
//!
//! Each task's proc bumps a counter row (`UPDATE eff SET hits = hits + 1`),
//! deliberately NOT idempotent so every execution is visible. After a final
//! no-kill drain the invariants are:
//!
//! - every task is `done` (none lost, none failed/dead — the proc never errs
//!   and `--max-attempts` is generous);
//! - for every task, `1 ≤ hits ≤ attempts`: it ran at least once, and every
//!   run was preceded by its own committed claim — `hits > attempts` would be
//!   a double-run (two runners inside one claim), the bug this fuzz exists to
//!   catch. In a kill-free world hits == attempts == 1; a rerun after a
//!   killed runner's lease expired is the at-least-once contract, and it is
//!   visible here as attempts > 1.
//! - page accounting verifies (`Database::verify`).
//!
//! A runner that exits ZERO on its own is hibernation working (queue looked
//! idle) and is respawned; a nonzero self-exit is a protocol error and fails
//! the fuzz.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mpedb::{Database, ExecResult, Value};
use mpedb_proc::{Lang, ProcEngine};

use crate::args;
use crate::queue::{ensure_table, enqueue_task, EnqueueSpec};
use crate::util::{
    runtime, usage, CliResult, Failure, Rng, Watchdog, write_config_durable,
};

const BOOT_TOML: &str = r#"[[table]]
name = "boot"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#;

/// The task body: one atomic proc txn per run, visibly counting executions.
const BUMP_PROC: &str = r#"
def qc_bump(i):
    db.execute("UPDATE eff SET hits = hits + 1 WHERE id = $1", [i])
    return i
"#;

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

fn count_where(db: &Database, sql: &str) -> Result<i64, Failure> {
    match db.query(sql, &[])? {
        ExecResult::Rows { rows, .. } => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok(*n),
            other => runtime(format!("count gave {other:?}")),
        },
        other => runtime(format!("count expected rows, got {other:?}")),
    }
}

struct Slot {
    child: Child,
    kill_at: u128,
}

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["dir", "runners", "tasks", "secs", "kill-ms", "durability", "seed", "lease"],
        &[],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let runners = p.u64_or("runners", 3)?.max(1);
    let tasks = p.u64_or("tasks", 48)?.max(1);
    let secs = p.u64_or("secs", 6)?.max(1);
    let kill_ms = p.u64_or("kill-ms", 40)?.max(1);
    let lease_s = p.u64_or("lease", 1)?.max(1);
    let seed = p.u64_or("seed", 1)?;
    let durability = p.value("durability").unwrap_or("none");
    if !matches!(durability, "none" | "commit" | "wal") {
        return usage(format!("--durability must be none|commit|wal, got `{durability}`"));
    }

    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let cfg = dir.join("config.toml");
    let db_path = dir.join("queue.mpedb");
    for f in [&db_path, &dir.join("queue.mpedb-wal")] {
        let _ = std::fs::remove_file(f);
    }
    write_config_durable(&cfg, &db_path, 64, BOOT_TOML, durability, None)?;
    let cfg_s = cfg.to_str().expect("utf-8 path").to_owned();

    let _wd = Watchdog::arm(secs + 120, "queue-collide");
    let mut rng = Rng::seeded(&[0x9e5e_u64, seed, runners, tasks]);

    // Seed: queue table, effect table with `tasks` zeroed counters, the proc.
    let db = Database::open(&cfg)?;
    ensure_table(&db)?;
    db.query(
        "CREATE TABLE eff (id INTEGER PRIMARY KEY, hits INTEGER NOT NULL)",
        &[],
    )?;
    for i in 0..tasks {
        db.query(
            "INSERT INTO eff (id, hits) VALUES ($1, 0)",
            &[Value::Int(i as i64)],
        )?;
    }
    ProcEngine::new(&db).define(BUMP_PROC, Lang::Python)?;

    let enqueue = |i: u64| -> Result<i64, Failure> {
        enqueue_task(
            &db,
            &EnqueueSpec {
                queue: "default",
                proc: "qc_bump",
                args: &[i.to_string()],
                priority: 100,
                run_at: crate::queue::now_micros(),
                max_attempts: 1_000,
            },
        )
    };

    // Half the tasks are due before any runner exists; the rest trickle in
    // DURING the storm so enqueue-vs-claim and enqueue-vs-recovery interleave.
    let upfront = tasks / 2;
    for i in 0..upfront {
        enqueue(i)?;
    }

    let exe = std::env::current_exe()?;
    let spawn = |rng: &mut Rng| -> Result<Slot, Failure> {
        let child = Command::new(&exe)
            .args(["queue", "run", &cfg_s])
            .args(["--lease", &lease_s.to_string()])
            .args(["--retry-delay", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Slot { child, kill_at: now_ms() + 3 + rng.below(kill_ms) as u128 })
    };

    let deadline = now_ms() + secs as u128 * 1000;
    let mut slots: Vec<Slot> = Vec::new();
    for _ in 0..runners {
        slots.push(spawn(&mut rng)?);
    }

    let mut kills = 0u64;
    let mut clean_exits = 0u64;
    let mut bad_exits = 0u64;
    let mut next_task = upfront;
    let trickle_every = (secs as u128 * 1000) / (tasks - upfront + 1) as u128;
    let mut next_trickle = now_ms() + trickle_every;

    while now_ms() < deadline {
        std::thread::sleep(Duration::from_millis(3));
        if next_task < tasks && now_ms() >= next_trickle {
            enqueue(next_task)?;
            next_task += 1;
            next_trickle = now_ms() + trickle_every;
        }
        for slot in &mut slots {
            if let Ok(Some(status)) = slot.child.try_wait() {
                // Self-exit: zero = hibernated (idle), normal. Nonzero and not
                // a signal = a runner hit a protocol/engine error — a bug.
                use std::os::unix::process::ExitStatusExt;
                if status.success() || status.signal().is_some() {
                    clean_exits += 1;
                } else {
                    bad_exits += 1;
                }
                *slot = spawn(&mut rng)?;
            } else if now_ms() >= slot.kill_at {
                let _ = slot.child.kill();
                let _ = slot.child.wait();
                kills += 1;
                *slot = spawn(&mut rng)?;
            }
        }
    }
    for slot in &mut slots {
        let _ = slot.child.kill();
        let _ = slot.child.wait();
    }
    // Any tasks the trickle did not reach (slow machine) go in now — the
    // final drain must still complete exactly `tasks` of them.
    for i in next_task..tasks {
        enqueue(i)?;
    }

    // Final no-kill drain: repeat the public run command until nothing is
    // pending or claimed. Claims from freshly killed runners need the lease
    // to expire first, hence the retry loop (the watchdog bounds it).
    loop {
        let status = Command::new(&exe)
            .args(["queue", "run", &cfg_s])
            .args(["--lease", &lease_s.to_string()])
            .args(["--retry-delay", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            return runtime("final drain runner exited nonzero");
        }
        let open = count_where(
            &db,
            "SELECT count(*) FROM mq_task WHERE state = 'pending' OR state = 'claimed'",
        )?;
        if open == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    verify(&db, tasks, kills, clean_exits, bad_exits)?;
    if bad_exits > 0 {
        return runtime(format!("{bad_exits} runner(s) self-exited with an error"));
    }
    Ok(())
}

/// The invariants over the drained queue (module doc). Fails loudly with the
/// offending task id.
fn verify(
    db: &Database,
    tasks: u64,
    kills: u64,
    clean_exits: u64,
    bad_exits: u64,
) -> CliResult {
    let total = count_where(db, "SELECT count(*) FROM mq_task")?;
    let done = count_where(db, "SELECT count(*) FROM mq_task WHERE state = 'done'")?;
    if total != tasks as i64 || done != tasks as i64 {
        return runtime(format!(
            "expected {tasks} tasks all done, found total={total} done={done}"
        ));
    }

    let ExecResult::Rows { rows, .. } =
        db.query("SELECT id, payload, attempts FROM mq_task ORDER BY id", &[])?
    else {
        return runtime("task dump expected rows");
    };
    let ExecResult::Rows { rows: eff_rows, .. } =
        db.query("SELECT id, hits FROM eff ORDER BY id", &[])?
    else {
        return runtime("eff dump expected rows");
    };
    let hits_of: std::collections::HashMap<i64, i64> = eff_rows
        .iter()
        .filter_map(|r| match &r[..] {
            [Value::Int(id), Value::Int(h)] => Some((*id, *h)),
            _ => None,
        })
        .collect();

    let (mut sum_hits, mut max_attempts, mut reruns) = (0i64, 0i64, 0i64);
    for row in &rows {
        let [Value::Int(id), Value::Text(payload), Value::Int(attempts)] = &row[..] else {
            return runtime(format!("task row has unexpected shape: {row:?}"));
        };
        let eff_id: i64 = payload
            .parse()
            .map_err(|_| Failure::Runtime(format!("task {id}: payload {payload:?} not an id")))?;
        let Some(&hits) = hits_of.get(&eff_id) else {
            return runtime(format!("task {id}: no eff row {eff_id}"));
        };
        if hits < 1 {
            return runtime(format!("task {id}: done but its effect never ran (hits=0)"));
        }
        if hits > *attempts {
            return runtime(format!(
                "task {id}: DOUBLE-RUN — {hits} executions but only {attempts} committed \
                 claims (two runners ran inside one claim)"
            ));
        }
        sum_hits += hits;
        max_attempts = max_attempts.max(*attempts);
        reruns += hits - 1;
    }

    db.verify()?;
    println!(
        "queue-collide ok: tasks={tasks} kills={kills} clean_exits={clean_exits} \
         bad_exits={bad_exits} sum_hits={sum_hits} reruns={reruns} max_attempts={max_attempts}"
    );
    Ok(())
}
