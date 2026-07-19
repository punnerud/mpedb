//! SQL window functions (design/DESIGN-WINDOW.md stage 1) — differential against the
//! `sqlite3` CLI (3.45).
//!
//! Ships: `row_number()`/`rank()`/`dense_rank()` and aggregate `OVER` (`sum`,
//! `count(*)`, `count(x)`, `min`, `max`, `avg`, `total`) with
//! `OVER ([PARTITION BY …] [ORDER BY …])` and the default frame. Every case here
//! is cross-checked against sqlite, which is where the tie / NULL / RANGE-peer
//! rules are easy to get wrong: `rank()` skips after ties (1,1,3), `dense_rank()`
//! does not (1,1,2), `row_number()` is always distinct, and an aggregate over the
//! default (RANGE) frame gives every peer the SAME cumulative value.
//!
//! (The plan-bytes truncation sweep for the new window list lives with the other
//! decoder truncation tests, in `mpedb-sql`'s `plan::tests`.)

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

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
        "{dir}/mpedb-window-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&p);
    p
}

// ---- the main single-table fixture ----------------------------------------

/// `id` PK; `grp` a nullable text PARTITION key (a NULL row exercises the
/// "NULLs form one partition" rule); `val` a nullable INT ORDER key (with ties,
/// and NULLs to exercise NULLS-FIRST ordering + the skip in aggregates); `amt`
/// a non-null INT for running aggregates.
const ROWS: &[(i64, Option<&str>, Option<i64>, i64)] = &[
    (1, Some("a"), Some(10), 5),
    (2, Some("a"), Some(20), 7),
    (3, Some("a"), Some(20), 3), // tie on val within 'a'
    (4, Some("b"), Some(5), 2),
    (5, Some("b"), Some(5), 4), // tie on val within 'b'
    (6, Some("b"), Some(15), 1),
    (7, None, Some(30), 9),  // NULL partition
    (8, None, None, 6),      // NULL partition AND NULL order key
    (9, Some("a"), None, 8), // NULL order key inside 'a'
];

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, grp, val, amt)| {
            let g = grp.map_or("NULL".to_string(), |x| format!("'{x}'"));
            let v = val.map_or("NULL".to_string(), |x| x.to_string());
            format!("INSERT INTO t (id, grp, val, amt) VALUES ({id}, {g}, {v}, {amt})")
        })
        .collect()
}

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

