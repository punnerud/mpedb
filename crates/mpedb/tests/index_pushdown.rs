//! LIMIT and the residual filter, pushed INTO the index walk.
//!
//! The index access paths used to materialize their whole matching range and
//! let the caller truncate. `WHERE hex = 'x' LIMIT 1` therefore paid for every
//! row equal on `hex`, which on a low-cardinality column is a large slice of
//! the table. Measured on a 945 234-row track table before the fix: 528 MB
//! peak RSS to return ONE row, against 7,5 MB for the same one row reached by
//! table scan — a path that has had `scan_rows_capped` all along.
//!
//! Peak RSS is a poor thing to assert in a test: it moves with the allocator,
//! the page cache and the machine. The engine's #74 work meter is the same
//! property made deterministic — one charge per index entry visited — so a
//! budget the un-pushed version cannot fit is a precise statement that the
//! walk stopped early. That is the pattern `agg_over_index.rs` already uses
//! for exactly this kind of claim.

use mpedb::{Database, ExecResult, Value};
use mpedb_types::Config;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};

static UNIQ: AtomicU32 = AtomicU32::new(0);

struct Tmp {
    db: Database,
    path: String,
}
impl Deref for Tmp {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

/// A database with a work budget, then `t` filled at runtime: `g` is a
/// low-cardinality indexed column (four distinct values over `n` rows — the
/// shape that makes an equality probe expensive), `r` an indexed range column,
/// and `pad` an unindexed column for the residual filter to test.
fn open(tag: &str, max_work_rows: u64, n: i64) -> Tmp {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-idxpush-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = {max_work_rows}\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    // The budget must not swallow the setup, so build under a fresh handle's
    // write transaction and let the reads below meet it.
    for d in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER NOT NULL, \
         r INTEGER NOT NULL, pad INTEGER NOT NULL)",
        "CREATE INDEX t_g ON t (g)",
        "CREATE INDEX t_r ON t (r)",
    ] {
        db.query(d, &[]).unwrap();
    }
    let mut w = db.begin().unwrap();
    for i in 1..=n {
        w.query(
            "INSERT INTO t (id, g, r, pad) VALUES ($1, $2, $3, $4)",
            &[Value::Int(i), Value::Int(i % 4), Value::Int(i), Value::Int(i % 10)],
        )
        .unwrap();
    }
    w.commit().unwrap();
    Tmp { db, path }
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// An equality probe on a low-cardinality index with `LIMIT 1` charges for the
/// rows it RETURNS, not for the rows that share the value.
///
/// `g` takes four values over 400 rows, so `g = 1` matches 100. Under the old
/// code the probe drained all 100 before the caller kept one; a budget of 5
/// therefore refused a query that has one row to give.
#[test]
fn an_equality_probe_under_limit_stops_at_the_limit() {
    let d = open("point", 5, 400);
    let got = rows(d.query("SELECT id FROM t WHERE g = 1 LIMIT 1", &[]).unwrap());
    assert_eq!(got.len(), 1, "one row, within a budget of 5");

    // …and the same probe without a LIMIT still costs what it always did:
    // 100 matching rows do not fit a budget of 5. Without this the test would
    // pass on a build that had simply stopped charging.
    assert!(
        d.query("SELECT id FROM t WHERE g = 1", &[]).is_err(),
        "the full probe must still meet the budget it always met"
    );
}

/// The same for a range: `LIMIT k` over an indexed range walks k entries.
///
/// No `ORDER BY` here, deliberately. A sort has to see every row before it can
/// name the first three, so the cap cannot reach the scan through one — the
/// budget below would trip, and rightly. What this pins is the access path's
/// own cost when nothing above it needs the whole input.
#[test]
fn an_index_range_under_limit_stops_at_the_limit() {
    let d = open("range", 5, 400);
    let got = rows(d.query("SELECT id FROM t WHERE r > 10 LIMIT 3", &[]).unwrap());
    assert_eq!(got.len(), 3, "three rows, within a budget of 5 out of 390 matches");
}

/// …and the rows are the RIGHT ones. A cheaper plan that answers differently
/// is not a fix, so this asks the same question with the order pinned and a
/// budget wide enough for the sort the `ORDER BY` requires.
#[test]
fn an_index_range_under_limit_returns_the_right_rows() {
    let d = open("rangeord", 400, 400);
    let got = rows(d.query("SELECT id FROM t WHERE r > 10 ORDER BY r LIMIT 3", &[]).unwrap());
    assert_eq!(
        got,
        vec![vec![Value::Int(11)], vec![Value::Int(12)], vec![Value::Int(13)]]
    );
}

/// A residual filter goes down WITH the cap, and this is the case that would
/// break if it did not: the cap counts rows the filter has KEPT.
///
/// `pad = 7` holds for one row in ten, so `LIMIT 2` needs about 20 entries.
/// Capping at 2 entries instead of 2 kept rows would return one row, or none —
/// a wrong answer that a memory measurement would have called a success.
#[test]
fn the_cap_counts_kept_rows_not_visited_ones() {
    let d = open("resid", 400, 400);
    let got = rows(
        d.query("SELECT id FROM t WHERE r > 0 AND pad = 7 ORDER BY r LIMIT 2", &[]).unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![Value::Int(7)], vec![Value::Int(17)]],
        "two rows that pass the filter, not the first two the index visited"
    );
}

/// No LIMIT, but a residual filter: the walk still must not build the rows it
/// is about to discard, and the answer must be every row that passes.
#[test]
fn a_filtered_range_returns_exactly_the_matches() {
    let d = open("filt", 400, 400);
    let got = rows(d.query("SELECT id FROM t WHERE r > 380 AND pad = 3 ORDER BY r", &[]).unwrap());
    assert_eq!(
        got,
        vec![vec![Value::Int(383)], vec![Value::Int(393)]],
        "381..400 filtered to pad = 3"
    );
}

/// An empty probe stays empty, and a probe on NULL matches nothing: a row with
/// a NULL in an indexed column has no index entry at all, so the streaming
/// path has to answer that the same way the materializing one did.
#[test]
fn null_and_empty_probes_are_unchanged() {
    let d = open("empty", 400, 40);
    assert!(rows(d.query("SELECT id FROM t WHERE g = 99", &[]).unwrap()).is_empty());
    assert!(rows(d.query("SELECT id FROM t WHERE g = NULL", &[]).unwrap()).is_empty());
    assert!(rows(d.query("SELECT id FROM t WHERE r > 1000000", &[]).unwrap()).is_empty());
}
