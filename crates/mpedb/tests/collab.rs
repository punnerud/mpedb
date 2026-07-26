//! **The 1 s feedback contract and its admission control (#150).**
//!
//! The load-bearing tests here are not "a conflict is caught" — the guard's own
//! suite covers that. They are the ones that would still pass against a global
//! lock or against a no-op, and therefore have to be written to fail against
//! them:
//!
//! * `at_capacity_is_immediate` — a seat refusal must arrive at once, not as a
//!   deadline expiry a second later.
//! * `a_live_editor_is_never_evicted` — liveness must err toward alive.
//! * `a_seat_is_not_a_lock` — holding a seat does not decide who wins.

use std::time::Duration;

use mpedb::collab::{Admission, EditVerdict, Lease};
use mpedb::{Config, Database, ExecResult, Value};

fn db(tag: &str) -> (Database, std::path::PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!("mpedb-collab-{tag}-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "block"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "body"
  type = "int64"

{}
"#,
        path.display(),
        mpedb::collab::LEASE_SCHEMA
    );
    let d = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    d.query("INSERT INTO block (id, body) VALUES (1, 0)", &[]).unwrap();
    (d, path)
}

const READ: &str = "SELECT body FROM block WHERE id = $1";
const WRITE: &str = "UPDATE block SET body = $1 WHERE id = $2";

