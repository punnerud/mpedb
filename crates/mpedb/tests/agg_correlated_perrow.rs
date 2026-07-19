//! A correlated subquery in the PER-ROW positions of an aggregate query (#97):
//! a `GROUP BY` key, an aggregate ARGUMENT, and (already shipped) an aggregate's
//! `FILTER (WHERE …)`.
//!
//! #73 §1 admitted a correlated subquery only in the `WHERE`, on the argument
//! that "those are evaluated over the grouped tuple AFTER the per-row
//! correlation has been collapsed". That is true of `HAVING` and of the grouped
//! projection. It is **not** true of a GROUP BY key or an aggregate argument:
//! `exec_aggregate`'s row loop evaluates both — and the `FILTER` predicate, and
//! the bare-column witness's min/max argument — against `row_params`, which is
//! precisely THAT row's scratch with its correlated slots already filled by
//! `correlated_survivors`. So "which row's correlation?" has an answer: this
//! one's. The executor needed no change; only the planner refusal and its
//! `validate` mirror moved from "WHERE only" to "per-row positions only".
//!
//! This is the whole of Django's `.annotate(x=Subquery(...)).values('x').
//! annotate(Count(...))` shape — `SELECT (corr), count(*) … GROUP BY 1` — where
//! `lift_aggs` maps the select item onto the grouped tuple's `__g0`, so the
//! projection reads the KEY the row loop computed, never the slot.
//!
//! What STAYS refused, and why it must: `HAVING`, and a SELECT-list expression
//! that is NOT itself a group key. Both run after the collapse, against `params`,
//! where a correlated slot is still NULL — reading one there is a silent wrong
//! answer, not a missing feature.
//!
//! Every query is differential against the `sqlite3` CLI (3.45.1), run with and
//! without an index on the correlated column: an access-path change must not
//! change an answer.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// `a` is the aggregated outer, `b` the correlated inner. The data pins the
/// corners: `a.g` is NULL in one row (the correlation value is NULL), `b.k` is
/// NULL in one row, `b.k` = 10 twice (the inner counts more than one), `a.g` =
/// 40 and 50 match NOTHING (the inner is EMPTY, so `count(*)` is 0 and
/// `max(w)` is NULL), and three `a` rows share `g` = 10 so the per-correlation
/// memo genuinely hits inside one group and across groups.
const SEED: &[&str] = &[
    "INSERT INTO a VALUES (1, 'eng',   10)",
    "INSERT INTO a VALUES (2, 'eng',   20)",
    "INSERT INTO a VALUES (3, 'sales', 10)",
    "INSERT INTO a VALUES (4, 'sales', 40)",
    "INSERT INTO a VALUES (5, 'hr',    50)",
    "INSERT INTO a VALUES (6, 'eng',   10)",
    "INSERT INTO a VALUES (7, 'hr',    NULL)",
    "INSERT INTO a VALUES (8, NULL,    20)",
    "INSERT INTO b VALUES (1, 10, 100)",
    "INSERT INTO b VALUES (2, 20, 200)",
    "INSERT INTO b VALUES (3, 10, 300)",
    "INSERT INTO b VALUES (4, NULL, 400)",
];

const SQLITE_SCHEMA: &str = "\
CREATE TABLE a (id INTEGER PRIMARY KEY, dept TEXT, g INT) STRICT;
CREATE TABLE b (bid INTEGER PRIMARY KEY, k INT, w INT) STRICT;
";
const SQLITE_INDEXES: &str = "CREATE INDEX a_g ON a(g); CREATE INDEX b_k ON b(k);\n";

fn mpedb_open(indexed: bool) -> (Database, String) {
    let path = format!(
        "/dev/shm/mpedb-aggcorrpr-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let idx = if indexed { "indexed = true\n" } else { "" };
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"a\"\nprimary_key = [\"id\"]\n\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"dept\"\ntype = \"text\"\nnullable = true\n\n\
         [[table.column]]\nname = \"g\"\ntype = \"int64\"\nnullable = true\n{idx}\n\
         [[table]]\nname = \"b\"\nprimary_key = [\"bid\"]\n\n\
         [[table.column]]\nname = \"bid\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"k\"\ntype = \"int64\"\nnullable = true\n{idx}\n\
         [[table.column]]\nname = \"w\"\ntype = \"int64\"\nnullable = true\n"
    );
    let db =
        Database::open_with_config(Config::from_toml_str(&toml).expect("config")).expect("open");
    for s in SEED {
        db.query(s, &[]).expect(s);
    }
    (db, path)
}

