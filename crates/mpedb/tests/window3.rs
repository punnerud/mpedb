//! SQL window RANK / DISTRIBUTION functions (design/DESIGN-WINDOW.md stage 2b) —
//! differential against the `sqlite3` CLI (3.45).
//!
//! Ships: `ntile(n)`, `percent_rank()`, `cume_dist()` with
//! `OVER ([PARTITION BY …] ORDER BY …)` and the default frame. Every case here is
//! cross-checked against sqlite, which is where the bucketing / tie / peer rules
//! are easy to get wrong:
//!   * ntile distributes the partition's rows into `n` buckets — the first
//!     `rows % n` buckets get the extra row (`ceil`), the rest `floor`; it needs
//!     an ORDER BY (else the buckets are order-dependent, so mpedb refuses it);
//!   * percent_rank is `(rank - 1) / (rows - 1)` with rank() ties, or 0.0 for a
//!     one-row partition;
//!   * cume_dist is `(rows whose order key ≤ the current row's, peers included)
//!     / rows` — every peer shares the value.
//!
//! ntile is an integer, compared cell-for-cell; percent_rank/cume_dist are floats,
//! compared NUMERICALLY (the two engines format REALs differently).
//!
//! (The plan-bytes truncation sweep for the format-35 window bytes lives with the
//! other decoder truncation tests, in `mpedb-sql`'s `plan::tests`.)

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
        "{dir}/mpedb-window3-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&p);
    p
}

// ---- fixture ---------------------------------------------------------------

/// `id` PK; `g` a nullable TEXT partition key (with a NULL-partition row and a
/// SINGLE-ROW partition `'c'`, so percent_rank/cume_dist hit the one-row special
/// case); `k` a nullable INT order key (with ties AND a NULL, to exercise peer
/// groups + NULLS-FIRST). One fixture row: `(id, g, k)`.
type Row = (i64, Option<&'static str>, Option<i64>);

const ROWS: &[Row] = &[
    (1, Some("a"), Some(10)),
    (2, Some("a"), Some(20)),
    (3, Some("a"), Some(20)), // tie on k=20 within 'a'
    (4, Some("a"), Some(30)),
    (5, Some("b"), Some(5)),
    (6, Some("b"), Some(5)), // tie on k=5 within 'b'
    (7, Some("b"), Some(15)),
    (8, Some("c"), Some(99)), // single-row partition
    (9, None, Some(30)),      // NULL partition
    (10, None, None),         // NULL partition AND NULL order key
];

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, g, k)| {
            let gg = g.map_or("NULL".to_string(), |x| format!("'{x}'"));
            let kk = k.map_or("NULL".to_string(), |x| x.to_string());
            format!("INSERT INTO w3 (id, g, k) VALUES ({id}, {gg}, {kk})")
        })
        .collect()
}

const CREATE: &str = "CREATE TABLE w3 (id INTEGER PRIMARY KEY, g TEXT, k INTEGER);";

fn db() -> Tmp {
    let path = shm_path("w3");
    let schema = r#"[[table]]
name = "w3"
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
"#;
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{schema}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

/// Canonical cell rendering matching the `sqlite3` CLI's default "list" mode:
/// NULL as empty, ints verbatim. (Floats go through `float_diff`, not here.)
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

/// Assert mpedb and sqlite agree, cell for cell, on an int/NULL query (ntile).
fn assert_same(d: &Database, query: &str) {
    let got = mpedb_rows(d, query);
    let want = sqlite_rows(query);
    assert_eq!(got, want, "mismatch on `{query}`");
}

/// Assert mpedb and sqlite agree NUMERICALLY on the LAST column of each row
/// (percent_rank / cume_dist floats). The leading `id` column is checked exactly.
fn float_diff(d: &Database, query: &str) {
    let got_rows = match d.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    let got: Vec<(String, f64)> = got_rows
        .into_iter()
        .map(|mut r| {
            let f = match r.pop() {
                Some(Value::Float(f)) => f,
                Some(Value::Int(i)) => i as f64,
                other => panic!("unexpected non-float cell {other:?} for `{query}`"),
            };
            (render(&r[0]), f)
        })
        .collect();
    let want: Vec<(String, f64)> = sqlite_rows(query)
        .into_iter()
        .map(|mut r| {
            let cell = r.pop().unwrap_or_default();
            let f = cell
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("bad float `{cell}` for `{query}`"));
            (r[0].clone(), f)
        })
        .collect();
    assert_eq!(got.len(), want.len(), "row count mismatch on `{query}`");
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g.0, w.0, "row {i}: id mismatch on `{query}`");
        assert!(
            (g.1 - w.1).abs() < 1e-9,
            "row {i}: mpedb {} vs sqlite {} on `{query}`",
            g.1,
            w.1
        );
    }
}

