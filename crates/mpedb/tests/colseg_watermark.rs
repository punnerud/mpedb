//! Stage 5: the moving boundary (DESIGN-COLUMNAR §7). Segments cover the first
//! `W` rows; rows appended ABOVE the watermark are served by a row-tail fold and
//! do not force a re-compaction, while any write AT OR BELOW the watermark
//! deletes it so the read falls back to the row scan. Every test proves the
//! answer is bit-identical to a pure row scan (obtained by dropping the segments
//! in the same binary) AND asserts, via the watermark observer, whether the
//! split scan was armed or correctly disarmed — so "right answer by lucky
//! fallback" cannot pass for "right answer by the split path".

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
        "mpedb-colwm-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 128
max_readers = 16

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

/// Seed rows with the given ids. `amount = id * 1.5`, `day_id = id % 100`.
fn insert(db: &Database, ids: impl Iterator<Item = i64>) {
    let mut s = db.begin().unwrap();
    for i in ids {
        s.query(
            "INSERT INTO fact (id, day_id, amount) VALUES ($1, $2, $3)",
            &[Value::Int(i), Value::Int(i % 100), Value::Float(i as f64 * 1.5)],
        )
        .unwrap();
    }
    s.commit().unwrap();
}

fn scalar(db: &Database, sql: &str) -> Value {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        o => panic!("{o:?}"),
    }
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        o => panic!("{o:?}"),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// The split scan's answer for `sql` (segments present) MUST equal the pure row
/// scan's (segments dropped) — bit-for-bit. Returns the shared value.
fn assert_split_equals_row(db: &Database, sql: &str) -> Value {
    let split = scalar(db, sql);
    db.drop_column_segments().unwrap();
    let row = scalar(db, sql);
    assert!(
        same(&split, &row),
        "split path {split:?} != row scan {row:?} for `{sql}`"
    );
    split
}

#[test]
fn appends_above_the_watermark_keep_it_and_stay_correct() {
    let (db, _g) = open("append");
    insert(&db, 0..3000);
    db.compact_columns().unwrap();
    // The watermark covers the 3000 built rows.
    assert_eq!(db.columnar_watermark_covered("fact").unwrap(), Some(3000));

    // Append 250 rows strictly above the max PK (2999).
    insert(&db, 3000..3250);

    // Still armed, still covering only the original 3000 — the appends live in
    // the row tail, not the segments.
    assert_eq!(
        db.columnar_watermark_covered("fact").unwrap(),
        Some(3000),
        "an append above the watermark must not invalidate it"
    );
    // And the aggregate over all 3250 rows is exactly the row scan's.
    assert_split_equals_row(&db, "SELECT sum(amount) FROM fact");
}

#[test]
fn covered_update_invalidates_the_watermark() {
    let (db, _g) = open("upd");
    insert(&db, 0..2000);
    db.compact_columns().unwrap();
    assert_eq!(db.columnar_watermark_covered("fact").unwrap(), Some(2000));

    // Update a row well below the watermark.
    let mut s = db.begin().unwrap();
    s.query(
        "UPDATE fact SET amount = $1 WHERE id = $2",
        &[Value::Float(-999.0), Value::Int(42)],
    )
    .unwrap();
    s.commit().unwrap();

    assert_eq!(
        db.columnar_watermark_covered("fact").unwrap(),
        None,
        "a covered-row UPDATE must delete the watermark"
    );
    // The row scan is the only source now, and it reflects the update.
    let got = scalar(&db, "SELECT sum(amount) FROM fact");
    // Manual reference: original sum minus 42*1.5 plus (-999).
    let orig: f64 = (0..2000).map(|i| i as f64 * 1.5).sum();
    let want = orig - 42.0 * 1.5 - 999.0;
    assert!(same(&got, &Value::Float(want)), "{got:?} vs {want}");
}

#[test]
fn covered_delete_invalidates_but_delete_above_keeps_it() {
    let (db, _g) = open("del");
    insert(&db, 0..2000);
    db.compact_columns().unwrap();

    // Append above the watermark, then DELETE one of the APPENDED rows: the
    // watermark stays (the deletion is above it; the tail re-reads fresh).
    insert(&db, 2000..2100);
    assert_eq!(db.columnar_watermark_covered("fact").unwrap(), Some(2000));
    let mut s = db.begin().unwrap();
    s.query("DELETE FROM fact WHERE id = $1", &[Value::Int(2050)]).unwrap();
    s.commit().unwrap();
    assert_eq!(
        db.columnar_watermark_covered("fact").unwrap(),
        Some(2000),
        "deleting an appended (above-watermark) row must NOT invalidate"
    );
    assert_split_equals_row(&db, "SELECT sum(amount) FROM fact");

    // Rebuild, then DELETE a COVERED row: the watermark goes.
    db.compact_columns().unwrap();
    assert!(db.columnar_watermark_covered("fact").unwrap().is_some());
    let mut s = db.begin().unwrap();
    s.query("DELETE FROM fact WHERE id = $1", &[Value::Int(7)]).unwrap();
    s.commit().unwrap();
    assert_eq!(
        db.columnar_watermark_covered("fact").unwrap(),
        None,
        "deleting a covered row must invalidate"
    );
}

