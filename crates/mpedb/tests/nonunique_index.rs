//! #48: a non-unique secondary index (`indexed = true`) allows duplicate values
//! where `unique = true` would reject them, and the index stays page-consistent.
//! The SQL half: the planner picks the index for `WHERE indexed_col = ?` and the
//! executor returns EVERY match — pinned here because the first wiring of
//! `secondary_indexes` shipped without it, and `WHERE cid = ?` silently
//! returned 0 rows (exact-get against composite `(value ‖ pk)` keys).
use mpedb::{params, Config, Database, ExecResult, Value};
use std::ops::Deref;

struct Tmp { db: Database, path: String }
impl Deref for Tmp { type Target = Database; fn deref(&self) -> &Database { &self.db } }
impl Drop for Tmp { fn drop(&mut self) { let _ = std::fs::remove_file(&self.path); let _ = std::fs::remove_file(format!("{}-wal", self.path)); } }

fn db(tag: &str, col_attr: &str) -> Tmp {
    let path = format!("/dev/shm/mpedb-nuidx-{tag}-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let cfg = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\n\
         [[table]]\nname = \"orders\"\nprimary_key = [\"oid\"]\n  \
         [[table.column]]\n  name = \"oid\"\n  type = \"int64\"\n  \
         [[table.column]]\n  name = \"cid\"\n  type = \"int64\"\n  nullable = false\n  {col_attr}"
    );
    let db = Database::open_with_config(Config::from_toml_str(&cfg).unwrap()).unwrap();
    Tmp { db, path }
}

/// The headline: `indexed = true` builds a lookup index that ALLOWS duplicates.
/// Two orders for the same customer both insert; the index is maintained; page
/// accounting (which walks every index tree) stays consistent.
#[test]
fn indexed_allows_duplicates_and_stays_consistent() {
    let d = db("dup", "indexed = true");
    let ins = d.prepare("INSERT INTO orders (oid, cid) VALUES ($1, $2)").unwrap();
    d.execute(&ins, &params![1i64, 100i64]).unwrap();
    d.execute(&ins, &params![2i64, 100i64]).unwrap(); // same cid — must succeed
    d.execute(&ins, &params![3i64, 200i64]).unwrap();
    d.verify().expect("index + page accounting consistent after duplicate inserts");
    // delete one of the duplicates; the other and its index entry survive
    let del = d.prepare("DELETE FROM orders WHERE oid = $1").unwrap();
    d.execute(&del, &params![1i64]).unwrap();
    d.verify().expect("consistent after deleting one of a duplicate pair");
}

/// The guard: `unique = true` still rejects the second duplicate. The two must
/// not have collapsed into one behaviour by the composite-key change.
#[test]
fn unique_still_rejects_duplicates() {
    let d = db("uniq", "unique = true");
    let ins = d.prepare("INSERT INTO orders (oid, cid) VALUES ($1, $2)").unwrap();
    d.execute(&ins, &params![1i64, 100i64]).unwrap();
    let err = d.execute(&ins, &params![2i64, 100i64]).unwrap_err();
    assert!(
        matches!(err, mpedb::Error::UniqueViolation { .. }),
        "a UNIQUE column must still reject a duplicate: {err:?}"
    );
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn explain(d: &Database, sql: &str) -> String {
    match d.query(sql, &params![100i64]).unwrap() {
        ExecResult::Explain(text) => text,
        other => panic!("expected an explain rendering, got {other:?}"),
    }
}

fn seed(d: &Database) {
    // cids: 100, 100, 100, 200, 300 — three duplicates and two singles.
    let ins = d.prepare("INSERT INTO orders (oid, cid) VALUES ($1, $2)").unwrap();
    for (oid, cid) in [(1i64, 100i64), (2, 100), (3, 100), (4, 200), (5, 300)] {
        d.execute(&ins, &params![oid, cid]).unwrap();
    }
}

/// THE BUG THIS FILE PINS: `WHERE cid = ?` on an `indexed` column must return
/// every match — the first wiring returned 0 rows, silently. The plan must
/// actually use the index (EXPLAIN says IndexScan), so this cannot quietly
/// pass by falling back to a full scan.
#[test]
fn select_via_nonunique_index_returns_all_matches() {
    let d = db("sel", "indexed = true");
    seed(&d);
    let got = rows(d.query("SELECT oid FROM orders WHERE cid = $1 ORDER BY oid", &params![100i64]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)]]);

    let text = explain(&d, "EXPLAIN SELECT oid FROM orders WHERE cid = $1");
    assert!(
        text.contains("IndexScan(cid = $1) via index 1"),
        "the non-unique index must be chosen and rendered honestly: {text}"
    );

    // Aggregates ride the same access path.
    let n = rows(d.query("SELECT count(*) FROM orders WHERE cid = $1", &params![100i64]).unwrap());
    assert_eq!(n, vec![vec![Value::Int(3)]]);

    // A value with no matches is empty, not an error.
    let none = rows(d.query("SELECT oid FROM orders WHERE cid = $1", &params![999i64]).unwrap());
    assert!(none.is_empty());
}