#[test]
fn ntile_matches_sqlite() {
    let d = db();
    for q in [
        // Whole table (10 rows). Even splits: ntile(2)=5,5 and ntile(5)=2 each.
        // Uneven splits: ntile(3) and ntile(4) — the earlier buckets get the extra
        // row. `, id` gives a total order so tied `k` rows land deterministically
        // (matching sqlite's rowid tiebreak).
        "SELECT id, ntile(2) OVER (ORDER BY k, id) FROM w3 ORDER BY id",
        "SELECT id, ntile(3) OVER (ORDER BY k, id) FROM w3 ORDER BY id",
        "SELECT id, ntile(4) OVER (ORDER BY k, id) FROM w3 ORDER BY id",
        "SELECT id, ntile(5) OVER (ORDER BY k, id) FROM w3 ORDER BY id",
        // n larger than the partition ⇒ the first `rows` buckets get one row each.
        "SELECT id, ntile(20) OVER (ORDER BY k, id) FROM w3 ORDER BY id",
        // n == 1 ⇒ every row in bucket 1.
        "SELECT id, ntile(1) OVER (ORDER BY k, id) FROM w3 ORDER BY id",
        // Partitioned: 'a'=4, 'b'=3, 'c'=1, NULL=2 rows — even and uneven per part.
        "SELECT id, ntile(2) OVER (PARTITION BY g ORDER BY k, id) FROM w3 ORDER BY id",
        "SELECT id, ntile(3) OVER (PARTITION BY g ORDER BY k, id) FROM w3 ORDER BY id",
        "SELECT id, ntile(5) OVER (PARTITION BY g ORDER BY k, id) FROM w3 ORDER BY id",
        // DESC ordering (NULLS move last).
        "SELECT id, ntile(3) OVER (PARTITION BY g ORDER BY k DESC, id) FROM w3 ORDER BY id",
    ] {
        assert_same(&d, q);
    }
}

#[test]
fn percent_rank_and_cume_dist_match_sqlite() {
    let d = db();
    for q in [
        // Whole table, with ties (rank() semantics: peers share, then skip).
        "SELECT id, percent_rank() OVER (ORDER BY k) FROM w3 ORDER BY id",
        "SELECT id, cume_dist() OVER (ORDER BY k) FROM w3 ORDER BY id",
        // Partitioned, including the SINGLE-ROW partition 'c' (percent_rank = 0.0,
        // cume_dist = 1.0) and tie peer groups in 'a'/'b'.
        "SELECT id, percent_rank() OVER (PARTITION BY g ORDER BY k) FROM w3 ORDER BY id",
        "SELECT id, cume_dist() OVER (PARTITION BY g ORDER BY k) FROM w3 ORDER BY id",
        // DESC ordering (NULLS move last) — the peer-inclusive count follows the
        // ORDER BY direction.
        "SELECT id, percent_rank() OVER (ORDER BY k DESC) FROM w3 ORDER BY id",
        "SELECT id, cume_dist() OVER (ORDER BY k DESC) FROM w3 ORDER BY id",
        // No ORDER BY: the whole partition is ONE peer group ⇒ percent_rank 0.0
        // and cume_dist 1.0 everywhere (well-defined, so allowed and matches).
        "SELECT id, percent_rank() OVER () FROM w3 ORDER BY id",
        "SELECT id, cume_dist() OVER (PARTITION BY g) FROM w3 ORDER BY id",
        // Combined in one SELECT with a ranking function (shared-nothing slots).
        "SELECT id, rank() OVER (ORDER BY k), percent_rank() OVER (ORDER BY k), \
         cume_dist() OVER (ORDER BY k) FROM w3 ORDER BY id",
    ] {
        float_diff(&d, q);
    }
}

/// The out-of-scope / ill-formed forms are REFUSED with a clean error, never a
/// differing answer — the 0-wrong-answer contract.
#[test]
fn ntile_refusals() {
    let d = db();
    let err = |sql: &str| -> String {
        match d.db.query(sql, &[]) {
            Ok(r) => panic!("expected `{sql}` to be refused, got {r:?}"),
            Err(e) => e.to_string(),
        }
    };
    // ntile without ORDER BY is order-dependent ⇒ refused (never guessed).
    assert!(err("SELECT id, ntile(3) OVER () FROM w3")
        .to_lowercase()
        .contains("order by"));
    assert!(err("SELECT id, ntile(3) OVER (PARTITION BY g) FROM w3")
        .to_lowercase()
        .contains("order by"));
    // The bucket count must be a positive integer constant.
    err("SELECT id, ntile(0) OVER (ORDER BY k) FROM w3");
    err("SELECT id, ntile(-2) OVER (ORDER BY k) FROM w3");
    err("SELECT id, ntile(k) OVER (ORDER BY id) FROM w3");
    err("SELECT id, ntile(2.5) OVER (ORDER BY k) FROM w3");
    // ntile takes exactly one argument.
    err("SELECT id, ntile() OVER (ORDER BY k) FROM w3");
    err("SELECT id, ntile(2, 3) OVER (ORDER BY k) FROM w3");
    // Only valid as a window function (OVER required).
    err("SELECT ntile(3) FROM w3");
    // percent_rank/cume_dist take no argument.
    err("SELECT id, percent_rank(k) OVER (ORDER BY k) FROM w3");
    err("SELECT id, cume_dist(k) OVER (ORDER BY k) FROM w3");
    // Explicit frames remain refused on these functions too.
    assert!(err(
        "SELECT id, ntile(3) OVER (ORDER BY k ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w3"
    )
    .to_lowercase()
    .contains("frame"));
}
