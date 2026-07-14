//! M8.3 sqlite specials (DESIGN-MIRROR §10.7). Two behaviours verified against a
//! real sqlite engine (see the probe in the M8.3 commit):
//!
//!  - **FK cascade capture** — sqlite fires AFTER DELETE triggers on a foreign-key
//!    cascade, so the changelog records the cascade-deleted children and a plain
//!    pull propagates them.
//!  - **REPLACE-hole → anti-entropy** — `INSERT OR REPLACE` that collides on a
//!    *secondary* UNIQUE silently deletes the conflicting row WITHOUT firing the
//!    DELETE trigger (unlike an FK cascade). A pull therefore misses that delete
//!    and mpedb drifts; the anti-entropy `reconcile` (source-wins full compare)
//!    is the safety net that converges it.

use mpedb::{Database, ExecResult};
use mpedb_mirror::switch::drain_pull;
use mpedb_mirror::{import_sqlite, reconcile, verify, ImportOptions, SqliteAdapter};
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

fn ids(db: &Database, table: &str) -> Vec<i64> {
    match db.query(&format!("SELECT id FROM {table}"), &[]).unwrap() {
        ExecResult::Rows { mut rows, .. } => {
            let mut v: Vec<i64> = rows
                .drain(..)
                .map(|r| match r[0] {
                    Value::Int(i) => i,
                    _ => panic!(),
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn fk_cascade_deletes_are_captured_and_pulled() {
    let src = tmp("fk-src", "db");
    let mid = tmp("fk-mid", "mpedb");
    {
        let c = Connection::open(&src).unwrap();
        c.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE parent(id INTEGER PRIMARY KEY);
             CREATE TABLE child(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id) ON DELETE CASCADE);
             INSERT INTO parent VALUES (1),(2);
             INSERT INTO child VALUES (10,1),(11,1),(20,2);",
        )
        .unwrap();
    }
    let db = {
        let mut c = Connection::open(&src).unwrap();
        import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
    };
    assert_eq!(ids(&db, "parent"), vec![1, 2]);
    assert_eq!(ids(&db, "child"), vec![10, 11, 20]);

    let mut a = SqliteAdapter::new(Connection::open(&src).unwrap(), None, &[]).unwrap();
    a.install_triggers().unwrap();

    // delete parent 1 with FK ON → children 10,11 cascade (triggers fire)
    a.conn().execute_batch("PRAGMA foreign_keys=ON; DELETE FROM parent WHERE id=1;").unwrap();

    let pulled = drain_pull(&db, &mut a).unwrap();
    assert!(pulled >= 3, "parent + 2 cascaded children (got {pulled})");
    assert_eq!(ids(&db, "parent"), vec![2], "parent 1 gone");
    assert_eq!(ids(&db, "child"), vec![20], "cascaded children gone, sibling kept");
    assert!(verify(&db, &mut a).unwrap());

    for p in [src, mid] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn replace_secondary_unique_hole_drifts_pull_then_reconcile_converges() {
    let src = tmp("repl-src", "db");
    let mid = tmp("repl-mid", "mpedb");
    {
        let c = Connection::open(&src).unwrap();
        c.execute_batch(
            "CREATE TABLE u(id INTEGER PRIMARY KEY, e TEXT NOT NULL UNIQUE);
             INSERT INTO u VALUES (1,'a@x'),(2,'b@x');",
        )
        .unwrap();
    }
    let db = {
        let mut c = Connection::open(&src).unwrap();
        import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
    };
    let mut a = SqliteAdapter::new(Connection::open(&src).unwrap(), None, &[]).unwrap();
    a.install_triggers().unwrap();

    // REPLACE colliding on the secondary UNIQUE `e`: id=3 takes 'a@x', which
    // SILENTLY deletes id=1 — and that delete does NOT fire the DELETE trigger.
    a.conn().execute("INSERT OR REPLACE INTO u(id,e) VALUES (3,'a@x')", []).unwrap();
    // source truth is now {2, 3}
    let src_ids: Vec<i64> = {
        let mut s = a.conn().prepare("SELECT id FROM u ORDER BY id").unwrap();
        s.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    assert_eq!(src_ids, vec![2, 3]);

    // a plain pull only sees id=3's insert; it never learns id=1 was deleted, so
    // mpedb drifts (id=1 lingers) — the REPLACE-hole.
    drain_pull(&db, &mut a).unwrap();
    assert!(ids(&db, "u").contains(&1), "the missed delete leaves id=1 as a phantom");
    assert!(!verify(&db, &mut a).unwrap(), "mpedb and source diverge");

    // anti-entropy reconcile (source-wins full compare) closes the hole.
    let st = reconcile(&db, &mut a).unwrap();
    assert!(st.deletes >= 1, "reconcile removes the phantom");
    assert_eq!(ids(&db, "u"), vec![2, 3], "converged to the source");
    assert!(verify(&db, &mut a).unwrap());

    for p in [src, mid] {
        let _ = std::fs::remove_file(p);
    }
}
