//! Stage M5: the stored cost layer. The oracle is the stage-A star flip —
//! the one plan choice this session can PROVE moves with pricing: with the
//! NDV channel on, the dimension drives; off, the fact scans. Tunables and
//! the policy spell must move it coherently across handles, and the stats
//! report must show what the engine believes.

use mpedb::spellfn::SpellLang;
use mpedb::{Config, Database, ExecResult, Value};

fn db(tag: &str) -> (Database, String) {
    let path = format!(
        "{}/costlayer-{tag}-{}.mpedb",
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
name = "fact"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "product_id"
  type = "int64"
  nullable = false
  indexed = true

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
  indexed = true
"#
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

/// The stage-A star in miniature (the ndv_stats test's shape): 50 products in
/// 5 categories, 2000 facts — buckets sized so the NDV discount flips the
/// join order.
fn seed(d: &Database) {
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
            "INSERT INTO fact (id, product_id) VALUES ($1, $2)",
            &[Value::Int(id), Value::Int(id % 50)],
        )
        .unwrap();
    }
    s.commit().unwrap();
    d.analyze().unwrap();
}

const STAR: &str = "SELECT f.id FROM fact f, product p \
                    WHERE f.product_id = p.id AND p.category = 'cat3'";

fn driver(d: &Database) -> String {
    match d.query(&format!("EXPLAIN {STAR}"), &[]).unwrap() {
        ExecResult::Explain(t) => {
            let line = t.lines().find(|l| l.contains("join order")).unwrap_or("").to_string();
            if line.contains("join order: product") {
                "product".into()
            } else {
                "fact".into()
            }
        }
        other => panic!("expected explain, got {other:?}"),
    }
}

#[test]
fn tunables_move_pricing_coherently_across_handles() {
    let (d, path) = db("tune");
    seed(&d);
    assert_eq!(driver(&d), "product", "analyzed: the NDV discount flips the star");

    // Switch the channel off on handle 1; handle 2 must re-price too.
    let d2 = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    d.set_tunable("ndv_discount=false").unwrap();
    assert_eq!(driver(&d), "fact", "channel off: pre-stage-A pricing");
    assert_eq!(driver(&d2), "fact", "the OTHER handle re-priced through the gen gate");

    // And back on.
    d2.set_tunable("ndv_discount=true").unwrap();
    assert_eq!(driver(&d), "product");
    assert_eq!(driver(&d2), "product");

    // Refusals name the known set.
    let e = d.set_tunable("warp_factor=9").unwrap_err();
    assert!(e.to_string().contains("ndv_discount"), "{e}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_cost_policy_spell_adjusts_pricing_programmably() {
    let (d, path) = db("policy");
    seed(&d);
    assert_eq!(driver(&d), "product");

    // A policy that VETOES the discount for the fact table only — the
    // selective form of the tunable, expressed as code. (Zeroing only the
    // DIMENSION's discount would tie worst_log at 11 and residual-placement
    // still prefers dimension-first — pricing arithmetic, not a bug; zeroing
    // the fact side prices dimension-first at 14 > 11 and flips.) It receives
    // the model's archetype too — the ladder's top as a cost input.
    d.set_cost_policy(
        SpellLang::Python,
        "def policy(kind, table, index_no, bucket, rows_bucket, archetype):\n\
         \x20   if kind == \"ndv\":\n\
         \x20       if table == \"fact\":\n\
         \x20           return 0\n\
         \x20   return bucket\n",
    )
    .unwrap();
    assert_eq!(driver(&d), "fact", "the policy zeroed the fact side's discount");

    // A second handle prices identically — the stored spell IS the coherence.
    let d2 = Database::open_from_file(std::path::Path::new(&path)).unwrap();
    assert_eq!(driver(&d2), "fact");

    // Dropping restores the base pricing everywhere.
    assert!(d2.drop_cost_policy().unwrap());
    assert_eq!(driver(&d), "product");
    assert_eq!(driver(&d2), "product");

    // A policy that cannot run fails the PREPARE, naming itself.
    d.set_cost_policy(
        SpellLang::Python,
        "def policy(kind, table, index_no, bucket, rows_bucket, archetype):\n\
         \x20   x = 0\n\
         \x20   while x >= 0:\n\
         \x20       x = x + 1\n\
         \x20   return bucket\n",
    )
    .unwrap();
    let e = d.query(STAR, &[]).unwrap_err();
    assert!(e.to_string().contains("cost policy"), "{e}");
    assert!(d.drop_cost_policy().unwrap());

    // Wrong arity refuses at set time, naming the signature.
    let e = d
        .set_cost_policy(SpellLang::Python, "def policy(kind):\n    return 0\n")
        .unwrap_err();
    assert!(e.to_string().contains("6 arguments"), "{e}");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_stats_report_shows_what_the_engine_believes() {
    let (d, path) = db("stats");

    // Before analyze: rows counted, NDV unknown.
    let mut s = d.begin().unwrap();
    for id in 0..100i64 {
        s.query(
            "INSERT INTO product (id, category) VALUES ($1, $2)",
            &[Value::Int(id), Value::Text(format!("c{}", id % 4))],
        )
        .unwrap();
    }
    s.commit().unwrap();

    let lines = d.stats_report().unwrap();
    let prod = lines
        .iter()
        .find(|l| l.table == "product" && l.columns == ["category"])
        .expect("the category index is reported");
    assert_eq!(prod.rows, 100);
    assert_eq!(prod.ndv_bucket, None, "never analyzed reads as unknown, not as a guess");

    d.analyze().unwrap();
    let lines = d.stats_report().unwrap();
    let prod = lines
        .iter()
        .find(|l| l.table == "product" && l.columns == ["category"])
        .unwrap();
    assert_eq!(prod.ndv_bucket, Some(3), "4 distinct categories → bucket 3");

    let _ = std::fs::remove_file(&path);
}
