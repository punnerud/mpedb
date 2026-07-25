//! End-to-end `mpedb listen` and `queue run --wait` (#141 S3).
//!
//! These go through the built binary because the thing being tested is
//! CROSS-PROCESS: one process parks in the kernel, another commits, and the
//! first must come back. A same-process test would exercise the futex but not
//! the shared-memory publication that makes it work between processes, which
//! is the whole claim.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mpedb")
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> TestDir {
        let dir = mpedb_testkit::scratch_base()
            .join(format!("mpedb-listen-cli-{name}-{}", std::process::id()));
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

fn write_config(dir: &Path) -> String {
    let cfg = dir.join("config.toml");
    let db = dir.join("db.mpedb");
    std::fs::write(
        &cfg,
        format!(
            r#"
[database]
path = "{}"
size_mb = 16
max_readers = 16

[[table]]
name = "orders"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"

[[table]]
name = "other"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
            db.display()
        ),
    )
    .unwrap();
    cfg.to_string_lossy().into_owned()
}

fn exec(cfg: &str, sql: &str) {
    let out = Command::new(bin()).args(["exec", cfg, sql]).output().unwrap();
    assert!(out.status.success(), "exec failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// A listener in another process wakes on a commit here, and names the table.
#[test]
fn listen_wakes_on_a_commit_from_another_process() {
    let dir = TestDir::new("wake");
    let cfg = write_config(dir.path());
    exec(&cfg, "INSERT INTO orders (id, v) VALUES (1, 1)");

    let child = Command::new(bin())
        .args(["listen", &cfg, "orders", "--timeout", "30"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // Give the child time to reach its park. It is a separate process, so
    // there is no handshake to wait on short of one we would have to invent;
    // the 30 s timeout is what keeps a slow start from being a false pass.
    std::thread::sleep(Duration::from_millis(400));
    exec(&cfg, "INSERT INTO orders (id, v) VALUES (2, 2)");

    let t0 = Instant::now();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "listen exited {:?} instead of reporting a change",
        out.status.code()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "orders");
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "listen took {:?} to return after the commit — it timed out rather than woke",
        t0.elapsed()
    );
}

/// The per-table filter holds across processes: a write to `other` must not
/// wake a listener on `orders`. This is the assertion that separates the
/// design from a global "the database changed" signal.
#[test]
fn listen_is_not_woken_by_another_table() {
    let dir = TestDir::new("cross");
    let cfg = write_config(dir.path());

    let child = Command::new(bin())
        .args(["listen", &cfg, "orders", "--timeout", "2"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    exec(&cfg, "INSERT INTO other (id) VALUES (1)");

    let out = child.wait_with_output().unwrap();
    assert!(
        !out.status.success(),
        "a listener on `orders` reported a change when only `other` was written: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
}

/// Nothing happens ⇒ exit 1, so `if mpedb listen …; then` is a usable shape.
#[test]
fn listen_exits_nonzero_on_timeout() {
    let dir = TestDir::new("timeout");
    let cfg = write_config(dir.path());
    let out = Command::new(bin())
        .args(["listen", &cfg, "orders", "--timeout", "1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "timeout should exit 1, not {:?}", out.status.code());
}

/// A table that does not exist is an error BEFORE the block, not a wait that
/// never returns.
#[test]
fn listen_rejects_an_unknown_table_immediately() {
    let dir = TestDir::new("unknown");
    let cfg = write_config(dir.path());
    let t0 = Instant::now();
    let out = Command::new(bin()).args(["listen", &cfg, "nosuch"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "unknown table should be a usage error");
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "it blocked for {:?} before rejecting a typo",
        t0.elapsed()
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such table"));
}

/// `queue run --wait` parks instead of exiting at idle, and an enqueue from
/// another process brings it back to run the task. Without `--wait` the same
/// invocation drains and exits, which is the behaviour that must not change.
///
/// Note what `--wait <s>` means, because it is easy to assume the other
/// thing: the runner is a SERVICE LOOP that returns to the doorbell after
/// every batch and exits only when `s` seconds pass with nothing new. It does
/// not exit as soon as a task completes. So the timing here is "started,
/// woken, ran it, parked again, timed out" — and the short wait is what keeps
/// that whole sequence inside the test's patience.
#[test]
fn queue_run_wait_parks_and_resumes_on_an_enqueue() {
    let dir = TestDir::new("qwait");
    let cfg = write_config(dir.path());

    let init = Command::new(bin()).args(["queue", "init", &cfg]).output().unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));

    // A real proc, so the woken runner has something to actually complete —
    // otherwise it wakes, fails the task, retries, and the test cannot tell
    // "the doorbell fired" from "the runner never slept".
    let procfile = dir.path().join("bump.py");
    std::fs::write(
        &procfile,
        "def bump(i):\n    db.execute(\"INSERT INTO orders (id, v) VALUES ($1, 1)\", [i])\n    return i\n",
    )
    .unwrap();
    let def = Command::new(bin())
        .args(["proc", "define", &cfg, procfile.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(def.status.success(), "{}", String::from_utf8_lossy(&def.stderr));

    // Idle and no --wait: drains nothing and exits promptly. The default must
    // stay drain-and-exit (Model A), or every existing cron caller changes
    // behaviour.
    let t0 = Instant::now();
    let out = Command::new(bin()).args(["queue", "run", &cfg]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "plain `queue run` blocked for {:?} — the default must stay drain-and-exit",
        t0.elapsed()
    );

    // With --wait: parks at idle, an enqueue wakes it, it runs the task, parks
    // once more, and exits when that park times out.
    let t1 = Instant::now();
    let runner = Command::new(bin())
        .args(["queue", "run", &cfg, "--wait", "3"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let enq = Command::new(bin())
        .args(["queue", "enqueue", &cfg, "bump", "7"])
        .output()
        .unwrap();
    assert!(enq.status.success(), "{}", String::from_utf8_lossy(&enq.stderr));

    let out = runner.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("bump"),
        "the parked runner never ran the enqueued task — the doorbell did not \
         fire, or it exited before the enqueue landed: {stdout}"
    );
    // It must have SLEPT rather than spun: had it polled, it would have found
    // the task and finished long before its own wait elapsed.
    assert!(
        t1.elapsed() >= Duration::from_secs(3),
        "the runner returned after {:?} — it did not park for its full wait",
        t1.elapsed()
    );
    assert!(
        t1.elapsed() < Duration::from_secs(25),
        "the runner hung for {:?} instead of timing out at 3 s + one batch",
        t1.elapsed()
    );
}
