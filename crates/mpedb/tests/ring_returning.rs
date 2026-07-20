//! A CONTENDED write carrying `RETURNING` under `durability = commit` (the
//! intent-ring regime) must take the direct writer-lock path, never the ring.
//!
//! Regression: the `use_ring` predicate admitted RETURNING-carrying writes,
//! so under contention they were published as intents — and a ring result
//! slot carries only an affected count (design/DESIGN.md §5.3), so the leader
//! executing the foreign intent failed with the internal error "write plan
//! returned rows" (surfaced verbatim by `mpedb queue-collide --durability
//! commit`, whose atomic claim is exactly `UPDATE … RETURNING`). The fix
//! keeps such plans off the ring, like host-call and deadline-carrying ones.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn test_config() -> (Config, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!("mpedb-ring-returning-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
  nullable = false
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
}

/// Threads hammer `UPDATE … RETURNING` (and `INSERT … RETURNING`) on one
/// handle so `try_begin_write` keeps losing the race — the contended branch
/// that used to enqueue the plan as an intent. Every call must yield Rows.
#[test]
fn contended_returning_write_stays_off_the_ring() {
    let (mut cfg, path) = test_config();
    // durability=commit is what routes contended writes through the ring.
    cfg.options.durability = mpedb::Durability::Commit;
    let db = Arc::new(Database::open_with_config(cfg).unwrap());
    match db
        .query("INSERT INTO t (id, v) VALUES (1, 0) RETURNING id", &[])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("uncontended INSERT RETURNING gave {other:?}"),
    }

    let failed = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = (0..4)
        .map(|worker| {
            let db = db.clone();
            let failed = failed.clone();
            std::thread::spawn(move || {
                for i in 0..25 {
                    let res = db.query(
                        "UPDATE t SET v = v + 1 WHERE id = 1 RETURNING id, v",
                        &[],
                    );
                    match res {
                        Ok(ExecResult::Rows { rows, .. }) => {
                            assert_eq!(rows.len(), 1, "worker {worker} iter {i}: {rows:?}");
                        }
                        other => {
                            eprintln!("worker {worker} iter {i}: {other:?}");
                            failed.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert!(
        !failed.load(Ordering::Relaxed),
        "a contended RETURNING write rode the intent ring (or otherwise failed)"
    );

    // 4 workers x 25 increments all committed exactly once.
    match db.query("SELECT v FROM t WHERE id = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(100)]]),
        other => panic!("final read gave {other:?}"),
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
}
