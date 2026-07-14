//! M8.4 under-load fuzz (DESIGN-MIRROR §10.4, in-process variant). Several source
//! writer threads churn the sqlite source while the mirror pulls concurrently on
//! the main thread; at the end everything must converge to a shared model with
//! nothing lost or duplicated.
//!
//! Robust-by-construction (no flaky SQLITE_BUSY): the source is WAL (a reader
//! never blocks the single writer), writers serialize through the model mutex so
//! their SQL + model update is one atomic step, and the pull is a pure reader.
//! The adversarial element is scheduling: the pull's read snapshot interleaves
//! arbitrarily with writer commits, so a cursor/coalescing bug would surface as
//! lost or duplicated rows versus the model.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use mpedb::{Database, ExecResult};
use mpedb_mirror::switch::drain_pull;
use mpedb_mirror::{import_sqlite, verify, ImportOptions, SqliteAdapter};
use mpedb_types::Value;
use rusqlite::Connection;

fn tmp(name: &str, ext: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir()
        .join("mpedb-mirror-tests")
        .join(format!("{name}-{}.{ext}", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&p);
    p
}

fn open_tuned(path: &std::path::Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.busy_timeout(Duration::from_secs(10)).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
    c
}

fn mpedb_map(db: &Database) -> BTreeMap<i64, i64> {
    let ExecResult::Rows { rows, .. } = db.query("SELECT id, v FROM t", &[]).unwrap() else {
        panic!()
    };
    rows.iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Int(id), Value::Int(v)) => (*id, *v),
            other => panic!("bad row {other:?}"),
        })
        .collect()
}

fn source_map(c: &Connection) -> BTreeMap<i64, i64> {
    let mut s = c.prepare("SELECT id, v FROM t").unwrap();
    s.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn run(writers: usize, ops_each: usize, keyspace: i64, seed: u64) {
    let src = tmp(&format!("conc-src-{seed}"), "db");
    let mid = tmp(&format!("conc-mid-{seed}"), "mpedb");
    {
        let c = open_tuned(&src);
        c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER); INSERT INTO t VALUES (1,1);")
            .unwrap();
    }
    let db = {
        let mut c = open_tuned(&src);
        import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
    };
    let mut adapter = SqliteAdapter::new(open_tuned(&src), None, &[]).unwrap();
    adapter.install_triggers().unwrap();

    let model = Arc::new(Mutex::new(source_map(adapter.conn())));
    let done = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for w in 0..writers {
        let model = Arc::clone(&model);
        let done = Arc::clone(&done);
        let src = src.clone();
        handles.push(thread::spawn(move || {
            let conn = open_tuned(&src);
            let mut state = seed ^ ((w as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut next = || {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                state.wrapping_mul(0x2545_f491_4f6c_dd1d)
            };
            for _ in 0..ops_each {
                let id = 1 + (next() % keyspace as u64) as i64;
                // hold the model lock across the SQL write so model == source
                let mut m = model.lock().unwrap();
                if next() % 5 == 0 && m.contains_key(&id) {
                    conn.execute("DELETE FROM t WHERE id=?1", rusqlite::params![id]).unwrap();
                    m.remove(&id);
                } else {
                    let v = (next() % 1_000_000) as i64;
                    if m.contains_key(&id) {
                        conn.execute("UPDATE t SET v=?1 WHERE id=?2", rusqlite::params![v, id]).unwrap();
                    } else {
                        conn.execute("INSERT INTO t(id,v) VALUES(?1,?2)", rusqlite::params![id, v]).unwrap();
                    }
                    m.insert(id, v);
                }
                drop(m);
            }
            done.fetch_add(1, Ordering::SeqCst);
        }));
    }

    // pull concurrently with the writers, then drain the tail
    while done.load(Ordering::SeqCst) < writers {
        drain_pull(&db, &mut adapter).unwrap();
        thread::yield_now();
    }
    for h in handles {
        h.join().unwrap();
    }
    drain_pull(&db, &mut adapter).unwrap();

    // convergence: mpedb == source == model, nothing lost or duplicated
    let m = model.lock().unwrap().clone();
    assert_eq!(source_map(adapter.conn()), m, "source == model (mutex invariant)");
    assert_eq!(mpedb_map(&db), m, "mpedb converged to the model under load");
    assert!(verify(&db, &mut adapter).unwrap());

    for p in [src, mid] {
        let _ = std::fs::remove_file(p);
    }
    // WAL sidecars
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", tmp(&format!("conc-src-{seed}"), "db").display()));
    }
}

#[test]
fn concurrent_source_writers_and_pull_converge() {
    for seed in [0x1111_2222u64, 0x3333_4444] {
        run(3, 40, 10, seed);
    }
}

#[test]
#[ignore = "slow: heavier concurrent-load soak (run with --ignored)"]
fn concurrent_load_soak() {
    run(5, 300, 24, 0xc0ff_ee00);
}
