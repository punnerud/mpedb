//! SQL window VALUE / OFFSET functions (design/DESIGN-WINDOW.md stage 2) —
//! differential against the `sqlite3` CLI (3.45).
//!
//! Ships: `lag(expr[, offset[, default]])`, `lead(expr[, offset[, default]])`,
//! `first_value(expr)`, `last_value(expr)`, `nth_value(expr, n)` with
//! `OVER ([PARTITION BY …] [ORDER BY …])` and the default frame. Every case here
//! is cross-checked against sqlite, which is where the frame / peer-group / NULL
//! rules are easy to get wrong:
//!   * lag/lead are frame-INDEPENDENT physical-row offsets (out of range ⇒
//!     default, or NULL);
//!   * first_value is the partition's first row (frame starts UNBOUNDED
//!     PRECEDING);
//!   * last_value is the current row's PEER-GROUP end under the default RANGE
//!     frame (so tied rows share it), or the partition end with no ORDER BY;
//!   * nth_value(expr, n) is the fixed n-th row, visible only once the growing
//!     frame reaches it.
//!
//! The out-of-scope forms (explicit frames, named `WINDOW w AS`, `FILTER`, a
//! non-constant/non-integer offset, `n < 1`, and a value function used without
//! `OVER`) are asserted to be REFUSED — the 0-wrong-answer contract means "clean
//! error", never a differing answer. (ntile/percent_rank/cume_dist now ship —
//! see `window3.rs`.)
//!
//! (The plan-bytes truncation sweep for the format-34 window bytes lives with the
//! other decoder truncation tests, in `mpedb-sql`'s `plan::tests`.)

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database, so a panicking test does not leak a `/dev/shm` file.
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
        "{dir}/mpedb-window2-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&p);
    p
}

// ---- fixture ---------------------------------------------------------------

/// `id` PK; `g` a nullable TEXT partition key (with a NULL-partition row); `k` a
/// nullable INT order key (with ties AND NULLs, to exercise peer groups +
/// NULLS-FIRST); `v` a nullable INT value (a NULL value row); `s` a nullable
/// TEXT value (so first/last/nth are tested on text too).
/// One fixture row: `(id, g, k, v, s)`.
type Row = (i64, Option<&'static str>, Option<i64>, Option<i64>, Option<&'static str>);

const ROWS: &[Row] = &[
    (1, Some("a"), Some(10), Some(100), Some("x")),
    (2, Some("a"), Some(20), Some(200), Some("y")),
    (3, Some("a"), Some(20), Some(300), Some("z")), // tie on k=20 within 'a'
    (4, Some("a"), None, None, None),               // NULL order key AND NULL values in 'a'
    (5, Some("b"), Some(5), Some(50), Some("p")),
    (6, Some("b"), Some(5), Some(60), Some("q")), // tie on k=5 within 'b'
    (7, Some("b"), Some(15), Some(70), Some("r")),
    (8, None, Some(30), Some(80), Some("m")), // NULL partition
    (9, None, None, Some(90), Some("n")),     // NULL partition, NULL order key
];

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, g, k, v, s)| {
            let gg = g.map_or("NULL".to_string(), |x| format!("'{x}'"));
            let kk = k.map_or("NULL".to_string(), |x| x.to_string());
            let vv = v.map_or("NULL".to_string(), |x| x.to_string());
            let ss = s.map_or("NULL".to_string(), |x| format!("'{x}'"));
            format!("INSERT INTO w2 (id, g, k, v, s) VALUES ({id}, {gg}, {kk}, {vv}, {ss})")
        })
        .collect()
}

const CREATE: &str =
    "CREATE TABLE w2 (id INTEGER PRIMARY KEY, g TEXT, k INTEGER, v INTEGER, s TEXT);";

