//! Columnar segment blocks are stored as CONTIGUOUS extents (DESIGN-COLUMNAR
//! §7.3) via `sys_put_extent`, i.e. as genuine `ExtentRef` catalog cells. Two
//! properties this pins, because the run lives in the crash-critical extent
//! allocator:
//!
//!   1. `verify_page_accounting` accepts the database at EVERY stage — after a
//!      build, after a re-compaction that replaces the blocks, and after a drop.
//!      Before the ExtentRef conversion the extent map held the run but no tree
//!      cell referenced it, and the verifier tripped `refs != mapped` the moment
//!      a segment went live.
//!   2. Repeated build→drop cycles do NOT grow the file: deleting a block record
//!      frees its extent through the btree's one free-old-val path, and the
//!      freed run is reused (bounded high-water, not a per-cycle leak).

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
        "mpedb-extleak-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 256
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

fn fill(db: &Database, n: i64) {
    let mut s = db.begin().unwrap();
    for i in 0..n {
        s.query(
            "INSERT INTO fact (id, amount, label) VALUES ($1, $2, $3)",
            &[
                Value::Int(i),
                Value::Float((i % 7) as f64 * 0.1),
                Value::Text(format!("row-{}", i % 13)),
            ],
        )
        .unwrap();
    }
    s.commit().unwrap();
}

fn high_water(db: &Database) -> u64 {
    db.leak_counters().unwrap().1
}

#[test]
fn extent_segments_verify_at_every_stage() {
    let (db, _g) = open();
    fill(&db, 40_000);
    db.verify().expect("verify: rows only");

    // Build: multiple 512 KiB blocks per column land as extents.
    db.compact_columns().unwrap();
    db.verify().expect("verify: after build");

    // Re-compaction replaces every block (delete old record -> auto-free its
    // run, write new). The verifier must still balance.
    db.compact_columns().unwrap();
    db.verify().expect("verify: after recompaction");

    // Drop frees every run; the extent map returns to what it was pre-build.
    db.drop_column_segments().unwrap();
    db.verify().expect("verify: after drop");
}

#[test]
fn repeated_build_drop_does_not_leak_high_water() {
    let (db, _g) = open();
    fill(&db, 40_000);

    // One warm-up cycle so the freelist has the runs to hand back; then the
    // high-water must stay flat across further cycles (freed extents reused,
    // not re-grown every time — the leak the read-only-refill discipline and
    // the auto-free path together guarantee).
    db.compact_columns().unwrap();
    db.drop_column_segments().unwrap();
    let hw = high_water(&db);

    for _ in 0..4 {
        db.compact_columns().unwrap();
        db.verify().unwrap();
        db.drop_column_segments().unwrap();
        db.verify().unwrap();
    }
    assert_eq!(
        high_water(&db),
        hw,
        "columnar build/drop cycle grew the file (extent leak)"
    );
}
