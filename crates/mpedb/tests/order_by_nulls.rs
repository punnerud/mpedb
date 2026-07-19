//! `ORDER BY … NULLS FIRST/LAST` (Django gap #6, second half) cross-checked
//! row-by-row against the real `sqlite3` CLI 3.45.
//!
//! sqlite's DEFAULT placement is a function of the direction — NULLs first for
//! `ASC`, last for `DESC` — and the explicit clause overrides it independently,
//! including on a term with no `ASC`/`DESC` of its own. The placement is NOT
//! reversed by `DESC`: `NULLS FIRST` means first in the delivered order either
//! way.
//!
//! Two things make this a place where a near-miss would be invisible:
//!
//! 1. It changes the ORDER rows come back in, not whether the query runs. So
//!    every case here is a full row list compared against sqlite, never a
//!    "does it parse".
//! 2. The DEFAULT placement is the regression surface. Every explicit case is
//!    therefore paired with its default spelling, and `default_placement_*`
//!    sweeps the plain ASC/DESC forms across every pipeline shape.
//!
//! `ORDER BY … LIMIT n` over base-row keys takes the streaming top-K heap
//! rather than a full sort — a genuinely separate comparator call site — so the
//! LIMIT/OFFSET cases are checked against sqlite AND against this engine's own
//! full sort, which is what would catch the two paths drifting apart.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// (id, s, n, allnull, nonull). `s`/`n` are mixed; `allnull` is NULL in every
/// row and `nonull` in none, which are the two degenerate columns where a
/// placement bug hides behind "the answer looked sorted".
const DATA: &[(i64, Option<&str>, Option<i64>, &str)] = &[
    (1, Some("b"), Some(5), "p"),
    (2, None, None, "q"),
    (3, Some("a"), Some(1), "r"),
    (4, None, Some(3), "s"),
    (5, Some("c"), None, "t"),
    (6, Some("A"), Some(-2), "u"),
    (7, None, Some(1), "v"),
    (8, Some(""), Some(0), "w"),
];

const SCHEMA: &str = r#"
[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "s"
  type = "text"
  nullable = true
  [[table.column]]
  name = "n"
  type = "int64"
  nullable = true
  [[table.column]]
  name = "allnull"
  type = "text"
  nullable = true
  [[table.column]]
  name = "nonull"
  type = "text"
"#;

fn inserts() -> Vec<String> {
    DATA.iter()
        .map(|(id, s, n, nonull)| {
            let s = match s {
                Some(v) => format!("'{}'", v.replace('\'', "''")),
                None => "NULL".to_string(),
            };
            let n = match n {
                Some(v) => v.to_string(),
                None => "NULL".to_string(),
            };
            format!(
                "INSERT INTO t (id, s, n, allnull, nonull) VALUES ({id}, {s}, {n}, NULL, '{nonull}')"
            )
        })
        .collect()
}

fn open() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-nullsord-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 8\n{SCHEMA}",
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for i in inserts() {
        db.query(&i, &[]).unwrap();
    }
    (db, path)
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Text(s) => s.clone(),
        Value::Float(f) => format!("{f}"),
        other => format!("{other:?}"),
    }
}

