//! #159 stage 2: several PROCESSES on one `.mpedb` file.
//!
//! Stage 1 ends at "one process opens a file, reads and writes". This is the
//! stage that decides whether the port is worth anything, because attaching
//! from more than one process at a time is the whole product — an engine that
//! only works single-process is a library with a file format.
//!
//! Deliberately `fork`-free. The CLI's stress/crash/collide harnesses are POSIX
//! to the bone (`fork` + signals) and porting them is stage 4; nothing here
//! needs them. A child is this same test binary re-invoked with `--exact
//! <child test> --ignored` and env vars set, which is what `std::process`
//! gives us identically on both platforms. Even the hard kill is portable:
//! `Child::kill` is `SIGKILL` on Unix and `TerminateProcess` on Windows, and
//! neither gives the victim a chance to clean up — which is the point.
//!
//! Four properties, each one a different shared primitive:
//!
//! 1. **Mapping coherence** — a child's committed rows are visible to a parent
//!    that was already attached. Windows: separate `CreateFileMapping` calls on
//!    the same file, which the OS is required to keep coherent.
//! 2. **Writer exclusion** — while a child holds the writer lock, the parent's
//!    `try_begin_write` returns `None` rather than a second writer.
//! 3. **Snapshot isolation across processes** — a reader pinned at txn T keeps
//!    reading T while another process commits past it, and holds the
//!    reclaim bound down for as long as it lives.
//! 4. **Owner death** — a child killed mid-write leaves the lock acquirable and
//!    the database consistent, with the partial transaction absent.

use mpedb_core::engine::Engine;
use mpedb_types::{Config, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// Env contract with the child half. `ROLE` also gates the child tests into
// no-ops during a normal run, so `cargo test` never spawns anything by itself.
const ROLE: &str = "MPEDB_MP_ROLE";
const PATH: &str = "MPEDB_MP_PATH";
const DUR: &str = "MPEDB_MP_DUR";

fn db_path(name: &str) -> PathBuf {
    let p = mpedb_testkit::scratch_base().join(format!("mpedb-mp-{name}.mpedb"));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(format!("{}-lock", p.display()));
    p
}

/// The child's durability, read from the env the PARENT set on its `Command`.
/// The parent never sets it on itself: `set_var` mutates the whole process, and
/// this binary runs its tests on several threads, so one arm choosing `wal`
/// would silently reconfigure every other test in flight. That is exactly what
/// happened — green under `--test-threads 1`, red on CI.
fn child_durability() -> String {
    std::env::var(DUR).unwrap_or_else(|_| "none".into())
}

fn cfg_for(path: &Path, durability: &str) -> Config {
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 16
durability = "{}"

[[table]]
name = "kv"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
"#,
        mpedb_testkit::toml_path(path),
        durability
    );
    Config::from_toml_str(&toml).unwrap()
}

fn open(path: &Path) -> Engine {
    open_dur(path, "none")
}

fn open_dur(path: &Path, durability: &str) -> Engine {
    let cfg = cfg_for(path, durability);
    Engine::open(&cfg, vec![vec![]; cfg.schema.tables.len()]).unwrap()
}

/// Re-invoke this binary as `role`, pointed at `path`.
fn spawn(role: &str, path: &Path) -> std::process::Child {
    spawn_dur(role, path, "none")
}

fn spawn_dur(role: &str, path: &Path, durability: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "--ignored", "--nocapture", role])
        .env(ROLE, role)
        .env(PATH, path)
        .env(DUR, durability)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn child_env() -> Option<PathBuf> {
    std::env::var_os(ROLE)?;
    Some(PathBuf::from(std::env::var_os(PATH)?))
}