/// `cid = NULL` is UNKNOWN: the probe returns nothing (NULLs are never indexed).
#[test]
fn null_probe_is_empty() {
    let d = db("null", "indexed = true");
    seed(&d);
    let got = rows(d.query("SELECT oid FROM orders WHERE cid = $1", &[Value::Null]).unwrap());
    assert!(got.is_empty(), "cid = NULL must match nothing: {got:?}");
}

/// ORDER BY + LIMIT over the index probe (the top-K path): all matches are
/// gathered, then sorted and capped.
#[test]
fn topk_over_nonunique_index() {
    let d = db("topk", "indexed = true");
    seed(&d);
    let got = rows(
        d.query(
            "SELECT oid FROM orders WHERE cid = $1 ORDER BY oid DESC LIMIT 2",
            &params![100i64],
        )
        .unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(3)], vec![Value::Int(2)]]);
}

/// UPDATE and DELETE route through the same gather: every matching row is
/// touched, and updating the INDEXED column itself exercises the old-key/new-key
/// maintenance pair under the composite layout.
#[test]
fn update_and_delete_via_nonunique_index() {
    let d = db("dml", "indexed = true");
    seed(&d);

    // Move every cid=100 row to cid=300: 3 rows must be hit, not 0 (the bug).
    let upd = d.prepare("UPDATE orders SET cid = $1 WHERE cid = $2").unwrap();
    match d.execute(&upd, &params![300i64, 100i64]).unwrap() {
        ExecResult::Affected(n) => assert_eq!(n, 3, "all three duplicates must be updated"),
        other => panic!("expected an affected count, got {other:?}"),
    }
    d.verify().expect("index consistent after moving duplicates to a new value");

    // The old value is gone; the new value now has 3 + the original 1 = 4 rows.
    assert!(rows(d.query("SELECT oid FROM orders WHERE cid = $1", &params![100i64]).unwrap()).is_empty());
    let moved = rows(d.query("SELECT oid FROM orders WHERE cid = $1 ORDER BY oid", &params![300i64]).unwrap());
    assert_eq!(moved.len(), 4);

    // DELETE via the index removes exactly the matches.
    let del = d.prepare("DELETE FROM orders WHERE cid = $1").unwrap();
    match d.execute(&del, &params![300i64]).unwrap() {
        ExecResult::Affected(n) => assert_eq!(n, 4),
        other => panic!("expected an affected count, got {other:?}"),
    }
    d.verify().expect("index consistent after deleting all matches");
    let left = rows(d.query("SELECT oid FROM orders", &[]).unwrap());
    assert_eq!(left, vec![vec![Value::Int(4)]]); // only cid=200 remains
}

/// ON CONFLICT can only probe a key that identifies ONE row: an `indexed`
/// (non-unique) column is refused at prepare — and the "usable here" hint must
/// not list it as if it were UNIQUE.
#[test]
fn on_conflict_on_indexed_column_is_refused() {
    let d = db("conf", "indexed = true");
    let err = d
        .prepare("INSERT INTO orders (oid, cid) VALUES ($1, $2) ON CONFLICT (cid) DO UPDATE SET cid = excluded.cid")
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("UNIQUE"), "the refusal must say why: {msg}");
    let usable = msg.split("Usable here:").nth(1).expect("the hint lists what IS usable");
    assert!(usable.contains("(oid)"), "the PK is usable and must be listed: {msg}");
    assert!(!usable.contains("(cid)"), "a non-unique column must NOT be offered as usable: {msg}");
}

/// The unique probe keeps its exact-get rendering — one row, `IndexPoint`.
#[test]
fn unique_probe_explains_as_index_point() {
    let d = db("uexp", "unique = true");
    let text = explain(&d, "EXPLAIN SELECT oid FROM orders WHERE cid = $1");
    assert!(
        text.contains("IndexPoint(cid = $1) via index 1"),
        "unique rendering must stay stable: {text}"
    );
}
