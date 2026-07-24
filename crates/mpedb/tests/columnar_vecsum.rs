//! The vectorized `sum(FLOAT)` (DESIGN-COLUMNAR §7.2) must be BIT-IDENTICAL to
//! the per-value row fold across every float encoding — raw, dictionary
//! (low-cardinality), and run-of-default (sparse) — because float addition is
//! not associative and the sum is compared bitwise. It sums in the same k-order
//! the row scan does; this pins that.

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
        "mpedb-vecsum-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 64
max_readers = 8
[[table]]
name = "fact"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "amount"
  type = "float64"
  nullable = false
"#,
        path.display()
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        Guard(path),
    )
}

fn sum_bits(db: &Database) -> u64 {
    match db.query("SELECT sum(amount) FROM fact", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Float(f) => f.to_bits(),
            ref o => panic!("{o:?}"),
        },
        o => panic!("{o:?}"),
    }
}

/// Fill `amount` with `f(id)`, build segments, and assert the segment `sum`
/// equals the row-scan `sum` BIT-FOR-BIT.
fn check(name: &str, amount: impl Fn(i64) -> f64) {
    let (db, _g) = open();
    let mut s = db.begin().unwrap();
    for i in 0..40_000i64 {
        s.query(
            "INSERT INTO fact (id, amount) VALUES ($1, $2)",
            &[Value::Int(i), Value::Float(amount(i))],
        )
        .unwrap();
    }
    s.commit().unwrap();

    // Segment path (vectorized sum), then the row scan (segments dropped).
    db.compact_columns().unwrap();
    let seg = sum_bits(&db);
    db.drop_column_segments().unwrap();
    let row = sum_bits(&db);
    assert_eq!(seg, row, "{name}: segment sum {seg:#x} != row scan {row:#x}");
}

#[test]
fn raw_high_cardinality_sum_is_bit_identical() {
    // Every value distinct → raw f64 encoding.
    check("raw", |i| i as f64 * 1.5);
}

#[test]
fn dictionary_low_cardinality_sum_is_bit_identical() {
    // A handful of distinct values → dictionary encoding, the price-column shape
    // the OLAP bench actually hits (and where the vectorized path first
    // declined until it learned to read dict codes).
    check("dict", |i| ((i % 7) as f64) * 0.1 + 0.03);
}

#[test]
fn sparse_run_default_sum_is_bit_identical() {
    // Almost all one value, a few exceptions → run-of-default.
    check("sparse", |i| if i % 500 == 0 { i as f64 } else { 2.5 });
}

#[test]
fn negatives_and_fractions_sum_is_bit_identical() {
    // Mixed signs and fractions where the addition ORDER decides the last ULP.
    check("mixed", |i| (i as f64 - 20_000.0) * 0.7);
}