/// mpedb's answer as `|`-joined cells, through the FULL prepare → encode →
/// decode → **validate** → execute path (`Database::query` never validates, and
/// the moved rule has a `validate` mirror).
fn mpedb_rows(db: &Database, q: &str) -> Vec<Vec<String>> {
    let plan = db
        .prepare_detached(q)
        .unwrap_or_else(|e| panic!("mpedb refused `{q}`: {e}"));
    let rows = match db
        .execute_detached(&plan, &[])
        .unwrap_or_else(|e| panic!("mpedb failed `{q}`: {e}"))
    {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{q}`, got {other:?}"),
    };
    rows.iter().map(|r| r.iter().map(render).collect()).collect()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => (*b as u8).to_string(),
        Value::Text(t) => t.clone(),
        // sqlite prints REAL with ~15 significant digits; every value this test
        // produces is a small exact integer-valued double, so `%g`-ish rendering
        // agrees. Compared numerically below anyway.
        Value::Float(f) => format!("{f}"),
        other => panic!("unexpected value {other:?}"),
    }
}

fn sqlite_rows(q: &str, indexed: bool) -> Vec<Vec<String>> {
    let mut script = String::new();
    script.push_str(SQLITE_SCHEMA);
    if indexed {
        script.push_str(SQLITE_INDEXES);
    }
    for s in SEED {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(q);
    script.push_str(";\n");
    sqlite_oracle::script_stdout(&script, "NULL")
        .lines()
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// One cell: exact for everything except a REAL, which compares numerically
/// (sqlite renders `avg()`/`sum()` of ints as `%!.15g` text).
fn cell_eq(mine: &str, theirs: &str) -> bool {
    if mine == theirs {
        return true;
    }
    match (mine.parse::<f64>(), theirs.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
        _ => false,
    }
}

const QUERIES: &[&str] = &[
    // --- correlated subquery as an aggregate ARGUMENT -----------------------
    // `a.g` = 10 → 2, 20 → 1, 40/50 → 0, NULL → 0. Scalar (no GROUP BY).
    "SELECT sum((SELECT count(*) FROM b WHERE b.k = a.g)) FROM a",
    "SELECT count((SELECT max(b.w) FROM b WHERE b.k = a.g)) FROM a",
    "SELECT min((SELECT max(b.w) FROM b WHERE b.k = a.g)), max((SELECT max(b.w) FROM b WHERE b.k = a.g)) FROM a",
    // DISTINCT inside the aggregate, over correlated values.
    "SELECT count(DISTINCT (SELECT count(*) FROM b WHERE b.k = a.g)) FROM a",
    // Grouped, with a NULL group key (`a.dept` is NULL in row 8).
    "SELECT a.dept, sum((SELECT count(*) FROM b WHERE b.k = a.g)) FROM a GROUP BY a.dept ORDER BY 1",
    // The argument is an EXPRESSION over the correlated value.
    "SELECT a.dept, sum((SELECT count(*) FROM b WHERE b.k = a.g) * 2 + 1) FROM a GROUP BY a.dept ORDER BY 1",
    // A correlated aggregate argument alongside a correlated WHERE.
    "SELECT sum((SELECT count(*) FROM b WHERE b.k = a.g)) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g)",
    // …and alongside an ordinary aggregate.
    "SELECT a.dept, count(*), sum((SELECT count(*) FROM b WHERE b.k = a.g)) FROM a GROUP BY a.dept ORDER BY 1",
    // --- correlated subquery as the GROUP BY key ----------------------------
    // The Django shape: the annotation is BOTH the select item and the key.
    "SELECT (SELECT max(b.w) FROM b WHERE b.k = a.g) AS x, count(*) FROM a GROUP BY 1 ORDER BY 1",
    "SELECT EXISTS (SELECT 1 FROM b WHERE b.k = a.g) AS e, count(*) FROM a GROUP BY 1 ORDER BY 1",
    "SELECT (SELECT count(*) FROM b WHERE b.k = a.g) AS n, count(*), sum(a.id) FROM a GROUP BY 1 ORDER BY 1",
    // The key written out rather than by ordinal, and NOT projected.
    "SELECT count(*) FROM a GROUP BY (SELECT max(b.w) FROM b WHERE b.k = a.g) ORDER BY 1",
    // A correlated key beside an ordinary key.
    "SELECT a.dept, EXISTS (SELECT 1 FROM b WHERE b.k = a.g) AS e, count(*) FROM a GROUP BY a.dept, 2 ORDER BY 1, 2",
    // A correlated IN as the key (#97's other half).
    "SELECT a.g IN (SELECT b.k FROM b WHERE b.w < 350) AS m, count(*) FROM a GROUP BY 1 ORDER BY 1",
    "SELECT a.g IN (SELECT b.k FROM b WHERE b.w < a.g * 20) AS m, count(*) FROM a GROUP BY 1 ORDER BY 1",
    // An UNCORRELATED subquery as the key (filled once, before dispatch).
    "SELECT (SELECT max(b.w) FROM b) AS x, count(*) FROM a GROUP BY 1 ORDER BY 1",
    // --- ORDER BY / LIMIT over a correlated key -----------------------------
    "SELECT (SELECT count(*) FROM b WHERE b.k = a.g) AS n, count(*) FROM a GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 2",
    // --- the sqlite bare-column witness, governed by a correlated min() -----
    // sqlite carries `a.id` from the row that achieved the min; that min's
    // ARGUMENT is the correlated value, so the witness itself depends on the
    // per-row fill.
    "SELECT a.dept, min((SELECT count(*) FROM b WHERE b.k = a.g)), a.id FROM a GROUP BY a.dept ORDER BY 1",
    // --- FILTER (already legal, kept here so the three per-row positions are
    //     exercised together) ------------------------------------------------
    "SELECT a.dept, count(*) FILTER (WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.g)), sum((SELECT count(*) FROM b WHERE b.k = a.g)) FROM a GROUP BY a.dept ORDER BY 1",
    // --- a JOINed base row ---------------------------------------------------
    "SELECT a.dept, sum((SELECT count(*) FROM b WHERE b.k = a.g)) FROM a LEFT JOIN b ON a.g = b.k GROUP BY a.dept ORDER BY 1",
];

fn agree(indexed: bool) {
    let (db, path) = mpedb_open(indexed);
    for q in QUERIES {
        let mine = mpedb_rows(&db, q);
        let theirs = sqlite_rows(q, indexed);
        let ok = mine.len() == theirs.len()
            && mine.iter().zip(&theirs).all(|(m, s)| {
                m.len() == s.len() && m.iter().zip(s).all(|(x, y)| cell_eq(x, y))
            });
        assert!(
            ok,
            "\n  query : {q}\n  indexed: {indexed}\n  mpedb : {mine:?}\n  sqlite: {theirs:?}"
        );
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn per_row_correlated_aggregate_matches_sqlite() {
    for indexed in [false, true] {
        agree(indexed);
    }
    // The memo caches a correlated result per encoded correlation tuple; three
    // `a` rows share `g = 10`, so it genuinely hits. Same answers without it.
    std::env::set_var("MPEDB_NO_SUBPLAN_MEMO", "1");
    for indexed in [false, true] {
        agree(indexed);
    }
    std::env::remove_var("MPEDB_NO_SUBPLAN_MEMO");
}

/// The per-GROUP positions stay refused, each by name. These run after the
/// collapse, against `params`, where every correlated slot is still NULL — so
/// admitting them would not be a feature, it would be a silent wrong answer.
#[test]
fn per_group_positions_stay_refused() {
    let (db, path) = mpedb_open(false);
    let refuse = |sql: &str, needle: &str| {
        let e = db
            .prepare_detached(sql)
            .expect_err(&format!("expected a refusal for `{sql}`"))
            .to_string();
        assert!(e.contains(needle), "wrong refusal for `{sql}`: {e}");
    };
    // A correlated subquery in the SELECT list of an aggregate query that is
    // NOT a GROUP BY key: the projection runs over the collapsed group.
    refuse(
        "SELECT count(*), (SELECT count(*) FROM b WHERE b.k = a.g) FROM a",
        "a correlated subquery in an aggregate query is only supported where it is \
         evaluated PER ROW",
    );
    refuse(
        "SELECT a.dept, count(*), (SELECT count(*) FROM b WHERE b.k = a.g) FROM a GROUP BY a.dept",
        "a correlated subquery in an aggregate query is only supported where it is \
         evaluated PER ROW",
    );
    // ANY subquery written inside HAVING is refused by the lift, before any of
    // this. That is broader than the per-row rule strictly needs — `HAVING
    // sum((SELECT …)) > 1` puts the correlated value in a per-ROW aggregate
    // ARGUMENT, which the row loop could evaluate — but the lift does not
    // descend into HAVING at all, so the refusal is uniform and named. Left as
    // is: it is a refusal, never an answer.
    refuse(
        "SELECT a.dept, count(*) FROM a GROUP BY a.dept HAVING (SELECT count(*) FROM b WHERE b.k = a.g) > 0",
        "a subquery in HAVING is not supported yet",
    );
    refuse(
        "SELECT a.dept FROM a GROUP BY a.dept HAVING sum((SELECT count(*) FROM b WHERE b.k = a.g)) > 1",
        "a subquery in HAVING is not supported yet",
    );
    // The SAME subquery spelled out in both the select list and the GROUP BY
    // lifts TWICE, into two slots, so the item is not recognised as the key —
    // a clean refusal. (`GROUP BY 1`, above, takes the matching path.)
    refuse(
        "SELECT (SELECT count(*) FROM b WHERE b.k = a.g), count(*) FROM a \
         GROUP BY (SELECT count(*) FROM b WHERE b.k = a.g)",
        "a correlated subquery in an aggregate query is only supported where it is \
         evaluated PER ROW",
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}
