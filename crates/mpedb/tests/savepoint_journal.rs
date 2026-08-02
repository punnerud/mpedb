//! N2 fase 4: `savepoint_full` captures pre-images LAZILY (a journal layer
//! per open savepoint, filled at first mutation) instead of copying every
//! dirty page eagerly. These pins hold the shapes the layer machinery must
//! keep exact: nested savepoints, RELEASE folding into the parent, repeat
//! rollbacks, DDL inside a named savepoint (the anonymous-layer fold), and
//! the Django fixture shape (big dirty set, then per-test cycles) judged by
//! the page-accounting verifier.

use mpedb::{Config, Database, ExecResult, Value};

fn memdb() -> Database {
    let toml = "[database]\npath = \":memory:\"\nsize_mb = 128\nmax_readers = 8\n\n\
                [[table]]\nname = \"t\"\nprimary_key = [\"v\"]\n\
                [[table.column]]\nname = \"v\"\ntype = \"int64\"\n\n\
                [[table]]\nname = \"d\"\nprimary_key = [\"id\"]\n\
                [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
                [[table.column]]\nname = \"txt\"\ntype = \"text\"\n";
    Database::open_with_config(Config::from_toml_str(toml).unwrap()).unwrap()
}

fn count_ge(db: &Database, lo: i64) -> i64 {
    match db.query("SELECT count(*) FROM t WHERE v >= $1", &[Value::Int(lo)]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            _ => panic!(),
        },
        _ => panic!(),
    }
}

/// RELEASE folds the inner layer into the outer: a later ROLLBACK TO the
/// OUTER name must undo writes made inside the (released) inner scope.
#[test]
fn release_folds_the_inner_scope_into_the_outer() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES (1)", &[]).unwrap();
    s.query("SAVEPOINT a", &[]).unwrap();
    s.query("INSERT INTO t VALUES (100)", &[]).unwrap();
    s.query("SAVEPOINT b", &[]).unwrap();
    s.query("INSERT INTO t VALUES (101)", &[]).unwrap();
    s.query("RELEASE SAVEPOINT b", &[]).unwrap();
    s.query("ROLLBACK TO SAVEPOINT a", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(count_ge(&db, 100), 0, "released scope's writes must fall with the outer rollback");
    assert_eq!(count_ge(&db, 1), 1, "the pre-savepoint write stays");
    db.verify().unwrap();
}

/// Nested rollbacks, inner first, then outer — each undoes exactly its scope.
#[test]
fn nested_rollbacks_peel_exactly() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES (1)", &[]).unwrap();
    s.query("SAVEPOINT a", &[]).unwrap();
    s.query("INSERT INTO t VALUES (100)", &[]).unwrap();
    s.query("SAVEPOINT b", &[]).unwrap();
    s.query("INSERT INTO t VALUES (101)", &[]).unwrap();
    s.query("ROLLBACK TO SAVEPOINT b", &[]).unwrap();
    assert_eq!(
        match s.query("SELECT count(*) FROM t WHERE v >= 101", &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows[0][0].clone(),
            _ => panic!(),
        },
        Value::Int(0)
    );
    s.query("ROLLBACK TO SAVEPOINT a", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(count_ge(&db, 100), 0);
    assert_eq!(count_ge(&db, 1), 1);
    db.verify().unwrap();
}

/// The same name rolled back to repeatedly, with fresh writes between — the
/// target's journal layer survives each rollback emptied and refills.
#[test]
fn repeat_rollback_refills_the_layer() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    for i in 0..50 {
        s.query("INSERT INTO t VALUES ($1)", &[Value::Int(i)]).unwrap();
    }
    s.query("SAVEPOINT sp", &[]).unwrap();
    for round in 0..20 {
        s.query("INSERT INTO t VALUES ($1)", &[Value::Int(1000 + round)]).unwrap();
        s.query("ROLLBACK TO SAVEPOINT sp", &[]).unwrap();
    }
    s.commit().unwrap();
    assert_eq!(count_ge(&db, 1000), 0);
    assert_eq!(count_ge(&db, 0), 50);
    db.verify().unwrap();
}