fn body(d: &Database) -> i64 {
    match d.query(READ, &[Value::Int(1)]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(v) => v,
            _ => panic!("expected int"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

// ------------------------------------------------------------ act_within

/// The ordinary path: an uncontended edit commits and says so.
#[test]
fn an_uncontended_edit_commits() {
    let (d, path) = db("commit");
    let k = [Value::Int(1)];
    let w = [Value::Int(7), Value::Int(1)];
    let snap = d.snapshot_txn();
    let v = d
        .submit_within(
            Duration::from_secs(1),
            snap,
            &[(READ, &k[..]), (WRITE, &w[..])],
            |s| {
                s.query(WRITE, &[Value::Int(7), Value::Int(1)])?;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(v, EditVerdict::Committed);
    assert_eq!(body(&d), 7);
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **First wins, and the loser is told at once — with the winner's txn.**
///
/// This is the contract in one test: two editors decide against the same
/// version of a block, both submit, exactly one lands, and the other gets a
/// definite answer immediately rather than after the deadline.
#[test]
fn a_losing_edit_is_told_immediately_and_names_the_winner() {
    let (d, path) = db("lost");
    let k = [Value::Int(1)];
    let w = [Value::Int(0), Value::Int(1)];
    let surface = [(READ, &k[..]), (WRITE, &w[..])];

    // Both editors read the same version and think.
    let snap = d.snapshot_txn();

    let first = d
        .submit_within(Duration::from_secs(1), snap, &surface, |s| {
            s.query(WRITE, &[Value::Int(11), Value::Int(1)])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(first, EditVerdict::Committed);

    let t0 = std::time::Instant::now();
    let second = d
        .submit_within(Duration::from_secs(5), snap, &surface, |s| {
            s.query(WRITE, &[Value::Int(22), Value::Int(1)])?;
            Ok(())
        })
        .unwrap();

    match second {
        EditVerdict::Lost { at_txn } => {
            assert!(
                at_txn > snap,
                "Lost named txn {at_txn}, which is not newer than the snapshot {snap} the \
                 edit was decided against — the client cannot find the winner from that"
            );
        }
        other => panic!(
            "expected Lost, got {other:?} — the second edit was decided against a version \
             that had already moved, and letting it through is the lost update itself"
        ),
    }
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "the loser waited {:?} for its answer — a refusal must not cost the deadline",
        t0.elapsed()
    );
    assert_eq!(body(&d), 11, "the first submission is the one that stands");

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **An attempt that cannot finish inside the budget is not started.** With a
/// deadline of zero there is no budget at all, so the answer must be
/// `DeadlineExpired` — and it must be counted, because that counter is what
/// admission control regulates on.
#[test]
fn a_deadline_that_cannot_be_met_expires_and_is_counted() {
    let (d, path) = db("deadline");
    let k = [Value::Int(1)];
    let w = [Value::Int(0), Value::Int(1)];
    let before = d.guard_stats().4;

    let v = d
        .act_within(Duration::ZERO, &[(READ, &k[..]), (WRITE, &w[..])], |s| {
            s.query(WRITE, &[Value::Int(3), Value::Int(1)])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(v, EditVerdict::DeadlineExpired);
    assert_eq!(
        d.guard_stats().4,
        before + 1,
        "a deadline expiry that is not counted cannot drive admission control"
    );
    // And nothing was written: an expiry is not a partial edit.
    assert_eq!(body(&d), 0);

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// `UntilDeadline` re-reads, so a retry is a fresh decision rather than a
/// replay. Increment-from-read is the case that would corrupt if it were not.
#[test]
fn until_deadline_re_reads_on_every_attempt() {
    let (d, path) = db("reread");
    let k = [Value::Int(1)];
    let w = [Value::Int(0), Value::Int(1)];
    let mut seen = Vec::new();
    let v = d
        .act_within(Duration::from_secs(1), &[(READ, &k[..]), (WRITE, &w[..])], |s| {
                let cur = match s.query(READ, &[Value::Int(1)])? {
                    ExecResult::Rows { rows, .. } => match rows[0][0] {
                        Value::Int(v) => v,
                        _ => 0,
                    },
                    _ => 0,
                };
                seen.push(cur);
            s.query(WRITE, &[Value::Int(cur + 1), Value::Int(1)])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(v, EditVerdict::Committed);
    assert_eq!(seen, vec![0], "the closure must read inside the attempt");
    assert_eq!(body(&d), 1);
    drop(d);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------- leases

/// **The cap binds, and the refusal is immediate.** A seat that arrives as a
/// deadline expiry a second later is not admission control — the whole point is
/// that a viewer learns it is a viewer at once.
#[test]
fn at_capacity_is_immediate() {
    let (d, path) = db("cap");
    let l = Lease::new(&d, Duration::from_secs(30));
    let now = 1_000_000i64;

    assert_eq!(l.acquire(1, 10, 2, now).unwrap(), Admission::Admitted);
    assert_eq!(l.acquire(1, 11, 2, now).unwrap(), Admission::Admitted);

    let t0 = std::time::Instant::now();
    let third = l.acquire(1, 12, 2, now).unwrap();
    assert!(
        matches!(third, Admission::AtCapacity { holders: 2 }),
        "expected AtCapacity, got {third:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "a capacity refusal took {:?} — it must be immediate",
        t0.elapsed()
    );

    // And a seat freed is a seat available: a cap that never releases is a leak,
    // not a policy.
    l.release(1, 11).unwrap();
    assert_eq!(l.acquire(1, 12, 2, now).unwrap(), Admission::Admitted);

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// Seats are per block. Two blocks at capacity two hold four editors between
/// them — this is the whole "split the document" lever, at its smallest.
#[test]
fn seats_are_per_block() {
    let (d, path) = db("perblock");
    let l = Lease::new(&d, Duration::from_secs(30));
    let now = 1_000_000i64;
    for block in 1..=2i64 {
        for editor in 0..2i64 {
            assert_eq!(
                l.acquire(block, editor, 2, now).unwrap(),
                Admission::Admitted,
                "block {block} editor {editor}"
            );
        }
        assert!(matches!(
            l.acquire(block, 9, 2, now).unwrap(),
            Admission::AtCapacity { .. }
        ));
    }
    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **Liveness errs toward alive.** A seat held by this very process — plainly
/// alive — must survive a capacity check, or admission control would evict real
/// editors. This is the direction #136 and #147 both turn on.
#[test]
fn a_live_editor_is_never_evicted() {
    let (d, path) = db("live");
    let l = Lease::new(&d, Duration::from_secs(30));
    let now = 1_000_000i64;
    assert_eq!(l.acquire(1, 10, 2, now).unwrap(), Admission::Admitted);

    // Another editor arriving runs the reaper. Ours must still be there.
    assert_eq!(l.acquire(1, 11, 2, now).unwrap(), Admission::Admitted);
    assert!(l.holds(1, 10, now).unwrap(), "a live editor's seat was reaped");
    assert!(matches!(
        l.acquire(1, 12, 2, now).unwrap(),
        Admission::AtCapacity { holders: 2 }
    ));

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// A seat whose heartbeat stopped is reclaimed — that is what the heartbeat is
/// FOR. Time is a parameter, so this needs no sleeping.
#[test]
fn a_silent_editor_loses_its_seat() {
    let (d, path) = db("silent");
    let l = Lease::new(&d, Duration::from_secs(30));
    let t0 = 1_000_000i64;
    assert_eq!(l.acquire(1, 10, 1, t0).unwrap(), Admission::Admitted);
    assert!(matches!(
        l.acquire(1, 11, 1, t0).unwrap(),
        Admission::AtCapacity { .. }
    ));

    // 31 s later, with no heartbeat, the seat is stale.
    let t1 = t0 + 31_000;
    assert_eq!(l.acquire(1, 11, 1, t1).unwrap(), Admission::Admitted);
    assert!(!l.holds(1, 10, t1).unwrap());

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// And a heartbeat keeps it: without this, the test above is satisfied by
/// "always expire".
#[test]
fn a_heartbeat_keeps_the_seat() {
    let (d, path) = db("beat");
    let l = Lease::new(&d, Duration::from_secs(30));
    let t0 = 1_000_000i64;
    assert_eq!(l.acquire(1, 10, 1, t0).unwrap(), Admission::Admitted);

    // Beat at +20 s, then check at +31 s: 11 s since the beat, still fresh.
    assert!(l.beat(1, 10, t0 + 20_000).unwrap());
    let t1 = t0 + 31_000;
    assert!(l.holds(1, 10, t1).unwrap(), "a beaten seat expired anyway");
    assert!(matches!(
        l.acquire(1, 11, 1, t1).unwrap(),
        Admission::AtCapacity { .. }
    ));

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **A reused pid cannot inherit someone else's seat.** The identity is
/// `(pid, start-time)`, and the guard on the heartbeat is what enforces it —
/// the same shape the task queue puts on `(claimed_by, claimed_at)`.
#[test]
fn a_reused_pid_cannot_take_over_a_seat() {
    let (d, path) = db("pidreuse");
    let l = Lease::new(&d, Duration::from_secs(30));
    let now = 1_000_000i64;
    assert_eq!(l.acquire(1, 10, 4, now).unwrap(), Admission::Admitted);

    // Same pid, different start time — a recycled pid, which is exactly what a
    // long-lived database sees.
    let mut s = d.begin().unwrap();
    s.query(
        "UPDATE edit_lease SET pid_start = pid_start + 1 WHERE block = 1 AND editor = 10",
        &[],
    )
    .unwrap();
    s.commit().unwrap();

    assert!(
        !l.beat(1, 10, now + 1).unwrap(),
        "a heartbeat matched a seat held under a different process identity"
    );
    assert!(!l.holds(1, 10, now + 1).unwrap());

    drop(d);
    let _ = std::fs::remove_file(&path);
}

/// **A seat is not a lock.** Two editors both hold seats on one block and both
/// write it; exactly one commit wins. If holding a seat granted exclusivity
/// this would deadlock or both would succeed — and the architecture would have
/// grown the one thing it does not have.
#[test]
fn a_seat_is_not_a_lock() {
    let (d, path) = db("notalock");
    let l = Lease::new(&d, Duration::from_secs(30));
    let now = 1_000_000i64;
    assert_eq!(l.acquire(1, 10, 4, now).unwrap(), Admission::Admitted);
    assert_eq!(l.acquire(1, 11, 4, now).unwrap(), Admission::Admitted);

    let k = [Value::Int(1)];
    let w = [Value::Int(0), Value::Int(1)];
    let snap = d.snapshot_txn();

    let mut a = d.begin_guarded_with(snap, &[(READ, &k[..]), (WRITE, &w[..])]).unwrap();
    a.query(WRITE, &[Value::Int(1), Value::Int(1)]).unwrap();
    a.commit().expect("the first seat holder should commit");

    let mut b = d.begin_guarded_with(snap, &[(READ, &k[..]), (WRITE, &w[..])]).unwrap();
    b.query(WRITE, &[Value::Int(2), Value::Int(1)]).unwrap();
    match b.commit() {
        Err(mpedb::Error::WriteConflict) => {}
        other => panic!(
            "expected WriteConflict, got {other:?} — a seat is admission control, not \
             mutual exclusion, so first-committer must still decide"
        ),
    }
    assert_eq!(body(&d), 1, "the first commit is the one that stands");

    drop(d);
    let _ = std::fs::remove_file(&path);
}
