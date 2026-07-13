//! Shared workload for the key-locality drain-order A/B facade tests.
//!
//! Two integration-test binaries include this module: `ring_locality.rs`
//! (default arm: sorted drain) and `ring_locality_nosort.rs` (sets
//! `MPEDB_NO_BATCH_ROUTING=1` before any ring use — the switch is read once
//! per process, which is exactly why each arm is its own binary). Both arms
//! run the identical contended multi-thread workload under
//! `durability = commit` (on tmpfs, so msyncs are cheap but the intent ring
//! engages) and assert the identical canonical committed state — proving
//! both drain orders serialize a causally-concurrent batch to the same
//! result.

use mpedb::{params, Config, Database, Error, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const THREADS: i64 = 8;
const PER_THREAD: i64 = 40;
const DUP_IDS: i64 = 10;

fn test_config(name: &str) -> (Config, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-ring-locality-{name}-{}.mpedb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 64

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true

  [[table.column]]
  name = "age"
  type = "int64"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
}

/// The contended workload. Its final state is deterministic under ANY batch
/// ordering: every thread owns a disjoint, interleaved key set (thread t
/// writes ids t, t+8, t+16, ... — so one drained batch holds ADJACENT keys
/// from DIFFERENT threads and the sorted arm genuinely reorders it), plus a
/// duplicate-PK race where all threads insert the identical row (same-key
/// intents keep their relative slot order; either way the surviving state is
/// the same row).
pub fn run_contended_workload_and_assert_canonical_state(arm: &str) {
    let (mut cfg, path) = test_config(arm);
    // durability=commit is what routes contended writes through the intent
    // ring (group commit); on tmpfs the msyncs are cheap enough for a test.
    cfg.options.durability = mpedb::Durability::Commit;
    let db = Arc::new(Database::open_with_config(cfg.clone()).unwrap());
    let ins = db
        .prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
        .unwrap();
    let upd = db.prepare("UPDATE users SET age = $2 WHERE id = $1").unwrap();
    let del = db.prepare("DELETE FROM users WHERE id = $1").unwrap();

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            // phase 1: interleaved point inserts
            for i in 0..PER_THREAD {
                let id = t + i * THREADS;
                match db.execute(&ins, &params![id, format!("u{id}@x.no"), 0]) {
                    Ok(ExecResult::Affected(1)) => {}
                    other => panic!("insert id {id}: {other:?}"),
                }
            }
            // phase 2: point updates + a few point deletes, own keys only
            for i in 0..PER_THREAD {
                let id = t + i * THREADS;
                if i % 10 == 7 {
                    match db.execute(&del, &params![id]) {
                        Ok(ExecResult::Affected(1)) => {}
                        other => panic!("delete id {id}: {other:?}"),
                    }
                } else {
                    match db.execute(&upd, &params![id, id % 100]) {
                        Ok(ExecResult::Affected(1)) => {}
                        other => panic!("update id {id}: {other:?}"),
                    }
                }
            }
            // phase 3: duplicate-PK race — identical row from every thread
            let mut wins = 0u64;
            for d in 0..DUP_IDS {
                let id = 1_000_000 + d;
                match db.execute(&ins, &params![id, format!("dup{d}@x.no"), 42]) {
                    Ok(ExecResult::Affected(1)) => wins += 1,
                    Err(Error::PrimaryKeyViolation { .. }) | Err(Error::UniqueViolation { .. }) => {}
                    other => panic!("dup insert id {id}: {other:?}"),
                }
            }
            wins
        }));
    }
    let wins: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(wins, DUP_IDS as u64, "exactly one winner per contested id");
    db.verify().unwrap();

    // canonical expected state, computed independently of any execution order
    let mut expected: Vec<(i64, String, i64)> = Vec::new();
    for t in 0..THREADS {
        for i in 0..PER_THREAD {
            if i % 10 == 7 {
                continue; // deleted
            }
            let id = t + i * THREADS;
            expected.push((id, format!("u{id}@x.no"), id % 100));
        }
    }
    for d in 0..DUP_IDS {
        expected.push((1_000_000 + d, format!("dup{d}@x.no"), 42));
    }
    expected.sort();

    // read back through a FRESH handle: this is the committed state
    let db2 = Database::open_with_config(cfg).unwrap();
    let rows = match db2
        .query("SELECT id, email, age FROM users", &params![])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    let mut got: Vec<(i64, String, i64)> = rows
        .into_iter()
        .map(|r| match &r[..] {
            [Value::Int(id), Value::Text(email), Value::Int(age)] => (*id, email.clone(), *age),
            other => panic!("unexpected row shape {other:?}"),
        })
        .collect();
    got.sort();
    assert_eq!(
        got, expected,
        "committed state must be canonical regardless of drain order ({arm} arm)"
    );
    let _ = std::fs::remove_file(&path);
}
