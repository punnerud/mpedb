//! Statement atomicity ACROSS a ring batch (design/DESIGN.md §5.3).
//!
//! §5.3 promises "one failing intent errors alone while the batch commits
//! around it". A failing intent must therefore leave NOTHING behind — the same
//! all-or-nothing a failing statement gets on the direct path, where the whole
//! transaction is aborted.
//!
//! The hard case is a member of the batch that fails *after applying part of
//! its effects* when an EARLIER member already COWed the pages it writes: those
//! pages are txn-dirty, so the mutation happens IN PLACE and the cheap
//! `WriteTxn::rollback_to` (root pointers + accounting only) cannot undo the
//! bytes — restoring the root restores a pointer to the same, already-mutated
//! page. Before the §5.3 round-restart rule the batch then committed the doomed
//! rows while their caller got the error.
//!
//! The batch is built deterministically: one thread holds the writer lock in an
//! interactive session while every worker enqueues, so all intents are READY
//! when the lock is released and land in ONE drained batch. The key-locality
//! drain sort is used to put the doomed member LAST: single-row point inserts
//! carry a `Point` footprint on a low key, multi-row inserts degrade to `Full`
//! (which sorts last within the table) and a range UPDATE sorts on its lo bound
//! — so in every scenario below the successful members dirty the leaf first.

use mpedb::{params, Config, Database, Error, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Pre-inserted row every doomed INSERT collides with on its LAST value.
const POISON: i64 = -1;

fn test_config(name: &str, extra_col: &str) -> (Config, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-ring-atomicity-{name}-{}.mpedb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 64

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
{extra_col}
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
}

fn rows_of(db: &Database, sql: &str) -> Vec<(i64, i64)> {
    match db.query(sql, &params![]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            let mut v: Vec<(i64, i64)> = rows
                .into_iter()
                .map(|r| match (&r[0], &r[1]) {
                    (Value::Int(a), Value::Int(b)) => (*a, *b),
                    other => panic!("unexpected row {other:?}"),
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// A failing MULTI-ROW INSERT drained after a successful intent that dirtied
/// the same leaf. Covers both leader arms: the foreign-intent loop (workers)
/// and the leader's OWN statement (the main thread, which takes the
/// uncontended fast path while foreign intents are still queued).
#[test]
fn a_failing_multi_row_intent_leaves_nothing_behind() {
    const GOOD: i64 = 6;
    const BAD: i64 = 6;
    const OWN: i64 = 40;

    let (mut cfg, path) = test_config("multirow", "");
    // durability=commit is what engages the intent ring at all.
    cfg.options.durability = mpedb::Durability::Commit;
    let db = Arc::new(Database::open_with_config(cfg.clone()).unwrap());

    let good = db.prepare("INSERT INTO t (id, v) VALUES ($1, $2)").unwrap();
    let bad = db
        .prepare("INSERT INTO t (id, v) VALUES ($1, $2), ($3, $4), ($5, $6)")
        .unwrap();
    db.execute(&good, &params![POISON, 0]).unwrap();

    // Hold the writer lock so every worker below is forced to ENQUEUE.
    let holder = db.begin().unwrap();

    let mut handles = Vec::new();
    for k in 0..GOOD {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            match db.execute(&good, &params![k * 8, k]) {
                Ok(ExecResult::Affected(1)) => {}
                other => panic!("good insert {k}: {other:?}"),
            }
        }));
    }
    for k in 0..BAD {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            // rows 1 and 2 are brand new and adjacent to the good keys (same
            // leaf); row 3 collides with POISON and must fail the statement.
            let r = db.execute(&bad, &params![k * 8 + 1, k, k * 8 + 2, k, POISON, k]);
            match r {
                Err(Error::PrimaryKeyViolation { .. }) => {}
                other => panic!("bad insert {k} must fail with a PK violation: {other:?}"),
            }
        }));
    }
    // Let every worker publish its intent, then release the lock: one leader
    // drains all GOOD+BAD intents in a single batch and commits once.
    std::thread::sleep(std::time::Duration::from_millis(400));
    holder.rollback();
    // …and keep firing doomed statements from THIS thread while the drain runs,
    // so some of them acquire the lock uncontended and become the leader's OWN
    // statement with foreign intents already applied ahead of them.
    for k in 0..OWN {
        let r = db.execute(&bad, &params![k * 8 + 3, k, k * 8 + 4, k, POISON, k]);
        match r {
            Err(Error::PrimaryKeyViolation { .. }) => {}
            other => panic!("own bad insert {k} must fail with a PK violation: {other:?}"),
        }
    }
    for h in handles {
        h.join().unwrap();
    }

    db.verify().unwrap();
    let mut want: Vec<(i64, i64)> = (0..GOOD).map(|k| (k * 8, k)).collect();
    want.push((POISON, 0));
    want.sort();
    assert_eq!(
        rows_of(&db, "SELECT id, v FROM t"),
        want,
        "a failing multi-row statement must leave NO rows behind (§5.3)"
    );

    // …and the committed file agrees (a fresh handle re-reads the meta).
    let db2 = Database::open_with_config(cfg).unwrap();
    assert_eq!(
        rows_of(&db2, "SELECT id, v FROM t"),
        want,
        "committed state must match too"
    );
    drop(db2);
    let _ = std::fs::remove_file(&path);
}

/// The same hazard from the UPDATE side: a range UPDATE that rewrites several
/// rows and trips a UNIQUE violation on a later one, drained after successful
/// inserts that already dirtied the leaf.
#[test]
fn a_failing_multi_row_update_leaves_nothing_behind() {
    const GOOD: i64 = 6;
    const BAD: i64 = 4;

    let (mut cfg, path) = test_config("multirow-update", "  unique = true\n");
    cfg.options.durability = mpedb::Durability::Commit;
    let db = Arc::new(Database::open_with_config(cfg.clone()).unwrap());

    let ins = db.prepare("INSERT INTO t (id, v) VALUES ($1, $2)").unwrap();
    // Rows the doomed UPDATE walks. High ids so the drain sort puts the UPDATE
    // (which sorts on its lo bound) AFTER the good point inserts below.
    for i in 0..3 {
        db.execute(&ins, &params![900 + i, 9000 + i]).unwrap();
    }
    let upd = db.prepare("UPDATE t SET v = $1 WHERE id >= $2").unwrap();

    let holder = db.begin().unwrap();
    let mut handles = Vec::new();
    for k in 0..GOOD {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            match db.execute(&ins, &params![k + 1, 1000 + k]) {
                Ok(ExecResult::Affected(1)) => {}
                other => panic!("good insert {k}: {other:?}"),
            }
        }));
    }
    for k in 0..BAD {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            // id 900 takes v=777; id 901 then collides on the UNIQUE index.
            match db.execute(&upd, &params![777 + k, 900]) {
                Err(Error::UniqueViolation { .. }) => {}
                other => panic!("bad update {k} must fail on UNIQUE: {other:?}"),
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    holder.rollback();
    for h in handles {
        h.join().unwrap();
    }

    db.verify().unwrap();
    let mut want: Vec<(i64, i64)> = (0..GOOD).map(|k| (k + 1, 1000 + k)).collect();
    want.extend((0..3).map(|i| (900 + i, 9000 + i)));
    want.sort();
    assert_eq!(
        rows_of(&db, "SELECT id, v FROM t"),
        want,
        "a failing multi-row UPDATE must leave NO row rewritten (§5.3)"
    );
    let _ = std::fs::remove_file(&path);
}
