//! #110: what the shim's always-on busy policy costs, and why it stays.
//!
//! A write carrying a busy deadline never publishes a ring intent — the gate
//! is `use_ring = deadline.is_none() && …` in `Database::run_write_plan` —
//! and the shim sets a busy policy on every connection (#109 made
//! `busy_timeout` real end-to-end). So every write through the shim takes the
//! direct writer-lock path and forfeits group-commit batch membership. Under
//! `durability = commit` a commit is an `msync`, i.e. a device flush, so N
//! contended shim writers cost N flushes where N ring writers cost one.
//!
//! Two things live here:
//!
//! 1. [`ring_forfeit_measure`] — the measurement. `#[ignore]`d; wants a real
//!    disk. Three paired arms, interleaved over freshly seeded files:
//!    `shim` (C API), `nativebusy` (facade + `set_busy_timeout`: the same
//!    gate without the shim's per-statement overhead) and `native` (facade,
//!    no busy policy: ring-eligible). `native` vs `nativebusy` isolates the
//!    gate; `shim` gives the absolute.
//!
//!    ```text
//!    MPEDB_BENCH_DIR=/mnt/ext4 cargo test --release -p mpedb-capi \
//!        --test ring_forfeit -- --ignored --nocapture
//!    ```
//!
//!    /mnt/ext4, 250 rows/writer, 5 reps, median of the slowest writer:
//!
//!    | writers | shim | nativebusy | native (ring)  |
//!    |---------|------|------------|----------------|
//!    | 1       | 157  | 149        | 179 rows/s     |
//!    | 4       | 146  | 156        | **366 rows/s** |
//!
//!    Reproduced on a second full run: 177 / 182 / 169 at one writer,
//!    **161 / 167 / 392** at four.
//!
//!    Uncontended the arms are indistinguishable — an uncontended write leads
//!    directly, the ring is not on that path. Contended, the busy policy
//!    costs **2.3–2.5×**: four shim writers deliver the aggregate throughput
//!    of ONE writer, because each pays its own msync.
//!
//! 2. [`a_ring_enabled_shim_write_still_answers_busy_at_its_deadline`] — the
//!    guard. Deleting `deadline.is_none()` from that gate buys exactly the
//!    forfeited throughput (measured: shim n=4 146 → 383 rows/s) and breaks
//!    this test: a 200 ms budget returns `SQLITE_OK` after **1.4996 s**,
//!    because a published intent cannot be withdrawn and the enqueued
//!    wait-or-lead loop makes no progress while a foreign transaction holds
//!    the writer lock. The overshoot is set by that transaction's length, not
//!    by batch latency, so no budget threshold bounds it. `design/DESIGN-CAPI.md`
//!    §7 has the full argument and the protocol change that would close it.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;
use std::time::{Duration, Instant};

use mpedb_sqlite3::*;

/// Rows per writer in the timed window (plus one untimed warm-up row).
const ROWS: u64 = 250;

fn bench_dir() -> PathBuf {
    PathBuf::from(std::env::var("MPEDB_BENCH_DIR").unwrap_or_else(|_| "/mnt/ext4".to_string()))
}

fn scratch_dir() -> PathBuf {
    if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    }
}

/// A `durability = commit` seed — the one configuration in which the ring is
/// live. The shim cannot produce it: `open_impl`'s seed config omits
/// `durability`, so a shim-CREATED database is `none` and `ring_enabled` is
/// false for it. #110 is reachable only when the shim attaches a durable file
/// some other mpedb tool made, which is what these tests do.
fn seed_toml(path: &Path) -> String {
    format!(
        r#"
[database]
path = "{}"
size_mb = 256
max_readers = 64
durability = "commit"

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
    )
}

fn seed(path: &Path) {
    let _ = std::fs::remove_file(path);
    let cfg = mpedb::Config::from_toml_str(&seed_toml(path)).unwrap();
    let _db = mpedb::Database::open_with_config(cfg).unwrap();
}

// ------------------------------------------------------------------ child

/// One writer process of the measurement. Selected by `MPEDB_RF_ARM`; a
/// normal `cargo test` run finds the variable unset and returns instantly.
#[test]
fn ring_forfeit_child() {
    let Ok(arm) = std::env::var("MPEDB_RF_ARM") else {
        return;
    };
    let path = PathBuf::from(std::env::var("MPEDB_RF_PATH").unwrap());
    let base: u64 = std::env::var("MPEDB_RF_BASE").unwrap().parse().unwrap();
    let go = PathBuf::from(std::env::var("MPEDB_RF_GO").unwrap());
    let ready = PathBuf::from(std::env::var("MPEDB_RF_READY").unwrap());

    match arm.as_str() {
        "shim" => child_shim(&path, base, &go, &ready),
        "native" | "nativebusy" => child_native(&path, base, &go, &ready, arm == "nativebusy"),
        other => panic!("unknown arm {other}"),
    }
}

