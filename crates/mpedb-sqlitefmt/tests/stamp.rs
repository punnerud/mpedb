//! The settled stamp against a real sqlite writer: every mutation class the
//! design names must move the tuple, and quiescence must not.

use mpedb_sqlitefmt::stamp::{settle_and_read, BaseStamp};
use rusqlite::Connection;
use std::path::PathBuf;

fn scratch_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir()
        .join("mpedb-stamp-tests")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn settled_stamp_catches_every_named_mutation_class() {
    let dir = scratch_dir("classes");
    let base = dir.join("b.db");
    let probe = dir.join("probe");
    let c = Connection::open(&base).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = DELETE; CREATE TABLE t (id INTEGER PRIMARY KEY, x TEXT); \
         INSERT INTO t VALUES (1, 'a');",
    )
    .unwrap();

    // Settled: quiescence keeps both the cheap and the strong check true.
    let s = settle_and_read(&base, &probe).unwrap();
    assert!(s.stat_matches(&base).unwrap());
    assert!(s.matches(&base).unwrap());

    // A committed write moves it (counter AND mtime).
    c.execute("INSERT INTO t VALUES (2, 'b')", []).unwrap();
    assert!(!s.matches(&base).unwrap());
    let s2 = settle_and_read(&base, &probe).unwrap();
    assert!(s2.change_counter > s.change_counter);

    // DDL moves the schema cookie.
    c.execute("CREATE TABLE u (y)", []).unwrap();
    let s3 = BaseStamp::read(&base).unwrap();
    assert!(s3.schema_cookie > s2.schema_cookie);
    assert!(!s2.matches(&base).unwrap());

    // A journal-mode flip moves bytes 18/19 (and the wal witness appears).
    drop(c);
    let c = Connection::open(&base).unwrap();
    let s4 = settle_and_read(&base, &probe).unwrap();
    c.execute_batch("PRAGMA journal_mode = WAL; INSERT INTO t VALUES (3, 'c');")
        .unwrap();
    assert!(!s4.matches(&base).unwrap());
    let s5 = BaseStamp::read(&base).unwrap();
    assert_eq!(s5.format_versions, [2, 2]);
    assert!(s5.wal.is_some());

    // WAL-mode commits move the -wal witness even when the main file's
    // counter sleeps — the exact hole the salts+size exist to cover.
    let s6 = settle_and_read(&base, &probe).unwrap();
    c.execute("INSERT INTO t VALUES (4, 'd')", []).unwrap();
    assert!(!s6.matches(&base).unwrap(), "wal commit must move the stamp");

    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settle_makes_mtime_strictly_ordered() {
    let dir = scratch_dir("settle");
    let base = dir.join("b.db");
    let probe = dir.join("probe");
    Connection::open(&base)
        .unwrap()
        .execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
        .unwrap();
    // Take a settled stamp, then IMMEDIATELY write: the new mtime must be
    // strictly greater — the same-tick invisibility the settle exists to
    // kill. Repeat to make a same-tick collision likely without the settle.
    for _ in 0..20 {
        let s = settle_and_read(&base, &probe).unwrap();
        Connection::open(&base)
            .unwrap()
            .execute("INSERT INTO t VALUES (2)", [])
            .unwrap();
        let after = std::fs::metadata(&base).unwrap().modified().unwrap();
        assert!(after > s.mtime, "post-settle write landed in the stamp's tick");
        assert!(!s.stat_matches(&base).unwrap());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
