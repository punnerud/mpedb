//! #109: `Database::set_busy_timeout` bounds writer-lock waits across
//! PROCESSES — the liveness half of the C-API shim's `busy_timeout`.
//!
//! Multi-process is normally the CLI stress/crash suite's turf; these tests
//! are the sanctioned exception (no external suite covers cross-process BUSY
//! semantics). The child is this same test binary re-invoked with `--exact
//! busy_child_holder` and env vars set — in a normal run that test is an
//! instant no-op.
//!
//! What must hold, with elapsed-time evidence:
//! - holder alive + timeout T: the waiter gets `Error::Busy` after >= T (and
//!   well under forever);
//! - timeout 0: immediate Busy on contention — sqlite's no-busy-handler
//!   default;
//! - holder SIGKILLed mid-hold: the waiter's bounded poll ACQUIRES via
//!   owner-death recovery (EOWNERDEAD adopt) — never a hang past the
//!   deadline.

use mpedb::{Config, Database, Error};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn db_path(name: &str) -> PathBuf {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    dir.join(format!(
        "mpedb-busy-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn open_db(path: &Path) -> Database {
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 16

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "text"
"#,
        path.display()
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

fn marker(path: &Path) -> PathBuf {
    let mut m = path.as_os_str().to_owned();
    m.push(".held");
    PathBuf::from(m)
}

/// Spawn this test binary as the lock-holding child process.
fn spawn_holder(path: &Path, hold_ms: u64) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["busy_child_holder", "--exact", "--nocapture"])
        .env("MPEDB_BUSY_CHILD_PATH", path)
        .env("MPEDB_BUSY_CHILD_HOLD_MS", hold_ms.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child holder")
}

fn wait_for_marker(path: &Path) {
    let m = marker(path);
    let t0 = Instant::now();
    while !m.exists() {
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "child never signalled lock-held"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(marker(path));
}

/// CHILD ENTRY (env-gated; instant no-op in a normal suite run): attach the
/// file, open a write transaction, signal via the marker file, hold for
/// HOLD_MS, roll back, clean up.
#[test]
fn busy_child_holder() {
    let Ok(path) = std::env::var("MPEDB_BUSY_CHILD_PATH") else {
        return;
    };
    let hold_ms: u64 = std::env::var("MPEDB_BUSY_CHILD_HOLD_MS")
        .expect("hold ms")
        .parse()
        .expect("hold ms parse");
    let path = PathBuf::from(path);
    let db = Database::open_from_file(&path).expect("child attach");
    let mut s = db.begin().expect("child begin");
    s.query("INSERT INTO t (id, v) VALUES (999999, 'holder')", &[])
        .expect("child insert");
    std::fs::write(marker(&path), b"1").expect("child marker");
    std::thread::sleep(Duration::from_millis(hold_ms));
    s.rollback();
    let _ = std::fs::remove_file(marker(&path));
}

/// Holder alive: `begin()` and autocommit DML both answer `Error::Busy`
/// after >= the timeout — bounded, not forever.
#[test]
fn busy_timeout_expires_bounded_two_process() {
    let path = db_path("expire");
    cleanup(&path);
    let db = open_db(&path);
    let mut child = spawn_holder(&path, 4000);
    wait_for_marker(&path);

    // Interactive begin: Busy after ~300 ms.
    db.set_busy_timeout(Some(Duration::from_millis(300)));
    let t0 = Instant::now();
    let r = db.begin();
    let dt = t0.elapsed();
    let err = r.err().expect("expected Busy, got a session");
    assert!(matches!(err, Error::Busy), "expected Busy, got {err:?}");
    assert!(dt >= Duration::from_millis(300), "returned before the timeout: {dt:?}");
    assert!(dt < Duration::from_millis(3000), "overshot the timeout wildly: {dt:?}");
    eprintln!("busy(begin, timeout 300ms): elapsed {dt:?}");

    // Autocommit DML (run_write_plan's direct bounded path): same contract.
    let t0 = Instant::now();
    let r = db.query("INSERT INTO t (id, v) VALUES (1, 'parent')", &[]);
    let dt = t0.elapsed();
    assert!(matches!(r, Err(Error::Busy)), "expected Busy, got {r:?}");
    assert!(dt >= Duration::from_millis(300), "returned before the timeout: {dt:?}");
    assert!(dt < Duration::from_millis(3000), "overshot the timeout wildly: {dt:?}");
    eprintln!("busy(autocommit DML, timeout 300ms): elapsed {dt:?}");

    // Reads must NOT be blocked by the holder (opportunistic plan
    // publication): a first-compile SELECT proceeds under the writer.
    let r = db.query("SELECT id FROM t WHERE id = 42", &[]);
    assert!(r.is_ok(), "read blocked/failed under a writer: {r:?}");

    // After the child releases, the same handle proceeds.
    child.wait().expect("child exit");
    db.set_busy_timeout(Some(Duration::from_millis(5000)));
    let mut s = db.begin().expect("begin after release");
    s.query("INSERT INTO t (id, v) VALUES (2, 'after')", &[])
        .expect("insert after release");
    s.commit().expect("commit after release");
    cleanup(&path);
}

/// Timeout 0 (sqlite's no-busy-handler default): one immediate attempt,
/// immediate Busy on contention.
#[test]
fn busy_timeout_zero_is_immediate() {
    let path = db_path("zero");
    cleanup(&path);
    let db = open_db(&path);
    let mut child = spawn_holder(&path, 3000);
    wait_for_marker(&path);

    db.set_busy_timeout(Some(Duration::ZERO));
    let t0 = Instant::now();
    let r = db.begin();
    let dt = t0.elapsed();
    let err = r.err().expect("expected Busy, got a session");
    assert!(matches!(err, Error::Busy), "expected Busy, got {err:?}");
    assert!(dt < Duration::from_millis(500), "timeout 0 was not immediate: {dt:?}");
    eprintln!("busy(begin, timeout 0): elapsed {dt:?}");

    child.wait().expect("child exit");
    cleanup(&path);
}

/// A SIGKILLed holder must not turn the bounded wait into a hang: the
/// waiter's next poll adopts the dead owner's lock (EOWNERDEAD recovery) and
/// ACQUIRES — well before the deadline.
#[test]
fn sigkilled_holder_never_hangs_the_waiter() {
    let path = db_path("sigkill");
    cleanup(&path);
    let db = open_db(&path);
    let mut child = spawn_holder(&path, 60_000);
    wait_for_marker(&path);

    child.kill().expect("SIGKILL holder");
    child.wait().expect("reap holder");

    db.set_busy_timeout(Some(Duration::from_millis(3000)));
    let t0 = Instant::now();
    let r = db.begin();
    let dt = t0.elapsed();
    assert!(
        dt < Duration::from_millis(3000),
        "waiter blocked to (or past) its deadline despite a dead holder: {dt:?}"
    );
    eprintln!("begin after SIGKILLed holder (timeout 3000ms): elapsed {dt:?}");
    // On Linux the robust mutex hands the lock over; Busy would only be
    // acceptable if recovery itself were still in flight at the deadline.
    let mut s = r.expect("owner-death recovery should hand the lock to the waiter");
    // The dead child's uncommitted insert must be gone (COW: never flipped).
    s.query("INSERT INTO t (id, v) VALUES (999999, 'reclaimed')", &[])
        .expect("insert after recovery (holder's row must have vanished)");
    s.commit().expect("commit after recovery");
    cleanup(&path);
}
