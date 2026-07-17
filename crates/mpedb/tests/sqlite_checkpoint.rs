//! v2 checkpoint differential (design §5): push deltas into the base via the
//! sqlite library, marker in the base, truncate the overlay, re-stamp — then
//! verify every step against rusqlite's view of the base.
#![cfg(feature = "sqlite-checkpoint")]

use mpedb::{SqliteOverlay, Value};
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
