//! N2 (0.2.8-sporet): `WriteSession::query_many` — executemany's engine
//! half. The contract under pin: plan and per-statement facts resolve ONCE;
//! any row error rolls the WHOLE batch back and surfaces the error with the
//! session usable (that is what lets the dbapi rerun the same rows on the
//! per-row road — coercion and stop-at-failing-row semantics — with no
//! double-apply); triggers fire per row exactly as on the per-row road.

use mpedb::{Config, Database, ExecResult, Value};

fn memdb() -> Database {
    let toml = "[database]\npath = \":memory:\"\nsize_mb = 64\nmax_readers = 8\n\n\
                [[table]]\nname = \"t\"\nprimary_key = [\"v\"]\n\
                [[table.column]]\nname = \"v\"\ntype = \"int64\"\n\n\
                [[table]]\nname = \"log\"\nprimary_key = [\"v\"]\n\
                [[table.column]]\nname = \"v\"\ntype = \"int64\"\n";
    Database::open_with_config(Config::from_toml_str(toml).unwrap()).unwrap()
}

fn count(db: &Database, table: &str) -> i64 {
    match db.query(&format!("SELECT count(*) FROM {table}"), &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn bulk_applies_all_rows_and_counts() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    let rows: Vec<Vec<Value>> = (0..500).map(|i| vec![Value::Int(i)]).collect();
    let n = s.query_many("INSERT INTO t VALUES ($1)", &rows).unwrap();
    assert_eq!(n, Some(500));
    s.commit().unwrap();
    assert_eq!(count(&db, "t"), 500);
    db.verify().unwrap();
}

/// A duplicate key mid-batch: the error surfaces, the batch is rolled back
/// WHOLE (rows before the failing one included), and the session stays
/// usable — the dbapi's fallback rerun depends on all three.
#[test]
fn a_row_error_rolls_the_whole_batch_back() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO t VALUES ($1)", &[Value::Int(300)]).unwrap();
    let rows: Vec<Vec<Value>> = (0..500).map(|i| vec![Value::Int(i)]).collect();
    let err = s.query_many("INSERT INTO t VALUES ($1)", &rows);
    assert!(err.is_err(), "duplicate pk at row 300 must surface");
    // Nothing from the batch stands — not even rows 0..299.
    match s.query("SELECT count(*) FROM t", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        _ => panic!(),
    }
    // The session is usable: the per-row rerun (the dbapi's road) applies
    // rows up to the failing one, stdlib's semantics.
    let mut applied = 0;
    for r in &rows {
        if s.query("INSERT INTO t VALUES ($1)", r).is_err() {
            break;
        }
        applied += 1;
    }
    assert_eq!(applied, 300, "per-row rerun stops at the duplicate");
    s.commit().unwrap();
    assert_eq!(count(&db, "t"), 301);
    db.verify().unwrap();
}

/// Triggers fire per row through the bulk road exactly as per-statement —
/// the shared exec body is the proof, this is the pin that keeps it shared.
#[test]
fn triggers_fire_per_bulk_row() {
    let db = memdb();
    db.query(
        "CREATE TRIGGER tlog AFTER INSERT ON t BEGIN \
         INSERT INTO log VALUES (NEW.v); END",
        &[],
    )
    .unwrap();
    let mut s = db.begin().unwrap();
    let rows: Vec<Vec<Value>> = (0..50).map(|i| vec![Value::Int(i)]).collect();
    let n = s.query_many("INSERT INTO t VALUES ($1)", &rows).unwrap();
    assert_eq!(n, Some(50));
    s.commit().unwrap();
    assert_eq!(count(&db, "log"), 50, "one trigger firing per bulk row");
    db.verify().unwrap();
}

/// Non-DML and routed shapes fall back (`None`) — refusals and routing keep
/// their canonical per-row behavior.
#[test]
fn control_and_ddl_texts_fall_back() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    assert_eq!(s.query_many("SAVEPOINT sp", &[vec![]]).unwrap(), None);
    assert_eq!(
        s.query_many("CREATE INDEX i ON t (v)", &[vec![]]).unwrap(),
        None
    );
    s.rollback();
}
