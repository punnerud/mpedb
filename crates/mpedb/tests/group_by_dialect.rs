//! GROUP BY column-strictness dialect (`[compat] bare_group_by`, COMPAT.md).
//!
//! mpedb supports BOTH sqlite's lenient bare-column rule and PostgreSQL's strict
//! one, chosen by config. The hard constraint is mpedb's core guarantee: in
//! sqlite mode a bare column must produce sqlite's EXACT value or be refused —
//! never a guessed value. This test:
//!
//! (a) sqlite mode: differential-tests the accepted bare-column cases against the
//!     `sqlite3` CLI (const-folded-away `COALESCE`, and the single-min/max
//!     witness row — even alongside a count/sum — including ties, interior NULLs,
//!     all-NULL groups, no GROUP BY, and an empty table);
//! (b) sqlite mode, the ARBITRARY case (#88): a bare column with NO min/max — a
//!     count/sum/avg aggregate, or no aggregate — is now ACCEPTED and matches
//!     sqlite's lowest-rowid pick (differential, including out-of-rowid-order
//!     inserts). Still REFUSED where mpedb cannot reproduce that pick without a
//!     wrong answer: over a join, over a non-rowid (text/composite) primary key,
//!     or with two-or-more min/max (sqlite's order-dependent last-min/max pick) —
//!     EXCEPT when the last min/max is an unfiltered non-NULL-constant one, whose
//!     pick is provably the lowest-rowid row (differential-tested below);
//! (c) postgres mode: EVERY bare column is REJECTED (matching PostgreSQL, whose
//!     rejection was verified by hand against PG 16 — `column … must appear in
//!     the GROUP BY clause …`);
//! (d) the config default is sqlite, and an explicit `postgres` config is strict.
//!     (The mirror's PG-import → postgres default is covered in mpedb-mirror.)

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

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

# A join partner, for the "arbitrary bare column over a join is refused" edge.
[[table]]
name = "u"
primary_key = ["uid"]
  [[table.column]]
  name = "uid"
  type = "int64"
  [[table.column]]
  name = "gid"
  type = "int64"

# A table with a NON-rowid (text) primary key, for the "arbitrary bare column
# over a non-rowid PK is refused" edge — mpedb's min-PK is not sqlite's rowid.
[[table]]
name = "tk"
primary_key = ["k"]
  [[table.column]]
  name = "k"
  type = "text"
  [[table.column]]
  name = "g"
  type = "int64"
  [[table.column]]
  name = "v"
  type = "int64"
  nullable = true
"#;

/// The same table shape for the `sqlite3` reference engine (only `t` is used in
/// differential queries; the edge tables `u`/`tk` are mpedb-only refusal checks).
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

/// The arbitrary-case corpus, INSERTED OUT OF ROWID ORDER on purpose. sqlite's
/// bare-column pick follows the ROWID (the PK `id`), NOT insert order, so the
/// lowest-`id` row of each group is the reference answer:
///   g=10 → id 1 ('one'),  g=20 → id 4 ('four').
/// mpedb must reproduce that from its min-PK witness, not from insert order.
const OOO: &[Row] = &[
    (3, 10, Some(5), "three"),
    (1, 10, Some(9), "one"),
    (2, 10, Some(1), "two"),
    (6, 20, Some(4), "six"),
    (4, 20, None, "four"),
    (5, 20, Some(7), "five"),
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
        // Render a "clean" float the way the sqlite CLI does: an integral value
        // keeps a trailing `.0` (`5` → `5.0`); a terminating decimal (`5.5`) uses
        // the shortest round-trip form, which agrees with sqlite. (Repeating
        // decimals would diverge on the last digit, so the differential queries
        // that use `avg()` keep group averages clean.)
        Value::Float(f) if f.fract() == 0.0 && f.is_finite() => format!("{f:.1}"),
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

/// The bundled sqlite's answer to `query` over `DDL + inserts`, SORTED.
/// (Always available — it is compiled in — so the `Option` the subprocess
/// version had is gone; the differential half always runs.)
fn sqlite_rows(rows: &[Row], query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
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

    let mut parsed: Vec<Vec<String>> = sqlite_oracle::script_stdout(&script, "<NULL>")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(|s| s.to_string()).collect())
        .collect();
    parsed.sort();
    parsed
}

