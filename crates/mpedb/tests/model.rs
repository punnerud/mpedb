//! Stage M1: the workload model — storage round-trip, schema validation with
//! named refusals, cross-handle visibility, weighted advice, and the
//! level-equivalence property (shape-level advice covers statement-level
//! advice for the same workload). The preset models are validated against
//! their benches' schemas here, so the language cannot drift from the
//! workloads it describes.

use mpedb::advisor::WorkloadSource;
use mpedb::{Config, Database, Value, WorkloadModel};

fn db(tag: &str) -> (Database, String) {
    let path = format!(
        "{}/model-{tag}-{}.mpedb",
        if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" },
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    // A miniature of the olapbench star — same table/column names the preset
    // model names, so models/star-olap.toml validates against it.
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 32
max_readers = 8
durability = "none"

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
  name = "customer_id"
  type = "int64"
  nullable = false
  [[table.column]]
  name = "product_id"
  type = "int64"
  nullable = false
  [[table.column]]
  name = "store_id"
  type = "int64"
  nullable = false
  [[table.column]]
  name = "qty"
  type = "int64"
  nullable = false
  [[table.column]]
  name = "amount"
  type = "float64"
  nullable = false

[[table]]
name = "customer"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "nation_segment"
  type = "text"
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

[[table]]
name = "store"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "nation"
  type = "text"
  nullable = false

[[table]]
name = "day"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "year"
  type = "int64"
  nullable = false
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

#[test]
fn store_validate_and_share_across_handles() {
    let (d, path) = db("share");

    // The preset validates against the star schema it describes.
    let preset = include_str!("../../../models/star-olap.toml");
    d.set_model(preset).unwrap();
    assert_eq!(d.model_source().unwrap().unwrap(), preset, "source round-trips verbatim");
    let m = d.model().unwrap().unwrap();
    assert_eq!(m.archetype.map(|a| a.name()), Some("star-olap"));

    // A second attached handle (the multi-process shape) sees the same model.
    let d2 = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    assert_eq!(
        d2.model().unwrap().unwrap(),
        m,
        "the model is shared state, not per-handle state"
    );

    // Refusals name the offender.
    let e = d
        .set_model("[model]\n[[model.table]]\nname = \"orders\"")
        .unwrap_err();
    assert!(e.to_string().contains("orders"), "{e}");
    let e = d
        .set_model(
            "[model]\n[[model.table]]\nname = \"fact\"\n\
             [[model.table.access]]\nkind = \"filter-eq\"\ncolumns = [\"colour\"]",
        )
        .unwrap_err();
    assert!(e.to_string().contains("colour"), "{e}");

    // The other presets parse; the archetype-only one carries no table claims
    // and therefore validates against ANY schema — the founding example. The
    // rest validate against THEIR benches' schemas (graphbench, vecbench,
    // routebench), which is where set_model runs for them.
    d.set_model(include_str!("../../../models/sqlite3-general.toml")).unwrap();
    for preset in [
        include_str!("../../../models/graph.toml"),
        include_str!("../../../models/rag.toml"),
        include_str!("../../../models/routing.toml"),
    ] {
        WorkloadModel::from_toml_str(preset).unwrap();
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn shape_advice_covers_statement_advice_and_weights_rank() {
    let (d, path) = db("advise");
    let mut s = d.begin().unwrap();
    for id in 0..200i64 {
        s.query(
            "INSERT INTO product (id, category) VALUES ($1, $2)",
            &[Value::Int(id), Value::Text(format!("c{}", id % 8))],
        )
        .unwrap();
    }
    s.commit().unwrap();

    // Level 1: shapes with weights — category filtering dominates.
    let shape_model = WorkloadModel::from_toml_str(
        r#"
[model]
archetype = "star-olap"

[[model.table]]
name = "fact"
  [[model.table.access]]
  kind = "join-key"
  columns = ["product_id"]
  weight = 2.0

[[model.table]]
name = "product"
  [[model.table.access]]
  kind = "filter-eq"
  columns = ["category"]
  weight = 5.0
"#,
    )
    .unwrap();
    let shape = d.recommend_indexes(WorkloadSource::Model(shape_model)).unwrap();

    // Level 2: the same workload as concrete statements.
    let stmt_model = WorkloadModel::from_toml_str(
        r#"
[model]
[[model.statement]]
sql = "SELECT * FROM fact WHERE product_id = $1"
weight = 20
[[model.statement]]
sql = "SELECT * FROM product WHERE category = 'tools'"
weight = 50
"#,
    )
    .unwrap();
    let stmt = d.recommend_indexes(WorkloadSource::Model(stmt_model)).unwrap();

    let keys = |rep: &mpedb::advisor::AdviceReport| {
        rep.advices
            .iter()
            .map(|a| (a.table.clone(), a.columns.clone()))
            .collect::<Vec<_>>()
    };
    // The equivalence property: every candidate the statement level finds,
    // the shape level of the same workload also finds.
    for k in keys(&stmt) {
        assert!(keys(&shape).contains(&k), "shape advice missing {k:?}");
    }
    // Weights rank: category (weight 5.0 → 50) outranks product_id (2.0 → 20)
    // in both resolutions.
    assert_eq!(shape.advices[0].columns, ["category"]);
    assert_eq!(stmt.advices[0].columns, ["category"]);
    assert!(
        shape.advices[0].statements > shape.advices[1].statements,
        "declared weight must carry into the ranking"
    );

    let _ = std::fs::remove_file(&path);
}