/// Poll until `f` holds or the budget runs out. Multi-process tests have no
/// shared clock, and a fixed sleep is either flaky or slow — never both right.
fn until(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < budget {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

fn row_count(eng: &Engine) -> u64 {
    eng.begin_read().unwrap().row_count(0).unwrap()
}

// ---------------------------------------------------------------- 1. coherence

#[test]
fn a_second_process_writes_and_an_attached_reader_sees_it() {
    let path = db_path("coherent");
    let eng = open(&path);
    assert_eq!(row_count(&eng), 0);

    // The parent stays attached across the child's whole life: this must be
    // mapping coherence, not a re-open picking up a changed file.
    let out = spawn("mp_child_writes_ten", &path).wait_with_output().unwrap();
    assert!(out.status.success(), "child failed: {out:?}");

    assert!(
        until(Duration::from_secs(10), || row_count(&eng) == 10),
        "an already-attached process must see another process's commits; saw {}",
        row_count(&eng)
    );
    let r = eng.begin_read().unwrap();
    for id in 0..10i64 {
        let row = r.get_by_pk(0, &[Value::Int(id)]).unwrap().expect("row");
        assert_eq!(row[1], Value::Int(id * 7));
    }
    drop(r);
    let _ = std::fs::remove_file(&path);
}

// ----------------------------------------------------------------- 2. exclusion

#[test]
fn only_one_process_holds_the_writer_lock() {
    let path = db_path("exclusive");
    let eng = open(&path);

    let mut child = spawn("mp_child_holds_writer", &path);
    // The child signals "lock acquired" by committing row 999 BEFORE it starts
    // holding — a shared-memory handshake, so no sleep is load-bearing.
    assert!(
        until(Duration::from_secs(10), || row_count(&eng) >= 1),
        "child never took the lock"
    );

    let held = eng.try_begin_write().unwrap();
    assert!(
        held.is_none(),
        "try_begin_write must yield while another PROCESS holds the writer lock"
    );

    child.kill().unwrap();
    let _ = child.wait();

    // Owner death releases it — the kernel does this for us on every platform
    // we support (flock on macOS/Windows, robust mutex on Linux).
    assert!(
        until(Duration::from_secs(10), || eng
            .try_begin_write()
            .map(|w| w.is_some())
            .unwrap_or(false)),
        "the lock must be acquirable once the holder dies"
    );
    let _ = std::fs::remove_file(&path);
}

// ------------------------------------------------------------ 3. MVCC isolation

#[test]
fn a_pinned_reader_in_another_process_holds_the_snapshot() {
    let path = db_path("mvcc");
    {
        let eng = open(&path);
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &[Value::Int(0), Value::Int(100)]).unwrap();
        w.commit().unwrap();
    }

    let eng = open(&path);
    let child = spawn("mp_child_pins_a_snapshot", &path);
    // Row 500 is the child's "I am pinned" signal.
    assert!(
        until(Duration::from_secs(10), || eng
            .begin_read()
            .unwrap()
            .get_by_pk(0, &[Value::Int(500)])
            .unwrap()
            .is_some()),
        "child never pinned"
    );

    // Move the database well past the child's snapshot.
    for i in 1..40i64 {
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &[Value::Int(i), Value::Int(i)]).unwrap();
        w.commit().unwrap();
    }

    // The child re-reads its OWN snapshot at the end and asserts it still sees
    // exactly what it saw at pin time. Its exit status is that assertion.
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "reader's snapshot changed under it: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = std::fs::remove_file(&path);
}

// --------------------------------------------------------------- 4. owner death

#[test]
fn a_writer_killed_mid_transaction_leaves_no_partial_state() {
    let path = db_path("kill");
    let eng = open(&path);

    let mut child = spawn("mp_child_dies_holding_writes", &path);
    // Row 777 is committed first; the uncommitted batch that follows uses ids
    // 1000.., so "did anything partial survive" is a direct question.
    assert!(
        until(Duration::from_secs(10), || eng
            .begin_read()
            .unwrap()
            .get_by_pk(0, &[Value::Int(777)])
            .unwrap()
            .is_some()),
        "child never got started"
    );
    child.kill().unwrap();
    let _ = child.wait();

    // Recovery happens on the next writer's acquire.
    let w = until(Duration::from_secs(10), || {
        eng.try_begin_write().map(|w| w.is_some()).unwrap_or(false)
    });
    assert!(w, "lock never became acquirable after the holder was killed");

    let r = eng.begin_read().unwrap();
    assert!(
        r.get_by_pk(0, &[Value::Int(777)]).unwrap().is_some(),
        "the committed row must survive"
    );
    for id in 1000..1010i64 {
        assert!(
            r.get_by_pk(0, &[Value::Int(id)]).unwrap().is_none(),
            "an uncommitted write ({id}) survived a SIGKILL — COW is broken"
        );
    }
    drop(r);
    let _ = std::fs::remove_file(&path);
}

// -------------------------------------------- 5. durability across a hard kill