#[test]
fn middle_insert_below_the_watermark_invalidates() {
    let (db, _g) = open("middle");
    // Even ids only, so odd ids below the max are free "middle" slots.
    insert(&db, (0..4000).filter(|i| i % 2 == 0));
    db.compact_columns().unwrap();
    let covered = db.columnar_watermark_covered("fact").unwrap();
    assert_eq!(covered, Some(2000));

    // Insert an odd id BELOW the watermark (max even id is 3998).
    insert(&db, std::iter::once(1234 + 1)); // 1235, below 3998
    assert_eq!(
        db.columnar_watermark_covered("fact").unwrap(),
        None,
        "an insert with a PK below the watermark must invalidate"
    );
    assert_split_equals_row(&db, "SELECT sum(amount) FROM fact");
}

#[test]
fn filtered_scan_uses_the_split_path() {
    let (db, _g) = open("filter");
    insert(&db, 0..5000);
    db.compact_columns().unwrap();
    insert(&db, 5000..5400); // appends above the watermark

    assert_eq!(db.columnar_watermark_covered("fact").unwrap(), Some(5000));
    // A range predicate on day_id, aggregate on amount: zone-pruned segments for
    // the first 5000 rows plus a filtered fold of the 400-row tail.
    assert_split_equals_row(
        &db,
        "SELECT sum(amount) FROM fact WHERE day_id >= 90",
    );
    assert_split_equals_row(
        &db,
        "SELECT count(*) FROM fact WHERE day_id < 10",
    );
}

#[test]
fn group_by_uses_the_split_path() {
    let (db, _g) = open("group");
    insert(&db, 0..5000);
    db.compact_columns().unwrap();
    insert(&db, 5000..5300); // appends above the watermark
    assert_eq!(db.columnar_watermark_covered("fact").unwrap(), Some(5000));

    let split = rows(&db, "SELECT day_id, sum(amount) FROM fact GROUP BY day_id ORDER BY day_id");
    db.drop_column_segments().unwrap();
    let row = rows(&db, "SELECT day_id, sum(amount) FROM fact GROUP BY day_id ORDER BY day_id");
    assert_eq!(split.len(), row.len(), "group count differs");
    for (a, b) in split.iter().zip(&row) {
        assert!(same(&a[0], &b[0]) && same(&a[1], &b[1]), "group cell differs: {a:?} vs {b:?}");
    }
}

#[test]
fn add_and_drop_column_invalidate_the_watermark() {
    let (db, _g) = open("ddl");
    insert(&db, 0..1500);
    db.compact_columns().unwrap();
    assert!(db.columnar_watermark_covered("fact").unwrap().is_some());
    db.query("ALTER TABLE fact ADD COLUMN note int64", &[]).unwrap();
    assert_eq!(
        db.columnar_watermark_covered("fact").unwrap(),
        None,
        "ADD COLUMN must disarm the tail scan"
    );

    // Rebuild, then DROP the added column.
    db.compact_columns().unwrap();
    assert!(db.columnar_watermark_covered("fact").unwrap().is_some());
    db.query("ALTER TABLE fact DROP COLUMN note", &[]).unwrap();
    assert_eq!(
        db.columnar_watermark_covered("fact").unwrap(),
        None,
        "DROP COLUMN must disarm the tail scan"
    );
    // The answer is still correct off the row scan.
    assert_split_equals_row(&db, "SELECT sum(amount) FROM fact");
}

#[test]
fn recompaction_moves_the_watermark_forward() {
    let (db, _g) = open("recompact");
    insert(&db, 0..2000);
    db.compact_columns().unwrap();
    insert(&db, 2000..3000);
    assert_eq!(db.columnar_watermark_covered("fact").unwrap(), Some(2000));

    // Re-compact: the boundary advances to cover the appended rows too.
    db.compact_columns().unwrap();
    assert_eq!(
        db.columnar_watermark_covered("fact").unwrap(),
        Some(3000),
        "re-compaction absorbs the tail into the segments"
    );
    assert_split_equals_row(&db, "SELECT sum(amount) FROM fact");
}

#[test]
fn empty_tail_is_the_plain_whole_table_answer() {
    // A watermark whose row tail is empty (no appends) must give exactly the
    // whole-table answer — the split path degenerates cleanly.
    let (db, _g) = open("emptytail");
    insert(&db, 0..1000);
    db.compact_columns().unwrap();
    assert_eq!(db.columnar_watermark_covered("fact").unwrap(), Some(1000));
    assert_split_equals_row(&db, "SELECT sum(amount) FROM fact");
    assert_split_equals_row(&db, "SELECT sum(amount) FROM fact WHERE day_id >= 50");
}
