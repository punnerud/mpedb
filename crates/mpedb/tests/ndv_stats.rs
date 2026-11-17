//! Stage A of design/DESIGN-MPEE-GENERAL.md, end to end: `analyze()` persists
//! per-index NDV, a re-prepare picks the dimension-first star plan, and the
//! answer does not move by a row. One test, one flip, one determinism check —
//! the solver-level pricing details live in `mpedb-sql`'s
//! `ndv_flips_a_star_to_dimension_first`.

use mpedb::{Config, Database, ExecResult, Value};

fn scratch_dir() -> String {
    mpedb_testkit::scratch_base_str()
}

fn db() -> (Database, String) {
    let path = format!(
        "{}/ndv-stats-{}.mpedb",
        scratch_dir(),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{path}"
size_mb = 64
max_readers = 8
durability = "none"

[[table]]
name = "fact"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "product_id"
  type = "int64"
  indexed = true
  [[table.column]]
  name = "amount"
  type = "int64"

[[table]]
name = "product"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "category"
  type = "text"
  indexed = true
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn rows_sorted(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { mut rows, .. } => {
            rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            rows
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

fn explain(d: &Database, sql: &str) -> String {
    match d.query(&format!("EXPLAIN {sql}"), &[]).unwrap() {
        ExecResult::Explain(t) => t,
        other => panic!("expected explain, got {other:?}"),
    }
}

#[test]
fn analyze_flips_the_star_and_moves_no_rows() {
    let (d, path) = db();

    // 50 products across 5 categories, 2000 facts. Sized so the buckets
    // decide: blind, fact-first is bucket(2000)=11 vs dimension-first
    // 6+11=17; analyzed, dimension-first is (6−3)+(11−6)=8 < 11.
    let mut s = d.begin().unwrap();
    for id in 0..50i64 {
        s.query(
            "INSERT INTO product (id, category) VALUES ($1, $2)",
            &[Value::Int(id), Value::Text(format!("cat{}", id % 5))],
        )
        .unwrap();
    }
    for id in 0..2000i64 {
        s.query(
            "INSERT INTO fact (id, product_id, amount) VALUES ($1, $2, $3)",
            &[Value::Int(id), Value::Int(id % 50), Value::Int(id % 97)],
        )
        .unwrap();
    }
    // A NULL join key is not in the index. The broadcast scan walks every
    // row, so this is the one that would leak if the hash treated NULL as a
    // key. It must stay out of every result below.
    s.query(
        "INSERT INTO fact (id, product_id, amount) VALUES ($1, $2, $3)",
        &[Value::Int(2000), Value::Null, Value::Int(1)],
    )
    .unwrap();
    s.commit().unwrap();

    let sql = "SELECT f.amount FROM fact f, product p \
               WHERE f.product_id = p.id AND p.category = 'cat3'";
    // Residual ON (the equality is pushed; `amount > 50` stays). And the
    // aggregate shape the star bench times. Both have to survive analyze,
    // which is when the executor may scan the fact table instead of probing.
    let sql_residual = "SELECT f.amount FROM fact f, product p \
                        WHERE f.product_id = p.id AND p.category = 'cat3' \
                        AND f.amount > 50";
    let sql_agg = "SELECT p.category, sum(f.amount) FROM fact f, product p \
                   WHERE f.product_id = p.id AND p.category = 'cat3' \
                   GROUP BY p.category";

    // Unanalyzed: the pre-stage-A plan, and the reference answer.
    let before = explain(&d, sql);
    assert!(
        before.contains("join order: fact"),
        "unanalyzed must price exactly as before stage A; got:\n{before}"
    );
    let reference = rows_sorted(d.query(sql, &[]).unwrap());
    assert_eq!(reference.len(), 400, "10 matching products × 40 facts each");
    let reference_residual = rows_sorted(d.query(sql_residual, &[]).unwrap());
    assert!(!reference_residual.is_empty() && reference_residual.len() < 400);
    let reference_agg = rows_sorted(d.query(sql_agg, &[]).unwrap());

    // Analyze: both single-column indexes measured, exactly.
    let stats = d.analyze().unwrap();
    let ndv_of = |table: &str, ixno: u32| {
        stats
            .iter()
            .find(|st| st.table == table && st.index_no == ixno)
            .map(|st| st.ndv)
    };
    assert_eq!(ndv_of("product", 1), Some(5), "category NDV");
    assert_eq!(ndv_of("fact", 1), Some(50), "product_id NDV");

    // Re-prepare: the dimension drives, the fact is probed through its
    // join-key index — and the answer is the same rows.
    let after = explain(&d, sql);
    assert!(
        after.contains("join order: product"),
        "analyzed, the dimension must drive; got:\n{after}"
    );
    assert_eq!(rows_sorted(d.query(sql, &[]).unwrap()), reference);
    assert_eq!(rows_sorted(d.query(sql_residual, &[]).unwrap()), reference_residual);
    assert_eq!(rows_sorted(d.query(sql_agg, &[]).unwrap()), reference_agg);

    // Determinism: a second pass measures the same values (same snapshot
    // content ⇒ same records ⇒ same plans — the stability law).
    assert_eq!(d.analyze().unwrap(), stats);

    let _ = std::fs::remove_file(&path);
}