fn db() -> Tmp {
    let path = shm_path("w2");
    let schema = r#"[[table]]
name = "w2"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "g"
  type = "text"
  nullable = true
  [[table.column]]
  name = "k"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "v"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "s"
  type = "text"
  nullable = true
"#;
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{schema}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

/// Canonical cell rendering matching the `sqlite3` CLI's default "list" mode:
/// NULL as empty, ints/text verbatim.
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("unexpected value in window test: {other:?}"),
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

/// Run a full script (schema + data + one query) through the `sqlite3` CLI and
/// parse its default list-mode output into rows.
fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from(CREATE);
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

/// Assert mpedb and sqlite agree, cell for cell, on an int/text/NULL query.
fn assert_same(d: &Database, query: &str) {
    let got = mpedb_rows(d, query);
    let want = sqlite_rows(query);
    assert_eq!(got, want, "mismatch on `{query}`");
}

#[test]
fn lag_lead_match_sqlite() {
    let d = db();
    for q in [
        // Default offset 1, partitioned — NULL at the partition start; peers do
        // NOT collapse (physical row offset).
        "SELECT id, lag(v) OVER (PARTITION BY g ORDER BY k, id) FROM w2 ORDER BY id",
        "SELECT id, lead(v) OVER (PARTITION BY g ORDER BY k, id) FROM w2 ORDER BY id",
        // Explicit offset + default (the default is evaluated at the CURRENT row).
        "SELECT id, lag(v, 2, -1) OVER (PARTITION BY g ORDER BY k, id) FROM w2 ORDER BY id",
        "SELECT id, lead(v, 2, -9) OVER (PARTITION BY g ORDER BY k, id) FROM w2 ORDER BY id",
        // A default that references the current row's own column.
        "SELECT id, lag(v, 5, id) OVER (ORDER BY k, id) FROM w2 ORDER BY id",
        // offset 0 = the current row; a negative offset looks the other way.
        "SELECT id, lag(v, 0) OVER (ORDER BY k, id) FROM w2 ORDER BY id",
        "SELECT id, lag(v, -1) OVER (ORDER BY k, id) FROM w2 ORDER BY id",
        "SELECT id, lead(v, -2, 0) OVER (ORDER BY k, id) FROM w2 ORDER BY id",
        // Whole table (one partition). With ties in the window ORDER BY, the tie
        // order is gather (PK) order — matching sqlite's rowid order.
        "SELECT id, lag(v) OVER (ORDER BY k) FROM w2 ORDER BY id",
        // No ORDER BY: lag/lead walk the partition in scan (gather = PK) order,
        // which is sqlite's rowid order for a plain scan.
        "SELECT id, lag(v) OVER (), lead(v) OVER () FROM w2 ORDER BY id",
        // DESC ordering (NULLS move last).
        "SELECT id, lag(v) OVER (PARTITION BY g ORDER BY k DESC, id) FROM w2 ORDER BY id",
        // lag on a TEXT value.
        "SELECT id, lag(s) OVER (PARTITION BY g ORDER BY k, id) FROM w2 ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

#[test]
fn first_last_nth_value_match_sqlite() {
    let d = db();
    for q in [
        // first_value = partition's first row (constant across the partition).
        "SELECT id, first_value(v) OVER (PARTITION BY g ORDER BY k, id) FROM w2 ORDER BY id",
        // last_value = the current row's PEER-GROUP end under the default RANGE
        // frame — tied rows share it. Here `k` alone (with real ties) exercises
        // the peer group; adding id would make every group a singleton.
        "SELECT id, last_value(v) OVER (PARTITION BY g ORDER BY k) FROM w2 ORDER BY id",
        // nth_value: the fixed n-th row, visible only once the frame reaches it.
        "SELECT id, nth_value(v, 2) OVER (PARTITION BY g ORDER BY k) FROM w2 ORDER BY id",
        "SELECT id, nth_value(v, 3) OVER (PARTITION BY g ORDER BY k) FROM w2 ORDER BY id",
        // On TEXT values.
        "SELECT id, first_value(s) OVER (PARTITION BY g ORDER BY k, id) FROM w2 ORDER BY id",
        "SELECT id, last_value(s) OVER (PARTITION BY g ORDER BY k) FROM w2 ORDER BY id",
        // Whole table, one partition (total order via id so the peer groups are
        // singletons ⇒ last_value is the current row, nth is the fixed row).
        "SELECT id, first_value(v) OVER (ORDER BY k, id) FROM w2 ORDER BY id",
        "SELECT id, last_value(v) OVER (ORDER BY k, id) FROM w2 ORDER BY id",
        "SELECT id, nth_value(v, 4) OVER (ORDER BY k, id) FROM w2 ORDER BY id",
        // No ORDER BY ⇒ the frame is the WHOLE partition: first/last/nth are all
        // constant across the partition.
        "SELECT id, first_value(v) OVER (PARTITION BY g) FROM w2 ORDER BY id",
        "SELECT id, last_value(v) OVER (PARTITION BY g) FROM w2 ORDER BY id",
        "SELECT id, nth_value(v, 2) OVER (PARTITION BY g) FROM w2 ORDER BY id",
        // DESC ordering.
        "SELECT id, first_value(v) OVER (PARTITION BY g ORDER BY k DESC, id) FROM w2 ORDER BY id",
        "SELECT id, last_value(v) OVER (PARTITION BY g ORDER BY k DESC) FROM w2 ORDER BY id",
        // n larger than any frame ⇒ NULL everywhere.
        "SELECT id, nth_value(v, 99) OVER (PARTITION BY g ORDER BY k) FROM w2 ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

#[test]
fn value_functions_combine_and_order_match_sqlite() {
    let d = db();
    for q in [
        // Several value windows + a ranking + an aggregate in one SELECT.
        "SELECT id, lag(v) OVER (PARTITION BY g ORDER BY k, id), \
         first_value(v) OVER (PARTITION BY g ORDER BY k, id), \
         nth_value(v, 2) OVER (PARTITION BY g ORDER BY k), \
         row_number() OVER (PARTITION BY g ORDER BY k, id), \
         sum(v) OVER (PARTITION BY g ORDER BY k) FROM w2 ORDER BY id",
        // The SAME window reused across two calls shares one slot (computed once).
        "SELECT id, lag(v) OVER (PARTITION BY g ORDER BY k, id), \
         lead(v) OVER (PARTITION BY g ORDER BY k, id) FROM w2 ORDER BY id",
        // A value window in the outer ORDER BY that is NOT selected (junk column);
        // NULLS sort first, tie-broken by id for a total order.
        "SELECT id FROM w2 ORDER BY lag(v) OVER (PARTITION BY g ORDER BY k, id), id",
        // LIMIT/OFFSET apply AFTER the window phase.
        "SELECT id, first_value(v) OVER (ORDER BY k, id) FROM w2 ORDER BY id LIMIT 4 OFFSET 2",
        // Computed after a WHERE (over the surviving rows only).
        "SELECT id, lag(v) OVER (PARTITION BY g ORDER BY k, id) FROM w2 WHERE v IS NOT NULL ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

/// Everything still out of scope must be REFUSED with a clean error — never a
/// differing answer. Each of these is a `prepare`/parse/plan-time rejection.
#[test]
fn out_of_scope_forms_are_refused() {
    let d = db();
    for q in [
        // A frame on lag/lead is refused (sqlite silently ignores it; the frame
        // is meaningless for a physical-offset function). first_value/last_value/
        // nth_value DO take a frame now — see `window_frames.rs`.
        "SELECT id, lag(v) OVER (ORDER BY k ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w2",
        // (ntile/percent_rank/cume_dist now ship — see `window3.rs`; explicit
        // frames now ship — see `window_frames.rs`.)
        // Named window and FILTER.
        "SELECT id, lag(v) OVER w FROM w2 WINDOW w AS (ORDER BY k)",
        "SELECT id, count(v) FILTER (WHERE v > 0) OVER (ORDER BY k) FROM w2",
        // A non-constant / non-integer offset — sqlite's per-row coercion is not
        // reproducible, so it is refused (never guessed).
        "SELECT id, lag(v, k) OVER (ORDER BY id) FROM w2",
        "SELECT id, lag(v, 1.5) OVER (ORDER BY id) FROM w2",
        // nth_value's n must be a positive integer constant.
        "SELECT id, nth_value(v, 0) OVER (ORDER BY id) FROM w2",
        "SELECT id, nth_value(v, -1) OVER (ORDER BY id) FROM w2",
        // A value function used without OVER is not a scalar function.
        "SELECT first_value(v) FROM w2",
        "SELECT lag(v, 1) FROM w2",
    ] {
        assert!(
            d.query(q, &[]).is_err(),
            "expected `{q}` to be REFUSED (out of scope), but it succeeded"
        );
    }
}
