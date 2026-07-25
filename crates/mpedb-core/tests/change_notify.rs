use std::time::{Duration, Instant};

use mpedb_core::engine::Engine;
use mpedb_core::shm::NOTIFY_SLOTS;
use mpedb_types::{Config, Value};

/// Two tables, so "did a write to A wake a listener on B" is answerable.
fn cfg_for(path: &std::path::Path) -> Config {
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "a"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"

[[table]]
name = "b"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
"#,
        path.display()
    );
    Config::from_toml_str(&toml).unwrap()
}

fn open(tag: &str) -> (Engine, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-notify-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = cfg_for(&path);
    let n = cfg.schema.tables.len();
    (Engine::open(&cfg, vec![vec![]; n]).unwrap(), path)
}

/// Table ids are assigned by name order in the schema: a = 0, b = 1.
const A: u32 = 0;
const B: u32 = 1;

fn row(id: i64) -> Vec<Value> {
    vec![Value::Int(id), Value::Int(id)]
}

/// The generation moves for the table that was written, and ONLY for it.
#[test]
fn a_write_bumps_only_its_own_table() {
    let (eng, path) = open("own");
    let before_a = eng.change_generation(A).map(|(g, _)| g).unwrap_or(0);
    let before_b = eng.change_generation(B).map(|(g, _)| g).unwrap_or(0);

    let mut w = eng.begin_write().unwrap();
    w.insert_row(A, &row(1)).unwrap();
    w.commit().unwrap();

    let (after_a, _) = eng.change_generation(A).expect("table a owns its slot");
    assert!(after_a > before_a, "writing table a must move its generation");
    if let Some((after_b, _)) = eng.change_generation(B) {
        assert_eq!(
            after_b, before_b,
            "writing table a moved table b's generation — the per-table filter is gone"
        );
    }
    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// A listener parked on table `b` must NOT be woken by a commit to table `a`.
/// This is the assertion that distinguishes the design from a global "the
/// database changed" signal.
#[test]
fn a_write_does_not_wake_a_listener_on_another_table() {
    let (eng, path) = open("cross");
    let seen_b = eng.change_generation(B).map(|(g, _)| g).unwrap_or(0);

    // Write `a` from another thread while the listener parks on `b`.
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            let mut w = eng.begin_write().unwrap();
            w.insert_row(A, &row(7)).unwrap();
            w.commit().unwrap();
        });
        let t0 = Instant::now();
        let woke = eng.wait_for_change(&[B], &[seen_b], Duration::from_millis(250));
        assert!(
            woke.is_empty(),
            "a listener on table b was woken by a write to table a: {woke:?}"
        );
        // It should have waited out the timeout rather than returning early.
        assert!(
            t0.elapsed() >= Duration::from_millis(200),
            "the wait returned after {:?} — it did not actually park",
            t0.elapsed()
        );
    });
    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// The listener DOES wake for its own table, and promptly.
#[test]
fn a_write_wakes_a_listener_on_that_table() {
    let (eng, path) = open("same");
    let seen = eng.change_generation(A).map(|(g, _)| g).unwrap_or(0);

    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            let mut w = eng.begin_write().unwrap();
            w.insert_row(A, &row(9)).unwrap();
            w.commit().unwrap();
        });
        let woke = eng.wait_for_change(&[A], &[seen], Duration::from_secs(5));
        assert_eq!(woke, vec![A], "the listener on table a was not woken by a write to a");
    });
    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// A change that lands BETWEEN the caller's look and its park must not be slept
/// through. This is the lost-wakeup window every condition-variable protocol
/// has to close, and here it is closed by sampling the futex word before
/// testing the generation.
#[test]
fn a_change_racing_the_park_is_not_missed() {
    let (eng, path) = open("race");
    for round in 0..50 {
        let seen = eng.change_generation(A).map(|(g, _)| g).unwrap_or(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut w = eng.begin_write().unwrap();
                w.insert_row(A, &row(round + 100)).unwrap();
                w.commit().unwrap();
            });
            // No sleep: the writer and the parker race deliberately.
            let woke = eng.wait_for_change(&[A], &[seen], Duration::from_secs(5));
            assert_eq!(woke, vec![A], "round {round}: a racing commit was slept through");
        });
    }
    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// Nobody listening ⇒ the waiter count stays zero, which is what lets a commit
