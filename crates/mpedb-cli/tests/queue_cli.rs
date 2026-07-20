//! End-to-end `mpedb queue` tests against the built binary (design/
//! DESIGN-SERVICE.md §2, stage 1). Every invocation is its own process, so
//! enqueue→run also exercises cross-process proc/plan resolution; the
//! concurrency and crash tests spawn RACING processes, per the testing
//! convention that multi-process behavior goes through the CLI harnesses.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mpedb")
}

fn base_dir() -> PathBuf {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        shm.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> TestDir {
        let dir = base_dir().join(format!("mpedb-queue-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A config with one seed table (`eff`, the counter the procs bump); the
/// queue table itself is created live by `queue init`/first use.
fn write_config(dir: &Path) -> String {
    let cfg = dir.join("config.toml");
    let db = dir.join("db.mpedb");
    std::fs::write(
        &cfg,
        format!(
            r#"[database]
path = "{}"
size_mb = 32
durability = "none"

[[table]]
name = "eff"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "hits"
  type = "int64"
  nullable = false
"#,
            db.display()
        ),
    )
    .unwrap();
    cfg.to_str().unwrap().to_owned()
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().unwrap()
}

fn out_str(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err_str(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[track_caller]
fn assert_ok(o: &Output) {
    assert!(
        o.status.success(),
        "command failed: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        o.status,
        out_str(o),
        err_str(o)
    );
}

fn define_proc(cfg: &str, dir: &Path, name: &str, source: &str) {
    let file = dir.join(format!("{name}.py"));
    std::fs::write(&file, source).unwrap();
    assert_ok(&run(&["proc", "define", cfg, file.to_str().unwrap()]));
}

/// One cell of `exec <sql>` output: the value on the single data line.
fn exec_cell(cfg: &str, sql: &str) -> String {
    let o = run(&["exec", cfg, sql]);
    assert_ok(&o);
    out_str(&o).lines().nth(1).unwrap_or("").trim().to_owned()
}

// --------------------------------------------------------------------------

/// enqueue → run → done: the result lands in the row, the effect committed,
/// the runner exits idle. A second run has nothing to do.
#[test]
fn enqueue_run_complete() {
    let td = TestDir::new("basic");
    let cfg = write_config(td.path());
    assert_ok(&run(&["queue", "init", &cfg]));
    assert_ok(&run(&["exec", &cfg, "INSERT INTO eff (id, hits) VALUES (1, 0)"]));
    define_proc(
        &cfg,
        td.path(),
        "bump",
        "def bump(i):\n    db.execute(\"UPDATE eff SET hits = hits + 1 WHERE id = $1\", [i])\n    return i\n",
    );

    let o = run(&["queue", "enqueue", &cfg, "bump", "1"]);
    assert_ok(&o);
    assert_eq!(out_str(&o).trim(), "1", "first task id");

    let o = run(&["queue", "run", &cfg]);
    assert_ok(&o);
    let out = out_str(&o);
    assert!(out.contains("task 1 bump done: 1"), "run output: {out}");
    assert!(out.contains("idle after 1 task(s)"), "run output: {out}");

    assert_eq!(exec_cell(&cfg, "SELECT hits FROM eff WHERE id = 1"), "1");
    assert_eq!(
        exec_cell(&cfg, "SELECT state FROM mq_task WHERE id = 1"),
        "done"
    );

    // Idle wake: nothing due, exit 0 — the hibernation contract.
    let o = run(&["queue", "run", &cfg]);
    assert_ok(&o);
    assert!(out_str(&o).contains("idle after 0 task(s)"));
    assert_eq!(exec_cell(&cfg, "SELECT hits FROM eff WHERE id = 1"), "1");
}

/// Lower `priority` value runs first; among equals, id order.
#[test]
fn priority_orders_claims() {
    let td = TestDir::new("priority");
    let cfg = write_config(td.path());
    define_proc(&cfg, td.path(), "echo", "def echo(x):\n    return x\n");
    for (arg, prio) in [("a", "100"), ("b", "1"), ("c", "50")] {
        assert_ok(&run(&["queue", "enqueue", &cfg, "echo", arg, "--priority", prio]));
    }
    let o = run(&["queue", "run", &cfg]);
    assert_ok(&o);
    let out = out_str(&o);
    let done: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("done:"))
        .map(|l| l.rsplit(' ').next().unwrap())
        .collect();
    assert_eq!(done, ["b", "c", "a"]);
}

/// A proc that always fails: retried with backoff until max_attempts, then
/// terminal `failed` with the error recorded — never claimed again.
#[test]
fn failing_proc_retries_then_fails_terminally() {
    let td = TestDir::new("retry");
    let cfg = write_config(td.path());
    // init BEFORE define: the queue's CREATE TABLE would otherwise invalidate
    // the proc's embedded plan ("built against a different schema").
    assert_ok(&run(&["queue", "init", &cfg]));
    // A runtime error needs a legal statement that fails at run time: a PK
    // violation on the seeded row.
    assert_ok(&run(&["exec", &cfg, "INSERT INTO eff (id, hits) VALUES (1, 0)"]));
    define_proc(
        &cfg,
        td.path(),
        "boom",
        "def boom():\n    db.execute(\"INSERT INTO eff (id, hits) VALUES (1, 0)\", [])\n    return 0\n",
    );
    assert_ok(&run(&["queue", "enqueue", &cfg, "boom", "--max-attempts", "2"]));

    // Attempt 1: rescheduled (state pending, error recorded, run_at pushed).
    let o = run(&["queue", "run", &cfg, "--retry-delay", "1"]);
    assert_ok(&o);
    assert!(err_str(&o).contains("attempt 1/2 retry"), "stderr: {}", err_str(&o));
    assert_eq!(exec_cell(&cfg, "SELECT state FROM mq_task WHERE id = 1"), "pending");

    // Not yet due: an immediate wake hibernates without touching it.
    let o = run(&["queue", "run", &cfg, "--retry-delay", "1"]);
    assert_ok(&o);
    assert!(out_str(&o).contains("idle after 0 task(s)"));

    // Attempt 2 (after the 1s backoff): terminal failed.
    std::thread::sleep(std::time::Duration::from_millis(1300));
    let o = run(&["queue", "run", &cfg, "--retry-delay", "1"]);
    assert_ok(&o);
    assert!(err_str(&o).contains("attempt 2/2 failed"), "stderr: {}", err_str(&o));
    assert_eq!(exec_cell(&cfg, "SELECT state FROM mq_task WHERE id = 1"), "failed");
    assert!(exec_cell(&cfg, "SELECT error FROM mq_task WHERE id = 1")
        .contains("PRIMARY KEY violation"));

    let o = run(&["queue", "run", &cfg]);
    assert_ok(&o);
    assert!(out_str(&o).contains("idle after 0 task(s)"), "failed is terminal");
}

/// A claim whose owner is gone (forged: dead pid, ancient claimed_at) is
/// reclaimed once the lease has expired, and only then.
#[test]
fn expired_lease_is_reclaimed() {
    let td = TestDir::new("lease");
    let cfg = write_config(td.path());
    define_proc(&cfg, td.path(), "one", "def one():\n    return 1\n");
    assert_ok(&run(&["queue", "enqueue", &cfg, "one"]));
    // Forge: claimed 2020-01-01 by a pid that cannot exist.
    assert_ok(&run(&[
        "exec",
        &cfg,
        "UPDATE mq_task SET state = $1, claimed_by = 999999999, claimed_at = $2, \
         attempts = 1 WHERE id = 1",
        "claimed",
        "2020-01-01T00:00:00Z",
    ]));

    let o = run(&["queue", "run", &cfg]);
    assert_ok(&o);
    assert!(out_str(&o).contains("task 1 one done: 1"), "out: {}", out_str(&o));
    // Reclaim + fresh claim: attempts 1 (forged) + 1 (the real run).
    assert_eq!(exec_cell(&cfg, "SELECT attempts FROM mq_task WHERE id = 1"), "2");
}

/// A stale claim whose attempts are already exhausted goes to `dead`
/// (a crash loop that never completed), not back to pending.
#[test]
fn exhausted_stale_claim_goes_dead() {
    let td = TestDir::new("dead");
    let cfg = write_config(td.path());
    define_proc(&cfg, td.path(), "one", "def one():\n    return 1\n");
    assert_ok(&run(&["queue", "enqueue", &cfg, "one", "--max-attempts", "1"]));
    assert_ok(&run(&[
        "exec",
        &cfg,
        "UPDATE mq_task SET state = $1, claimed_by = 999999999, claimed_at = $2, \
         attempts = 1 WHERE id = 1",
        "claimed",
        "2020-01-01T00:00:00Z",
    ]));
    let o = run(&["queue", "run", &cfg]);
    assert_ok(&o);
    assert!(out_str(&o).contains("idle after 0 task(s)"));
    assert_eq!(exec_cell(&cfg, "SELECT state FROM mq_task WHERE id = 1"), "dead");
}

/// THE double-run test: several runner processes started simultaneously on a
/// full queue, NO kills, long lease — every task must be claimed by exactly
/// one runner and executed exactly once: hits == 1 and attempts == 1 for
/// every counter. The writer lock + the single-statement claim are the whole
/// mechanism; any hits > 1 here is a claim-protocol bug.
#[test]
fn simultaneous_runners_claim_disjoint_tasks() {
    let td = TestDir::new("disjoint");
    let cfg = write_config(td.path());
    const TASKS: i64 = 40;
    assert_ok(&run(&["queue", "init", &cfg])); // before define: see retry test
    define_proc(
        &cfg,
        td.path(),
        "bump",
        "def bump(i):\n    db.execute(\"UPDATE eff SET hits = hits + 1 WHERE id = $1\", [i])\n    return i\n",
    );
    for i in 0..TASKS {
        assert_ok(&run(&["exec", &cfg, &format!("INSERT INTO eff (id, hits) VALUES ({i}, 0)")]));
        assert_ok(&run(&["queue", "enqueue", &cfg, "bump", &i.to_string()]));
    }

    let runners: Vec<_> = (0..3)
        .map(|_| {
            Command::new(bin())
                .args(["queue", "run", &cfg, "--lease", "600"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut ran_total = 0u64;
    for r in runners {
        let o = r.wait_with_output().unwrap();
        assert_ok(&o);
        let out = out_str(&o);
        let idle = out.lines().find(|l| l.contains("idle after")).unwrap();
        ran_total += idle
            .split_whitespace()
            .nth(2)
            .unwrap()
            .parse::<u64>()
            .unwrap();
    }
    assert_eq!(ran_total, TASKS as u64, "every task ran in exactly one runner");
    assert_eq!(
        exec_cell(&cfg, "SELECT count(*) FROM mq_task WHERE state = 'done'"),
        TASKS.to_string()
    );
    // Exactly once, everywhere: no counter missed, none double-bumped.
    assert_eq!(
        exec_cell(&cfg, "SELECT count(*) FROM eff WHERE hits = 1"),
        TASKS.to_string()
    );
    assert_eq!(
        exec_cell(&cfg, "SELECT count(*) FROM mq_task WHERE attempts = 1"),
        TASKS.to_string()
    );
}

/// SIGKILL fuzz (see queue_collide.rs for the invariants): runners killed at
/// every instant; after the final drain every task is done, every effect ran
/// at least once, and hits ≤ attempts (hits > attempts = double-run).
#[test]
fn queue_collide_sigkill_fuzz() {
    let td = TestDir::new("collide");
    let o = run(&[
        "queue-collide",
        "--dir",
        td.path().to_str().unwrap(),
        "--runners",
        "3",
        "--tasks",
        "36",
        "--secs",
        "4",
        "--kill-ms",
        "30",
    ]);
    assert_ok(&o);
    assert!(out_str(&o).contains("queue-collide ok"), "out: {}", out_str(&o));
}

/// Same fuzz through the intent ring (durability=commit on real fsyncs is
/// exercised by the crash suite; tmpfs here still routes the ring). Slower,
/// so #[ignore]d like the other long harness runs.
#[test]
#[ignore = "slow (~15 s): SIGKILL fuzz under durability=commit"]
fn queue_collide_sigkill_fuzz_durable() {
    let td = TestDir::new("collide-durable");
    let o = run(&[
        "queue-collide",
        "--dir",
        td.path().to_str().unwrap(),
        "--runners",
        "3",
        "--tasks",
        "32",
        "--secs",
        "8",
        "--kill-ms",
        "30",
        "--durability",
        "commit",
    ]);
    assert_ok(&o);
    assert!(out_str(&o).contains("queue-collide ok"), "out: {}", out_str(&o));
}
