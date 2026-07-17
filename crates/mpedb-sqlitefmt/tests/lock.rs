//! The lock primitives against a REAL sqlite writer: our SHARED must read as
//! a normal busy database to them, their transactions must read as
//! writer-activity to us, and the hot-journal classifier must separate a
//! corpse from a live transaction and from a PERSIST leftover.

use mpedb_sqlitefmt::lock::{hot_journal, writer_active, BracketOutcome, ReadBracket, SharedLock};
use mpedb_sqlitefmt::stamp::BaseStamp;
use rusqlite::Connection;
use std::path::PathBuf;

fn mkdb(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("mpedb-lock-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(format!("{name}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(p.with_extension("db-journal"));
    let c = Connection::open(&p).unwrap();
    c.execute_batch("PRAGMA journal_mode = DELETE; CREATE TABLE t (x); INSERT INTO t VALUES (1);")
        .unwrap();
    p
}

#[test]
fn our_shared_is_their_normal_busy() {
    let p = mkdb("busy");
    let lock = SharedLock::acquire(&p).unwrap().expect("quiescent base");
    let c = Connection::open(&p).unwrap();
    c.busy_timeout(std::time::Duration::from_millis(80)).unwrap();
    // Reads coexist with SHARED…
    let n: i64 = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
    // …writes get their perfectly normal SQLITE_BUSY.
    let err = c.execute("INSERT INTO t VALUES (2)", []).unwrap_err();
    assert!(format!("{err}").contains("locked") || format!("{err}").contains("busy"), "{err}");
    // Release → the same write succeeds.
    drop(lock);
    c.execute("INSERT INTO t VALUES (2)", []).unwrap();
    let _ = std::fs::remove_file(&p);
}

#[test]
fn writer_activity_is_visible_and_brackets_behave() {
    let p = mkdb("probe");
    assert!(!writer_active(&p).unwrap());

    let c = Connection::open(&p).unwrap();
    c.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES (2);").unwrap();
    // RESERVED held → visible…
    assert!(writer_active(&p).unwrap());
    // …and a bracket STILL opens: a RESERVED-only writer has not touched
    // the file (mutation needs EXCLUSIVE, which our SHARED now excludes).
    match ReadBracket::open(&p).unwrap() {
        BracketOutcome::Held(b) => {
            let s = BaseStamp::read(&p).unwrap();
            assert!(b.stamp_matches(&s).unwrap());
        }
        other => panic!("expected Held during RESERVED, got {}", name(&other)),
    }
    c.execute_batch("COMMIT;").unwrap();
    assert!(!writer_active(&p).unwrap());

    // After the commit the stamp from before the commit no longer matches.
    let pre = BaseStamp::read(&p).unwrap();
    c.execute("INSERT INTO t VALUES (3)", []).unwrap();
    match ReadBracket::open(&p).unwrap() {
        BracketOutcome::Held(b) => assert!(!b.stamp_matches(&pre).unwrap()),
        other => panic!("expected Held, got {}", name(&other)),
    }
    let _ = std::fs::remove_file(&p);
}

#[test]
fn hot_journal_separates_corpse_from_live_and_persist() {
    let p = mkdb("hot");
    let jpath = {
        let mut s = p.as_os_str().to_owned();
        s.push("-journal");
        PathBuf::from(s)
    };
    // No journal: cold.
    assert!(!hot_journal(&p).unwrap());
    // A corpse: valid magic, no live writer → HOT.
    let magic = [0xd9u8, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];
    let mut body = magic.to_vec();
    body.extend_from_slice(&[0u8; 504]);
    std::fs::write(&jpath, &body).unwrap();
    assert!(hot_journal(&p).unwrap());
    // The bracket refuses it by name.
    assert!(matches!(ReadBracket::open(&p).unwrap(), BracketOutcome::HotJournal));
    // PERSIST leftover: zeroed header → cold.
    std::fs::write(&jpath, [0u8; 512]).unwrap();
    assert!(!hot_journal(&p).unwrap());
    // Valid magic BUT a live writer holds RESERVED → a transaction, not a
    // corpse → cold.
    std::fs::write(&jpath, &body).unwrap();
    let c = Connection::open(&p).unwrap();
    c.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES (9);").unwrap();
    assert!(!hot_journal(&p).unwrap());
    c.execute_batch("ROLLBACK;").unwrap();
    let _ = std::fs::remove_file(&jpath);
    let _ = std::fs::remove_file(&p);
}

fn name(o: &BracketOutcome) -> &'static str {
    match o {
        BracketOutcome::Busy => "Busy",
        BracketOutcome::HotJournal => "HotJournal",
        BracketOutcome::Held(_) => "Held",
    }
}