/// DDL inside a NAMED savepoint: the DDL's anonymous engine savepoint folds
/// its layer into the named one on success, so ROLLBACK TO the name undoes
/// the DDL's catalog-page mutations too (and the session's schema view is
/// rebuilt by the rollback).
#[test]
fn ddl_inside_a_named_savepoint_rolls_back_with_it() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES (1)", &[]).unwrap();
    s.query("SAVEPOINT s1", &[]).unwrap();
    s.query("ALTER TABLE t ADD COLUMN c INTEGER", &[]).unwrap();
    let wide = s.query("SELECT * FROM t", &[]).unwrap();
    match wide {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0].len(), 2),
        _ => panic!(),
    }
    s.query("ROLLBACK TO SAVEPOINT s1", &[]).unwrap();
    let narrow = s.query("SELECT * FROM t", &[]).unwrap();
    match narrow {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0].len(), 1, "ADD COLUMN must be undone"),
        _ => panic!(),
    }
    s.query("INSERT INTO t VALUES (2)", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(count_ge(&db, 1), 2);
    db.verify().unwrap();
}

/// Django's fixture shape at engine level: a big dirty set in the outer
/// transaction, then a savepoint cycle per "test". Correctness is the row
/// count; soundness is the page-accounting verifier.
#[test]
fn big_fixture_then_savepoint_cycles_stays_sound() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    for i in 0..2000 {
        s.query("INSERT INTO t VALUES ($1)", &[Value::Int(i)]).unwrap();
    }
    for k in 0..200 {
        s.query("SAVEPOINT tsp", &[]).unwrap();
        s.query("INSERT INTO t VALUES ($1)", &[Value::Int(100_000 + k)]).unwrap();
        s.query("ROLLBACK TO SAVEPOINT tsp", &[]).unwrap();
        s.query("RELEASE SAVEPOINT tsp", &[]).unwrap();
    }
    s.commit().unwrap();
    assert_eq!(count_ge(&db, 100_000), 0);
    assert_eq!(count_ge(&db, 0), 2000);
    db.verify().unwrap();
}

/// The adversarial review's find, all five lenses converged on it (fixed
/// before the journal ever merged): overflow pages freed by DELETE leave the
/// dirty set WITHOUT a `page_mut` (btree walks them read-only and calls
/// `free`), a later INSERT recycles them through `alloc()` whose zero-fill
/// bypasses the journal, and `ROLLBACK TO` restored a tree whose node bytes
/// were destroyed — durable corruption that `verify()` (accounting-only)
/// could not see. The fix journals the pre-image at `free` time. This is the
/// review's exact repro, kept verbatim as the pin.
#[test]
fn freed_then_recycled_pages_survive_rollback_to() {
    let db = memdb();
    let big = "X".repeat(3000); // overflow-chain sized, below the extent threshold
    let mut s = db.begin().unwrap();
    for i in 0..10i64 {
        s.query("INSERT INTO d VALUES ($1, $2)", &[Value::Int(i), Value::Text(big.clone())])
            .unwrap();
    }
    s.query("SAVEPOINT s1", &[]).unwrap();
    for i in 0..10i64 {
        s.query("DELETE FROM d WHERE id = $1", &[Value::Int(i)]).unwrap();
    }
    let mid = "y".repeat(500);
    for i in 1000..1500i64 {
        s.query("INSERT INTO d VALUES ($1, $2)", &[Value::Int(i), Value::Text(mid.clone())])
            .unwrap();
    }
    s.query("ROLLBACK TO SAVEPOINT s1", &[]).unwrap();
    match s.query("SELECT id, txt FROM d", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 10, "the ten pre-savepoint rows must be back");
            for r in &rows {
                assert_eq!(r[1], Value::Text(big.clone()), "row {:?} lost its bytes", r[0]);
            }
        }
        _ => panic!(),
    }
    s.commit().unwrap();
    match db.query("SELECT count(*) FROM d WHERE id < 100", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(10)),
        _ => panic!(),
    }
    db.verify().unwrap();
}

/// Abort with open savepoints and journaled layers: the whole-transaction
/// rollback restores committed state through the COW/in-place machinery —
/// the layers must neither block it nor leak into the next transaction.
#[test]
fn abort_with_open_savepoints_is_clean() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES (1)", &[]).unwrap();
    s.commit().unwrap();

    let mut s = db.begin().unwrap();
    s.query("SAVEPOINT a", &[]).unwrap();
    s.query("INSERT INTO t VALUES (100)", &[]).unwrap();
    s.query("SAVEPOINT b", &[]).unwrap();
    s.query("INSERT INTO t VALUES (101)", &[]).unwrap();
    s.rollback();
    assert_eq!(count_ge(&db, 100), 0);
    assert_eq!(count_ge(&db, 1), 1);

    let mut s = db.begin().unwrap();
    s.query("SAVEPOINT fresh", &[]).unwrap();
    s.query("INSERT INTO t VALUES (200)", &[]).unwrap();
    s.query("ROLLBACK TO SAVEPOINT fresh", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(count_ge(&db, 200), 0);
    db.verify().unwrap();
}