/// Arm the barrier, then wait for the start signal. The timed window opens
/// with every writer already attached, compiled and published, so it measures
/// commits and nothing else.
fn wait_for_go(go: &Path, ready: &Path) {
    std::fs::write(ready, b"1").unwrap();
    let t0 = Instant::now();
    while !go.exists() {
        assert!(t0.elapsed() < Duration::from_secs(60), "start signal never came");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn child_shim(path: &Path, base: u64, go: &Path, ready: &Path) {
    unsafe {
        let name = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut db: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(name.as_ptr(), &mut db), SQLITE_OK);
        // 60 s is far more budget than any batch could need: the point of
        // #110 is that NO budget, however generous, buys batch membership.
        assert_eq!(sqlite3_busy_timeout(db, 60_000), SQLITE_OK);
        let sql = CString::new("INSERT INTO t (id, v) VALUES (?, ?)").unwrap();
        let mut st: *mut Stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            SQLITE_OK
        );
        let v = CString::new("xxxxxxxxxxxxxxxx").unwrap();
        let one = |id: u64| {
            sqlite3_reset(st);
            assert_eq!(sqlite3_bind_int64(st, 1, id as i64), SQLITE_OK);
            assert_eq!(
                sqlite3_bind_text(st, 2, v.as_ptr(), -1, ptr::null_mut()),
                SQLITE_OK
            );
            let rc = sqlite3_step(st);
            assert_eq!(rc, SQLITE_DONE, "step: {rc}");
        };
        one(base); // warm: compile + publish the plan before the clock starts
        wait_for_go(go, ready);
        let t0 = Instant::now();
        for i in 1..ROWS {
            one(base + i);
        }
        println!("CHILD_ELAPSED_NS {}", t0.elapsed().as_nanos());
        sqlite3_finalize(st);
        sqlite3_close(db);
    }
}

fn child_native(path: &Path, base: u64, go: &Path, ready: &Path, busy: bool) {
    use mpedb::{Database, Value};
    let db = Database::open_from_file(path).unwrap();
    if busy {
        db.set_busy_timeout(Some(Duration::from_millis(60_000)));
    }
    let h = db.prepare("INSERT INTO t (id, v) VALUES ($1, $2)").unwrap();
    let one = |id: u64| {
        db.execute(&h, &[Value::Int(id as i64), Value::Text("xxxxxxxxxxxxxxxx".into())])
            .unwrap();
    };
    one(base);
    wait_for_go(go, ready);
    let t0 = Instant::now();
    for i in 1..ROWS {
        one(base + i);
    }
    println!("CHILD_ELAPSED_NS {}", t0.elapsed().as_nanos());
}

// ----------------------------------------------------------------- driver

