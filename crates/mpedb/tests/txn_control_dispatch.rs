//! N2 fase 1 steg 4: transaction/savepoint control inside a session takes
//! the `parse_txn_control` fast road — the REAL grammar behind a byte gate,
//! dispatching to the same methods `run_from`'s compiled-plan arms use.
//! These pins hold the syntactic forms the grammar accepts (they must all
//! keep WORKING, not merely parse) and the refusals whose canonical messages
//! the fail-open design preserves.

use mpedb::{Config, Database, ExecResult, Value};

fn memdb() -> Database {
    let toml = "[database]\npath = \":memory:\"\nsize_mb = 64\nmax_readers = 8\n\n\
                [[table]]\nname = \"t\"\nprimary_key = [\"v\"]\n\
                [[table.column]]\nname = \"v\"\ntype = \"int64\"\n";
    Database::open_with_config(Config::from_toml_str(toml).unwrap()).unwrap()
}

fn count(db: &Database) -> i64 {
    match db.query("SELECT count(*) FROM t", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            _ => panic!(),
        },
        _ => panic!(),
    }
}

/// Every syntactic form sqlite accepts for the three savepoint verbs, driven
/// through a real cycle each: quoted / bare / string-literal names, the long
/// keyword forms, mixed case, trailing semicolons. A form the fast road
/// mis-parsed would either error here or leak the rolled-back row.
#[test]
fn all_savepoint_forms_cycle_correctly() {
    let db = memdb();
    let forms: &[(&str, &str, &str)] = &[
        ("SAVEPOINT sp1", "ROLLBACK TO SAVEPOINT sp1", "RELEASE SAVEPOINT sp1"),
        ("SAVEPOINT \"sp two\"", "ROLLBACK TO \"sp two\"", "RELEASE \"sp two\""),
        ("SAVEPOINT 'sp3'", "ROLLBACK TRANSACTION TO SAVEPOINT 'sp3'", "RELEASE SAVEPOINT 'sp3'"),
        ("savepoint SP4", "rollback to savepoint sp4", "release savepoint Sp4"),
        ("SAVEPOINT sp5;", "ROLLBACK TO sp5;", "RELEASE sp5;"),
        ("  SAVEPOINT sp6", "  ROLLBACK TO SAVEPOINT sp6", "  RELEASE SAVEPOINT sp6"),
    ];
    let mut s = db.begin().unwrap();
    for (i, (open, back, rel)) in forms.iter().enumerate() {
        s.query(open, &[]).unwrap();
        s.query("INSERT INTO t VALUES ($1)", &[Value::Int(1000 + i as i64)])
            .unwrap();
        s.query(back, &[]).unwrap();
        s.query(rel, &[]).unwrap();
    }
    s.commit().unwrap();
    assert_eq!(count(&db), 0, "every rolled-back row must be gone");
}

/// The verbs that must refuse inside a session, with the one canonical
/// message both roads share.
#[test]
fn txn_verbs_refuse_in_session_with_one_message() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    for verb in ["BEGIN", "COMMIT", "ROLLBACK", "begin;", "commit"] {
        let e = s.query(verb, &[]).unwrap_err();
        let msg = format!("{e}");
        assert!(
            msg.contains("the session already is a transaction"),
            "{verb}: {msg}"
        );
    }
    s.rollback();
}

/// Fail-open: broken control statements fall through to the compile road and
/// keep its canonical refusals — a missing name, a missing savepoint, and a
/// multi-statement text must all still error (never silently no-op).
#[test]
fn broken_forms_keep_their_canonical_errors() {
    let db = memdb();
    let mut s = db.begin().unwrap();
    assert!(s.query("SAVEPOINT", &[]).is_err(), "nameless SAVEPOINT must error");
    assert!(
        s.query("RELEASE SAVEPOINT nosuch", &[]).is_err(),
        "RELEASE of an unknown savepoint must error"
    );
    assert!(
        s.query("ROLLBACK TO SAVEPOINT nosuch", &[]).is_err(),
        "ROLLBACK TO an unknown savepoint must error"
    );
    assert!(
        s.query("SAVEPOINT a; SAVEPOINT b", &[]).is_err(),
        "multi-statement text must keep erroring"
    );
    // The session survives refusals that applied nothing.
    s.query("INSERT INTO t VALUES ($1)", &[Value::Int(1)]).unwrap();
    s.commit().unwrap();
    assert_eq!(count(&db), 1);
}

/// A table named like a control verb stays reachable — the byte gate matches
/// the first WORD only and the real parser decides; `SELECT … FROM release`
/// was never at risk, but pin the closest shapes: names in DML positions.
#[test]
fn verb_lookalike_dml_flows_to_the_compile_road() {
    let toml = "[database]\npath = \":memory:\"\nsize_mb = 64\nmax_readers = 8\n\n\
                [[table]]\nname = \"release\"\nprimary_key = [\"v\"]\n\
                [[table.column]]\nname = \"v\"\ntype = \"int64\"\n";
    let db = Database::open_with_config(Config::from_toml_str(toml).unwrap()).unwrap();
    let mut s = db.begin().unwrap();
    s.query("INSERT INTO \"release\" VALUES ($1)", &[Value::Int(1)]).unwrap();
    match s.query("SELECT count(*) FROM \"release\"", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows[0][0], Value::Int(1)),
        _ => panic!(),
    }
    s.commit().unwrap();
}
