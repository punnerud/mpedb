//! GROUP BY column-strictness dialect (`[compat] bare_group_by`, COMPAT.md).
//!
//! mpedb supports BOTH sqlite's lenient bare-column rule and PostgreSQL's strict
//! one, chosen by config. The hard constraint is mpedb's core guarantee: in
//! sqlite mode a bare column must produce sqlite's EXACT value or be refused —
//! never a guessed value. This test:
//!
//! (a) sqlite mode: differential-tests the accepted bare-column cases against the
//!     `sqlite3` CLI (const-folded-away `COALESCE`, and the single-min/max
//!     witness row, including ties, interior NULLs, all-NULL groups, no GROUP BY,
//!     and an empty table);
//! (b) postgres mode: the same queries are REJECTED (matching PostgreSQL, whose
//!     rejection was verified by hand against PG 16 — `column … must appear in
//!     the GROUP BY clause …`);
//! (c) the genuinely-arbitrary bare column (bare + a non-min/max aggregate, two
//!     aggregates, or no aggregate) is cleanly REFUSED in sqlite mode too, rather
//!     than returning a value that might differ from sqlite;
//! (d) the config default is sqlite, and an explicit `postgres` config is strict.
//!     (The mirror's PG-import → postgres default is covered in mpedb-mirror.)

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

const SCHEMA: &str = r#"
[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "g"
  type = "int64"
  [[table.column]]
  name = "x"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "name"
  type = "text"
"#;

/// The same table shape for the `sqlite3` reference engine.
const SQLITE_DDL: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, g INTEGER, x INTEGER, name TEXT);";

/// One row of shared test data. `x` is nullable (the min/max argument).
type Row = (i64, i64, Option<i64>, &'static str);

/// The corpus both engines load. It exercises: ties on the extremum (g=10 has
/// two x=9 rows), an interior NULL after the extremum (g=20), and an all-NULL
/// group (g=30, no extremum at all).
const DATA: &[Row] = &[
    (1, 10, Some(5), "a"),
    (2, 10, Some(9), "b"),
    (3, 10, Some(9), "c"),
    (4, 20, Some(3), "d"),
    (5, 20, None, "e"),
    (6, 20, Some(7), "f"),
    (7, 30, None, "g"),
    (8, 30, None, "h"),
];

fn open(name: &str, compat: Option<&str>) -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-gbd-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let compat_section = match compat {
        Some(mode) => format!("\n[compat]\nbare_group_by = \"{mode}\"\n"),
        None => String::new(),
    };
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 16\n{}{}",
        path.display(),
        compat_section,
        SCHEMA
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (db, path)
}

fn load(db: &Database, rows: &[Row]) {
    for (id, g, x, name) in rows {
        let xv = match x {
            Some(v) => v.to_string(),
            None => "NULL".to_string(),
        };
        db.query(
            &format!("INSERT INTO t (id, g, x, name) VALUES ({id}, {g}, {xv}, '{name}')"),
            &[],
        )
        .unwrap();
    }
}

fn canon(v: &Value) -> String {
    match v {
        Value::Null => "<NULL>".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Float(f) => format!("{f}"),
        other => format!("{other:?}"),
    }
}

/// mpedb's answer to `query`, as a SORTED set of stringified rows.
fn mpedb_rows(db: &Database, query: &str) -> Vec<Vec<String>> {
    match db.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            let mut out: Vec<Vec<String>> =
                rows.iter().map(|r| r.iter().map(canon).collect()).collect();
            out.sort();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// sqlite3's answer to `query` over `DDL + inserts`, SORTED. `None` if the
/// `sqlite3` binary is not installed (the differential half is then skipped).
fn sqlite_rows(rows: &[Row], query: &str) -> Option<Vec<Vec<String>>> {
    let mut script = String::new();
    script.push_str(".mode list\n.separator |\n.nullvalue <NULL>\n");
    script.push_str(SQLITE_DDL);
    script.push('\n');
    for (id, g, x, name) in rows {
        let xv = match x {
            Some(v) => v.to_string(),
            None => "NULL".to_string(),
        };
        script.push_str(&format!(
            "INSERT INTO t VALUES ({id}, {g}, {xv}, '{name}');\n"
        ));
    }
    script.push_str(query);
    script.push_str(";\n");

    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
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
    let mut parsed: Vec<Vec<String>> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(|s| s.to_string()).collect())
        .collect();
    parsed.sort();
    Some(parsed)
}

