//! **Resumable listening across an incarnation change (#140, #141 S3).**
//!
//! The live in-process listener could never hit this: it dies with the reboot
//! that resets the counters. The client this is for is the one that STORES its
//! position and comes back — the reconnecting shape — and for it a bare
//! generation is a trap. It returns holding 900, meets a counter reset to 3,
//! and every `gen > seen` test says "unchanged" for the next 900 commits.
//!
//! So the tests below are about the FAILURE direction, as everything in this
//! feature is: a position from an epoch we do not recognise must read as
//! "everything may have moved", never as "nothing did".

use mpedb::{ChangeCursor, Config, Database, Value};

fn db(tag: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-curs-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
"#,
        path.display()
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (d, path)
}

const NOW: std::time::Duration = std::time::Duration::from_millis(50);

/// A listener starts at the present: work committed before it existed is not
/// re-reported, or every fresh listener would fire once for free.
#[test]
fn a_new_listener_starts_at_the_present() {
    let (d, path) = db("present");
    d.query("INSERT INTO t (id, v) VALUES (1, 1)", &[]).unwrap();
    let mut l = d.listen(&[0]);
    assert!(l.wait(NOW).is_empty(), "a fresh listener re-reported a change that predates it");
    d.query("INSERT INTO t (id, v) VALUES (2, 2)", &[]).unwrap();
    assert_eq!(l.wait(NOW), vec![0], "the listener missed a change committed after it started");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// Reporting a change advances the cursor past it — otherwise every later wait
/// re-reports the same change and the listener never sleeps again.
#[test]
fn a_reported_change_is_not_reported_twice() {
    let (d, path) = db("once");
    let mut l = d.listen(&[0]);
    d.query("INSERT INTO t (id, v) VALUES (1, 1)", &[]).unwrap();
    assert_eq!(l.wait(NOW), vec![0]);
    assert!(l.wait(NOW).is_empty(), "the same change was reported twice — the cursor did not advance");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A cursor round-trips within one epoch: store it, resume from it, and the
/// listener is exactly where it was.
#[test]
fn a_cursor_round_trips_within_an_epoch() {
    let (d, path) = db("rt");
    let mut l = d.listen(&[0]);
    d.query("INSERT INTO t (id, v) VALUES (1, 1)", &[]).unwrap();
    assert_eq!(l.wait(NOW), vec![0]);
    let saved = l.cursor();

    d.query("INSERT INTO t (id, v) VALUES (2, 2)", &[]).unwrap();
    let mut l2 = d.listen(&[0]);
    l2.resume(&saved);
    assert_eq!(
        l2.wait(NOW),
        vec![0],
        "resuming from a stored cursor missed the change committed after it"
    );
    assert!(l2.wait(NOW).is_empty());
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **The one that matters.** A cursor whose epoch we do not recognise — the
/// shape a reboot, a reformat, or a delete-and-recreate produces — must make
/// everything read as unseen. A stale HIGH generation presented against a
/// freshly reset counter is the exact input that used to answer "unchanged".
#[test]
fn a_cursor_from_a_foreign_epoch_reports_everything() {
    let (d, path) = db("epoch");
    // Commit a few times so the real generation is small but nonzero.
    for i in 1..=3i64 {
        d.query("INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(i)]).unwrap();
    }
    let mut l = d.listen(&[0]);
    assert!(l.wait(NOW).is_empty(), "listener should start caught up");

    // A position from a previous incarnation: an epoch that is not ours, and a
    // generation far beyond anything this file has reached.
    let stale = ChangeCursor { epoch: d.notify_epoch() ^ 0xDEAD_BEEF, seen: vec![900] };
    l.resume(&stale);
    assert_eq!(
        l.wait(NOW),
        vec![0],
        "a cursor from a foreign epoch was trusted — with seen=900 against a \
         reset counter, this listener would sleep through the next 900 commits"
    );
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The same guard, for a cursor that is the wrong SHAPE: a client that stored
/// a position while watching two tables and came back watching one cannot
/// have its numbers lined up by position.
#[test]
fn a_cursor_of_the_wrong_width_reports_everything() {
    let (d, path) = db("width");
    // The table must have moved at all, or "everything unseen" and "nothing
    // ever happened" are the same answer and the test proves nothing.
    d.query("INSERT INTO t (id, v) VALUES (1, 1)", &[]).unwrap();
    let mut l = d.listen(&[0]);
    assert!(l.wait(NOW).is_empty(), "listener should start caught up");
    let wrong = ChangeCursor { epoch: d.notify_epoch(), seen: vec![900, 900] };
    l.resume(&wrong);
    assert_eq!(l.wait(NOW), vec![0], "a cursor of the wrong width was applied by position");
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// The epoch must be stable while the file is: a listener that re-reads it
/// between commits must not see it move, or every cursor would be foreign.
#[test]
fn the_epoch_is_stable_across_commits() {
    let (d, path) = db("stable");
    let first = d.notify_epoch();
    assert_ne!(first, 0, "no epoch was ever stamped");
    for i in 1..=5i64 {
        d.query("INSERT INTO t (id, v) VALUES ($1, 1)", &[Value::Int(i)]).unwrap();
        assert_eq!(d.notify_epoch(), first, "the epoch moved on an ordinary commit");
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A freshly formatted file gets a DIFFERENT epoch than the one before it at
/// the same path — the delete-and-recreate case a boot id alone cannot see.
#[test]
fn recreating_the_file_changes_the_epoch() {
    let (d, path) = db("recreate");
    let before = d.notify_epoch();
    drop(d);
    let _ = std::fs::remove_file(&path);

    let (d2, path2) = db("recreate");
    let after = d2.notify_epoch();
    assert_ne!(
        before, after,
        "a recreated file reused the previous incarnation's epoch — a stored \
         cursor would be trusted against counters that restarted at 0"
    );
    drop(d2);
    let _ = std::fs::remove_file(&path2);
}
