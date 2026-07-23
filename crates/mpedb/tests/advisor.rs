//! Stage E: the workload-index advisor, recommend-only. The candidates it
//! derives must be the ones the #118 census counted — equalities sorted, one
//! range column, ORDER BY tail, served-by-prefix filtered — and the report
//! must say what it skipped rather than imply coverage.

use mpedb::advisor::WorkloadSource;
use mpedb::{Config, Database, Value};

fn db() -> (Database, String) {
    let path = format!(
        "{}/advisor-{}.mpedb",
        if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" },
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 8
durability = "none"

[[table]]
name = "orders"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "customer"
  type = "int64"
  [[table.column]]
  name = "status"
  type = "text"
  nullable = false
  [[table.column]]
  name = "created"
  type = "int64"
  [[table.column]]
  name = "region"
  type = "text"
  nullable = false
  indexed = true
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

#[test]
fn the_advisor_recommends_what_the_workload_filters_on() {
    let (d, path) = db();
    let mut s = d.begin().unwrap();
    for id in 0..500i64 {
        s.query(
            "INSERT INTO orders (id, customer, status, created, region) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                Value::Int(id),
                Value::Int(id % 40),
                Value::Text(if id % 3 == 0 { "open" } else { "done" }.into()),
                Value::Int(1000 + id),
                Value::Text(format!("r{}", id % 4)),
            ],
        )
        .unwrap();
    }
    s.commit().unwrap();

    // The workload, through prepare → the registry. Distinct SQL texts so
    // each is a distinct registered plan.
    for sql in [
        // Three shapes that want (customer): the eq column.
        "SELECT status FROM orders WHERE customer = 7",
        "SELECT created FROM orders WHERE customer = 9",
        "SELECT id FROM orders WHERE customer = 11",
        // One that wants (customer, status): two equalities, sorted-canonical.
        "SELECT id FROM orders WHERE status = 'open' AND customer = 3",
        // Equality + range → (status, created).
        "SELECT id FROM orders WHERE status = 'open' AND created > 1200",
        // Served by the existing region index: must NOT be recommended.
        "SELECT id FROM orders WHERE region = 'r1'",
        // Served by the PK: must NOT be recommended.
        "SELECT status FROM orders WHERE id = 42",
        // UPDATE's WHERE counts too — Django workloads are full of these.
        "UPDATE orders SET status = 'done' WHERE customer = 5",
    ] {
        d.prepare(sql).unwrap();
    }

    let rep = d.recommend_indexes(WorkloadSource::Registry).unwrap();

    let find = |cols: &[&str]| {
        rep.advices
            .iter()
            .find(|a| a.table == "orders" && a.columns == cols)
    };
    // (customer): 3 SELECTs + 1 UPDATE = 4 statements.
    let c = find(&["customer"]).expect("customer index recommended");
    assert_eq!(c.statements, 4, "3 selects + 1 update filter on customer");
    assert!(!c.index_id.is_empty() && c.index_id.len() == 64, "blake3 hex identity");
    // Two-equality candidate, columns in ordinal order (canonical for an eq set).
    assert!(find(&["customer", "status"]).is_some(), "eq-set candidate");
    // Equality then range.
    assert!(find(&["status", "created"]).is_some(), "eq-then-range candidate");
    // Served shapes are counted, not recommended.
    assert!(find(&["region"]).is_none(), "region is already indexed");
    assert!(find(&["id"]).is_none(), "the PK serves id");
    assert_eq!(rep.served, 2, "region + pk shapes counted as served");
    // The ranking: (customer) has the most statements, so it is first.
    assert_eq!(rep.advices[0].columns, ["customer"]);
    assert_eq!(rep.uncompilable, 0);

    // The Statements source: same rules, no registry involved.
    let rep2 = d
        .recommend_indexes(WorkloadSource::Statements(vec![
            "SELECT id FROM orders WHERE created < 1100".into(),
            "not sql at all".into(),
        ]))
        .unwrap();
    assert_eq!(rep2.uncompilable, 1, "a refusal is counted, not fatal");
    assert!(
        rep2.advices.iter().any(|a| a.columns == ["created"]),
        "range-only candidate from the statement list"
    );

    let _ = std::fs::remove_file(&path);
}
