//! Native full-text search (FTS5) + the `MATCH` operator (design/DESIGN-FTS.md
//! stage 1), cross-checked against the `sqlite3` CLI (3.45, built with FTS5).
//!
//! The `CREATE VIRTUAL TABLE … USING fts5(…)`, `INSERT`, `UPDATE`, `DELETE` and
//! `SELECT … MATCH` statements are byte-identical between the two engines, so
//! every case here runs the SAME script against mpedb and against sqlite and
//! asserts row-for-row equality. mpedb stage 1 requires an EXPLICIT `rowid` on
//! insert (auto-rowid is stage 1b), so the scripts always supply it — which
//! sqlite accepts too, keeping the diff exact.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database file, so a panicking test leaks nothing in /dev/shm.
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

fn fresh_path() -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    format!(
        "{dir}/mpedb-fts-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// A fresh mpedb database with a throwaway seed table (mpedb requires at least
/// one config table); everything FTS is created live by the test script.
fn open(path: &str) -> Database {
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

fn db_with(setup: &[&str]) -> Tmp {
    let path = fresh_path();
    let _ = std::fs::remove_file(&path);
    let db = open(&path);
    for stmt in setup {
        db.query(stmt, &[]).unwrap_or_else(|e| panic!("setup `{stmt}` failed: {e:?}"));
    }
    Tmp { db, path }
}

/// Render one cell exactly as the `sqlite3` CLI's default list mode does: NULL
/// empty, integers/text verbatim.
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("unexpected value in FTS test: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            rows.iter().map(|r| r.iter().map(render).collect()).collect()
        }
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Run `setup` + `query` through the sqlite3 CLI, parsing default list mode.
fn sqlite_rows(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
    for stmt in setup {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    sqlite_oracle::script_stdout(&script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Assert mpedb and sqlite return the SAME rows for every query, given the same
/// `setup` script.
fn differential(setup: &[&str], queries: &[&str]) {
    let d = db_with(setup);
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(setup, q), "mismatch on `{q}`");
    }
}

const DOCS: &[&str] = &[
    "CREATE VIRTUAL TABLE ft USING fts5(title, body)",
    "INSERT INTO ft(rowid, title, body) VALUES (1, 'The Quick Brown Fox', 'jumps over the lazy dog')",
    "INSERT INTO ft(rowid, title, body) VALUES (2, 'Quick Start Guide', 'brown sugar and spice')",
    "INSERT INTO ft(rowid, title, body) VALUES (3, 'Slow and Steady', 'the tortoise wins the race')",
    "INSERT INTO ft(rowid, title, body) VALUES (4, 'Running Fast', 'a fox runs quickly through fields')",
    "INSERT INTO ft(rowid, title, body) VALUES (5, 'Lazy Sunday', 'the dog sleeps all day')",
];

#[test]
fn single_term_whole_row_and_column() {
    differential(
        DOCS,
        &[
            // whole-row single term
            "SELECT rowid FROM ft WHERE ft MATCH 'fox' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'quick' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'the' ORDER BY rowid",
            // column-scoped
            "SELECT rowid FROM ft WHERE title MATCH 'quick' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE body MATCH 'brown' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE title MATCH 'brown' ORDER BY rowid",
            // no matches
            "SELECT rowid FROM ft WHERE ft MATCH 'elephant' ORDER BY rowid",
            // returns content columns in rowid order
            "SELECT rowid, title FROM ft WHERE ft MATCH 'fox' ORDER BY rowid",
        ],
    );
}

#[test]
fn boolean_and_or_not() {
    differential(
        DOCS,
        &[
            "SELECT rowid FROM ft WHERE ft MATCH 'quick AND brown' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'quick brown' ORDER BY rowid", // implicit AND
            "SELECT rowid FROM ft WHERE ft MATCH 'fox OR tortoise' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'the NOT dog' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'lazy OR quick NOT guide' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH '(fox OR dog) AND the' ORDER BY rowid",
            // AND binds tighter than OR
            "SELECT rowid FROM ft WHERE ft MATCH 'fox OR quick AND guide' ORDER BY rowid",
        ],
    );
}

#[test]
fn prefix_initial_and_column_filters() {
    differential(
        DOCS,
        &[
            // prefix
            "SELECT rowid FROM ft WHERE ft MATCH 'run*' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'quick*' ORDER BY rowid",
            // initial-token
            "SELECT rowid FROM ft WHERE ft MATCH '^the' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE title MATCH '^quick' ORDER BY rowid",
            // column filter col:term and {a b}:term
            "SELECT rowid FROM ft WHERE ft MATCH 'title:quick' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'body:brown' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH '{title body}:fox' ORDER BY rowid",
            // combined: prefix inside a column filter, and ^ with prefix
            "SELECT rowid FROM ft WHERE ft MATCH 'title:run*' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH '^run*' ORDER BY rowid",
        ],
    );
}

#[test]
fn residual_filter_and_projection() {
    differential(
        DOCS,
        &[
            // MATCH combined with an ordinary WHERE predicate (residual filter)
            "SELECT rowid FROM ft WHERE ft MATCH 'the' AND rowid > 2 ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'the' AND rowid < 5 ORDER BY rowid",
            // count over a MATCH
            "SELECT count(*) FROM ft WHERE ft MATCH 'the'",
            // ORDER BY + LIMIT over a MATCH (topk path)
            "SELECT rowid FROM ft WHERE ft MATCH 'the' ORDER BY rowid DESC LIMIT 2",
        ],
    );
}

#[test]
fn ascii_tokenizer() {
    let setup: &[&str] = &[
        "CREATE VIRTUAL TABLE ft USING fts5(x, tokenize='ascii')",
        "INSERT INTO ft(rowid, x) VALUES (1, 'Hello WORLD')",
        "INSERT INTO ft(rowid, x) VALUES (2, 'Cafe Latte')",
        "INSERT INTO ft(rowid, x) VALUES (3, 'hello there')",
    ];
    differential(
        setup,
        &[
            // ASCII casefold
            "SELECT rowid FROM ft WHERE ft MATCH 'hello' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'world' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'cafe' ORDER BY rowid",
        ],
    );
}

#[test]
fn unicode61_casefold_and_diacritics() {
    // café/crème/naïve fold to cafe/creme/naive under unicode61 — both engines.
    let setup: &[&str] = &[
        "CREATE VIRTUAL TABLE ft USING fts5(x)",
        "INSERT INTO ft(rowid, x) VALUES (1, 'Café CRÈME')",
        "INSERT INTO ft(rowid, x) VALUES (2, 'a naïve approach')",
        "INSERT INTO ft(rowid, x) VALUES (3, 'plain coffee')",
    ];
    differential(
        setup,
        &[
            "SELECT rowid FROM ft WHERE ft MATCH 'cafe' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'creme' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'naive' ORDER BY rowid",
        ],
    );
}

#[test]
fn null_column_contributes_no_postings() {
    let setup: &[&str] = &[
        "CREATE VIRTUAL TABLE ft USING fts5(a, b)",
        "INSERT INTO ft(rowid, a, b) VALUES (1, 'hello world', NULL)",
        "INSERT INTO ft(rowid, a, b) VALUES (2, 'hello there', 'hello friend')",
    ];
    differential(
        setup,
        &[
            "SELECT rowid FROM ft WHERE ft MATCH 'hello' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE b MATCH 'hello' ORDER BY rowid",
            "SELECT count(*) FROM ft WHERE b MATCH 'hello'",
        ],
    );
}

#[test]
fn update_and_delete_keep_index_current() {
    let setup: &[&str] = &[
        "CREATE VIRTUAL TABLE ft USING fts5(a)",
        "INSERT INTO ft(rowid, a) VALUES (1, 'old original text')",
        "INSERT INTO ft(rowid, a) VALUES (2, 'stays put forever')",
        "INSERT INTO ft(rowid, a) VALUES (3, 'gone soon enough')",
        "UPDATE ft SET a = 'new replacement words' WHERE rowid = 1",
        "DELETE FROM ft WHERE rowid = 3",
    ];
    differential(
        setup,
        &[
            // The updated row no longer matches its old tokens, matches the new.
            "SELECT rowid FROM ft WHERE ft MATCH 'old' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'original' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'new' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'replacement' ORDER BY rowid",
            // The deleted row's tokens are gone; the survivor stays.
            "SELECT rowid FROM ft WHERE ft MATCH 'gone' ORDER BY rowid",
            "SELECT rowid FROM ft WHERE ft MATCH 'stays' ORDER BY rowid",
        ],
    );
}

#[test]
fn match_on_non_fts_column_errors_identically() {
    // A plain (non-FTS) table: `col MATCH 'x'` must error exactly as sqlite —
    // "unable to use function MATCH in the requested context".
    let d = db_with(&["INSERT INTO seed(id) VALUES (1)"]);
    let e = d.query("SELECT id FROM seed WHERE id MATCH 5", &[]).unwrap_err();
    assert!(
        format!("{e}").contains("unable to use function MATCH in the requested context"),
        "got: {e}"
    );

    // The exact sqlite error text (both a scalar MATCH and a plain-column
    // MATCH). sqlite raises MATCH at STEP time — the plain-column form needs a
    // row present to trip it (mpedb rejects earlier, at bind, on any table).
    for q in [
        "SELECT 'abcdef' MATCH 'cde'",
        "CREATE TABLE t(x TEXT PRIMARY KEY); INSERT INTO t VALUES('a'); \
         SELECT x FROM t WHERE x MATCH 'w'",
    ] {
        let msg = sqlite_oracle::try_script_stdout(q, "")
            .expect_err("sqlite must reject the misused MATCH");
        assert!(
            msg.contains("unable to use function MATCH in the requested context"),
            "sqlite said: {msg}"
        );
    }
}

#[test]
fn match_misuse_is_a_clean_error() {
    let d = db_with(&[
        "CREATE VIRTUAL TABLE ft USING fts5(a)",
        "INSERT INTO ft(rowid, a) VALUES (1, 'apple')",
    ]);
    // MATCH in a SELECT-list item, a reversed literal MATCH, and MATCH on a
    // non-content column all raise the misuse error (never a wrong answer).
    for q in [
        "SELECT ft MATCH 'apple' FROM ft",
        "SELECT rowid FROM ft WHERE 'apple' MATCH a",
        "SELECT rowid FROM ft WHERE rowid MATCH 'apple'",
    ] {
        let e = d.query(q, &[]).unwrap_err();
        assert!(
            format!("{e}").contains("unable to use function MATCH in the requested context"),
            "`{q}` gave: {e}"
        );
    }
}

#[test]
fn stage2_and_3_features_refuse_by_name() {
    let d = db_with(&[
        "CREATE VIRTUAL TABLE ft USING fts5(a)",
        "INSERT INTO ft(rowid, a) VALUES (1, 'quick brown fox')",
    ]);
    // Phrases, NEAR are stage 2 → clean error, never a wrong answer.
    for q in [
        "SELECT rowid FROM ft WHERE ft MATCH '\"quick brown\"'",
        "SELECT rowid FROM ft WHERE ft MATCH 'NEAR(quick brown)'",
        "SELECT rowid FROM ft WHERE ft MATCH 'quick + brown'",
    ] {
        assert!(d.query(q, &[]).is_err(), "`{q}` should refuse cleanly");
    }
    // porter/trigram tokenizers and unsupported fts5 options refuse at CREATE.
    for q in [
        "CREATE VIRTUAL TABLE p USING fts5(a, tokenize='porter')",
        "CREATE VIRTUAL TABLE g USING fts5(a, content='base')",
        "CREATE VIRTUAL TABLE r USING rtree(id, x, y)",
    ] {
        assert!(d.query(q, &[]).is_err(), "`{q}` should refuse cleanly");
    }
}

#[test]
fn fts_plan_round_trips_through_encode_decode() {
    // `execute_detached` decodes + re-validates + re-hashes the plan blob, so
    // this exercises the whole `AccessPath::FtsScan` wire path (recursive
    // encode/decode of the FtsQuery tree, `check_access` FTS validation, the
    // format-25 hash) — not just the fresh-plan execution the other tests hit.
    let d = db_with(&[
        "CREATE VIRTUAL TABLE ft USING fts5(a, b)",
        "INSERT INTO ft(rowid,a,b) VALUES (1,'quick brown fox','lazy dog')",
        "INSERT INTO ft(rowid,a,b) VALUES (2,'quick start guide','brown sugar')",
    ]);
    let rows_of = |r: ExecResult| -> Vec<Vec<String>> {
        match r {
            ExecResult::Rows { rows, .. } => {
                rows.iter().map(|row| row.iter().map(render).collect()).collect()
            }
            other => panic!("expected rows, got {other:?}"),
        }
    };
    for q in [
        "SELECT rowid FROM ft WHERE ft MATCH 'quick AND brown' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '(brown OR start) NOT dog' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH 'a:^quick*' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{a b}:brown' ORDER BY rowid",
    ] {
        let dp = d.prepare_detached(q).unwrap(); // encode
        let via_blob = rows_of(d.execute_detached(&dp, &[]).unwrap()); // decode+validate+run
        let fresh = rows_of(d.query(q, &[]).unwrap());
        assert_eq!(via_blob, fresh, "round-trip mismatch on `{q}`");
    }
}

#[test]
fn index_and_content_persist_across_reopen() {
    // Prove the inverted index committed durably WITH the content rows: build,
    // close, reopen from the file alone, and the MATCH still finds the rows.
    let path = fresh_path();
    let _ = std::fs::remove_file(&path);
    {
        let db = open(&path);
        db.query("CREATE VIRTUAL TABLE ft USING fts5(a)", &[]).unwrap();
        db.query("INSERT INTO ft(rowid, a) VALUES (1, 'persisted content here')", &[]).unwrap();
        db.query("INSERT INTO ft(rowid, a) VALUES (2, 'more durable words')", &[]).unwrap();
    } // db dropped → closed
    let db = open(&path);
    let rows = mpedb_rows(&db, "SELECT rowid FROM ft WHERE ft MATCH 'durable' ORDER BY rowid");
    assert_eq!(rows, vec![vec!["2".to_string()]]);
    let rows = mpedb_rows(&db, "SELECT rowid FROM ft WHERE ft MATCH 'content' ORDER BY rowid");
    assert_eq!(rows, vec![vec!["1".to_string()]]);
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

#[test]
fn large_match_query_is_capped_at_bind_not_poisoned() {
    // Regression: the decoder caps total FTS query nodes; a flat OR/AND chain
    // must be capped at BIND with the same limit, or a query prepares here yet
    // fails to decode in another process — an undecodable "poison" plan in the
    // shared registry. A 40-way OR (well under the cap) must execute; a query
    // far over the cap must be a clean bind error, never a panic or a poison.
    let d = db_with(DOCS);
    let forty: String = (0..40).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" OR ");
    // 'quick' matches docs 1 and 2 in DOCS; the OR chain executes cleanly.
    let q = format!("SELECT rowid FROM ft WHERE ft MATCH '{forty} OR quick' ORDER BY rowid");
    assert_eq!(mpedb_rows(&d, &q), vec![vec!["1".to_string()], vec!["2".to_string()]]);
    // ~1200 nodes, far over MAX_FTS_DEPTH: a clean refusal, not a poison plan.
    let huge: String = (0..600).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" OR ");
    let e = d.query(&format!("SELECT rowid FROM ft WHERE ft MATCH '{huge}'"), &[]).unwrap_err();
    assert!(format!("{e}").contains("too large"), "expected 'too large', got {e}");
}

#[test]
fn streaming_insert_into_fts_is_refused() {
    // Regression: `insert_row_streaming` guarded only against a secondary UNIQUE
    // index, which FTS tables lack, so a streamed insert committed a content row
    // the inverted index never saw — silently unsearchable. Must refuse.
    let d = db_with(&["CREATE VIRTUAL TABLE ft USING fts5(body)"]);
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("mpedb-fts-stream-{}-{}.txt", std::process::id(), UNIQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&tmp, b"streamedtoken uniquephrase").unwrap();
    // FTS content table is (rowid, body); stream the last column (body, index 1).
    let mut s = d.begin().unwrap();
    let r = s.insert_file("ft", &[Value::Int(1), Value::Text(String::new())], 1, &tmp);
    let _ = std::fs::remove_file(&tmp);
    let e = r.expect_err("streaming insert into an FTS table must be refused");
    assert!(format!("{e}").contains("FTS"), "expected an FTS refusal, got {e}");
}