/// Assert mpedb's sqlite-mode answer equals sqlite's, over `rows`.
fn assert_matches_sqlite_data(db: &Database, rows: &[Row], query: &str) {
    let got = mpedb_rows(db, query);
    if let Some(want) = sqlite_rows(rows, query) {
        assert_eq!(got, want, "mpedb vs sqlite differ on `{query}`");
    }
}

/// Assert mpedb's sqlite-mode answer equals sqlite's, over the shared `DATA`.
fn assert_matches_sqlite(db: &Database, query: &str) {
    assert_matches_sqlite_data(db, DATA, query);
}

// ---------------------------------------------------------------------------
// (a) sqlite mode: accepted bare columns match sqlite exactly.
// ---------------------------------------------------------------------------

#[test]
fn sqlite_mode_coalesce_const_never_evaluates_the_bare_column() {
    // Case 1: `-24` is non-NULL, so `x` is never evaluated — const folding drops
    // it and the bare column disappears. Value is `-24` for every group.
    let (db, path) = open("case1", Some("sqlite"));
    load(&db, DATA);
    assert_matches_sqlite(&db, "SELECT g, COALESCE(-24, x) FROM t GROUP BY g");
    // A dead CASE branch is the same story.
    assert_matches_sqlite(&db, "SELECT g, CASE WHEN 1=1 THEN 0 ELSE x END FROM t GROUP BY g");
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_bare_column_follows_single_max() {
    // Case 2: one max(), no other aggregate → bare columns come from the max row.
    // g=10 ties at x=9 (rows 2 and 3): sqlite takes the FIRST, and so must mpedb.
    // g=30 is all-NULL: sqlite takes the LAST row; mpedb reproduces that.
    let (db, path) = open("case2max", Some("sqlite"));
    load(&db, DATA);
    assert_matches_sqlite(&db, "SELECT g, name, max(x) FROM t GROUP BY g");
    assert_matches_sqlite(&db, "SELECT g, id, name, max(x) FROM t GROUP BY g");
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_bare_column_follows_single_min() {
    let (db, path) = open("case2min", Some("sqlite"));
    load(&db, DATA);
    assert_matches_sqlite(&db, "SELECT g, name, min(x) FROM t GROUP BY g");
    assert_matches_sqlite(&db, "SELECT g, id, name, min(x) FROM t GROUP BY g");
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_bare_column_no_group_by() {
    // No GROUP BY: one group over the whole table, the bare column from the max
    // row. Also the min form and a bare column inside an expression.
    let (db, path) = open("nogroup", Some("sqlite"));
    load(&db, DATA);
    assert_matches_sqlite(&db, "SELECT name, max(x) FROM t");
    assert_matches_sqlite(&db, "SELECT name, min(x) FROM t");
    assert_matches_sqlite(&db, "SELECT name || '!', max(x) FROM t");
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_bare_column_over_empty_table_is_null() {
    // Empty table: sqlite returns one row with NULL bare columns and NULL max.
    let (db, path) = open("empty", Some("sqlite"));
    assert_matches_sqlite_data(&db, &[], "SELECT name, max(x) FROM t");
    // Sanity: exactly one row, both NULL.
    assert_eq!(
        mpedb_rows(&db, "SELECT name, max(x) FROM t"),
        vec![vec!["<NULL>".to_string(), "<NULL>".to_string()]]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_all_null_group_takes_last_row() {
    // Pin the all-NULL-group rule directly (it is the fragile one): g=30's x is
    // all NULL, so there is no extremum; sqlite fills the bare column from the
    // group's LAST row (id 8, 'h'). Verified against sqlite 3.45.1.
    let (db, path) = open("allnull", Some("sqlite"));
    load(&db, DATA);
    let rows = mpedb_rows(&db, "SELECT g, name, max(x) FROM t GROUP BY g");
    assert!(
        rows.contains(&vec!["30".to_string(), "h".to_string(), "<NULL>".to_string()]),
        "all-NULL group should take last row 'h', got {rows:?}"
    );
    assert_matches_sqlite(&db, "SELECT g, name, max(x) FROM t GROUP BY g");
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_distinct_over_bare_column_matches_sqlite() {
    // DISTINCT sorts/dedups the PROJECTION, which includes the bare column — the
    // witness values must still match sqlite before dedup.
    let (db, path) = open("distinct", Some("sqlite"));
    load(&db, DATA);
    assert_matches_sqlite(&db, "SELECT DISTINCT name, max(x) FROM t GROUP BY g");
    // EXPLAIN over a bare-column plan must render without panicking.
    let _ = db
        .query("EXPLAIN SELECT name, max(x) FROM t GROUP BY g", &[])
        .unwrap();
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// (b) postgres mode: the same queries are rejected (matching PostgreSQL).
// ---------------------------------------------------------------------------

#[test]
fn postgres_mode_rejects_every_bare_column() {
    let (db, path) = open("pgstrict", Some("postgres"));
    load(&db, DATA);
    for q in [
        "SELECT g, COALESCE(-24, x) FROM t GROUP BY g",
        "SELECT g, name, max(x) FROM t GROUP BY g",
        "SELECT g, name, min(x) FROM t GROUP BY g",
        "SELECT name, max(x) FROM t",
        "SELECT g, x FROM t GROUP BY g",
    ] {
        let err = db.query(q, &[]).unwrap_err().to_string();
        assert!(
            err.contains("must appear in GROUP BY"),
            "postgres mode should reject `{q}`, got: {err}"
        );
    }
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// (c) genuinely-arbitrary bare column: refused in sqlite mode too (never wrong).
// ---------------------------------------------------------------------------

#[test]
fn sqlite_mode_refuses_genuinely_arbitrary_bare_column() {
    let (db, path) = open("arbitrary", Some("sqlite"));
    load(&db, DATA);
    // sqlite ACCEPTS all of these (returning an arbitrary row); mpedb refuses
    // rather than risk a value that differs from sqlite.
    for q in [
        // bare + a non-min/max aggregate
        "SELECT name, count(*) FROM t GROUP BY g",
        "SELECT name, sum(x) FROM t GROUP BY g",
        // bare + BOTH min and max (two aggregates)
        "SELECT name, min(x), max(x) FROM t GROUP BY g",
        // bare with no aggregate at all
        "SELECT g, x FROM t GROUP BY g",
    ] {
        let err = db.query(q, &[]).unwrap_err().to_string();
        assert!(
            err.contains("must appear in GROUP BY"),
            "sqlite mode should refuse arbitrary bare column in `{q}`, got: {err}"
        );
    }
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// (d) defaults: config default is sqlite; explicit postgres is strict.
// ---------------------------------------------------------------------------

#[test]
fn config_default_is_sqlite() {
    // No [compat] section at all → lenient sqlite behavior (accepts the min/max
    // witness case and matches sqlite).
    let (db, path) = open("default", None);
    load(&db, DATA);
    assert_matches_sqlite(&db, "SELECT g, name, max(x) FROM t GROUP BY g");
    // And the never-evaluated case is accepted too.
    assert_matches_sqlite(&db, "SELECT g, COALESCE(-24, x) FROM t GROUP BY g");
    let _ = std::fs::remove_file(path);
}

#[test]
fn explicit_postgres_config_is_strict() {
    let (db, path) = open("explicitpg", Some("postgres"));
    load(&db, DATA);
    assert!(db
        .query("SELECT g, name, max(x) FROM t GROUP BY g", &[])
        .is_err());
    let _ = std::fs::remove_file(path);
}
