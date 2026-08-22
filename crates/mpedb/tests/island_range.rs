//! Islands: a range the query never wrote, derived by arithmetic from one it
//! did (`planner::interval`).
//!
//! A distance predicate confines each axis to a box, and the planner is
//! supposed to find that box without recognizing the expression. Two things
//! have to hold, and only one of them is about speed:
//!
//! * the ANSWER must not change — the island is a superset, so the residual
//!   filter still decides every row, and the rows must match what sqlite says;
//! * a predicate the analysis cannot bound must fall back to a full scan
//!   rather than guess.
//!
//! Every expectation here is cross-checked against real sqlite, which has no
//! such analysis and therefore answers from first principles.

use mpedb::{Database, ExecResult, Value};
use mpedb_sql::AccessPath;
use mpedb_types::Config;

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

const DDL: &[&str] = &[
    "CREATE TABLE p (id INTEGER PRIMARY KEY, lat REAL NOT NULL, lon REAL NOT NULL)",
    "CREATE INDEX p_lat ON p (lat)",
];

/// A grid dense enough that a circle cuts through it at many points, with the
/// two axes on different periods so no row is on a diagonal by accident.
fn data() -> Vec<(i64, f64, f64)> {
    (0..400i64)
        .map(|i| {
            let lat = 59.0 + ((i % 40) as f64) * 0.02;
            let lon = 10.0 + ((i % 37) as f64) * 0.02;
            (i + 1, lat, lon)
        })
        .collect()
}

fn setup(tag: &str) -> Database {
    let dir = mpedb_testkit::scratch_base();
    let path = dir
        .join(format!("mpedb-island-{tag}-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let cfg = Config::from_toml_str(&format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\n"
    ))
    .unwrap();
    let db = Database::open_with_config(cfg).unwrap();
    for d in DDL {
        db.query(d, &[]).unwrap();
    }
    for (id, lat, lon) in data() {
        db.query(
            "INSERT INTO p VALUES ($1, $2, $3)",
            &[Value::Int(id), Value::Float(lat), Value::Float(lon)],
        )
        .unwrap();
    }
    db
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The same statements run against real sqlite.
fn oracle(sql: &str) -> Vec<String> {
    let mut script = String::new();
    for d in DDL {
        script.push_str(d);
        script.push(';');
    }
    for (id, lat, lon) in data() {
        script.push_str(&format!("INSERT INTO p VALUES ({id},{lat},{lon});"));
    }
    script.push_str(sql);
    script.push(';');
    sqlite_oracle::script_stdout(&script, "")
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn ours(db: &Database, sql: &str) -> Vec<String> {
    rows(db.query(sql, &[]).unwrap())
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Int(i) => i.to_string(),
                    Value::Float(f) => format!("{f}"),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn access(db: &Database, sql: &str) -> AccessPath {
    let schema = db.schema();
    let p = mpedb_sql::prepare(sql, &schema).expect("prepare");
    match &p.stmt {
        mpedb_sql::PlanStmt::Select(s) => s.access.clone(),
        other => panic!("expected a select plan, got {other:?}"),
    }
}

const CIRCLE: &str = "SELECT id FROM p WHERE \
     (lat-59.4)*(lat-59.4) + (lon-10.4)*(lon-10.4) < 0.02 ORDER BY id";

#[test]
fn a_circle_uses_the_index_and_answers_exactly_as_sqlite() {
    let db = setup("circle");
    // The bound is derived, not written: sqrt(0.02) ≈ 0.1414 about 59.4.
    match access(&db, CIRCLE) {
        AccessPath::IndexRange { index_no, lo, hi } => {
            assert_eq!(index_no, 1);
            assert!(lo.is_some() && hi.is_some(), "both ends should be bounded");
        }
        other => panic!("expected IndexRange from the island, got {other:?}"),
    }
    let got = ours(&db, CIRCLE);
    assert_eq!(got, oracle(CIRCLE), "the island changed the ANSWER");
    assert!(!got.is_empty(), "the test circle must contain rows");
}

#[test]
fn an_unboundable_predicate_falls_back_to_a_scan() {
    let db = setup("unbounded");
    // A LOWER bound on a square admits arbitrarily large values in both
    // directions — no interval, so no island, so a scan.
    let sql = "SELECT id FROM p WHERE (lat-59.4)*(lat-59.4) > 0.02 ORDER BY id";
    assert!(
        matches!(access(&db, sql), AccessPath::FullScan),
        "a lower bound on a square must not produce an island"
    );
    assert_eq!(ours(&db, sql), oracle(sql));

    // Modulo has no inverse here.
    let sql = "SELECT id FROM p WHERE lat - 59.0 > 0.5 AND id % 7 = 0 ORDER BY id";
    assert_eq!(ours(&db, sql), oracle(sql));
}

#[test]
fn arithmetic_shapes_agree_with_sqlite() {
    let db = setup("arith");
    for sql in [
        // A scaled column: the factor has to divide the target.
        "SELECT id FROM p WHERE lat * 2.0 < 118.6 ORDER BY id",
        // A NEGATIVE factor swaps the ends; getting that backwards would
        // produce an empty island and lose every row.
        "SELECT id FROM p WHERE lat * -1.0 > -59.4 ORDER BY id",
        // Constant on the left.
        "SELECT id FROM p WHERE 59.5 > lat - 0.1 ORDER BY id",
        // A shifted square.
        "SELECT id FROM p WHERE (lat - 59.4)*(lat - 59.4) < 0.01 ORDER BY id",
        // Two axes, one bounded by the sum rule and one written outright.
        "SELECT id FROM p WHERE (lat-59.4)*(lat-59.4) + (lon-10.4)*(lon-10.4) < 0.02 \
         AND lon > 10.3 ORDER BY id",
    ] {
        assert_eq!(ours(&db, sql), oracle(sql), "disagreement on `{sql}`");
    }
}

#[test]
fn an_unsatisfiable_predicate_returns_nothing() {
    let db = setup("empty");
    // A square is never negative. The island is empty, and so is the answer.
    let sql = "SELECT id FROM p WHERE (lat-59.4)*(lat-59.4) < -1.0 ORDER BY id";
    assert_eq!(ours(&db, sql), oracle(sql));
    assert!(ours(&db, sql).is_empty());
}
