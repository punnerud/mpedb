//! What 32-bit ARM is actually a test of.
//!
//! x86-64 is TSO: a store-buffering race needs the store buffer to be visible,
//! and it usually is not. ARM is weakly ordered, so the fences in the reader-pin
//! protocol (design/DESIGN.md §4.3) are doing real work there and their absence would
//! be observable rather than theoretical.
//!
//! And armv7 is 32-bit: `usize` is 4 bytes, and every packed `{pid, seq}` word
//! and meta field is a `u64` atomic that the compiler has to build out of
//! `ldrexd`/`strexd`. If Rust could not do that lock-free, `target_has_atomic`
//! would be false and this would not compile — so the BUILD is half the test.
//! The other half is that a lock-based fallback would be silently wrong ACROSS
//! PROCESSES, since a lock in one process's memory guards nothing in another's.
//!
//! Usage: arm_check <dir> [seconds] [readers]
//!
//! Cross-compiling for a Pi (no Rust needed on the target — musl links static):
//!
//! ```sh
//! rustup target add armv7-unknown-linux-musleabihf
//! CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=rust-lld //!   cargo build --release --target armv7-unknown-linux-musleabihf //!               --example arm_check -p mpedb
//! scp target/armv7-unknown-linux-musleabihf/release/examples/arm_check pi@host:/tmp/
//! ssh pi@host /tmp/arm_check /dev/shm 8 2
//! ```
//!
//! The whole test suite travels the same way — `cargo test --no-run --target
//! armv7-…` builds the test binaries, and they run on the target unchanged.
//! That is how the 318-test armv7 run in the README's platform table was done.

use mpedb::{params, Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// The atomics check is a BUILD check, not a runtime one, and it belongs here
/// rather than in an `assert!` that clippy correctly calls constant.
///
/// Without native 64-bit atomics `AtomicU64` would not exist and mpedb would
/// not compile at all — so reaching `main` already proves it. What this adds is
/// the REASON, at the place a future port to a target without them (armv5,
/// some RISC-V profiles) would land: a lock-based fallback is not merely slow,
/// it is silently WRONG across processes, because the lock lives in one
/// process's memory and guards nothing in another's.
const _: () = assert!(
    cfg!(target_has_atomic = "64"),
    "mpedb needs NATIVE 64-bit atomics: the reader table's packed pid/seq \
     words and the meta double-buffer are shared across processes, and a \
     lock-based fallback would guard only the process that owns the lock"
);

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(a.get(1).cloned().unwrap_or_else(|| "/dev/shm".into()));
    let secs: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let readers: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(2);

    println!("arch: {} ({}-bit usize)", std::env::consts::ARCH, usize::BITS);

    println!(
        "AtomicU64: native (target_has_atomic=64 -> {})",
        cfg!(target_has_atomic = "64")
    );

    let path = dir.join(format!("armcheck-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let cfg = Config::from_toml_str(&format!(
        r#"
[database]
path = "{}"
size_mb = 32
max_readers = 32
durability = "none"

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "seq"
  type = "int64"

  [[table.column]]
  name = "mirror"
  type = "int64"
"#,
        path.display()
    ))
    .unwrap();
    let db = std::sync::Arc::new(Database::open_with_config(cfg).unwrap());
    db.query("INSERT INTO t (id, seq, mirror) VALUES (1, 0, 0)", &[]).unwrap();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let torn = std::sync::Arc::new(AtomicU64::new(0));
    let reads = std::sync::Arc::new(AtomicU64::new(0));

    // Readers: every snapshot must show seq == mirror. The writer sets them in
    // one transaction, so a reader that sees them disagree saw a torn commit —
    // which on a weakly-ordered machine is what a missing fence looks like.
    let mut hs = Vec::new();
    for _ in 0..readers {
        let (db, stop, torn, reads) = (db.clone(), stop.clone(), torn.clone(), reads.clone());
        hs.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(ExecResult::Rows { rows, .. }) =
                    db.query("SELECT seq, mirror FROM t WHERE id = 1", &[])
                {
                    if let [Value::Int(s), Value::Int(m)] = rows[0][..] {
                        if s != m {
                            torn.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    let t0 = std::time::Instant::now();
    let mut n = 0i64;
    while t0.elapsed().as_secs() < secs {
        for _ in 0..64 {
            n += 1;
            db.query(
                "UPDATE t SET seq = $1, mirror = $1 WHERE id = 1",
                &params![n],
            )
            .unwrap();
        }
    }
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join().unwrap();
    }

    let el = t0.elapsed().as_secs_f64();
    println!(
        "{n} writes ({:.0}/s), {} reads ({:.0}/s) across {readers} reader threads",
        n as f64 / el,
        reads.load(Ordering::Relaxed),
        reads.load(Ordering::Relaxed) as f64 / el
    );
    let t = torn.load(Ordering::Relaxed);
    println!("torn snapshots: {t}");
    db.verify().unwrap();
    println!("verify: OK");
    let _ = std::fs::remove_file(&path);
    assert_eq!(t, 0, "a reader saw a half-applied commit");
    println!("PASS");
}
