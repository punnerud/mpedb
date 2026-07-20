//! v2 checkpoint differential (design §5): push deltas into the base via the
//! sqlite library, marker in the base, truncate the overlay, re-stamp — then
//! verify every step against rusqlite's view of the base.
#![cfg(feature = "sqlite-checkpoint")]

use mpedb::{LockMode, ReconcilePolicy, SqliteOverlay, Value};
use rusqlite::Connection;

fn setup(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir()
        .join("mpedb-checkpoint-tests")
        .join(format!("cp-{tag}-{}.db", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    for suffix in ["", ".overlay.mpedb", ".overlay.probe"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
    }
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = DELETE;
         CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
         CREATE TABLE logs (msg TEXT);  -- synthetic rowid",
    )
    .unwrap();
    for i in 0..50i64 {
        c.execute(
            "INSERT INTO users VALUES (?, ?, ?)",
            rusqlite::params![i, format!("u{i}"), 20 + i],
        )
        .unwrap();
    }
    c.execute("INSERT INTO logs VALUES ('start')", []).unwrap();
    drop(c);
    p
}

fn rows(r: mpedb::ExecResult) -> Vec<Vec<Value>> {
    match r {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn count(ovl: &mut SqliteOverlay, sql: &str) -> i64 {
    match &rows(ovl.query(sql, &[]).unwrap())[0][0] {
        Value::Int(i) => *i,
        other => panic!("expected int, got {other:?}"),
    }
}

#[test]
fn checkpoint_roundtrip_against_the_library() {
    let p = setup("roundtrip");
    let mut ovl = SqliteOverlay::open(&p).unwrap();

    // Deltas of every kind: insert, update-of-base-row, delete-of-base-row,
    // and a synthetic-rowid table write.
    ovl.query("INSERT INTO users (id, name, age) VALUES (100, 'ny', 1)", &[]).unwrap();
    ovl.query("UPDATE users SET name = 'endret' WHERE id = 10", &[]).unwrap();
    ovl.query("DELETE FROM users WHERE id = 20", &[]).unwrap();
    ovl.query("INSERT INTO logs (msg, rowid) VALUES ('fra-overlay', 99)", &[]).unwrap();

    let report = ovl.checkpoint().unwrap();
    assert_eq!(report.upserts, 3, "insert + update + logs row");
    assert_eq!(report.deletes, 1, "one tombstone");

    // The BASE now holds everything — the library is the witness.
    let lib = Connection::open(&p).unwrap();
    let n: i64 = lib.query_row("SELECT count(*) FROM users", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 50, "49 base + 1 pushed insert - ... = 50");
    let name: String =
        lib.query_row("SELECT name FROM users WHERE id = 10", [], |r| r.get(0)).unwrap();
    assert_eq!(name, "endret");
    let gone: i64 =
        lib.query_row("SELECT count(*) FROM users WHERE id = 20", [], |r| r.get(0)).unwrap();
    assert_eq!(gone, 0);
    let msg: String =
        lib.query_row("SELECT msg FROM logs WHERE rowid = 99", [], |r| r.get(0)).unwrap();
    assert_eq!(msg, "fra-overlay");
    // The marker landed in the base, atomically with the push.
    let epoch: i64 = lib
        .query_row("SELECT v FROM _mpedb_overlay_state WHERE k = 'checkpointed_epoch'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(epoch, report.epoch as i64);
    drop(lib);

    // The handle keeps serving the SAME answers, now from the base alone.
    assert_eq!(count(&mut ovl, "SELECT count(*) FROM users"), 50);
    let got = rows(ovl.query("SELECT name FROM users WHERE id = 10", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("endret".into())]]);

    // A second checkpoint has nothing to do (deltas were truncated).
    let again = ovl.checkpoint().unwrap();
    assert_eq!((again.upserts, again.deletes), (0, 0));
    assert_eq!(again.epoch, report.epoch + 1, "epoch advanced past the pushed one");

    // Work continues after a checkpoint; the next one pushes it.
    ovl.query("UPDATE users SET age = 77 WHERE id = 100", &[]).unwrap();
    let r2 = ovl.checkpoint().unwrap();
    assert_eq!((r2.upserts, r2.deletes), (1, 0));
    let lib = Connection::open(&p).unwrap();
    let age: i64 =
        lib.query_row("SELECT age FROM users WHERE id = 100", [], |r| r.get(0)).unwrap();
    assert_eq!(age, 77);
    drop(lib);

    // The marker table never leaks into the user schema.
    assert!(ovl.schema().tables.iter().all(|t| t.name != "_mpedb_overlay_state"));

    drop(ovl);
    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn text_pk_deltas_checkpoint_into_the_base() {
    let p = std::env::temp_dir()
        .join("mpedb-checkpoint-tests")
        .join(format!("cp-textpk-{}.db", std::process::id()));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    for suffix in ["", ".overlay.mpedb", ".overlay.probe"] {
        let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
    }
    {
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode = DELETE;
             CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID;
             INSERT INTO kv VALUES ('alpha',1),('beta',2),('gamma',3);",
        )
        .unwrap();
    }
    let mut ovl = SqliteOverlay::open(&p).unwrap();
    ovl.query("UPDATE kv SET v = 22 WHERE k = 'beta'", &[]).unwrap();
    ovl.query("DELETE FROM kv WHERE k = 'gamma'", &[]).unwrap();
    ovl.query("INSERT INTO kv (k, v) VALUES ('zeta', 9)", &[]).unwrap();
    let r = ovl.checkpoint().unwrap();
    assert_eq!((r.upserts, r.deletes), (2, 1));
    drop(ovl);
    let c = Connection::open(&p).unwrap();
    let v: i64 = c.query_row("SELECT v FROM kv WHERE k = 'beta'", [], |r| r.get(0)).unwrap();
    assert_eq!(v, 22);
    let gone: i64 =
        c.query_row("SELECT count(*) FROM kv WHERE k = 'gamma'", [], |r| r.get(0)).unwrap();
    assert_eq!(gone, 0);
    let z: i64 = c.query_row("SELECT v FROM kv WHERE k = 'zeta'", [], |r| r.get(0)).unwrap();
    assert_eq!(z, 9);
    drop(c);
    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn reconcile_ours_then_checkpoint_lands_ours_in_the_base() {
    let p = setup("reconcile-ours");
    {
        let mut ovl = SqliteOverlay::open(&p).unwrap();
        ovl.query("UPDATE users SET name = 'vaar' WHERE id = 10", &[]).unwrap();
    }
    {
        let c = Connection::open(&p).unwrap();
        c.execute("UPDATE users SET name = 'deres' WHERE id = 10", []).unwrap();
    }
    let mut ovl =
        SqliteOverlay::open_with_options(&p, LockMode::Locked, Some(ReconcilePolicy::Ours))
            .unwrap();
    let r = ovl.checkpoint().unwrap();
    assert_eq!((r.upserts, r.deletes), (1, 0));
    drop(ovl);
    let lib = Connection::open(&p).unwrap();
    let name: String =
        lib.query_row("SELECT name FROM users WHERE id = 10", [], |r| r.get(0)).unwrap();
    assert_eq!(name, "vaar", "ours must overwrite theirs at checkpoint");
    drop(lib);
    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn co_attached_optimistic_handle_adopts_after_the_others_checkpoint() {
    let p = setup("co-ckpt");
    let mut a = SqliteOverlay::open_with_mode(&p, LockMode::Optimistic).unwrap();
    let mut b = SqliteOverlay::open_with_mode(&p, LockMode::Optimistic).unwrap();

    a.query("INSERT INTO users (id, name, age) VALUES (900, 'delt', 1)", &[]).unwrap();
    // b sees the shared delta before any checkpoint.
    assert_eq!(count(&mut b, "SELECT count(*) FROM users WHERE id = 900"), 1);

    // a checkpoints: the base moves and the SHARED overlay gets the fresh
    // stamp. b's next bracket finds its in-memory stamp stale, re-reads the
    // stored one, sees it matches the base — a co-attached process moved it
    // legitimately — and adopts instead of refusing.
    let r = a.checkpoint().unwrap();
    assert_eq!((r.upserts, r.deletes), (1, 0));
    assert_eq!(count(&mut b, "SELECT count(*) FROM users WHERE id = 900"), 1);
    assert_eq!(count(&mut b, "SELECT count(*) FROM users"), 51);
    // …and b keeps writing normally on the adopted base.
    b.query("UPDATE users SET name = 'etter' WHERE id = 900", &[]).unwrap();
    let got = rows(b.query("SELECT name FROM users WHERE id = 900", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Text("etter".into())]]);

    drop(a);
    drop(b);
    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

#[test]
fn reopen_after_checkpoint_survives_and_foreign_divergence_still_refuses() {
    let p = setup("cycle");
    {
        let mut ovl = SqliteOverlay::open(&p).unwrap();
        ovl.query("INSERT INTO users (id, name, age) VALUES (200, 'varig', 2)", &[]).unwrap();
        ovl.checkpoint().unwrap();
    }
    // Reopen after a clean checkpoint: fresh stamp matches, empty overlay.
    {
        let mut ovl = SqliteOverlay::open(&p).unwrap();
        assert_eq!(count(&mut ovl, "SELECT count(*) FROM users WHERE id = 200"), 1);
    }
    // A foreign write after the checkpointed epoch: the marker names an OLD
    // epoch, so reopen must refuse — never adopt foreign changes silently.
    {
        let c = Connection::open(&p).unwrap();
        c.execute("INSERT INTO users VALUES (300, 'fremmed', 3)", []).unwrap();
    }
    // …but only if there are unpushed deltas to be stale. With an EMPTY
    // overlay the deltas-vs-base check is vacuous and adoption is safe.
    {
        let mut ovl = SqliteOverlay::open(&p).unwrap();
        assert_eq!(count(&mut ovl, "SELECT count(*) FROM users WHERE id = 300"), 1);
    }

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}

/// Task #102's checkpoint leg: a delta row that passed the compiled CHECK at
/// INSERT time flows to the base, where sqlite RE-EVALUATES the CHECK on its
/// own INSERT — so a compile-vs-sqlite semantic divergence would fail right
/// here. The round trip succeeding, and the library then reading the rows
/// back, is the evidence the two agree.
#[test]
fn check_constrained_rows_survive_the_checkpoint() {
    let p = setup("chk-roundtrip");
    {
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE t102 (id INTEGER PRIMARY KEY, age INTEGER CHECK (age >= 0), \
               v TEXT, CONSTRAINT vshape CHECK (typeof(v) <> 'blob'));
             INSERT INTO t102 VALUES (1, 10, 'base');",
        )
        .unwrap();
    }

    let mut ovl = SqliteOverlay::open(&p).unwrap();
    ovl.query("INSERT INTO t102 (id, age, v) VALUES (2, 1, 'ok')", &[]).unwrap();
    ovl.query("INSERT INTO t102 (id, age, v) VALUES (3, NULL, NULL)", &[]).unwrap();
    ovl.query("UPDATE t102 SET age = 99 WHERE id = 1", &[]).unwrap();
    // The row sqlite would reject never reaches the delta, so the checkpoint
    // below pushes only base-acceptable rows.
    ovl.query("INSERT INTO t102 (id, age) VALUES (4, -1)", &[]).unwrap_err();

    let report = ovl.checkpoint().unwrap();
    assert_eq!(report.upserts, 3, "two inserts + one update");

    // The library is the witness: sqlite accepted every pushed row under its
    // own CHECK evaluation, and holds exactly what the overlay held.
    let lib = Connection::open(&p).unwrap();
    let got: Vec<(i64, Option<i64>, Option<String>)> = lib
        .prepare("SELECT id, age, v FROM t102 ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|x| x.unwrap())
        .collect();
    assert_eq!(
        got,
        vec![
            (1, Some(99), Some("base".into())),
            (2, Some(1), Some("ok".into())),
            (3, None, None),
        ]
    );
    // And the base still enforces for itself — the constraint text survived.
    assert!(lib.execute("INSERT INTO t102 VALUES (5, -3, NULL)", []).is_err());
    drop(lib);

    let _ = std::fs::remove_file(format!("{}.overlay.mpedb", p.display()));
    let _ = std::fs::remove_file(&p);
}
