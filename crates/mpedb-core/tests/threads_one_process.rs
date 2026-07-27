//! Does the writer lock exclude two THREADS of one process? (#165)
//!
//! Every multi-process property is covered by `multiproc_attach` and by the
//! CLI's SIGKILL harnesses — all of which use separate PROCESSES, so each one
//! gets its own file handle. Nothing covered the other shape: several threads
//! of ONE process writing through one mapping. The facade's suite is
//! single-threaded per test.
//!
//! That gap matters because the three platforms exclude threads by different
//! mechanisms, and only two of them are obviously right:
//!
//! * **Linux** — a `PROCESS_SHARED` robust mutex in the mapping. A mutex
//!   excludes threads by construction.
//! * **macOS** — FLD-2: a process-private ERRORCHECK `pthread_mutex` is HELD
//!   across the section, and the sidecar `flock` is only the cross-process
//!   layer. The mutex is what excludes threads.
//! * **Windows** — FLD-2 too, but the owner mutex is released immediately
//!   (it is a re-entrancy detector, not a lock), and the code's comment says
//!   the blocking `LockFileEx` is what makes a second thread wait. `LockFileEx`
//!   regions belong to the HANDLE, and all threads here share one handle, so
//!   that is a claim about Windows semantics rather than a mechanism anyone
//!   verified.
//!
//! What made this worth writing was a measurement, not a reading: on a 4-core
//! Windows runner mpedb does 76k contended inserts/s with ONE writer thread and
//! 17.4k with four — a collapse — while macOS on the same FLD-2 architecture
//! does 110k with four (#164). Something is different about how Windows
//! serialises threads, and "slower" and "not serialising at all" look the same
//! from the outside until something checks.
//!
//! The check is the engine's own page-accounting verifier plus an exact row
//! count. Two threads inside the writer section at once cannot leave both
//! intact.

use std::sync::Arc;

use mpedb_core::Engine;
use mpedb_types::Value;

fn cfg_path(name: &str) -> std::path::PathBuf {
    let dir = mpedb_testkit::scratch_base().join("mpedb-threads-one-process");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{name}-{}.mpedb", std::process::id()))
}

fn open(name: &str) -> (Arc<Engine>, std::path::PathBuf) {
    let path = cfg_path(name);
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 64
max_readers = 32
durability = "none"

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "s"
  type = "text"
"#,
        mpedb_testkit::toml_path(&path)
    );
    let cfg = mpedb_types::Config::from_toml_str(&toml).unwrap();
    let eng = Engine::open(&cfg, vec![vec![]; cfg.schema.tables.len()]).unwrap();
    (Arc::new(eng), path)
}

/// N threads, one process, one mapping — every write must land exactly once
/// and the page accounting must survive.
#[test]
fn concurrent_writer_threads_are_excluded_and_accounting_survives() {
    const THREADS: i64 = 4;
    const PER_THREAD: i64 = 400;
    let (eng, path) = open("excl");

    let mut hs = Vec::new();
    for t in 0..THREADS {
        let eng = eng.clone();
        hs.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                // Disjoint key ranges: a lost update would show as a missing
                // row, and a double-apply as a PK violation. Neither can be
                // blamed on the workload racing itself.
                let id = t * 1_000_000 + i;
                let mut w = eng.begin_write().unwrap();
                w.insert_row(0, &[Value::Int(id), Value::Text(format!("v{id}"))])
                    .unwrap();
                w.commit().unwrap();
            }
        }));
    }
    for h in hs {
        h.join().expect("a writer thread panicked — the writer lock did not hold");
    }

    // The two independent checks. `verify_page_accounting` catches a page
    // reachable twice or freed twice, which is what two writers inside the
    // section produce; the row count catches a lost or doubled commit, which
    // it does not.
    eng.verify_page_accounting()
        .expect("page accounting broken — two threads were inside the writer section");
    let r = eng.begin_read().unwrap();
    assert_eq!(
        r.row_count(0).unwrap(),
        (THREADS * PER_THREAD) as u64,
        "row count wrong: a commit was lost or applied twice"
    );
    r.finish().unwrap();

    drop(eng);
    let _ = std::fs::remove_file(&path);
}

/// The same shape with a NESTED write attempt from the holding thread, which
/// must be refused rather than deadlocking. The re-entrancy detector is the
/// one part of the Windows lock that is unambiguously a mechanism, and this
/// pins it on every platform.
#[test]
fn a_nested_write_from_the_holding_thread_is_refused() {
    let (eng, path) = open("nested");
    let w = eng.begin_write().unwrap();
    let second = eng.begin_write();
    assert!(
        second.is_err(),
        "a second write txn on the SAME thread must be refused, not granted"
    );
    drop(second);
    drop(w); // abort: Drop releases the writer lock
    // ...and the lock is usable again afterwards, so the refusal did not leak
    // the section.
    let mut w = eng.begin_write().unwrap();
    w.insert_row(0, &[Value::Int(1), Value::Text("x".into())]).unwrap();
    w.commit().unwrap();
    drop(eng);
    let _ = std::fs::remove_file(&path);
}