fn mpedb_rows(db: &Database, query: &str) -> Vec<String> {
    match db.query(query, &[]).unwrap_or_else(|e| panic!("mpedb `{query}`: {e}")) {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect(),
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<String> {
    let mut input = String::from(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, n INTEGER, allnull TEXT, nonull TEXT);\n",
    );
    for i in inserts() {
        input.push_str(&i);
        input.push_str(";\n");
    }
    input.push_str(query);
    input.push_str(";\n");
    sqlite_oracle::script_stdout(&input, "NULL")
        .lines()
        .map(|l| l.to_string())
        .collect()
}

fn cross_check(db: &Database, query: &str) {
    let m = mpedb_rows(db, query);
    let s = sqlite_rows(query);
    assert_eq!(m, s, "mpedb vs sqlite3 disagree on `{query}`");
}

/// ASC and DESC x FIRST and LAST, over a mixed column, an ALL-NULL column and a
/// NO-NULL column, in both the projection and the sort key.
#[test]
fn nulls_placement_matches_sqlite_for_every_direction_and_column() {
    let (db, path) = open();

    for col in ["s", "n", "allnull", "nonull"] {
        for dir in ["", " ASC", " DESC"] {
            for nulls in ["", " NULLS FIRST", " NULLS LAST"] {
                // `, id` makes the order TOTAL: without it the NULL rows tie and
                // neither engine promises which comes out first, so a difference
                // would mean nothing.
                cross_check(
                    &db,
                    &format!("SELECT id, {col} FROM t ORDER BY {col}{dir}{nulls}, id"),
                );
            }
        }
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The DEFAULT placement — the regression surface. Every plain ASC/DESC form,
/// across every pipeline shape the executor sorts in: base row, projection
/// (DISTINCT / computed key), grouped, compound, and the top-K heap.
///
/// A break here is what a NULL-placement change would most plausibly cause, and
/// it would look like "rows in a slightly odd order", not like a failure.
#[test]
fn default_placement_is_unchanged() {
    let (db, path) = open();

    for q in [
        // base row
        "SELECT id, s FROM t ORDER BY s, id",
        "SELECT id, s FROM t ORDER BY s DESC, id",
        "SELECT id, n FROM t ORDER BY n, id",
        "SELECT id, n FROM t ORDER BY n DESC, id",
        "SELECT id, allnull FROM t ORDER BY allnull, id",
        "SELECT id, allnull FROM t ORDER BY allnull DESC, id",
        // PK, where the planner elides the sort entirely
        "SELECT id FROM t ORDER BY id",
        "SELECT id FROM t ORDER BY id ASC",
        "SELECT id FROM t ORDER BY id DESC",
        // projection sort (an expression key, then DISTINCT)
        "SELECT id, n + 1 FROM t ORDER BY n + 1, id",
        "SELECT DISTINCT s FROM t ORDER BY s",
        "SELECT DISTINCT s FROM t ORDER BY s DESC",
        // grouped
        "SELECT n, count(*) FROM t GROUP BY n ORDER BY n",
        "SELECT n, count(*) FROM t GROUP BY n ORDER BY n DESC",
        "SELECT s, count(*) FROM t GROUP BY s ORDER BY count(*) DESC, s",
        // compound
        "SELECT s FROM t UNION ALL SELECT s FROM t ORDER BY 1",
        "SELECT s FROM t UNION SELECT s FROM t ORDER BY 1 DESC",
        // top-K heap
        "SELECT id, s FROM t ORDER BY s, id LIMIT 3",
        "SELECT id, s FROM t ORDER BY s DESC, id LIMIT 3",
        "SELECT id, n FROM t ORDER BY n, id LIMIT 4 OFFSET 2",
        // collation
        "SELECT id, s FROM t ORDER BY s COLLATE NOCASE, id",
        "SELECT id, s FROM t ORDER BY s COLLATE NOCASE DESC, id",
    ] {
        cross_check(&db, q);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A NULL placement on every pipeline shape, not only the base-row sort.
#[test]
fn nulls_placement_matches_sqlite_in_every_pipeline_shape() {
    let (db, path) = open();

    for q in [
        // projection sort: a computed key and DISTINCT
        "SELECT id, n + 1 FROM t ORDER BY n + 1 NULLS LAST, id",
        "SELECT id, n + 1 FROM t ORDER BY n + 1 DESC NULLS FIRST, id",
        "SELECT DISTINCT s FROM t ORDER BY s NULLS LAST",
        "SELECT DISTINCT s FROM t ORDER BY s DESC NULLS FIRST",
        // grouped: the key and an aggregate
        "SELECT n, count(*) FROM t GROUP BY n ORDER BY n NULLS LAST",
        "SELECT n, count(*) FROM t GROUP BY n ORDER BY n DESC NULLS LAST",
        "SELECT s, count(*) FROM t GROUP BY s ORDER BY s NULLS LAST",
        // compound, by ordinal
        "SELECT s FROM t UNION ALL SELECT s FROM t ORDER BY 1 NULLS LAST",
        "SELECT s FROM t UNION SELECT s FROM t ORDER BY 1 DESC NULLS LAST",
        // an ordinal in a plain SELECT
        "SELECT s, id FROM t ORDER BY 1 NULLS LAST, 2",
        // a collated key
        "SELECT id, s FROM t ORDER BY s COLLATE NOCASE NULLS LAST, id",
        "SELECT id, s FROM t ORDER BY s COLLATE NOCASE DESC NULLS FIRST, id",
        // the PK, whose sort the planner elides when the placement is default
        "SELECT id FROM t ORDER BY id NULLS LAST",
        "SELECT id FROM t ORDER BY id DESC NULLS FIRST",
    ] {
        cross_check(&db, q);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// MULTIPLE keys with DIFFERENT placements — the case where a single global
/// "nulls first" flag, or a placement folded into the direction, would pass
/// every single-key test above and still be wrong.
#[test]
fn per_key_placements_are_independent() {
    let (db, path) = open();

    for q in [
        "SELECT id, s, n FROM t ORDER BY s NULLS LAST, n NULLS FIRST, id",
        "SELECT id, s, n FROM t ORDER BY s NULLS FIRST, n NULLS LAST, id",
        "SELECT id, s, n FROM t ORDER BY s DESC NULLS LAST, n ASC NULLS FIRST, id",
        "SELECT id, s, n FROM t ORDER BY s ASC NULLS LAST, n DESC NULLS LAST, id",
        // one explicit key next to one default key
        "SELECT id, s, n FROM t ORDER BY s NULLS LAST, n, id",
        "SELECT id, s, n FROM t ORDER BY s, n DESC NULLS FIRST, id",
    ] {
        cross_check(&db, q);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// `ORDER BY … LIMIT n` over base-row keys takes the streaming top-K heap
/// instead of a full sort. Checked against sqlite AND against this engine's own
/// unbounded sort of the same query — the second assertion is what catches the
/// two comparator call sites drifting apart, which sqlite agreement alone
/// would not if BOTH were wrong the same way.
#[test]
fn the_top_k_heap_agrees_with_the_full_sort_and_with_sqlite() {
    let (db, path) = open();

    for order in [
        "s NULLS LAST, id",
        "s NULLS FIRST, id",
        "s DESC NULLS LAST, id",
        "s DESC NULLS FIRST, id",
        "n NULLS LAST, id",
        "n DESC NULLS FIRST, id",
        "allnull NULLS LAST, id",
        "allnull DESC NULLS FIRST, id",
        "s NULLS LAST, n NULLS FIRST, id",
        // the defaults, through the same path
        "s, id",
        "s DESC, id",
        "n, id",
    ] {
        let full = mpedb_rows(&db, &format!("SELECT id, s, n FROM t ORDER BY {order}"));
        for (limit, offset) in [(1, 0), (3, 0), (4, 2), (8, 0), (2, 6), (20, 0), (3, 7)] {
            let q = format!(
                "SELECT id, s, n FROM t ORDER BY {order} LIMIT {limit} OFFSET {offset}"
            );
            cross_check(&db, &q);
            let want: Vec<String> =
                full.iter().skip(offset).take(limit).cloned().collect();
            assert_eq!(
                mpedb_rows(&db, &q),
                want,
                "top-K disagrees with mpedb's own full sort for `{q}`"
            );
        }
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A malformed `NULLS` tail is refused BY NAME, and `NULLS FIRST/LAST` inside a
/// window's `OVER (ORDER BY …)` — which mpedb does NOT support — is refused by
/// name too, rather than being accepted and silently sorted the default way.
#[test]
fn malformed_and_unsupported_nulls_clauses_are_refused_by_name() {
    let (db, path) = open();
    for sql in [
        "SELECT id FROM t ORDER BY s NULLS",
        "SELECT id FROM t ORDER BY s NULLS SOMETIMES",
        "SELECT id FROM t ORDER BY s ASC NULLS 1",
    ] {
        let m = db.query(sql, &[]).expect_err(sql).to_string();
        assert!(
            m.contains("FIRST") && m.contains("LAST"),
            "the refusal must say what may follow NULLS: {m}\n  for {sql}"
        );
    }
    // The window ORDER BY is a DELIBERATE refusal: its comparator has no
    // placement to honour, and accepting the clause there would sort sqlite's
    // default way regardless of what was asked for.
    for sql in [
        "SELECT id, row_number() OVER (ORDER BY s NULLS LAST) FROM t",
        "SELECT id, rank() OVER (PARTITION BY n ORDER BY s DESC NULLS FIRST) FROM t",
    ] {
        let m = db.query(sql, &[]).expect_err(sql).to_string();
        assert!(
            m.contains("NULLS") && m.contains("not supported"),
            "the refusal must name NULLS inside OVER: {m}\n  for {sql}"
        );
    }
    // The same window statements WITHOUT the clause still work.
    assert!(db
        .query("SELECT id, row_number() OVER (ORDER BY s) FROM t", &[])
        .is_ok());
    drop(db);
    let _ = std::fs::remove_file(&path);
}