/// Assert mpedb's sqlite-mode answer equals sqlite's, over `rows`.
fn assert_matches_sqlite_data(db: &Database, rows: &[Row], query: &str) {
    let got = mpedb_rows(db, query);
    let want = sqlite_rows(rows, query);
    assert_eq!(got, want, "mpedb vs sqlite differ on `{query}`");
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
fn sqlite_mode_single_minmax_with_other_aggregate_follows_the_extremum() {
    // A single min()/max() governs the bare column EVEN alongside a count/sum/avg
    // — sqlite's documented rule ("exactly one min()/max()"). Verified vs sqlite
    // 3.45: `min(x), count(*)` follows the min row, not the lowest-rowid row.
    // (This whole shape was refused before #88.) Out-of-rowid-order data makes the
    // "extremum, not first-inserted" distinction visible.
    let (db, path) = open("mmplus", Some("sqlite"));
    load(&db, OOO);
    assert_matches_sqlite_data(&db, OOO, "SELECT g, name, min(x), count(*) FROM t GROUP BY g");
    assert_matches_sqlite_data(&db, OOO, "SELECT g, name, max(x), sum(x) FROM t GROUP BY g");
    assert_matches_sqlite_data(&db, OOO, "SELECT g, name, count(*), avg(x), max(x) FROM t GROUP BY g");
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
// (b) sqlite mode, the ARBITRARY case (#88): bare + no min/max now matches
//     sqlite's lowest-rowid pick; the unreproducible edges stay refused.
// ---------------------------------------------------------------------------

#[test]
fn sqlite_mode_arbitrary_bare_column_matches_lowest_rowid() {
    // bare + a non-min/max aggregate (count/sum/avg), and bare with NO aggregate.
    // sqlite's "arbitrary" pick is really the group's LOWEST-ROWID row; mpedb
    // reproduces it from its min-PK witness. OOO is inserted out of rowid order,
    // so a first-inserted (rather than lowest-rowid) pick would diverge here.
    let (db, path) = open("arb-match", Some("sqlite"));
    load(&db, OOO);
    for q in [
        "SELECT g, name, count(*) FROM t GROUP BY g",
        "SELECT g, name, sum(x) FROM t GROUP BY g",
        "SELECT g, name, avg(x) FROM t GROUP BY g",
        "SELECT g, count(*), name FROM t GROUP BY g",
        "SELECT g, name || '?', count(*) FROM t GROUP BY g",
        // bare with NO aggregate at all
        "SELECT g, name FROM t GROUP BY g",
        // no GROUP BY: one group over the whole table, lowest-rowid row (id 1).
        "SELECT name, count(*) FROM t",
    ] {
        assert_matches_sqlite_data(&db, OOO, q);
    }
    // Pin the rule directly: lowest rowid is id 1 ('one') for g=10 and id 4
    // ('four') for g=20 — NOT the first-inserted 'three'/'six'.
    let rows = mpedb_rows(&db, "SELECT g, name, count(*) FROM t GROUP BY g");
    assert!(rows.contains(&vec!["10".into(), "one".into(), "3".into()]), "{rows:?}");
    assert!(rows.contains(&vec!["20".into(), "four".into(), "3".into()]), "{rows:?}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_arbitrary_bare_column_lowest_rowid_survives_a_filter() {
    // The pick is the lowest rowid AMONG THE ROWS THAT SURVIVE THE WHERE — for
    // g=20 that drops id 4 (x NULL), so the pick becomes id 5 ('five'). mpedb's
    // min-PK is taken over the same filtered set, so it still matches sqlite.
    let (db, path) = open("arb-filter", Some("sqlite"));
    load(&db, OOO);
    assert_matches_sqlite_data(
        &db,
        OOO,
        "SELECT g, name, count(*) FROM t WHERE x IS NOT NULL GROUP BY g",
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_refuses_the_unreproducible_arbitrary_edges() {
    // These are the cases mpedb CANNOT reproduce as sqlite's exact value, so it
    // refuses (never a wrong answer) even though sqlite accepts them:
    let (db, path) = open("arb-refuse", Some("sqlite"));
    load(&db, DATA);
    for q in [
        // Two-or-more min/max: sqlite follows the LAST min/max — an order-
        // dependent pick its own docs call "arbitrary" (verified: `min(x), max(x)`
        // and `max(x), min(x)` give DIFFERENT bare rows). Refused.
        "SELECT name, min(x), max(x) FROM t GROUP BY g",
        "SELECT name, max(x), max(x + 1) FROM t GROUP BY g",
        // Over a JOIN there is no single rowid to pick by (the row is
        // `[outer ‖ inner]`), so the arbitrary case stays refused.
        "SELECT t.name, count(*) FROM t JOIN u ON t.g = u.gid GROUP BY t.g",
    ] {
        let err = db.query(q, &[]).unwrap_err().to_string();
        assert!(
            err.contains("must appear in GROUP BY"),
            "sqlite mode should refuse `{q}`, got: {err}"
        );
    }
    // Over a NON-rowid (text) primary key, mpedb's min-PK is not sqlite's rowid,
    // so the arbitrary case is refused there too — but a single min/max still
    // works (its witness rule does not depend on the PK being the rowid).
    for q in [
        "SELECT g, v FROM tk GROUP BY g",
        "SELECT g, v, count(*) FROM tk GROUP BY g",
    ] {
        let err = db.query(q, &[]).unwrap_err().to_string();
        assert!(
            err.contains("must appear in GROUP BY"),
            "non-rowid PK arbitrary bare column should be refused `{q}`, got: {err}"
        );
    }
    assert!(
        db.query("SELECT g, v, min(v) FROM tk GROUP BY g", &[]).is_ok(),
        "a single min/max over a non-rowid PK is still accepted (witness rule)"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn sqlite_mode_two_minmax_with_const_last_follows_lowest_rowid() {
    // The ≥2-min/max carve-out (slt_good_12.test:72600 shape): sqlite follows
    // the LAST min/max, and one with a non-NULL CONSTANT argument "improves"
    // only on the group's first row — so the pick is the LOWEST-ROWID row,
    // which mpedb reproduces with the same min-PK witness as the 0-min/max
    // case. Every accepted shape is verified against the bundled sqlite.
    let (db, path) = open("mm-const-last", Some("sqlite"));
    load(&db, DATA);
    for q in [
        // min-const and max-const last; includes the ties (g=10), the interior
        // NULL (g=20) and the all-NULL group (g=30 — sqlite still takes the
        // FIRST row here, not the all-NULL "last row" of the single-min rule).
        "SELECT g, name, min(x), min(-52) FROM t GROUP BY g",
        "SELECT g, name, min(x), max(99) FROM t GROUP BY g",
        // Three min/max: only the LAST governs, the others may be arbitrary.
        "SELECT g, name, min(x), max(x), min(-52) FROM t GROUP BY g",
        // The corpus shape: both min/max live in HAVING, const one last.
        "SELECT g, name FROM t GROUP BY g HAVING min(x) IS NULL OR min(-52) = -52",
    ] {
        assert_matches_sqlite(&db, q);
    }
    let (db2, path2) = open("mm-const-last-ooo", Some("sqlite"));
    load(&db2, OOO);
    assert_matches_sqlite_data(
        &db2,
        OOO,
        "SELECT g, name, min(x), min(-52) FROM t GROUP BY g",
    );
    let _ = std::fs::remove_file(path2);
    // Still refused — each of these breaks the "provably lowest-rowid" proof:
    for q in [
        // The const min/max is NOT last: sqlite follows min(x)'s extremum row.
        "SELECT name, min(-52), min(x) FROM t GROUP BY g",
        // A NULL constant never "improves", so sqlite drifts to the LAST row.
        "SELECT name, min(x), max(NULL) FROM t GROUP BY g",
        // A FILTER moves the pick to the first row the filter ACCEPTS.
        "SELECT name, min(x), min(-52) FILTER (WHERE x > 3) FROM t GROUP BY g",
        // ORDER BY references a min/max: sqlite builds its aggregate list as
        // SELECT → ORDER BY → HAVING, so ITS last min/max is min(x) (probed on
        // 3.45: the bare column follows the HAVING min/max, not the ORDER BY
        // one) while mpedb lifts HAVING first — refused instead of guessing.
        "SELECT g, name FROM t GROUP BY g HAVING min(x) < 100 ORDER BY min(-52)",
    ] {
        let err = db.query(q, &[]).unwrap_err().to_string();
        assert!(
            err.contains("must appear in GROUP BY"),
            "should refuse `{q}`, got: {err}"
        );
    }
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// (c) postgres mode: EVERY bare column is rejected (matching PostgreSQL).
// ---------------------------------------------------------------------------

#[test]
fn postgres_mode_rejects_every_bare_column() {
    let (db, path) = open("pgstrict", Some("postgres"));
    load(&db, DATA);
    for q in [
        "SELECT g, COALESCE(-24, x) FROM t GROUP BY g",
        "SELECT g, name, max(x) FROM t GROUP BY g",
        "SELECT g, name, min(x) FROM t GROUP BY g",
        "SELECT g, name, count(*) FROM t GROUP BY g",
        "SELECT g, name, sum(x) FROM t GROUP BY g",
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