/// #159 stage 3. The same kill as test 4, but at a durability setting that
/// makes a promise about the DISK — and re-opened by a THIRD process, so the
/// answer comes from the file and the log, never from a mapping that happened
/// to survive.
///
/// `commit` msyncs and barriers on every commit; `wal` appends and replays.
/// On Windows those are `FlushViewOfFile` + `FlushFileBuffers`, which compose
/// the way macOS's `msync` + `F_FULLFSYNC` do — the ordering stage 3 exists to
/// check. This is not a power-loss test: `Child::kill` takes the process, not
/// the page cache. It is the strongest crash test that is portable, and the
/// real power-loss simulator (`mpedb powerloss`) is stage 4's `fork`-bound
/// work.
fn durable_kill_leaves_a_prefix(mode: &str) {
    let path = db_path(&format!("durable-{mode}"));

    let mut child = spawn_dur("mp_child_dies_holding_writes", &path, mode);
    {
        // A separate attach, only to watch for the child's committed marker.
        let watch = open_dur(&path, mode);
        assert!(
            until(Duration::from_secs(20), || watch
                .begin_read()
                .unwrap()
                .get_by_pk(0, &[Value::Int(777)])
                .unwrap()
                .is_some()),
            "[{mode}] child never committed its marker"
        );
    }
    child.kill().unwrap();
    let _ = child.wait();

    // Third process — nothing of the writer's state is inherited.
    let out = spawn_dur("mp_child_verifies_prefix", &path, mode)
        .wait_with_output()
        .unwrap();
    assert!(
        out.status.success(),
        "[{mode}] reopened database did not hold the all-or-nothing line: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
}

#[test]
fn a_kill_at_durability_commit_leaves_a_committed_prefix() {
    durable_kill_leaves_a_prefix("commit");
}

#[test]
fn a_kill_at_durability_wal_leaves_a_committed_prefix() {
    durable_kill_leaves_a_prefix("wal");
}

// =============================================================== child halves
// `#[ignore]`d so a normal run never executes them; each is a no-op unless the
// parent set the env, which keeps `--ignored` runs harmless too.

#[test]
#[ignore]
fn mp_child_writes_ten() {
    let Some(path) = child_env() else { return };
    let eng = open_dur(&path, &child_durability());
    for id in 0..10i64 {
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &[Value::Int(id), Value::Int(id * 7)]).unwrap();
        w.commit().unwrap();
    }
}

#[test]
#[ignore]
fn mp_child_holds_writer() {
    let Some(path) = child_env() else { return };
    let eng = open_dur(&path, &child_durability());
    {
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &[Value::Int(999), Value::Int(1)]).unwrap();
        w.commit().unwrap();
    }
    let _held = eng.begin_write().unwrap();
    // Hold until the parent kills us. Bounded so a parent that dies first
    // cannot leave this process behind.
    std::thread::sleep(Duration::from_secs(60));
}

#[test]
#[ignore]
fn mp_child_pins_a_snapshot() {
    let Some(path) = child_env() else { return };
    let eng = open_dur(&path, &child_durability());
    {
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &[Value::Int(500), Value::Int(500)]).unwrap();
        w.commit().unwrap();
    }
    let r = eng.begin_read().unwrap();
    let at_pin = r.row_count(0).unwrap();
    // The parent commits ~39 rows during this window.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        r.row_count(0).unwrap(),
        at_pin,
        "a pinned snapshot changed while another process committed"
    );
    assert!(
        r.get_by_pk(0, &[Value::Int(39)]).unwrap().is_none(),
        "a pinned snapshot saw a row committed after it was taken"
    );
}

/// The verifier half of test 5: a fresh process, so recovery has to run from
/// the file. Its exit status IS the assertion.
#[test]
#[ignore]
fn mp_child_verifies_prefix() {
    let Some(path) = child_env() else { return };
    let eng = open_dur(&path, &child_durability());
    let r = eng.begin_read().unwrap();
    assert!(
        r.get_by_pk(0, &[Value::Int(777)]).unwrap().is_some(),
        "the committed row did not survive the writer's death"
    );
    for id in 1000..1010i64 {
        assert!(
            r.get_by_pk(0, &[Value::Int(id)]).unwrap().is_none(),
            "an uncommitted write ({id}) became visible after recovery"
        );
    }
}

#[test]
#[ignore]
fn mp_child_dies_holding_writes() {
    let Some(path) = child_env() else { return };
    let eng = open_dur(&path, &child_durability());
    {
        let mut w = eng.begin_write().unwrap();
        w.insert_row(0, &[Value::Int(777), Value::Int(777)]).unwrap();
        w.commit().unwrap();
    }
    let mut w = eng.begin_write().unwrap();
    for id in 1000..1010i64 {
        w.insert_row(0, &[Value::Int(id), Value::Int(id)]).unwrap();
    }
    // Never committed: die here.
    std::thread::sleep(Duration::from_secs(60));
    drop(w);
}
