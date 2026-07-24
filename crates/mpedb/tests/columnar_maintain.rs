//! Stage B: the cheap live check (`columnar_maintenance_plan`) + the bounded
//! adaptive drain (`maintain_columnar`). "Automatic which (the model), bounded
//! when (build only what went stale)." No daemon: the check is O(model tables)
//! reads, and re-running a settled database does nothing.

use mpedb::colseg::MaintainAction;
use mpedb::{Config, Database, Value};
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
        "mpedb-colmaint-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 96
max_readers = 8

[[table]]
name = "fact1"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "amount"
  type = "float64"
  nullable = false

[[table]]
name = "fact2"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "amount"
  type = "float64"
  nullable = false

[[table]]
name = "dim"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "label"
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
name = "m"
archetype = "star-olap"
[[model.table]]
name = "fact1"
role = "fact"
[[model.table]]
name = "fact2"
role = "fact"
[[model.table]]
name = "dim"
role = "dimension"
"#;

fn fill(db: &Database, table: &str, ids: std::ops::Range<i64>) {
    let mut s = db.begin().unwrap();
    for i in ids {
        if table == "dim" {
            s.query(
                &format!("INSERT INTO {table} (id, label) VALUES ($1, $2)"),
                &[Value::Int(i), Value::Text(format!("l{i}"))],
            )
            .unwrap();
        } else {
            s.query(
                &format!("INSERT INTO {table} (id, amount) VALUES ($1, $2)"),
                &[Value::Int(i), Value::Float(i as f64)],
            )
            .unwrap();
        }
    }
    s.commit().unwrap();
}

fn action_for(db: &Database, table: &str, frac: f64) -> Option<MaintainAction> {
    db.columnar_maintenance_plan(frac)
        .unwrap()
        .into_iter()
        .find(|m| m.table == table)
        .map(|m| m.action)
}

#[test]
fn maintain_builds_facts_and_leaves_the_dimension() {
    let (db, _g) = open();
    fill(&db, "fact1", 0..2000);
    fill(&db, "fact2", 0..2000);
    fill(&db, "dim", 0..200);
    db.set_model(MODEL).unwrap();

    // The live check: both facts want building, the dimension does not appear.
    let plan = db.columnar_maintenance_plan(0.25).unwrap();
    assert_eq!(plan.len(), 2, "two facts to build: {plan:?}");
    assert!(plan.iter().all(|m| m.action == MaintainAction::Build));
    assert!(!plan.iter().any(|m| m.table == "dim"));

    let out = db.maintain_columnar(0.25, 0).unwrap();
    assert_eq!(out.columnarized.len(), 2, "{out:?}");
    assert_eq!(db.columnar_watermark_covered("fact1").unwrap(), Some(2000));
    assert_eq!(db.columnar_watermark_covered("fact2").unwrap(), Some(2000));
    assert_eq!(db.columnar_watermark_covered("dim").unwrap(), None);

    // Settled: the live check now finds nothing, and a second drain is a no-op.
    assert!(db.columnar_maintenance_plan(0.25).unwrap().is_empty());
    let out2 = db.maintain_columnar(0.25, 0).unwrap();
    assert!(out2.columnarized.is_empty() && out2.dropped.is_empty(), "{out2:?}");
}

#[test]
fn maintain_rebuilds_only_when_the_tail_grows_past_the_fraction() {
    let (db, _g) = open();
    fill(&db, "fact1", 0..1000);
    db.set_model(MODEL).unwrap();
    db.maintain_columnar(0.25, 0).unwrap();
    assert_eq!(db.columnar_watermark_covered("fact1").unwrap(), Some(1000));

    // A small append (10%) is below the fraction — no rebuild wanted.
    fill(&db, "fact1", 1000..1100);
    assert_eq!(action_for(&db, "fact1", 0.25), None, "10% tail stays");
    // A bigger append (now 40%) crosses it.
    fill(&db, "fact1", 1100..1400);
    match action_for(&db, "fact1", 0.25) {
        Some(MaintainAction::Rebuild { covered, tail }) => {
            assert_eq!(covered, 1000);
            assert_eq!(tail, 400);
        }
        other => panic!("expected Rebuild, got {other:?}"),
    }
    // Draining absorbs the tail: the watermark advances to cover all 1400.
    db.maintain_columnar(0.25, 0).unwrap();
    assert_eq!(db.columnar_watermark_covered("fact1").unwrap(), Some(1400));
}

#[test]
fn maintain_drops_segments_the_model_stopped_wanting() {
    let (db, _g) = open();
    fill(&db, "fact1", 0..1000);
    db.set_model(MODEL).unwrap();
    db.maintain_columnar(0.25, 0).unwrap();
    assert!(db.columnar_watermark_covered("fact1").unwrap().is_some());

    // Re-declare fact1 as a dimension: the live check now says Drop.
    let remodel = MODEL.replace(
        "name = \"fact1\"\nrole = \"fact\"",
        "name = \"fact1\"\nrole = \"dimension\"",
    );
    db.set_model(&remodel).unwrap();
    assert_eq!(action_for(&db, "fact1", 0.25), Some(MaintainAction::Drop));

    let out = db.maintain_columnar(0.25, 0).unwrap();
    assert!(out.dropped.iter().any(|t| t == "fact1"), "{out:?}");
    assert_eq!(db.columnar_watermark_covered("fact1").unwrap(), None);
}

#[test]
fn max_rebuilds_bounds_one_pass() {
    let (db, _g) = open();
    fill(&db, "fact1", 0..1000);
    fill(&db, "fact2", 0..1000);
    db.set_model(MODEL).unwrap();

    // Two facts want building; cap the pass at one.
    let out = db.maintain_columnar(0.25, 1).unwrap();
    assert_eq!(out.columnarized.len(), 1, "capped to one: {out:?}");
    let built = db.columnar_watermark_covered("fact1").unwrap().is_some() as u8
        + db.columnar_watermark_covered("fact2").unwrap().is_some() as u8;
    assert_eq!(built, 1, "exactly one fact built this pass");

    // The next pass finishes the other.
    let out2 = db.maintain_columnar(0.25, 1).unwrap();
    assert_eq!(out2.columnarized.len(), 1, "{out2:?}");
    assert!(db.columnar_watermark_covered("fact1").unwrap().is_some());
    assert!(db.columnar_watermark_covered("fact2").unwrap().is_some());
}

#[test]
fn maintenance_requires_a_model() {
    let (db, _g) = open();
    fill(&db, "fact1", 0..10);
    assert!(db.columnar_maintenance_plan(0.25).is_err(), "no model -> error");
    assert!(db.maintain_columnar(0.25, 0).is_err());
}
