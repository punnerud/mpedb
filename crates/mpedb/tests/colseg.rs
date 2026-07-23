//! Column segments (DESIGN-COLUMNAR stage 1): a segment-fed aggregate must
//! return BIT-IDENTICAL answers to the row scan, and must decline — silently,
//! back to the row scan — the moment the table changes under it.

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
        "mpedb-colseg-{name}-{}-{}.mpedb",
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
  name = "qty"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "amount"
  type = "float64"
  nullable = true
  [[table.column]]
  name = "label"
  type = "text"
  nullable = true
"#,
        path.display()
    );
    (
        Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        Guard(path),
    )
}

fn one(db: &Database, sql: &str) -> Vec<Value> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows.into_iter().next().unwrap_or_default(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Compare bitwise: a float sum that merely rounds the same is NOT the claim —
/// the claim is the identical value, which only holds if the segment feeds the
/// accumulators in the row scan's order.
fn assert_same(a: &[Value], b: &[Value], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: arity");
    for (x, y) in a.iter().zip(b) {
        match (x, y) {
            (Value::Float(p), Value::Float(q)) => {
                assert_eq!(p.to_bits(), q.to_bits(), "{what}: float bits")
            }
            _ => assert_eq!(x, y, "{what}"),
        }
    }
}

fn seed(db: &Database, n: i64) {
    let h = db
        .prepare("INSERT INTO fact (id, qty, amount, label) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let mut s = db.begin().unwrap();
    let mut x = 0x9E37_79B9u64 | 1;
    for i in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // Every 17th row is NULL in both nullable columns — the bitmap has to
        // restore the exact interleaving or the aggregate shifts.
        let (q, a) = if i % 17 == 0 {
            (Value::Null, Value::Null)
        } else {
            (
                Value::Int((x % 1000) as i64 - 500),
                Value::Float((x >> 5) as f64 / 1000.0),
            )
        };
        s.execute(&h, &[Value::Int(i), q, a, Value::Text(format!("r{i}"))])
            .unwrap();
    }
    s.commit().unwrap();
}

const AGGS: &[&str] = &[
    "SELECT sum(qty) FROM fact",
    "SELECT count(qty) FROM fact",
    "SELECT min(qty), max(qty) FROM fact",
    "SELECT avg(qty) FROM fact",
    "SELECT sum(amount) FROM fact",
    "SELECT min(amount), max(amount) FROM fact",
    "SELECT avg(amount) FROM fact",
];

#[test]
fn segment_fed_aggregates_are_bit_identical_to_the_row_scan() {
    // Deliberately spans several blocks' worth of rows only in spirit (the
    // block is 65 536); the interleaving and NULL handling are what matter.
    let (db, _g) = open("identical");
    seed(&db, 70_000);

    let before: Vec<Vec<Value>> = AGGS.iter().map(|q| one(&db, q)).collect();
    let built = db.compact_columns().unwrap();
    assert!(
        built.iter().any(|s| s.column == "qty") && built.iter().any(|s| s.column == "amount"),
        "numeric columns get segments: {built:?}"
    );
    assert!(
        !built.iter().any(|s| s.column == "label"),
        "text is stage 3, not stage 1: {built:?}"
    );
    // More than one block, so the multi-block path is actually exercised.
    assert!(built.iter().any(|s| s.blocks > 1), "{built:?}");

    for (q, want) in AGGS.iter().zip(&before) {
        assert_same(&one(&db, q), want, q);
    }

    // And dropping them changes nothing but speed.
    db.drop_column_segments().unwrap();
    for (q, want) in AGGS.iter().zip(&before) {
        assert_same(&one(&db, q), want, q);
    }
}

#[test]
fn a_write_invalidates_the_segments_and_the_answer_stays_right() {
    let (db, _g) = open("stale");
    seed(&db, 5_000);
    db.compact_columns().unwrap();
    let before = one(&db, "SELECT sum(qty) FROM fact");

    // Every kind of mutation must make the stale segments unusable. If any of
    // these failed to invalidate, the assert below would read the OLD sum —
    // which is precisely the wrong answer mod_gen exists to prevent.
    db.query("INSERT INTO fact (id, qty, amount) VALUES (999001, 7, 1.5)", &[]).unwrap();
    let after_insert = one(&db, "SELECT sum(qty) FROM fact");
    assert_ne!(before, after_insert, "the insert must be visible");
    assert_eq!(after_insert, one(&db, "SELECT sum(qty) FROM fact"));

    db.query("UPDATE fact SET qty = 1000 WHERE id = 999001", &[]).unwrap();
    let after_update = one(&db, "SELECT sum(qty) FROM fact");
    assert_ne!(after_insert, after_update, "the update must be visible");

    db.query("DELETE FROM fact WHERE id = 999001", &[]).unwrap();
    assert_same(&one(&db, "SELECT sum(qty) FROM fact"), &before, "delete restores");

    // Rebuild against the new state, then confirm it is used and correct.
    db.compact_columns().unwrap();
    assert_same(&one(&db, "SELECT sum(qty) FROM fact"), &before, "rebuilt");
}

#[test]
fn segments_survive_reopen_and_another_handle_reads_them() {
    let (db, g) = open("reopen");
    seed(&db, 3_000);
    db.compact_columns().unwrap();
    let want = one(&db, "SELECT sum(qty), count(qty), min(qty) FROM fact");

    let db2 = Database::open_from_file(&g.0).unwrap();
    assert_same(
        &one(&db2, "SELECT sum(qty), count(qty), min(qty) FROM fact"),
        &want,
        "a second handle reads the same segments",
    );

    // A write through the OTHER handle must invalidate for this one too — the
    // generation lives in the file, not in a process.
    db2.query("INSERT INTO fact (id, qty) VALUES (888001, 5)", &[]).unwrap();
    let after = one(&db, "SELECT sum(qty) FROM fact");
    let want_sum = match (&want[0], ) {
        (Value::Int(v), ) => Value::Int(v + 5),
        _ => panic!("sum is an int"),
    };
    assert_eq!(after[0], want_sum, "the cross-process write is visible");
}
