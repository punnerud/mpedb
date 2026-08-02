//! The private `:memory:` backing's in-place adoption vs savepoints (found by
//! the first real Django consumer's fourth field report, mechanism pinned by
//! adversarial review): `adopt_inplace` mutates a COMMITTED page under its
//! ORIGINAL id, a third page state the savepoint machinery's COW dichotomy
//! never covered — after a PRISTINE savepoint, `ROLLBACK TO` restored the
//! root (a no-op, the id never moved) and had no image to restore, so the
//! writes silently survived. File backing was always immune (pure COW).
//! Every case here failed before the fix and is byte-for-byte sqlite's
//! answer after it.

use mpedb::{Config, Database, ExecResult, Value};

fn memdb() -> Database {
    let toml = "[database]\npath = \":memory:\"\nsize_mb = 64\nmax_readers = 8\n\n\
                [[table]]\nname = \"t\"\nprimary_key = [\"v\"]\n\
                [[table.column]]\nname = \"v\"\ntype = \"int64\"\n";
    Database::open_with_config(Config::from_toml_str(toml).unwrap()).unwrap()
}

fn count30(db: &Database) -> i64 {
    match db.query("SELECT count(*) FROM t WHERE v >= 30", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            _ => panic!(),
        },
        _ => panic!(),
    }
}

/// The exact repro: committed history, then a savepoint as the FIRST
/// statement of the next transaction.
#[test]
fn a_pristine_savepoint_after_history_rolls_back() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES (1)", &[]).unwrap();
    s.commit().unwrap();

    let mut s = db.begin().unwrap();
    s.query("SAVEPOINT rep", &[]).unwrap();
    s.query("INSERT INTO t VALUES (30)", &[]).unwrap();
    s.query("ROLLBACK TO SAVEPOINT rep", &[]).unwrap();
    // Repeated rollback to the same name must also hold (the snapshot is
    // cloned, not consumed).
    s.query("INSERT INTO t VALUES (31)", &[]).unwrap();
    s.query("ROLLBACK TO SAVEPOINT rep", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(count30(&db), 0, "adopted-page writes must not survive ROLLBACK TO");
}

/// The review's second-order finding: after a ROLLBACK TO, a re-adoption of
/// the same page must snapshot COMMITTED bytes — otherwise a full ROLLBACK
/// of the transaction restores the leaked state instead of the committed one.
#[test]
fn abort_after_rollback_to_restores_committed_state() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES (1)", &[]).unwrap();
    s.commit().unwrap();

    let mut s = db.begin().unwrap();
    s.query("SAVEPOINT sp", &[]).unwrap();
    s.query("INSERT INTO t VALUES (30)", &[]).unwrap();
    s.query("ROLLBACK TO SAVEPOINT sp", &[]).unwrap();
    // Re-adopt the same page with a NEW write, then abort the whole txn.
    s.query("INSERT INTO t VALUES (31)", &[]).unwrap();
    s.rollback();
    assert_eq!(count30(&db), 0, "full rollback must restore the committed state");
    // And the committed row is intact.
    match db.query("SELECT count(*) FROM t WHERE v = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        _ => panic!(),
    }
}

/// A savepoint taken AFTER a write (non-pristine) stays exact too — the
/// adopted page travels in the image channel; this held before the fix and
/// must keep holding after it.
#[test]
fn a_non_pristine_savepoint_still_holds() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES (1)", &[]).unwrap();
    s.commit().unwrap();

    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES (29)", &[]).unwrap();
    s.query("SAVEPOINT sp", &[]).unwrap();
    s.query("INSERT INTO t VALUES (30)", &[]).unwrap();
    s.query("ROLLBACK TO SAVEPOINT sp", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(count30(&db), 0);
    match db.query("SELECT count(*) FROM t WHERE v = 29", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        _ => panic!(),
    }
}
