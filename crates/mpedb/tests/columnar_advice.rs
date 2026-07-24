//! Stage A: `recommend_columnar` reads the workload and says which tables the
//! application SCANS (column segments pay) vs POINTS at (row tree cheaper) —
//! the safe, recommend-only half of "automatic via MPEE". The differential is
//! against hand-written workloads: a scan-aggregate fact must come back
//! `Column`, a key-probed dimension `Row`.

use mpedb::advisor::{Orient, WorkloadSource};
use mpedb::{Config, Database};
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
        "mpedb-coladvice-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
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
  name = "day_id"
  type = "int64"
  nullable = false
  [[table.column]]
  name = "store_id"
  type = "int64"
  nullable = false
  [[table.column]]
  name = "amount"
  type = "float64"
  nullable = false

[[table]]
name = "product"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "category"
  type = "text"
  nullable = false
"#,
        path.display()
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        Guard(path),
    )
}

fn advice<'a>(
    rep: &'a mpedb::advisor::ColumnarReport,
    table: &str,
) -> &'a mpedb::advisor::ColumnarAdvice {
    rep.advices
        .iter()
        .find(|a| a.table == table)
        .unwrap_or_else(|| panic!("no advice for {table}: {:?}", rep.advices))
}

#[test]
fn scan_aggregate_fact_is_column_point_dimension_is_row() {
    let (db, _g) = open();
    let stmts = vec![
        // fact: scanned and aggregated — the columnar shape.
        "SELECT sum(amount) FROM fact".to_string(),
        "SELECT day_id, sum(amount) FROM fact GROUP BY day_id".to_string(),
        "SELECT store_id, count(*) FROM fact WHERE day_id > 5 GROUP BY store_id".to_string(),
        // product: pointed at by key — the row shape.
        "SELECT category FROM product WHERE id = 3".to_string(),
        "SELECT category FROM product WHERE id = 7".to_string(),
        "SELECT * FROM product WHERE id = 11".to_string(),
    ];
    let rep = db.recommend_columnar(WorkloadSource::Statements(stmts)).unwrap();
    assert!(rep.compiled >= 6, "all six compiled: {rep:?}");

    let fact = advice(&rep, "fact");
    assert_eq!(fact.orient, Orient::Column, "fact is scanned: {fact:?}");
    assert!(fact.scan_weight >= 3, "three scan-aggregates: {fact:?}");
    assert_eq!(fact.point_weight, 0);
    // The segments it would build cover the columns the scans read.
    assert!(fact.scan_columns.contains(&"amount".to_string()), "{fact:?}");
    assert!(fact.scan_columns.contains(&"day_id".to_string()), "{fact:?}");

    let product = advice(&rep, "product");
    assert_eq!(product.orient, Orient::Row, "product is pointed at: {product:?}");
    assert!(product.point_weight >= 3, "three point reads: {product:?}");
    assert_eq!(product.scan_weight, 0);

    // Most-scanned floats to the top.
    assert_eq!(rep.advices[0].table, "fact");
}

#[test]
fn writes_and_appends_vote_the_right_way() {
    let (db, _g) = open();
    let stmts = vec![
        // A scanned fact that is ALSO appended to: the append (INSERT) must not
        // vote against columns (stage 5 keeps segments valid), so the scan wins.
        "SELECT sum(amount) FROM fact".to_string(),
        "INSERT INTO fact (id, day_id, store_id, amount) VALUES (1, 2, 3, 4.0)".to_string(),
        "INSERT INTO fact (id, day_id, store_id, amount) VALUES (2, 3, 4, 5.0)".to_string(),
        // An UPDATE, by contrast, invalidates covered segments → row-favouring.
        "UPDATE product SET category = 'x' WHERE id = 9".to_string(),
        "UPDATE product SET category = 'y' WHERE id = 8".to_string(),
    ];
    let rep = db.recommend_columnar(WorkloadSource::Statements(stmts)).unwrap();
    assert_eq!(advice(&rep, "fact").orient, Orient::Column, "append is neutral");
    assert_eq!(advice(&rep, "fact").point_weight, 0, "inserts don't vote row");
    assert_eq!(advice(&rep, "product").orient, Orient::Row, "updates vote row");
}

#[test]
fn model_emitter_proposes_the_roles() {
    let (db, _g) = open();
    let stmts = vec![
        "SELECT sum(amount) FROM fact".to_string(),
        "SELECT category FROM product WHERE id = 1".to_string(),
    ];
    let rep = db.recommend_columnar(WorkloadSource::Statements(stmts)).unwrap();
    let toml = rep.to_model_toml();
    // A valid, applyable model with the derived roles.
    let model = mpedb_types::model::WorkloadModel::from_toml_str(&toml)
        .unwrap_or_else(|e| panic!("emitted model must parse: {e}\n{toml}"));
    let fact = model.tables.iter().find(|t| t.name == "fact").unwrap();
    assert_eq!(fact.role, Some(mpedb_types::model::TableRole::Fact));
    let product = model.tables.iter().find(|t| t.name == "product").unwrap();
    assert_eq!(product.role, Some(mpedb_types::model::TableRole::Dimension));
    assert!(toml.contains("star-olap"), "archetype reflects the scan: {toml}");
}

#[test]
fn registry_source_sees_what_was_actually_run() {
    let (db, _g) = open();
    // Run real queries so their plans land in the shared registry.
    let _ = db.query("SELECT sum(amount) FROM fact", &[]).unwrap();
    let _ = db.query("SELECT day_id, sum(amount) FROM fact GROUP BY day_id", &[]).unwrap();
    let _ = db.query("SELECT category FROM product WHERE id = 1", &[]).unwrap();

    let rep = db.recommend_columnar(WorkloadSource::Registry).unwrap();
    assert_eq!(advice(&rep, "fact").orient, Orient::Column, "{rep:?}");
    assert_eq!(advice(&rep, "product").orient, Orient::Row, "{rep:?}");
}
