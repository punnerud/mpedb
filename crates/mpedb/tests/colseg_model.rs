//! Stage 4: `sync_columnar` builds segments for the tables the MODEL marks
//! scan-heavy (fact / star-olap) and drops them for point-oriented ones —
//! "automatic + sparse + dynamic via MPEE".

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

fn open(name: &str) -> (Database, Guard) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-colmodel-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 64
max_readers = 16

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

const MODEL: &str = r#"
[model]
name = "star"
archetype = "star-olap"

[[model.table]]
name = "fact"
role = "fact"
  [[model.table.access]]
  kind = "filter-range"
  columns = ["amount"]

[[model.table]]
name = "product"
role = "dimension"
  [[model.table.access]]
  kind = "filter-eq"
  columns = ["category"]
"#;

fn seed(db: &Database) {
    let mut s = db.begin().unwrap();
    for i in 0..3000i64 {
        s.query(
            "INSERT INTO fact (id, product_id, amount) VALUES ($1, $2, $3)",
            &[Value::Int(i), Value::Int(i % 50), Value::Float(i as f64 * 1.5)],
        )
        .unwrap();
    }
    for i in 0..50i64 {
        s.query(
            "INSERT INTO product (id, category) VALUES ($1, $2)",
            &[Value::Int(i), Value::Text(format!("c{}", i % 5))],
        )
        .unwrap();
    }
    s.commit().unwrap();
}

fn sum(db: &Database, sql: &str) -> Value {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        o => panic!("{o:?}"),
    }
}

#[test]
fn sync_columnar_follows_the_model_roles() {
    let (db, _g) = open("roles");
    seed(&db);
    db.set_model(MODEL).unwrap();

    let want = sum(&db, "SELECT sum(amount) FROM fact");

    let r = db.sync_columnar().unwrap();
    // The fact table is columnarized; the dimension is not.
    assert!(
        r.columnarized.iter().any(|(t, _)| t == "fact"),
        "fact is scan-heavy: {r:?}"
    );
    assert!(
        !r.columnarized.iter().any(|(t, _)| t == "product"),
        "the dimension stays on the row tree: {r:?}"
    );

    // The fact aggregate is answered from segments and is bit-identical.
    let got = sum(&db, "SELECT sum(amount) FROM fact");
    match (&want, &got) {
        (Value::Float(a), Value::Float(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        _ => assert_eq!(want, got),
    }

    // Idempotent: a second sync rebuilds the same set, drops nothing new.
    let r2 = db.sync_columnar().unwrap();
    assert!(r2.columnarized.iter().any(|(t, _)| t == "fact"));
    assert!(r2.dropped.is_empty());
}

#[test]
fn changing_a_role_to_dimension_drops_the_segments() {
    let (db, _g) = open("rerole");
    seed(&db);
    db.set_model(MODEL).unwrap();
    db.sync_columnar().unwrap();
    let want = sum(&db, "SELECT sum(amount) FROM fact");

    // Re-declare fact as a dimension (point-oriented): its segments must go.
    let dim_model = MODEL.replace("role = \"fact\"", "role = \"dimension\"");
    db.set_model(&dim_model).unwrap();
    let r = db.sync_columnar().unwrap();
    assert!(r.dropped.contains(&"fact".to_string()), "fact segments dropped: {r:?}");

    // The answer is still right — now from the row scan.
    let got = sum(&db, "SELECT sum(amount) FROM fact");
    match (&want, &got) {
        (Value::Float(a), Value::Float(b)) => assert_eq!(a.to_bits(), b.to_bits()),
        _ => assert_eq!(want, got),
    }
}

#[test]
fn sync_columnar_refuses_without_a_model() {
    let (db, _g) = open("nomodel");
    seed(&db);
    let e = db.sync_columnar().unwrap_err();
    assert!(e.to_string().contains("no model stored"), "{e}");
}
