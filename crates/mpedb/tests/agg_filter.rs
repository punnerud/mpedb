//! Aggregate `FILTER (WHERE …)` (sqlite 3.30+ / PostgreSQL): an aggregate
//! accumulates ONLY the rows for which its filter predicate is TRUE (3VL — NULL
//! and FALSE skip the row). Each aggregate in a SELECT filters independently, it
//! composes with `GROUP BY` (per-group filtering), `DISTINCT` (filter first, then
//! dedupe), and a filter may reference a DIFFERENT column than the aggregate's
//! argument. An empty filtered set yields the empty-group value (0 for count,
//! NULL for sum/avg/min/max).
//!
//! Every case is DIFFERENTIALLY verified against the `sqlite3` CLI 3.45: mpedb
//! runs the query, sqlite runs the identical `CREATE TABLE` + `INSERT`s + query,
//! and the two outputs must match exactly (integers exact, floats within a
//! relative tolerance since sqlite prints ~15 digits, NULL as `NULL`).
//!
//! FILTER on a WINDOW aggregate (`… OVER (…)`) is refused with a clean error —
//! standard SQL allows it, but mpedb only supports FILTER on plain
//! grouped/scalar aggregates.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// The table under test, created at RUNTIME. `g` groups; `x`/`y` are the
/// aggregate arguments and filter operands (with a NULL `x` and a NULL `y`);
/// `tag` is a text filter operand (with a NULL tag). The NULLs exercise 3VL:
/// a row whose filter evaluates to NULL is skipped.
const CREATE: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, x INTEGER, y INTEGER, tag TEXT)";

/// `(id, g, x, y, tag)`.
type Row = (i64, i64, Option<i64>, Option<i64>, Option<&'static str>);

const ROWS: &[Row] = &[
    (1, 0, Some(5), Some(10), Some("a")),
    (2, 0, Some(9), Some(20), Some("b")),
    (3, 0, Some(3), Some(30), Some("a")),
    (4, 1, Some(7), Some(40), Some("b")),
    (5, 1, Some(2), Some(50), Some("a")),
    (6, 1, None, Some(60), Some("a")), // NULL x → an `x > …` filter skips it
    (7, 1, Some(8), None, None),       // NULL tag → a `tag = …` filter skips it; NULL y
    (8, 2, Some(100), Some(1), Some("c")), // group 2: a single row
];

fn ilit(v: Option<i64>) -> String {
    v.map_or("NULL".to_string(), |i| i.to_string())
}
fn tlit(v: Option<&str>) -> String {
    v.map_or("NULL".to_string(), |s| format!("'{}'", s.replace('\'', "''")))
}

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, g, x, y, tag)| {
            format!(
                "INSERT INTO t (id, g, x, y, tag) VALUES ({id}, {g}, {}, {}, {})",
                ilit(*x),
                ilit(*y),
                tlit(*tag)
            )
        })
        .collect()
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-agg-filter-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    // A throwaway seed table satisfies the config schema; the real table is
    // CREATEd at runtime. The default dialect is sqlite-lenient (so a bare
    // column under GROUP BY is allowed), matching the sqlite CLI it is checked
    // against.
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    db.query(CREATE, &[]).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Run `CREATE t` + inserts + the query through the sqlite3 CLI in list mode
/// (`|`-separated, NULL rendered as `NULL`) and parse each row's cells.
fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from(".mode list\n.nullvalue NULL\n");
    script.push_str(CREATE);
    script.push_str(";\n");
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
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "sqlite3 failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Does one mpedb value agree with sqlite's rendered cell? Integers compare
/// exactly, floats within a relative tolerance (sqlite prints ~15 digits), NULL
/// as `NULL`, text/bool exact.
fn cell_matches(m: &Value, s: &str) -> bool {
    match m {
        Value::Null => s == "NULL",
        Value::Int(i) => s.parse::<i64>().map(|y| y == *i).unwrap_or(false),
        Value::Float(x) => match s.parse::<f64>() {
            Ok(y) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
            Err(_) => false,
        },
        Value::Bool(b) => s == if *b { "1" } else { "0" },
        Value::Text(t) => s == t,
        other => panic!("unexpected value type in agg FILTER result: {other:?}"),
    }
}

fn agree(db: &Database, query: &str) {
    let m = mpedb_rows(db, query);
    let s = sqlite_rows(query);
    assert_eq!(
        m.len(),
        s.len(),
        "row count differs for `{query}`: mpedb {m:?} vs sqlite {s:?}"
    );
    for (mr, sr) in m.iter().zip(&s) {
        assert_eq!(
            mr.len(),
            sr.len(),
            "column count differs for `{query}`: mpedb {mr:?} vs sqlite {sr:?}"
        );
        for (mv, sv) in mr.iter().zip(sr) {
            assert!(
                cell_matches(mv, sv),
                "cell mismatch for `{query}`: mpedb {mv:?} vs sqlite {sv:?}\n  \
                 full mpedb row {mr:?}\n  full sqlite row {sr:?}"
            );
        }
    }
}