/// Canonical cell rendering matching the `sqlite3` CLI's default "list" mode:
/// NULL as empty, a boolean as sqlite's 1/0, ints/text verbatim. Floats are NOT
/// rendered here — they are compared numerically (see `float_diff`).
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
fn sqlite_rows(create: &str, inserts: &[String], query: &str) -> Vec<Vec<String>> {
    let mut script = String::from(create);
    script.push('\n');
    for stmt in inserts {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the sqlite3 CLI (3.45) must be on PATH for this cross-check");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "sqlite3 failed on `{query}`: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

const CREATE_T: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, grp TEXT, val INTEGER, amt INTEGER);";

/// Assert mpedb and sqlite agree, cell for cell, on a query whose columns are
/// all integer/text/NULL (never float).
fn assert_same(d: &Database, query: &str) {
    let got = mpedb_rows(d, query);
    let want = sqlite_rows(CREATE_T, &insert_statements(), query);
    assert_eq!(got, want, "mismatch on `{query}`");
}

#[test]
fn ranking_matches_sqlite() {
    let d = db();
    for q in [
        // PARTITION + ORDER, with ties and a NULL partition and NULL order keys.
        "SELECT id, row_number() OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        "SELECT id, rank() OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        "SELECT id, dense_rank() OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        // No PARTITION — one partition over the whole table.
        "SELECT id, row_number() OVER (ORDER BY val) FROM t ORDER BY id",
        "SELECT id, rank() OVER (ORDER BY val) FROM t ORDER BY id",
        "SELECT id, dense_rank() OVER (ORDER BY val) FROM t ORDER BY id",
        // DESC ordering (NULLS move last).
        "SELECT id, rank() OVER (PARTITION BY grp ORDER BY val DESC) FROM t ORDER BY id",
        // No ORDER BY at all: row_number is 1..n in scan order; rank/dense_rank
        // are all 1 (the whole partition is one peer group).
        "SELECT id, row_number() OVER () FROM t ORDER BY id",
        "SELECT id, rank() OVER (PARTITION BY grp) FROM t ORDER BY id",
        "SELECT id, dense_rank() OVER (PARTITION BY grp) FROM t ORDER BY id",
        // Multi-key partition and order.
        "SELECT id, rank() OVER (PARTITION BY grp ORDER BY val, id) FROM t ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

#[test]
fn integer_aggregates_match_sqlite() {
    let d = db();
    for q in [
        // Cumulative (default RANGE frame with ORDER BY): peers share the value.
        "SELECT id, sum(amt) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        "SELECT id, count(val) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        "SELECT id, min(val) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        "SELECT id, max(amt) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        // Whole partition (no ORDER BY): every row gets the partition total.
        "SELECT id, sum(amt) OVER (PARTITION BY grp) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (PARTITION BY grp) FROM t ORDER BY id",
        "SELECT id, min(val) OVER (PARTITION BY grp) FROM t ORDER BY id",
        "SELECT id, max(val) OVER (PARTITION BY grp) FROM t ORDER BY id",
        // Whole table, no partition, no order.
        "SELECT id, sum(amt) OVER () FROM t ORDER BY id",
        "SELECT id, count(*) OVER () FROM t ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

#[test]
fn multiple_windows_and_order_and_limit_match_sqlite() {
    let d = db();
    for q in [
        // Several distinct windows in one SELECT.
        "SELECT id, row_number() OVER (PARTITION BY grp ORDER BY val), \
         rank() OVER (PARTITION BY grp ORDER BY val), \
         sum(amt) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        // A window in the outer ORDER BY that is NOT selected (junk column).
        "SELECT id FROM t ORDER BY sum(amt) OVER (PARTITION BY grp ORDER BY val), id",
        "SELECT id, grp FROM t ORDER BY rank() OVER (PARTITION BY grp ORDER BY val DESC), id",
        // LIMIT/OFFSET apply AFTER the window phase.
        "SELECT id, row_number() OVER (ORDER BY val) AS rn FROM t ORDER BY id LIMIT 3",
        "SELECT id, row_number() OVER (ORDER BY val) AS rn FROM t ORDER BY id LIMIT 3 OFFSET 4",
        // Empty input: zero rows, no synthetic group.
        "SELECT id, row_number() OVER (PARTITION BY grp ORDER BY val) FROM t WHERE id > 100 ORDER BY id",
        // A window ordered-by that IS also selected computes once (and matches).
        "SELECT id, rank() OVER (ORDER BY val) AS r FROM t ORDER BY r, id",
        // Window computed AFTER a WHERE filter (over the surviving rows only).
        "SELECT id, row_number() OVER (PARTITION BY grp ORDER BY val) FROM t WHERE amt >= 4 ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

/// Float-valued windows (`avg`, `total`) are compared NUMERICALLY against
/// sqlite, since the two engines format REALs differently.
#[test]
fn float_aggregates_match_sqlite() {
    let d = db();
    for q in [
        "SELECT avg(amt) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        "SELECT avg(amt) OVER (PARTITION BY grp) FROM t ORDER BY id",
        "SELECT total(amt) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
        "SELECT avg(val) OVER (PARTITION BY grp ORDER BY val) FROM t ORDER BY id",
    ] {
        float_diff(&d, q);
    }
}

fn float_diff(d: &Database, query: &str) {
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
    let want: Vec<Option<f64>> = sqlite_rows(CREATE_T, &insert_statements(), query)
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

// ---- window over a join ----------------------------------------------------

#[test]
fn window_over_join_matches_sqlite() {
    let path = shm_path("join");
    let schema = r#"[[table]]
name = "emp"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "dept"
  type = "text"
  [[table.column]]
  name = "sal"
  type = "int64"

[[table]]
name = "dept"
primary_key = ["name"]
  [[table.column]]
  name = "name"
  type = "text"
"#;
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{schema}");
    let d = Tmp {
        db: Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(),
        path,
    };
    let emp = [
        (1, "eng", 100),
        (2, "eng", 100), // tie
        (3, "eng", 90),
        (4, "sales", 80),
        (5, "sales", 70),
    ];
    let mut inserts: Vec<String> = Vec::new();
    for name in ["eng", "sales"] {
        inserts.push(format!("INSERT INTO dept (name) VALUES ('{name}')"));
    }
    for (id, dp, sal) in emp {
        inserts.push(format!(
            "INSERT INTO emp (id, dept, sal) VALUES ({id}, '{dp}', {sal})"
        ));
    }
    for s in &inserts {
        d.db.query(s, &[]).unwrap();
    }
    let create = "CREATE TABLE emp (id INTEGER PRIMARY KEY, dept TEXT, sal INTEGER);\n\
                  CREATE TABLE dept (name TEXT PRIMARY KEY);";
    let q = "SELECT e.id, rank() OVER (PARTITION BY e.dept ORDER BY e.sal DESC) \
             FROM emp e JOIN dept d ON e.dept = d.name ORDER BY e.id";
    let got = mpedb_rows(&d.db, q);
    let want = sqlite_rows(create, &inserts, q);
    assert_eq!(got, want, "mismatch on join window `{q}`");
}

// ---- refusals (each a clean bind error, never a wrong answer) ---------------

#[test]
fn stage1_refusals() {
    let d = db();
    let err = |sql: &str| -> String {
        match d.db.query(sql, &[]) {
            Ok(r) => panic!("expected `{sql}` to be refused, got {r:?}"),
            Err(e) => e.to_string(),
        }
    };
    // A ranking function without OVER is not a scalar function.
    assert!(err("SELECT rank() FROM t").to_lowercase().contains("over"));
    assert!(err("SELECT row_number() FROM t")
        .to_lowercase()
        .contains("over"));
    // A ranking function takes no arguments.
    assert!(err("SELECT rank(val) OVER (ORDER BY val) FROM t")
        .to_lowercase()
        .contains("argument"));
    // Explicit frames now ship (see `window_frames.rs`); the brittle/ignored
    // shapes stay refused. A `RANGE` value-offset frame is refused (its
    // DESC/NULL value arithmetic is version-brittle).
    assert!(err(
        "SELECT sum(amt) OVER (ORDER BY val RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t"
    )
    .to_lowercase()
    .contains("range"));
    // A frame on a ranking function (which sqlite silently ignores) is refused.
    assert!(err(
        "SELECT rank() OVER (ORDER BY val ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t"
    )
    .to_lowercase()
    .contains("frame"));
    // The full standard window-function set now ships (ranking + aggregate OVER
    // in `window.rs`; value/offset in `window2.rs`; ntile/percent_rank/cume_dist
    // in `window3.rs`). What remains refused: a scalar function used with OVER
    // (not a window function), and the out-of-scope forms — named windows and
    // FILTER. Each is a clean parse/plan-time rejection, never a wrong answer.
    assert!(err("SELECT abs(val) OVER (ORDER BY val) FROM t")
        .to_lowercase()
        .contains("window"));
    // A named window (`OVER w` + `WINDOW w AS (...)`) is not supported.
    err("SELECT rank() OVER w FROM t WINDOW w AS (ORDER BY val)");
    // FILTER on a window aggregate is not supported.
    err("SELECT count(amt) FILTER (WHERE amt > 0) OVER (ORDER BY val) FROM t");
    // ntile without an ORDER BY is refused (its buckets would be order-dependent).
    assert!(err("SELECT ntile(4) OVER () FROM t")
        .to_lowercase()
        .contains("order by"));
    // A window may not appear in WHERE.
    assert!(err("SELECT id FROM t WHERE row_number() OVER (ORDER BY val) = 1")
        .to_lowercase()
        .contains("window"));
    // Window + aggregate in one SELECT is refused.
    assert!(err("SELECT count(*), row_number() OVER (ORDER BY val) FROM t")
        .to_lowercase()
        .contains("window"));
    // DISTINCT inside a window aggregate is refused (sqlite refuses it too).
    assert!(err("SELECT sum(DISTINCT amt) OVER (ORDER BY val) FROM t")
        .to_lowercase()
        .contains("distinct"));
}
