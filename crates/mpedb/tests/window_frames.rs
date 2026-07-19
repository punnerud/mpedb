//! Explicit window frames (design/DESIGN-WINDOW.md stage 2, PLAN_FORMAT 36) —
//! differential against the `sqlite3` CLI (3.45).
//!
//! Ships explicit `{ROWS | RANGE | GROUPS} BETWEEN <bound> AND <bound>` (and the
//! `{…} <bound>` shorthand) on aggregate windows and on
//! `first_value`/`last_value`/`nth_value` (the functions whose result depends on
//! the frame). Every case here is cross-checked against sqlite, cell for cell,
//! since the frame semantics are exactly where an engine gets ties / NULLs /
//! peer-grouping / empty-frame edges wrong:
//!   - ROWS is a PHYSICAL row offset (a total ORDER BY is used so both engines
//!     agree on row order among ties);
//!   - RANGE (with UNBOUNDED/CURRENT ROW bounds) and GROUPS are LOGICAL — peers
//!     (rows equal on the ORDER BY) are framed together;
//!   - an empty frame yields NULL for sum/min/max/avg and 0 for count.
//!
//! The refused shapes (RANGE with an offset, a frame on a ranking/offset
//! function, an order-dependent frame without ORDER BY, an inverted frame) are
//! asserted to be clean prepare-time errors, never a wrong answer.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

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

fn shm_path(tag: &str) -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let p = format!(
        "{dir}/mpedb-wframe-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&p);
    p
}

/// `id` PK (a TOTAL order for ROWS frames); `grp` a nullable partition key (a
/// NULL row exercises the "NULLs form one partition" rule); `val` a nullable INT
/// ORDER key WITH TIES and NULLs (peer grouping for RANGE/GROUPS, NULLS-FIRST for
/// ordering); `amt` a non-null INT to aggregate.
const DATA: &[(i64, Option<&str>, Option<i64>, i64)] = &[
    (1, Some("a"), Some(10), 5),
    (2, Some("a"), Some(20), 7),
    (3, Some("a"), Some(20), 3), // tie on val within 'a'
    (4, Some("b"), Some(5), 2),
    (5, Some("b"), Some(5), 4), // tie on val within 'b'
    (6, Some("b"), Some(15), 1),
    (7, None, Some(30), 9),  // NULL partition
    (8, None, None, 6),      // NULL partition AND NULL order key
    (9, Some("a"), None, 8), // NULL order key inside 'a'
    (10, Some("a"), Some(20), 2), // another tie on 20 within 'a'
    (11, Some("b"), Some(5), 5),  // another tie on 5 within 'b'
];

fn insert_statements() -> Vec<String> {
    DATA.iter()
        .map(|(id, grp, val, amt)| {
            let g = grp.map_or("NULL".to_string(), |x| format!("'{x}'"));
            let v = val.map_or("NULL".to_string(), |x| x.to_string());
            format!("INSERT INTO t (id, grp, val, amt) VALUES ({id}, {g}, {v}, {amt})")
        })
        .collect()
}

const CREATE_T: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER, amt INTEGER);";

