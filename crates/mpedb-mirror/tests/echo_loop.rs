//! M8.2 echo-loop fixpoint (DESIGN-MIRROR §10.6). The single most load-bearing
//! liveness property of the whole write-back design: a sync loop (pull then
//! push) over a mirror that diverged on BOTH sides must reach a fixpoint and
//! then do *nothing* — if echo suppression were broken, our own pushed rows
//! would bounce back through the next pull and the loop would never quiesce.
//!
//! Exercises the public API only (import → SqliteAdapter → drain_pull/drain_push
//! → verify → conflicts), so it doubles as a smoke test of the crate surface.

use mpedb::{Database, ExecResult};
use mpedb_mirror::switch::{drain_pull, drain_push};
use mpedb_mirror::{conflicts, import_sqlite, verify, ImportOptions, SqliteAdapter};
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

/// One sync round in source-authoritative mode: pull then push. Returns
/// (rows pulled, rows pushed, push conflicts).
fn sync_round(db: &Database, a: &mut SqliteAdapter) -> (u64, u64, u64) {
    let pulled = drain_pull(db, a).unwrap();
    let s = drain_push(db, a).unwrap();
    (pulled, s.upserts + s.deletes, s.conflicts)
}

fn mpedb_v(db: &Database, id: i64) -> Option<i64> {
    match db.query("SELECT v FROM t WHERE id=$1", &[Value::Int(id)]).unwrap() {
        ExecResult::Rows { rows, .. } => rows.first().map(|r| match r[0] {
            Value::Int(i) => i,
            _ => panic!(),
        }),
        other => panic!("{other:?}"),
    }
}

fn src_v(c: &Connection, id: i64) -> Option<i64> {
    c.query_row("SELECT v FROM t WHERE id=?1", [id], |r| r.get::<_, i64>(0)).ok()
}

#[test]
fn sync_loop_reaches_a_no_op_fixpoint_without_echo() {
    let src = tmp("echo-src", "db");
    let mid = tmp("echo-mid", "mpedb");
    {
        let c = Connection::open(&src).unwrap();
        c.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);
             INSERT INTO t VALUES (1,10),(2,20),(3,30);",
        )
        .unwrap();
    }
    let db = {
        let mut c = Connection::open(&src).unwrap();
        import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
    };
    let mut a = SqliteAdapter::new(Connection::open(&src).unwrap(), None, &[]).unwrap();
    a.install_triggers().unwrap();

    // diverge BOTH sides simultaneously:
    //   local-only : id=1 changed, id=10 inserted
    //   source-only: id=2 changed, id=11 inserted
    //   divergence : id=3 changed on both (source B must win in S2)
    db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(111), Value::Int(1)]).unwrap();
    db.query("INSERT INTO t (id,v) VALUES ($1,$2)", &[Value::Int(10), Value::Int(100)]).unwrap();
    db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(333), Value::Int(3)]).unwrap(); // local A
    a.conn()
        .execute_batch(
            "UPDATE t SET v=222 WHERE id=2;
             INSERT INTO t VALUES (11,110);
             UPDATE t SET v=999 WHERE id=3;", // source B
        )
        .unwrap();

    // round 1 does the work; round 2 MUST be a complete no-op (no echo bounce).
    let (p1, q1, _c1) = sync_round(&db, &mut a);
    assert!(p1 > 0 && q1 > 0, "round 1 should move rows both ways (got {p1},{q1})");
    let (p2, q2, c2) = sync_round(&db, &mut a);
    assert_eq!((p2, q2, c2), (0, 0, 0), "round 2 must be a fixpoint — no echo storm");

    // and a third round is likewise silent
    assert_eq!(sync_round(&db, &mut a), (0, 0, 0));

    // both sides identical at the fixpoint
    assert!(verify(&db, &mut a).unwrap(), "mpedb and source converge");

    // spot-check the resolution: source-only + local-only rows crossed over,
    // and the divergent id=3 resolved source-wins (999, not the local 333).
    assert_eq!(mpedb_v(&db, 11), Some(110), "source-only insert reached mpedb");
    assert_eq!(src_v(a.conn(), 10), Some(100), "local-only insert reached the source");
    assert_eq!(mpedb_v(&db, 1), Some(111), "local change survived");
    assert_eq!(src_v(a.conn(), 2), Some(222), "source change survived");
    assert_eq!(mpedb_v(&db, 3), Some(999), "divergence resolved source-wins");
    assert_eq!(src_v(a.conn(), 3), Some(999));

    // the divergence was audited (parked), the rest were not
    let parked = conflicts::list(&db).unwrap();
    assert_eq!(parked.len(), 1, "only id=3 diverged");
    assert_eq!(parked[0].pk, vec![Value::Int(3)]);

    for p in [src, mid] {
        let _ = std::fs::remove_file(p);
    }
}