/// Run `arm` with `n` writer processes against a freshly seeded file; return
/// the slowest writer's timed-window seconds — the wall time to drain the
/// whole offered load.
fn run_arm(arm: &str, n: u64, rep: u64) -> f64 {
    let path = bench_dir().join(format!("mpedb-rf-{}-{arm}-{n}-{rep}.mpedb", std::process::id()));
    seed(&path);
    let go = path.with_extension("go");
    let readies: Vec<PathBuf> = (0..n).map(|i| path.with_extension(format!("r{i}"))).collect();
    let _ = std::fs::remove_file(&go);
    for r in &readies {
        let _ = std::fs::remove_file(r);
    }

    let kids: Vec<_> = (0..n)
        .map(|i| {
            Command::new(std::env::current_exe().unwrap())
                .args(["ring_forfeit_child", "--exact", "--nocapture"])
                .env("MPEDB_RF_ARM", arm)
                .env("MPEDB_RF_PATH", &path)
                .env("MPEDB_RF_BASE", (1 + i * 1_000_000).to_string())
                .env("MPEDB_RF_GO", &go)
                .env("MPEDB_RF_READY", &readies[i as usize])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();

    let t0 = Instant::now();
    while !readies.iter().all(|r| r.exists()) {
        assert!(t0.elapsed() < Duration::from_secs(120), "writers never armed");
        std::thread::sleep(Duration::from_millis(2));
    }
    std::fs::write(&go, b"1").unwrap();

    let mut worst = 0f64;
    for k in kids {
        let out = k.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "writer failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let ns: u128 = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("CHILD_ELAPSED_NS "))
            .expect("writer printed no elapsed")
            .trim()
            .parse()
            .unwrap();
        worst = worst.max(ns as f64 / 1e9);
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&go);
    for r in &readies {
        let _ = std::fs::remove_file(r);
    }
    worst
}

fn report(label: &str, n: u64, secs: &[f64]) {
    let rows = (ROWS - 1) * n;
    let mut v = secs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (lo, hi, med) = (v[0], v[v.len() - 1], v[v.len() / 2]);
    println!(
        "{label:<12} n={n}  median {med:>8.3} s  ({:>9.0} rows/s)  min {lo:.3}  max {hi:.3}  spread {:.1}%",
        rows as f64 / med,
        100.0 * (hi - lo) / med
    );
}

#[test]
#[ignore = "measurement: wants a real disk (MPEDB_BENCH_DIR); run with --nocapture"]
fn ring_forfeit_measure() {
    let reps: usize = std::env::var("MPEDB_RF_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    for n in [1u64, 4] {
        let mut acc: Vec<(&str, Vec<f64>)> = vec![
            ("shim", Vec::new()),
            ("nativebusy", Vec::new()),
            ("native", Vec::new()),
        ];
        // Interleaved: one rep of every arm before the next rep, so drift in
        // disk state or machine load hits all three arms alike.
        for rep in 0..reps as u64 {
            for (arm, xs) in acc.iter_mut() {
                xs.push(run_arm(arm, n, rep));
            }
        }
        for (arm, xs) in &acc {
            report(arm, n, xs);
        }
    }
}

// ------------------------------------------------------- the safety guard

/// The property any #110 fix must not break, on the ONE configuration where
/// #110 exists at all: a **ring-enabled** (`durability = commit`) database,
/// attached by the shim, written under a busy timeout while another
/// connection holds the writer lock.
///
/// Delete `deadline.is_none()` from `run_write_plan`'s `use_ring` and this
/// fails, measured: `SQLITE_OK` after 1.4996 s against a 200 ms budget. The
/// write publishes an intent, a published intent cannot be withdrawn (§5.3
/// pins READY+stamped slots to their incarnation precisely so the leader's
/// collect→stage→post cannot be raced), and the enqueued wait-or-lead loop
/// makes no progress while a foreign transaction holds the lock — so the
/// caller waits that transaction out, whatever its length. That is compat
/// gap E1 reopening.
#[test]
fn a_ring_enabled_shim_write_still_answers_busy_at_its_deadline() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let path = scratch_dir().join(format!("mpedb-rf-guard-{}.mpedb", std::process::id()));
    seed(&path);

    // Holder THREAD with its own shim connection: the ERRORCHECK writer mutex
    // is per-thread-owned, so this contends exactly as a second process does
    // (the construction `capi.rs::busy_timeout_bounds_writer_lock_wait` uses).
    let held = Arc::new(AtomicBool::new(false));
    let holder = {
        let (path, held) = (path.clone(), held.clone());
        std::thread::spawn(move || unsafe {
            let name = CString::new(path.to_string_lossy().as_bytes()).unwrap();
            let mut a: *mut Sqlite3 = ptr::null_mut();
            assert_eq!(sqlite3_open(name.as_ptr(), &mut a), SQLITE_OK);
            let run = |sql: &str| {
                let s = CString::new(sql).unwrap();
                sqlite3_exec(a, s.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
            };
            assert_eq!(run("BEGIN"), SQLITE_OK);
            assert_eq!(run("INSERT INTO t (id, v) VALUES (9000001, 'held')"), SQLITE_OK);
            held.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(run("COMMIT"), SQLITE_OK);
            sqlite3_close(a);
        })
    };
    let t0 = Instant::now();
    while !held.load(Ordering::Acquire) {
        assert!(t0.elapsed() < Duration::from_secs(20), "holder never signalled");
        std::thread::sleep(Duration::from_millis(2));
    }

    unsafe {
        let name = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut b: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(name.as_ptr(), &mut b), SQLITE_OK);
        assert_eq!(sqlite3_busy_timeout(b, 200), SQLITE_OK);
        let sql = CString::new("INSERT INTO t (id, v) VALUES (9000002, 'waiter')").unwrap();
        let t0 = Instant::now();
        let rc = sqlite3_exec(b, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
        let dt = t0.elapsed();
        assert_eq!(
            rc, SQLITE_BUSY,
            "a deadline-carrying write must not join a batch it cannot leave (took {dt:?})"
        );
        assert!(dt >= Duration::from_millis(200), "BUSY before the timeout: {dt:?}");
        assert!(dt < Duration::from_millis(1200), "waited past the deadline: {dt:?}");
        sqlite3_close(b);
    }
    holder.join().unwrap();
    let _ = std::fs::remove_file(&path);
}