#[test]
fn agg_filter_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // ---- count(*) FILTER ------------------------------------------------
        "SELECT count(*) FILTER (WHERE x > 5) FROM t",
        // NULL-in-filter is skipped (row 6 has NULL x; row 7 has NULL tag).
        "SELECT count(*) FILTER (WHERE x > 0) FROM t",
        "SELECT count(*) FILTER (WHERE tag = 'a') FROM t",
        // ---- sum / avg / min / max FILTER -----------------------------------
        "SELECT sum(y) FILTER (WHERE tag = 'a') FROM t",
        "SELECT avg(y) FILTER (WHERE tag = 'a') FROM t",
        "SELECT min(x) FILTER (WHERE g = 1) FROM t",
        "SELECT max(x) FILTER (WHERE g = 1) FROM t",
        // ---- a filter on a DIFFERENT column than the argument ---------------
        "SELECT sum(y) FILTER (WHERE x > 5) FROM t",
        // ---- two aggregates with DIFFERENT filters in one SELECT ------------
        "SELECT sum(y) FILTER (WHERE g = 0), sum(y) FILTER (WHERE g = 1) FROM t",
        "SELECT count(*) FILTER (WHERE x > 5), count(*) FILTER (WHERE tag = 'a'), count(*) FROM t",
        // ---- FILTER + GROUP BY: per-group filtering -------------------------
        "SELECT g, count(*) FILTER (WHERE x > 4), sum(y) FILTER (WHERE tag = 'a') \
         FROM t GROUP BY g ORDER BY g",
        // ---- an ALL-EXCLUDED group: empty → 0 (count) / NULL (sum/avg/max) --
        "SELECT g, count(*) FILTER (WHERE x > 1000), sum(y) FILTER (WHERE x > 1000), \
         max(x) FILTER (WHERE x > 1000) FROM t GROUP BY g ORDER BY g",
        // A whole-table all-excluded scalar aggregate (one group, no GROUP BY).
        "SELECT count(*) FILTER (WHERE x > 1000), sum(y) FILTER (WHERE x > 1000) FROM t",
        // ---- FILTER + DISTINCT: filter first, then dedupe -------------------
        "SELECT count(DISTINCT x) FILTER (WHERE x IS NOT NULL) FROM t",
        "SELECT count(DISTINCT tag) FILTER (WHERE g = 0) FROM t",
        // group 0 x-values are {5,9,3}; filtered to x<8 → {5,3} → 2 distinct.
        "SELECT g, count(DISTINCT x) FILTER (WHERE x < 8) FROM t GROUP BY g ORDER BY g",
        // ---- a bare column governed by a single min/max FILTER (COMPAT) -----
        // sqlite fixes the bare `tag` from the FILTERED extremum's witness row.
        "SELECT g, tag, max(x) FILTER (WHERE x < 8) FROM t GROUP BY g ORDER BY g",
        // ---- `filter` NOT followed by `(` is an output ALIAS, not the keyword
        // (sqlite/PG parse it that way; the FILTER keyword needs the paren) -----
        "SELECT count(*) filter FROM t",
    ];
    for q in queries {
        agree(&d, q);
    }
}

/// FILTER on a WINDOW aggregate (`OVER (…)`) is refused with a clean error; the
/// same aggregate without FILTER is a valid window and without OVER is a valid
/// filtered aggregate — so the refusal is specifically the combination.
#[test]
fn filter_on_window_aggregate_is_refused() {
    let d = db();
    // A plain filtered aggregate works…
    assert!(d.query("SELECT sum(y) FILTER (WHERE g = 0) FROM t", &[]).is_ok());
    // …and the same aggregate as a window works…
    assert!(d.query("SELECT id, sum(y) OVER () FROM t", &[]).is_ok());
    // …but FILTER on a window aggregate is refused with a clear message.
    let err = d
        .query("SELECT id, sum(y) FILTER (WHERE g = 0) OVER () FROM t", &[])
        .expect_err("FILTER on a window aggregate must be refused");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("filter") && msg.contains("window"),
        "refusal should name FILTER and window, got: {err}"
    );
}

/// `FILTER` must be `FILTER (WHERE <predicate>)` — the `WHERE` is mandatory, as
/// in sqlite/PG. A missing `WHERE` is a clean parse error, not a silent accept.
#[test]
fn filter_requires_where() {
    let d = db();
    assert!(
        d.query("SELECT count(*) FILTER (x > 5) FROM t", &[]).is_err(),
        "FILTER without WHERE must be a parse error"
    );
}
