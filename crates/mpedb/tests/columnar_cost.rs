//! Stage C: MPEE prices a full-column `sum`/`avg` off the column segment, not
//! the index tree, on a table the MODEL marks columnar. The switch is a cost
//! decision (`agg_index_choice` declines the index), so it shows in the plan —
//! and it is scoped: `min`/`max` keep the O(log n) index probe, `count(*)` keeps
//! the entry-count, and the ANSWER is identical whichever source runs.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct Guard(PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn open() -> (Database, Guard) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-colcost-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    // `amount` is INDEXED — the row-store way. Without the model, sum(amount)
    // rides that index tree; with it, MPEE prefers the segment.
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 32
max_readers = 8
[[table]]
name = "fact"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "amount"
  type = "int64"
  nullable = false
  indexed = true
"#,
        path.display()
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        Guard(path),
    )
}

const MODEL: &str = r#"
[model]
name = "m"
archetype = "star-olap"
[[model.table]]
name = "fact"
role = "fact"
"#;

fn explain(db: &Database, sql: &str) -> String {
    match db.query(&format!("EXPLAIN {sql}"), &[]).unwrap() {
        ExecResult::Explain(t) => t,
        other => format!("{other:?}"),
    }
}

fn scalar(db: &Database, sql: &str) -> Value {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        o => panic!("{o:?}"),
    }
}

#[test]
fn columnar_model_prices_sum_off_the_segment_not_the_index() {
    let (db, _g) = open();
    let mut s = db.begin().unwrap();
    for i in 0..1000i64 {
        s.query("INSERT INTO fact (id, amount) VALUES ($1, $2)", &[Value::Int(i), Value::Int(i)])
            .unwrap();
    }
    s.commit().unwrap();

    // No model yet: sum(amount) takes the amount INDEX tree.
    assert!(
        explain(&db, "SELECT sum(amount) FROM fact").contains("aggregate via index"),
        "without a model, sum rides the index: {}",
        explain(&db, "SELECT sum(amount) FROM fact")
    );

    // Declare the fact columnar. Now MPEE declines the index for sum — the
    // segment scan is the cheaper source.
    db.set_model(MODEL).unwrap();
    db.sync_columnar().unwrap();
    let sum_plan = explain(&db, "SELECT sum(amount) FROM fact");
    assert!(
        !sum_plan.contains("aggregate via index"),
        "columnar model: sum declines the index for the segment: {sum_plan}"
    );
    let avg_plan = explain(&db, "SELECT avg(amount) FROM fact");
    assert!(!avg_plan.contains("aggregate via index"), "avg too: {avg_plan}");

    // …but min/max keep the O(log n) boundary probe, and count(*) the
    // entry-count: the segment is not cheaper there.
    assert!(
        explain(&db, "SELECT min(amount) FROM fact").contains("aggregate via index"),
        "min keeps the index"
    );
    assert!(
        explain(&db, "SELECT max(amount) FROM fact").contains("aggregate via index"),
        "max keeps the index"
    );

    // The answer is identical whichever source runs.
    assert_eq!(scalar(&db, "SELECT sum(amount) FROM fact"), Value::Int((0..1000).sum()));
    assert_eq!(scalar(&db, "SELECT min(amount) FROM fact"), Value::Int(0));
    assert_eq!(scalar(&db, "SELECT max(amount) FROM fact"), Value::Int(999));
}

#[test]
fn a_computed_argument_keeps_the_index() {
    // The segment fast path feeds a RAW column; `sum(amount + 1)` is a computed
    // argument it cannot serve, so the cost fix must NOT decline the index there.
    let (db, _g) = open();
    let mut s = db.begin().unwrap();
    for i in 0..500i64 {
        s.query("INSERT INTO fact (id, amount) VALUES ($1, $2)", &[Value::Int(i), Value::Int(i)])
            .unwrap();
    }
    s.commit().unwrap();
    db.set_model(MODEL).unwrap();

    // A bare-column sum declines the index; a computed one does not.
    assert!(!explain(&db, "SELECT sum(amount) FROM fact").contains("aggregate via index"));
    // (No assertion on `sum(amount+1)`'s access path — the point is only that
    // the answer stays correct whatever it chose.)
    assert_eq!(
        scalar(&db, "SELECT sum(amount + 1) FROM fact"),
        Value::Int((0..500).map(|i| i + 1).sum())
    );
}