fn db() -> Tmp {
    let path = shm_path("t");
    let schema = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "grp"
  type = "text"
  nullable = true
  [[table.column]]
  name = "val"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "amt"
  type = "int64"
"#;
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{schema}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("unexpected value in frame test: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from(CREATE_T);
    script.push('\n');
    for stmt in insert_statements() {
        script.push_str(&stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    sqlite_oracle::script_stdout(&script, "")
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Integer/text/NULL query: compare cell for cell.
fn assert_same(d: &Database, query: &str) {
    let got = mpedb_rows(d, query);
    let want = sqlite_rows(query);
    assert_eq!(got, want, "mismatch on `{query}`");
}

/// Float query (avg/total): the LAST column is compared numerically.
fn assert_same_float(d: &Database, query: &str) {
    let got: Vec<Option<f64>> = match d.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|mut r| match r.pop() {
                Some(Value::Float(f)) => Some(f),
                Some(Value::Int(i)) => Some(i as f64),
                Some(Value::Null) | None => None,
                other => panic!("unexpected float cell {other:?} for `{query}`"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    };
    let want: Vec<Option<f64>> = sqlite_rows(query)
        .into_iter()
        .map(|mut r| {
            let cell = r.pop().unwrap_or_default();
            if cell.is_empty() {
                None
            } else {
                Some(cell.parse::<f64>().unwrap_or_else(|_| panic!("bad float `{cell}`")))
            }
        })
        .collect();
    assert_eq!(got.len(), want.len(), "row count mismatch on `{query}`");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        match (g, w) {
            (None, None) => {}
            (Some(g), Some(w)) => assert!(
                (g - w).abs() < 1e-9,
                "row {i}: mpedb {g} vs sqlite {w} on `{query}`"
            ),
            _ => panic!("row {i}: NULL mismatch ({g:?} vs {w:?}) on `{query}`"),
        }
    }
}

// ---- ROWS frames (physical offsets over a TOTAL order) ---------------------

#[test]
fn rows_frames_match_sqlite() {
    let d = db();
    for q in [
        // Running sum: UNBOUNDED PRECEDING → CURRENT ROW.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // Centered moving window: 1 PRECEDING → 1 FOLLOWING.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        // Trailing suffix: CURRENT ROW → UNBOUNDED FOLLOWING.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
        // Wider trailing window.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // Shorthand `ROWS 2 PRECEDING` ≡ BETWEEN 2 PRECEDING AND CURRENT ROW.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS 2 PRECEDING) FROM t ORDER BY id",
        // Look-ahead only.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 3 FOLLOWING) FROM t ORDER BY id",
        // Look-behind only.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN 3 PRECEDING AND 1 PRECEDING) FROM t ORDER BY id",
        // Equal-rank, non-empty: 2 PRECEDING AND 1 PRECEDING.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING) FROM t ORDER BY id",
        // Equal-rank, ALWAYS empty (start index past end): 1 PRECEDING AND 2 PRECEDING ⇒ NULL / 0.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 2 PRECEDING) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 2 PRECEDING) FROM t ORDER BY id",
        // Whole partition via a fully-unbounded ROWS frame.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
        // A huge (near-i64::MAX) offset must clamp to the partition edge, not overflow.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN 9223372036854775807 PRECEDING AND 9223372036854775807 FOLLOWING) FROM t ORDER BY id",
        // min / max / count over a sliding ROWS frame.
        "SELECT id, min(amt) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, max(amt) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        // count(x) SKIPS NULLs even inside the frame (val has NULLs).
        "SELECT id, count(val) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, sum(val) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // DESC total order.
        "SELECT id, sum(amt) OVER (ORDER BY id DESC ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

#[test]
fn rows_frames_partitioned_match_sqlite() {
    let d = db();
    for q in [
        // Partitioned running sum with a total per-partition order.
        "SELECT id, sum(amt) OVER (PARTITION BY grp ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, sum(amt) OVER (PARTITION BY grp ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, min(val) OVER (PARTITION BY grp ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (PARTITION BY grp ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING) FROM t ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

// ---- GROUPS frames (peer-group offsets) ------------------------------------

#[test]
fn groups_frames_match_sqlite() {
    let d = db();
    for q in [
        // Peer groups on a tied order key: 1 PRECEDING counts one group back.
        "SELECT id, val, sum(amt) OVER (ORDER BY val GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, val, count(*) OVER (ORDER BY val GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, val, sum(amt) OVER (ORDER BY val GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, val, sum(amt) OVER (ORDER BY val GROUPS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
        "SELECT id, val, sum(amt) OVER (ORDER BY val GROUPS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // Shorthand.
        "SELECT id, val, sum(amt) OVER (ORDER BY val GROUPS 1 PRECEDING) FROM t ORDER BY id",
        // Offset both sides (all FOLLOWING).
        "SELECT id, val, sum(amt) OVER (ORDER BY val GROUPS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM t ORDER BY id",
        // Partitioned GROUPS.
        "SELECT id, val, sum(amt) OVER (PARTITION BY grp ORDER BY val GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // Multi-key ORDER BY (peers = equal on BOTH keys).
        "SELECT id, sum(amt) OVER (ORDER BY grp, val GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // GROUPS with NO ORDER BY: one peer group ⇒ whole partition, deterministic.
        "SELECT id, sum(amt) OVER (PARTITION BY grp GROUPS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // DESC peer ordering.
        "SELECT id, val, sum(amt) OVER (ORDER BY val DESC GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

// ---- RANGE frames (UNBOUNDED / CURRENT ROW bounds only) --------------------

#[test]
fn range_frames_match_sqlite() {
    let d = db();
    for q in [
        // The default-frame equivalent, written explicitly: cumulative through
        // the current peer group (ties share the value).
        "SELECT id, val, sum(amt) OVER (ORDER BY val RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // Suffix from the current peer group to the end.
        "SELECT id, val, sum(amt) OVER (ORDER BY val RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
        // Just the current peer group.
        "SELECT id, val, sum(amt) OVER (ORDER BY val RANGE BETWEEN CURRENT ROW AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, val, count(*) OVER (ORDER BY val RANGE BETWEEN CURRENT ROW AND CURRENT ROW) FROM t ORDER BY id",
        // Whole partition.
        "SELECT id, sum(amt) OVER (PARTITION BY grp ORDER BY val RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
        // Partitioned cumulative.
        "SELECT id, val, sum(amt) OVER (PARTITION BY grp ORDER BY val RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // RANGE with no ORDER BY (one peer group ⇒ whole partition).
        "SELECT id, sum(amt) OVER (PARTITION BY grp RANGE BETWEEN CURRENT ROW AND CURRENT ROW) FROM t ORDER BY id",
        // DESC.
        "SELECT id, val, sum(amt) OVER (ORDER BY val DESC RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

// ---- value functions whose result depends on the frame ---------------------

#[test]
fn value_functions_under_frames_match_sqlite() {
    let d = db();
    for q in [
        // first_value/last_value/nth_value follow the frame edges.
        "SELECT id, first_value(amt) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, last_value(amt) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, nth_value(amt, 2) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        // last_value with the full running frame (a classic gotcha vs the default).
        "SELECT id, last_value(amt) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // last_value with the whole partition (the "look ahead" fix).
        "SELECT id, last_value(amt) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
        // nth_value past the frame end ⇒ NULL.
        "SELECT id, nth_value(amt, 3) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING) FROM t ORDER BY id",
        // first_value over an EMPTY frame ⇒ NULL.
        "SELECT id, first_value(amt) OVER (ORDER BY id ROWS BETWEEN 2 FOLLOWING AND 3 FOLLOWING) FROM t ORDER BY id",
        // Value functions under GROUPS / RANGE.
        "SELECT id, val, first_value(amt) OVER (ORDER BY val GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, val, last_value(amt) OVER (ORDER BY val RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, val, nth_value(amt, 2) OVER (ORDER BY val GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

// ---- float aggregates under frames (numeric compare) -----------------------

#[test]
fn float_aggregates_under_frames_match_sqlite() {
    let d = db();
    for q in [
        "SELECT id, avg(amt) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, total(amt) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, avg(val) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, avg(amt) OVER (ORDER BY val GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, avg(amt) OVER (ORDER BY val RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // avg over an empty frame ⇒ NULL.
        "SELECT id, avg(amt) OVER (ORDER BY id ROWS BETWEEN 3 FOLLOWING AND 5 FOLLOWING) FROM t ORDER BY id",
    ] {
        assert_same_float(&d, q);
    }
}

// ---- refusals: each a clean prepare-time error, never a wrong answer -------

#[test]
fn brittle_and_unsupported_frames_are_refused() {
    let d = db();
    for q in [
        // RANGE with a PRECEDING/FOLLOWING offset — value arithmetic refused.
        "SELECT id, sum(amt) OVER (ORDER BY val RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT id, sum(amt) OVER (ORDER BY val RANGE BETWEEN CURRENT ROW AND 5 FOLLOWING) FROM t",
        // A frame on a function that does not take one.
        "SELECT id, row_number() OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT id, rank() OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT id, lag(amt) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT id, ntile(3) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        // Order-dependent ROWS frame with NO ORDER BY.
        "SELECT id, sum(amt) OVER (ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT id, sum(amt) OVER (PARTITION BY grp ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
        // Inverted / illegal boundaries.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM t",
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW) FROM t",
        // Start UNBOUNDED FOLLOWING / end UNBOUNDED PRECEDING.
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) FROM t",
        "SELECT id, sum(amt) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND UNBOUNDED PRECEDING) FROM t",
        // Named windows are still refused (stage 3).
        "SELECT id, sum(amt) OVER w FROM t WINDOW w AS (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)",
    ] {
        assert!(
            d.query(q, &[]).is_err(),
            "expected a clean refusal for `{q}`, but it was accepted"
        );
    }
}

/// A refused-in-mpedb frame that sqlite ACCEPTS must still be a clean error, not
/// a panic or a silent wrong answer — the fallback that keeps the 0-wrong-answer
/// contract intact when we ship a subset.
#[test]
fn order_dependent_no_order_by_is_refused_not_guessed() {
    let d = db();
    // sqlite computes this (in its own row order); mpedb refuses rather than
    // return an order-dependent answer.
    let q = "SELECT id, sum(amt) OVER (ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t";
    let err = d.query(q, &[]).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("ORDER BY"),
        "refusal should explain the missing ORDER BY, got: {msg}"
    );
}
