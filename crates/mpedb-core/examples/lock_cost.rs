//! What does one writer-lock acquire/release actually cost, and how much of
//! it is the FILE lock? (#164)
//!
//! ## Why a separate instrument
//!
//! The throughput probe (`mpedb/examples/runner_noise.rs`) narrowed #164 to a
//! per-transaction cost and killed the obvious explanation: on a 4-core Linux
//! box, turning the writer lock's spin OFF cost two percentage points, so the
//! missing spin does not explain why Windows keeps 37 % of its single-writer
//! rate under contention where every POSIX arm keeps 73–80 %.
//!
//! What is left is a difference visible in the source: Windows takes a
//! `LockFileEx`/`UnlockFileEx` pair per transaction where macOS takes
//! `flock`, both INSIDE the section. At one writer, with no contention at all,
//! Windows is already 13 µs per insert against macOS's 6.7 µs. Attributing
//! that gap to the file lock is an inference. This measures it.
//!
//! ## How it isolates the file layer, without new syscall bindings
//!
//! `Shm::open_memory` (the `:memory:` path) builds a writer lock with NO
//! sidecar — `WriterLock::private()`, which exists because a process-private
//! mapping has no second process to exclude. Everything else about the lock is
//! identical: the same intra-process primitive, the same DIRTY-word protocol,
//! the same `writer_lock`/`writer_unlock` entry points.
//!
//! So:
//!
//! ```text
//!   file-backed  =  intra-process lock + DIRTY word + FILE LOCK PAIR
//!   :memory:     =  intra-process lock + DIRTY word
//!   difference   =  the file lock pair
//! ```
//!
//! **Linux is the null control.** There the writer lock is one
//! `PROCESS_SHARED` robust mutex in the mapping and there is no sidecar in
//! either configuration, so the difference must come out near zero. If it does
//! not, this probe is measuring something other than what it claims, and the
//! Windows number it prints should not be believed.
//!
//! Uncontended and single-threaded on purpose: this is the cost every
//! transaction pays even when nothing is competing, which is the quantity the
//! inference above is about.
//!
//! ```text
//! cargo run --release -p mpedb-core --example lock_cost -- --iters 200000 --dir /tmp/x
//! ```

use std::path::PathBuf;
use std::time::Instant;

use mpedb_core::shm::Shm;
use mpedb_types::Durability;

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

/// Median of `reps` timings of `iters` lock/unlock pairs, in nanoseconds per
/// pair. Median rather than mean: one descheduling blip in a 200k-iteration
/// run would drag a mean and say nothing about the syscall.
fn time_pairs(shm: &Shm, iters: u32, reps: usize) -> f64 {
    let mut per_rep: Vec<f64> = Vec::with_capacity(reps);
    for _ in 0..reps {
        let t = Instant::now();
        for _ in 0..iters {
            shm.writer_lock().expect("writer_lock");
            shm.writer_unlock();
        }
        per_rep.push(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    per_rep.sort_by(|a, b| a.partial_cmp(b).unwrap());
    per_rep[per_rep.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let iters: u32 = arg(&args, "--iters", "100000").parse().expect("--iters");
    let reps: usize = arg(&args, "--reps", "5").parse().expect("--reps");
    let dir = PathBuf::from(arg(&args, "--dir", "."));
    std::fs::create_dir_all(&dir).expect("--dir");

    let zero = [0u8; 32];
    let size = 16 << 20;

    let path = dir.join(format!("lockcost-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let filed = Shm::open(
        &path,
        size,
        16,
        Durability::None,
        &zero,
        &mpedb_types::FilePerms::default(),
    )
    .expect("open file-backed");
    let mem = Shm::open_memory(size, 16, Durability::None, &zero).expect("open :memory:");

    // Warm both: first touch faults the mapping in, and that has nothing to do
    // with the lock.
    time_pairs(&filed, iters.min(10_000), 1);
    time_pairs(&mem, iters.min(10_000), 1);

    let f = time_pairs(&filed, iters, reps);
    let m = time_pairs(&mem, iters, reps);

    println!("writer-lock acquire+release, uncontended, {iters} iters x {reps} reps (median)\n");
    println!("  file-backed (with the sidecar lock) : {f:>9.0} ns/pair");
    println!("  :memory:    (no sidecar at all)     : {m:>9.0} ns/pair");
    println!("  -> the file-lock pair costs          : {:>9.0} ns", f - m);
    println!(
        "\n  as a share of one uncontended insert (~13 us on the Windows runner,\n\
         ~6.7 us on the M3): {:.1}% / {:.1}%",
        100.0 * (f - m) / 13_000.0,
        100.0 * (f - m) / 6_700.0
    );
    if cfg!(target_os = "linux") {
        println!(
            "\n  NULL CONTROL: on Linux both configurations use the same robust\n\
             mutex in the mapping and neither has a sidecar, so the difference\n\
             above should be near zero. A large number here means this probe is\n\
             not isolating what it claims, and the other platforms' numbers are\n\
             not evidence."
        );
    }

    drop(filed);
    let _ = std::fs::remove_file(&path);
}
