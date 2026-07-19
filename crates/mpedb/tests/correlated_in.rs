//! CORRELATED `x IN (SELECT …)` (#97).
//!
//! When the `List` subplan kind landed (#70) the per-row correlation fill did
//! not yet exist, so a correlated `IN` was refused by name ("rewrite as
//! EXISTS"). The refusal outlived its reason: everything a correlated `IN`
//! needs was already built for `EXISTS`/scalar and is kind-agnostic —
//!
//! * `split_correlated` classifies `BExpr::InParam(_, slot)` as a correlated
//!   reference, so the conjunct lands in `post_filter`, never gather-side;
//! * `correlated_survivors` fills the slot per outer row with the same
//!   `subplan_value` call, memoized by the same encoded correlation tuple;
//! * `validate`'s `gather_ok` already counts `Instr::InParam` as a slot read,
//!   so the gather-side discipline covers the new shape unchanged.
//!
//! The whole risk is therefore SEMANTIC, not structural, and it is concentrated
//! in one place: `IN`'s three-valued logic. `x IN (list)` is TRUE on a match,
//! FALSE only when the list is free of NULLs, and UNKNOWN otherwise — so
//! `NOT IN` over a NULL-bearing list is NEVER TRUE, and `NOT IN ()` over an
//! EMPTY list IS TRUE. Both halves are pinned below, per row, against the
//! `sqlite3` CLI (3.45.1).
//!
//! Every query runs four ways: {no index, index on the probed columns} ×
//! {memo on, memo off}. An access-path change and a memoization change must
//! each leave every answer bit-identical.

use mpedb::{Config, Database, ExecResult, Value};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// `a` is the outer, `b` the correlated inner.
///
/// The data is chosen so that ONE query exercises every 3VL corner at once:
/// * `a.k` is NULL in row 3 → the probe value is NULL;
/// * `b.ak` is NULL in row 3 → the inner list carries a NULL;
/// * `b.ak` = 10 twice → the inner list has duplicates;
/// * `a.v` = 0 in row 5 → the inner result is EMPTY for that row;
/// * rows 1 and 7 share `a.v` = 5 → the same correlation tuple twice, so the
///   memo HITS and must serve the same list;
/// * `a.k` = 99 in row 8 → a non-NULL probe that matches nothing, which is the
///   row `NOT IN` disagrees on.
const SEED: &[&str] = &[
    "INSERT INTO a VALUES (1, 10, 5)",
    "INSERT INTO a VALUES (2, 20, 15)",
    "INSERT INTO a VALUES (3, NULL, 25)",
    "INSERT INTO a VALUES (4, 10, 35)",
    "INSERT INTO a VALUES (5, 40, 0)",
    "INSERT INTO a VALUES (6, 30, 25)",
    "INSERT INTO a VALUES (7, 10, 5)",
    "INSERT INTO a VALUES (8, 99, 40)",
    "INSERT INTO b VALUES (1, 10, 1)",
    "INSERT INTO b VALUES (2, 10, 2)",
    "INSERT INTO b VALUES (3, NULL, 3)",
    "INSERT INTO b VALUES (4, 20, 4)",
    "INSERT INTO b VALUES (5, 40, 100)",
    "INSERT INTO b VALUES (6, 30, 20)",
];

const SQLITE_SCHEMA: &str = "\
CREATE TABLE a (id INTEGER PRIMARY KEY, k INT, v INT) STRICT;
CREATE TABLE b (id INTEGER PRIMARY KEY, ak INT, w INT) STRICT;
";
const SQLITE_INDEXES: &str = "CREATE INDEX a_k ON a(k); CREATE INDEX b_ak ON b(ak);\n";

fn mpedb_open(indexed: bool) -> (Database, String) {
    let path = format!(
        "/dev/shm/mpedb-corrin-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let idx = if indexed { "indexed = true\n" } else { "" };
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"a\"\nprimary_key = [\"id\"]\n\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"k\"\ntype = \"int64\"\nnullable = true\n{idx}\n\
         [[table.column]]\nname = \"v\"\ntype = \"int64\"\nnullable = true\n\n\
         [[table]]\nname = \"b\"\nprimary_key = [\"id\"]\n\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"ak\"\ntype = \"int64\"\nnullable = true\n{idx}\n\
         [[table.column]]\nname = \"w\"\ntype = \"int64\"\nnullable = true\n"
    );
    let db =
        Database::open_with_config(Config::from_toml_str(&toml).expect("config")).expect("open");
    for s in SEED {
        db.query(s, &[]).expect(s);
    }
    (db, path)
}