/// skip the wake syscalls entirely.
#[test]
fn an_idle_database_parks_nobody() {
    let (eng, path) = open("idle");
    let mut w = eng.begin_write().unwrap();
    w.insert_row(A, &row(1)).unwrap();
    w.commit().unwrap();
    assert_eq!(
        eng.notify_waiter_count(),
        0,
        "a commit left a phantom waiter behind — every later commit would pay for syscalls nobody needs"
    );
    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// Slot sharing is a false wakeup, never a false "unchanged": two tables
/// `NOTIFY_SLOTS` apart collide, and the reader must be told it cannot answer
/// rather than being handed the other table's generation.
#[test]
fn a_slot_collision_reports_unknown_not_a_wrong_answer() {
    let (eng, path) = open("collide");
    // Table 0 owns slot 0. Ask about a table that hashes to the same slot but
    // does not exist in this database.
    let colliding = NOTIFY_SLOTS;
    let mut w = eng.begin_write().unwrap();
    w.insert_row(A, &row(1)).unwrap();
    w.commit().unwrap();
    assert!(
        eng.change_generation(colliding).is_none(),
        "table {colliding} shares a slot with table 0 and was handed table 0's generation"
    );
    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// **N3.** A listener watching several tables must be woken by a change to ANY
/// of them, not just the first. Before the fix `wait_for_change` parked on
/// `tables[0]`'s futex word alone, so a write to `b` was noticed only when the
/// timeout expired — the return value was right and the latency was three
/// orders of magnitude wrong, which is why this asserts on elapsed time and
/// not just on what came back.
#[test]
fn a_multi_table_listener_wakes_on_the_second_table_promptly() {
    let (eng, path) = open("multi");
    let seen_a = eng.change_generation(A).map(|(g, _)| g).unwrap_or(0);
    let seen_b = eng.change_generation(B).map(|(g, _)| g).unwrap_or(0);

    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            let mut w = eng.begin_write().unwrap();
            w.insert_row(B, &row(3)).unwrap();
            w.commit().unwrap();
        });
        let t0 = Instant::now();
        // A long timeout, so "returned because it was woken" and "returned
        // because it gave up" are far apart and cannot be confused.
        let woke = eng.wait_for_change(&[A, B], &[seen_a, seen_b], Duration::from_secs(10));
        let waited = t0.elapsed();
        assert_eq!(woke, vec![B], "the listener did not report table b as changed");
        assert!(
            waited < Duration::from_secs(1),
            "waited {waited:?} for a change to the SECOND watched table — it \
             parked on table a's word and only noticed at timeout"
        );
    });
    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// **#147: a listener killed while parked must not leak the count forever.**
///
/// Before the waiter registry there was nothing but a bare counter, so a
/// SIGKILL mid-park left it raised until the next format or reboot, and every
/// commit after that paid wake syscalls for a process that was gone. The count
/// is only an optimisation, so this was never a wrong answer — but it was
/// unbounded in time and triggered by exactly the event this engine is built
/// around.
///
/// The phantom is planted directly rather than by forking and killing, so the
/// test does not depend on teardown timing. It uses OUR pid with a start time
/// that is not ours — the pid-reuse case the identity exists for. A dead-pid
/// phantom would be the obvious choice and is the wrong one: `kill(1, 0)`
/// answers EPERM for an unowned process, and the sweep treats EPERM as ALIVE
/// on purpose. Erring toward alive is the safe direction, because sweeping a
/// live waiter would undercount and an undercount is a missed wakeup.
#[test]
fn a_dead_waiters_slot_is_reclaimed_and_the_count_corrected() {
    let (eng, path) = open("sweep");
    let shm = eng.shm_for_test();
    assert_eq!(eng.notify_waiter_count(), 0, "a fresh database has no waiters");

    // Plant a phantom: a registered slot whose owner is not this incarnation.
    // pid 1 exists but its start time is not ours, so the identity check
    // rejects it — the same rule the reader table uses for pid reuse.
    let phantom_pid = std::process::id();
    let bogus_start = shm.own_start_time().expect("own start time") ^ 0xFFFF;
    shm.waiter_register(phantom_pid, bogus_start)
        .expect("a fresh registry has room");
    shm.notify_waiters().fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    assert_eq!(eng.notify_waiter_count(), 1, "the phantom should be counted");

    // A sweep must reclaim it and correct the count.
    assert_eq!(eng.sweep_listeners(), 1, "the dead slot was not reclaimed");
    assert_eq!(
        eng.notify_waiter_count(),
        0,
        "the slot was reclaimed but the count was left raised — every later \
         commit still pays wake syscalls for a listener that does not exist"
    );

    // And sweeping again is a no-op: reclaiming twice would UNDERCOUNT, which
    // is the fatal direction (a missed wakeup, not a wasted syscall).
    assert_eq!(eng.sweep_listeners(), 0, "a second sweep reclaimed the same slot again");
    assert_eq!(eng.notify_waiter_count(), 0);

    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// A LIVE waiter must never be swept: that would decrement a count its owner
/// will decrement again on the way out, and an undercount is a missed wakeup.
#[test]
fn a_live_waiters_slot_survives_a_sweep() {
    let (eng, path) = open("sweeplive");
    let shm = eng.shm_for_test();
    // Register through the same path a real listener uses, so the identity is
    // whatever this platform actually records rather than something the test
    // reconstructs and could get wrong.
    let pid = std::process::id();
    let start = shm.own_start_time().expect("own start time");

    shm.waiter_register(pid, start).expect("room");
    shm.notify_waiters().fetch_add(1, std::sync::atomic::Ordering::AcqRel);

    assert_eq!(eng.sweep_listeners(), 0, "a live listener's slot was reclaimed");
    assert_eq!(eng.notify_waiter_count(), 1, "a live listener was decremented");

    drop(eng);
    let _ = std::fs::remove_file(&path);
}
