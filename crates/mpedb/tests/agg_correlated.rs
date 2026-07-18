//! #73 §1 — aggregate over a correlated filter. `SELECT count(*) FROM a WHERE
//! EXISTS (SELECT 1 FROM b WHERE b.k = a.g)` and its GROUP BY / NOT EXISTS /
//! scalar-correlated forms were refused at plan time before this change; they
//! now run the correlated WHERE residual per outer row (via the shared
//! `correlated_survivors`) BEFORE accumulation, so aggregation still consumes
//! only the full `(WHERE ∧ policy)` set. Every expected value below was
//! cross-checked against the sqlite3 CLI (3.45).
//!
//! The refusal boundary that STAYS: a correlated slot may be read only by the
//! WHERE. One in the SELECT list, an aggregate argument, GROUP BY or HAVING has
//! no per-row meaning over a grouped tuple and is still refused.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-aggcorr-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 16\n\n[[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n",
        path.display()
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        path,
    )
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn one_int(res: ExecResult) -> i64 {
    let r = rows(res);
    assert_eq!(r.len(), 1, "expected exactly one row: {r:?}");
    assert_eq!(r[0].len(), 1, "expected exactly one column: {r:?}");
    match r[0][0] {
        Value::Int(n) => n,
        ref o => panic!("expected an int, got {o:?}"),
    }
}

fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn seed(db: &Database) {
    db.query("CREATE TABLE a (id INTEGER PRIMARY KEY, dept TEXT, g INT)", &[])
        .unwrap();
    db.query("CREATE TABLE b (bid INTEGER PRIMARY KEY, k INT)", &[])
        .unwrap();
    // g present in b: {10 (twice), 20}. g=30, g=40 absent.
    db.query(
        "INSERT INTO a (id, dept, g) VALUES \
         (1,'eng',10),(2,'eng',20),(3,'sales',10),(4,'sales',30),(5,'hr',40),(6,'eng',10)",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO b (bid, k) VALUES (1,10),(2,20),(3,10)", &[])
        .unwrap();
}

#[test]
fn aggregate_over_correlated_exists_matches_sqlite() {
    let (db, path) = open();
    seed(&db);

    // 1. count(*) over a correlated EXISTS: g in {10,20,10} across id 1,2,3,6.
    //    sqlite3: 4
    let got = one_int(
        db.query(
            "SELECT count(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g)",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got, 4);

    // 2. GROUP BY form. sqlite3: eng|3, sales|1 (hr and the g=30 sales row are
    //    filtered by EXISTS before grouping).
    let got = rows(
        db.query(
            "SELECT dept, count(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g) \
             GROUP BY dept ORDER BY dept",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![t("eng"), Value::Int(3)],
            vec![t("sales"), Value::Int(1)],
        ]
    );

    // 3. NOT EXISTS: g in {30,40} -> id 4,5. sqlite3: count 2.
    let got = one_int(
        db.query(
            "SELECT count(*) FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.g)",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got, 2);

    // 3b. NOT EXISTS grouped. sqlite3: hr|1, sales|1.
    let got = rows(
        db.query(
            "SELECT dept, count(*) FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.g) \
             GROUP BY dept ORDER BY dept",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![t("hr"), Value::Int(1)],
            vec![t("sales"), Value::Int(1)],
        ]
    );

    // 4. Scalar-correlated in the WHERE: count of matching b rows per g is
    //    g10 -> 2, g20 -> 1, else 0. `>= 2` keeps only the g=10 rows (id 1,3,6).
    //    sqlite3: 3.
    let got = one_int(
        db.query(
            "SELECT count(*) FROM a WHERE (SELECT count(*) FROM b WHERE b.k = a.g) >= 2",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got, 3);

    // 4b. Scalar-correlated filter + a real aggregate ARGUMENT (sum(g)) over the
    //     survivors, grouped. `>= 1` keeps g in {10,20} (id 1,2,3,6).
    //     sqlite3: eng|3|40, sales|1|10.
    let got = rows(
        db.query(
            "SELECT dept, count(*), sum(g) FROM a \
             WHERE (SELECT count(*) FROM b WHERE b.k = a.g) >= 1 \
             GROUP BY dept ORDER BY dept",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![t("eng"), Value::Int(3), Value::Int(40)],
            vec![t("sales"), Value::Int(1), Value::Int(10)],
        ]
    );

    // 5. Empty survivor set with no GROUP BY still yields ONE row (count 0),
    //    not zero rows — the empty-group zero accumulator. sqlite3: 0.
    let got = one_int(
        db.query(
            "SELECT count(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g AND b.k = 99999)",
            &[],
        )
        .unwrap(),
    );
    assert_eq!(got, 0);

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// The refusal boundary that STAYS (#73 §1.2c): a correlated subplan slot is
/// filled per outer row, so once the rows are grouped it has no single value.
/// A correlated subquery in an aggregate ARGUMENT, in the (grouped) SELECT list,
/// or in HAVING is therefore still refused — the direct query path proves this
/// in-process (validate mirrors it for the decode path).
#[test]
fn correlated_slot_outside_where_is_refused() {
    let (db, path) = open();
    seed(&db);

    // The correlated subquery is an aggregate ARGUMENT — no per-row correlation
    // survives into the grouped tuple, so refuse.
    let err = db
        .query(
            "SELECT sum((SELECT count(*) FROM b WHERE b.k = a.g)) FROM a",
            &[],
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("correlated") && msg.contains("WHERE"),
        "unexpected error for correlated aggregate argument: {msg}"
    );

    // The correlated subquery sits in the SELECT list of an aggregate query
    // (a non-grouped, non-aggregated projection over the grouped tuple). Refuse.
    let err = db
        .query(
            "SELECT count(*), (SELECT count(*) FROM b WHERE b.k = a.g) FROM a",
            &[],
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("correlated") && msg.contains("WHERE"),
        "unexpected error for correlated subquery in an aggregate SELECT list: {msg}"
    );

    // A NON-aggregate query may still read a correlated scalar subquery in its
    // projection — the boundary is aggregate-only, not a blanket ban.
    let got = rows(
        db.query(
            "SELECT id, (SELECT count(*) FROM b WHERE b.k = a.g) FROM a ORDER BY id",
            &[],
        )
        .unwrap(),
    );
    // sqlite3 per-id b-count: 2,1,2,0,0,2.
    let counts: Vec<i64> = got
        .iter()
        .map(|r| match r[1] {
            Value::Int(n) => n,
            ref o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(counts, vec![2, 1, 2, 0, 0, 2]);

    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