/// mpedb's answer, rendered the way sqlite's list mode renders it. Taken
/// through the FULL prepare → encode → decode → **validate** → execute path:
/// `Database::query` executes the in-memory plan and never validates, so a
/// stale validate rule would only surface on the registry/detached path — which
/// is exactly where the old "correlated IN-list subplan" refusal lived.
fn mpedb_rows(db: &Database, q: &str) -> Vec<String> {
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
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|c| match c {
                    Value::Null => "NULL".to_string(),
                    Value::Int(i) => i.to_string(),
                    // sqlite has no bool: a comparison yields 1/0.
                    Value::Bool(b) => (*b as u8).to_string(),
                    Value::Text(t) => t.clone(),
                    other => panic!("unexpected value {other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn sqlite_rows(q: &str, indexed: bool) -> Vec<String> {
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
    sqlite_oracle::script_stdout(&script, "NULL").lines().map(str::to_string).collect()
}

/// Every shape. Each is ORDER BY-ed so the comparison is order-exact.
const QUERIES: &[&str] = &[
    // --- the plain shape, WHERE position ------------------------------------
    // Row 3 probes NULL (UNKNOWN), row 5's inner is EMPTY (FALSE), row 8's
    // probe matches nothing though the list is NULL-bearing (UNKNOWN).
    "SELECT a.id FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) ORDER BY a.id",
    // The 3VL half that a naive `!contains` gets wrong: `NOT IN` over a
    // NULL-bearing list is NEVER TRUE, but `NOT IN ()` over an empty one IS.
    "SELECT a.id FROM a WHERE a.k NOT IN (SELECT b.ak FROM b WHERE b.w < a.v) ORDER BY a.id",
    // The inner filtered free of NULLs: now `NOT IN` can be TRUE.
    "SELECT a.id FROM a WHERE a.k NOT IN (SELECT b.ak FROM b WHERE b.w < a.v AND b.ak IS NOT NULL) ORDER BY a.id",
    // --- the value in the SELECT list: TRUE / FALSE / UNKNOWN, per row ------
    "SELECT a.id, a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) FROM a ORDER BY a.id",
    "SELECT a.id, a.k NOT IN (SELECT b.ak FROM b WHERE b.w < a.v) FROM a ORDER BY a.id",
    "SELECT a.id, a.k IN (SELECT b.ak FROM b WHERE b.w < a.v AND b.ak IS NOT NULL) FROM a ORDER BY a.id",
    // --- inside an aggregate query ------------------------------------------
    "SELECT count(*) FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v)",
    "SELECT a.k, count(*) FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) GROUP BY a.k ORDER BY 1",
    "SELECT a.k, count(*) FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) GROUP BY a.k HAVING count(*) > 1 ORDER BY 1",
    "SELECT sum(a.v) FROM a WHERE a.k NOT IN (SELECT b.ak FROM b WHERE b.w < a.v AND b.ak IS NOT NULL)",
    // An inner result that matches NOTHING for every row: the empty-group zero.
    "SELECT count(*) FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w > 10000)",
    // --- a JOINed outer ------------------------------------------------------
    "SELECT a.id, b.id FROM a JOIN b ON a.k = b.ak WHERE a.k IN (SELECT c.ak FROM b c WHERE c.w < a.v) ORDER BY a.id, b.id",
    "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k = b.ak WHERE a.k IN (SELECT c.ak FROM b c WHERE c.w < a.v) ORDER BY a.id, b.id",
    // Correlated to the JOINED side, not the outer table.
    "SELECT a.id, b.id FROM a JOIN b ON a.k = b.ak WHERE a.k IN (SELECT c.ak FROM b c WHERE c.w < b.w) ORDER BY a.id, b.id",
    // --- richer inner bodies -------------------------------------------------
    // GROUP BY + HAVING in the correlated body.
    "SELECT a.id FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v GROUP BY b.ak HAVING count(*) > 1) ORDER BY a.id",
    // ORDER BY + LIMIT in the correlated body (the consumer cap must NOT apply
    // to a List subplan — it needs every value).
    "SELECT a.id FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v ORDER BY b.w DESC LIMIT 2) ORDER BY a.id",
    "SELECT a.id FROM a WHERE a.k IN (SELECT DISTINCT b.ak FROM b WHERE b.w < a.v) ORDER BY a.id",
    // A NESTED subquery inside the correlated body.
    "SELECT a.id FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v AND b.w > (SELECT min(c.w) FROM b c)) ORDER BY a.id",
    // TWO correlation args.
    "SELECT a.id FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v AND b.ak <> a.id) ORDER BY a.id",
    // A correlated IN and a correlated EXISTS in the same WHERE.
    "SELECT a.id FROM a WHERE a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) AND EXISTS (SELECT 1 FROM b c WHERE c.w > a.v) ORDER BY a.id",
    // A correlated IN OR-ed with an ordinary predicate — the whole disjunct is
    // correlated, so it must run in `post_filter`, after the access path.
    "SELECT a.id FROM a WHERE a.id = 1 OR a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) ORDER BY a.id",
    // The outer's access path is a PK point AND the IN is correlated.
    "SELECT a.id FROM a WHERE a.id = 4 AND a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) ORDER BY a.id",
    // An expression, not a bare column, on the left of IN.
    "SELECT a.id FROM a WHERE a.k + 0 IN (SELECT b.ak FROM b WHERE b.w < a.v) ORDER BY a.id",
    // Negated with NOT(… IN …) rather than NOT IN.
    "SELECT a.id FROM a WHERE NOT (a.k IN (SELECT b.ak FROM b WHERE b.w < a.v)) ORDER BY a.id",
    // ORDER BY over a correlated IN value.
    "SELECT a.id FROM a ORDER BY a.k IN (SELECT b.ak FROM b WHERE b.w < a.v), a.id",
    // A correlated IN as a GROUP BY key, next to a correlated IN in the WHERE
    // (#97: a group key is a PER-ROW program, so both are filled per row).
    "SELECT a.k IN (SELECT b.ak FROM b) AS m, count(a.v) FROM a \
     WHERE a.id IN (SELECT b.id FROM b WHERE b.w < a.v) GROUP BY 1 ORDER BY 1",
    "SELECT a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) AS m, count(*) FROM a GROUP BY 1 ORDER BY 1",
];

fn agree(indexed: bool) {
    let (db, path) = mpedb_open(indexed);
    for q in QUERIES {
        let mine = mpedb_rows(&db, q);
        let theirs = sqlite_rows(q, indexed);
        assert_eq!(
            mine, theirs,
            "\n  query : {q}\n  indexed: {indexed}\n  mpedb : {mine:?}\n  sqlite: {theirs:?}"
        );
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The whole matrix, four ways. One `#[test]` because the memo A/B toggles a
/// PROCESS-wide environment variable, which parallel tests in the same binary
/// would race on.
#[test]
fn correlated_in_matches_sqlite() {
    for indexed in [false, true] {
        agree(indexed);
    }
    // Same answers with the per-correlation-tuple memo disabled. Rows 1 and 7
    // share a correlation tuple, so the memo genuinely hits above; this pass
    // proves the cached list is the list a re-execution would produce.
    std::env::set_var("MPEDB_NO_SUBPLAN_MEMO", "1");
    for indexed in [false, true] {
        agree(indexed);
    }
    std::env::remove_var("MPEDB_NO_SUBPLAN_MEMO");
}

/// The refusals that STAY. A correlated IN is admitted only where a correlated
/// EXISTS is: `HAVING`, a JOIN's `ON` and a grouped program are still holes at
/// the time the slot would be read, and are refused by name rather than read
/// unfilled.
#[test]
fn documented_refusals() {
    let (db, path) = mpedb_open(false);
    let refuse = |sql: &str, needle: &str| {
        let e = db
            .query(sql, &[])
            .expect_err(&format!("expected a refusal for `{sql}`"))
            .to_string();
        assert!(e.contains(needle), "wrong refusal for `{sql}`: {e}");
    };
    refuse(
        "SELECT count(*) FROM a GROUP BY a.k HAVING a.k IN (SELECT b.ak FROM b WHERE b.w < 3)",
        "a subquery in HAVING is not supported yet",
    );
    refuse(
        "SELECT a.id FROM a JOIN b ON a.k IN (SELECT c.ak FROM b c WHERE c.w < a.v)",
        "a subquery in a JOIN's ON condition is not supported yet",
    );
    // A grouped SELECT-list expression that is NOT itself a group key reads the
    // correlated slot after the collapse — refused. (`GROUP BY 1`, where the
    // item IS the key, is answered; see the matrix above.)
    refuse(
        "SELECT count(*), a.k IN (SELECT b.ak FROM b WHERE b.w < a.v) FROM a",
        "only supported where it is evaluated PER ROW",
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}
