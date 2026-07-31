//! SQL ROW-VALUE (tuple) comparisons — `(a, b) = (c, d)` and `<> < <= > >=`.
//!
//! Parser + binder only: a row value desugars to ordinary scalar boolean logic
//! (there is NO plan/format change). The desugar is provably NULL-correct 3VL and
//! is verified here DIFFERENTIALLY against the `sqlite3` CLI 3.45 — mpedb and
//! sqlite must agree bit-for-bit on the truth value of every tuple comparison,
//! including the NULL cases (`(1, NULL) < (1, 2)` → NULL, `(1, 2) < (2, NULL)`
//! → TRUE, `(1, NULL) = (1, 2)` → NULL). The keyset-pagination pattern
//! `WHERE (created, id) > (?, ?)` is exercised over a real table, and the
//! deferred / misuse forms (row-value IN-list, subquery RHS, arity mismatch, a
//! row value in a scalar position) are asserted to be cleanly refused.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

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

/// A fresh mpedb with only a throwaway `seed` table (the config schema); the real
/// tables under test are CREATEd at runtime via live DDL.
fn db() -> Tmp {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-rowvalue-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        other => panic!("unexpected value: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.into_iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

/// Run a whole `;`-terminated script through the sqlite3 CLI (default list mode)
/// and parse the pipe-separated rows. Every row this test emits has a non-empty
/// first column, so filtering blank lines cannot drop a genuine (possibly-NULL)
/// result value.
fn sqlite_rows(script: &str) -> Vec<Vec<String>> {
    sqlite_oracle::script_stdout(script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Evaluate one boolean tuple-comparison expression in BOTH engines and require
/// the truth value (TRUE / FALSE / NULL) to match. A constant tag column keeps
/// the sqlite list-mode line non-empty so a NULL result survives parsing.
fn agree_expr(d: &Database, expr: &str) {
    let q = format!("SELECT 7, {expr}");
    let got = mpedb_rows(d, &q);
    let want = sqlite_rows(&format!("{q};\n"));
    assert_eq!(got, want, "tuple comparison mismatch on `{expr}`");
}

/// The full 3VL truth-table battery for all six operators over 2- and 3-tuples,
/// including every NULL shape. mpedb (desugared) must equal sqlite exactly.
#[test]
fn tuple_comparisons_match_sqlite_3_45() {
    let d = db();
    let exprs = [
        // ---- `=` / `<>` on 2-tuples ----------------------------------------
        "(1, 2) = (1, 2)",
        "(1, 2) = (1, 3)",
        "(1, 2) = (9, 2)",
        "(1, 2) <> (1, 3)",
        "(1, 2) <> (1, 2)",
        // ---- ordering on 2-tuples (lexicographic) --------------------------
        "(1, 2) < (1, 3)",
        "(1, 2) < (2, 1)",
        "(1, 2) < (1, 2)",
        "(2, 2) < (1, 9)",
        "(1, 2) <= (1, 2)",
        "(1, 2) <= (1, 1)",
        "(2, 1) > (1, 9)",
        "(1, 2) > (1, 2)",
        "(1, 2) >= (1, 2)",
        "(1, 1) >= (1, 2)",
        // ---- 3-tuples ------------------------------------------------------
        "(1, 2, 3) = (1, 2, 3)",
        "(1, 2, 3) = (1, 2, 4)",
        "(1, 2, 3) <> (1, 2, 4)",
        "(1, 2, 3) < (1, 2, 4)",
        "(1, 2, 3) < (1, 3, 0)",
        "(1, 2, 3) < (1, 2, 3)",
        "(1, 2, 3) <= (1, 2, 3)",
        "(1, 2, 4) > (1, 2, 3)",
        "(1, 3, 0) > (1, 2, 9)",
        "(1, 2, 3) >= (1, 2, 3)",
        // ---- NULL 3VL — the cases the task calls out explicitly ------------
        "(1, NULL) < (1, 2)",   // NULL
        "(1, 2) < (2, NULL)",   // TRUE (decided at the first element)
        "(1, NULL) = (1, 2)",   // NULL
        "(1, NULL) = (2, 2)",   // FALSE (first element already differs)
        "(1, NULL) <> (2, 2)",  // TRUE
        "(1, NULL) <> (1, 2)",  // NULL
        "(NULL, 1) < (2, 2)",   // NULL
        "(NULL, 1) = (NULL, 1)",// NULL
        "(1, 2, NULL) < (1, 2, 3)", // NULL
        "(1, 2, NULL) < (1, 3, 0)", // TRUE (second element decides)
        "(1, 2, NULL) = (1, 3, 0)", // FALSE
        "(1, NULL) <= (1, 2)",  // NULL
        "(1, NULL) >= (1, 2)",  // NULL
        "(2, NULL) > (1, 9)",   // TRUE
    ];
    for e in exprs {
        agree_expr(&d, e);
    }
}

/// The keyset-pagination pattern `WHERE (created, id) > (?, ?)` over a table with
/// duplicate `created` values (so the `id` tiebreak matters), across all four
/// ordering operators, differentially against sqlite.
const KEYSET_SETUP: &[&str] = &[
    "CREATE TABLE page (id INTEGER PRIMARY KEY, created INTEGER, label TEXT)",
    "INSERT INTO page VALUES (1, 100, 'a')",
    "INSERT INTO page VALUES (2, 100, 'b')",
    "INSERT INTO page VALUES (3, 100, 'c')",
    "INSERT INTO page VALUES (4, 200, 'd')",
    "INSERT INTO page VALUES (5, 200, 'e')",
    "INSERT INTO page VALUES (6, 300, 'f')",
];

#[test]
fn keyset_pagination_matches_sqlite() {
    let d = db();
    for s in KEYSET_SETUP {
        d.query(s, &[]).unwrap();
    }
    let queries = [
        "SELECT id FROM page WHERE (created, id) > (100, 1) ORDER BY created, id",
        "SELECT id FROM page WHERE (created, id) > (100, 2) ORDER BY created, id",
        "SELECT id FROM page WHERE (created, id) > (200, 3) ORDER BY created, id",
        "SELECT id FROM page WHERE (created, id) > (200, 5) ORDER BY created, id",
        "SELECT id FROM page WHERE (created, id) > (300, 6) ORDER BY created, id",
        "SELECT id FROM page WHERE (created, id) > (50, 0) ORDER BY created, id",
        "SELECT id FROM page WHERE (created, id) >= (200, 5) ORDER BY created, id",
        "SELECT id FROM page WHERE (created, id) < (200, 4) ORDER BY created, id",
        "SELECT id FROM page WHERE (created, id) <= (100, 2) ORDER BY created, id",
        "SELECT id, created FROM page WHERE (created, id) = (200, 4) ORDER BY id",
        "SELECT id FROM page WHERE (created, id) <> (100, 1) ORDER BY created, id",
    ];
    // sqlite runs the same DDL + data + query.
    let mut prelude = String::new();
    for s in KEYSET_SETUP {
        prelude.push_str(s);
        prelude.push_str(";\n");
    }
    for q in queries {
        let got = mpedb_rows(&d, q);
        let want = sqlite_rows(&format!("{prelude}{q};\n"));
        assert_eq!(got, want, "keyset mismatch on `{q}`");
    }
}

/// The SAME keyset boundary supplied as bound `?` parameters must match the
/// literal form — this exercises the runtime (non-constant-folded) desugar path,
/// where each element pair reads a parameter rather than a constant.
#[test]
fn keyset_with_bound_parameters() {
    let d = db();
    for s in KEYSET_SETUP {
        d.query(s, &[]).unwrap();
    }
    let ids = |sql: &str, params: &[Value]| -> Vec<i64> {
        match d.query(sql, params).unwrap() {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| match r.into_iter().next().unwrap() {
                    Value::Int(i) => i,
                    other => panic!("{other:?}"),
                })
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    let q = "SELECT id FROM page WHERE (created, id) > (?, ?) ORDER BY created, id";
    assert_eq!(
        ids(q, &[Value::Int(100), Value::Int(2)]),
        vec![3, 4, 5, 6]
    );
    assert_eq!(ids(q, &[Value::Int(200), Value::Int(5)]), vec![6]);
    assert_eq!(
        ids(q, &[Value::Int(50), Value::Int(0)]),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert_eq!(ids(q, &[Value::Int(300), Value::Int(6)]), Vec::<i64>::new());
}

/// A few headline results asserted DIRECTLY (not only via the CLI string diff),
/// so a desugar regression fails as a semantic error and not merely a diff.
#[test]
fn tuple_comparison_semantics_direct() {
    let d = db();
    let val = |expr: &str| -> Value {
        match d.query(&format!("SELECT {expr}"), &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows.into_iter().next().unwrap().into_iter().next().unwrap(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(val("(1, 2) = (1, 2)"), Value::Bool(true));
    assert_eq!(val("(1, 2) = (1, 3)"), Value::Bool(false));
    assert_eq!(val("(1, 2) < (2, 1)"), Value::Bool(true));
    assert_eq!(val("(1, 2, 3) < (1, 2, 4)"), Value::Bool(true));
    // NULL 3VL — the three cases the task verified against sqlite directly.
    assert_eq!(val("(1, NULL) < (1, 2)"), Value::Null);
    assert_eq!(val("(1, 2) < (2, NULL)"), Value::Bool(true));
    assert_eq!(val("(1, NULL) = (1, 2)"), Value::Null);
}

/// Deferred / misuse forms — mpedb must cleanly REFUSE (a bind error, never a
/// wrong answer): a row value in a scalar position, arity mismatch, and a
/// comparison against a scalar. The row-value IN-list moved OUT of this list
/// when it was implemented — see
/// [`a_row_value_in_list_matches_sqlite_including_nulls`].
#[test]
fn misuse_and_deferred_forms_are_refused() {
    let d = db();
    d.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)", &[])
        .unwrap();

    let refuses = |sql: &str| {
        assert!(
            d.query(sql, &[]).is_err(),
            "expected `{sql}` to be refused, but it was accepted"
        );
    };

    // A row value used as a scalar (SELECT list, arithmetic operand, function arg).
    refuses("SELECT (1, 2)");
    refuses("SELECT 1 + (1, 2)");
    refuses("SELECT abs((1, 2))");
    refuses("SELECT (1, 2) FROM t");
    // A row value compared to a scalar (either side).
    refuses("SELECT (1, 2) = 3");
    refuses("SELECT 3 = (1, 2)");
    // Arity mismatch between the two tuples.
    refuses("SELECT (1, 2) = (1, 2, 3)");
    refuses("SELECT (1, 2, 3) < (1, 2)");
    // Row value COMPARED to a subquery (`=`, not `IN`) — still deferred: a
    // 2-column subquery cannot be a scalar operand, so either the subquery lift
    // or the binder refuses it. `IN` is a different question and is answered —
    // see `a_row_value_in_a_subquery_matches_sqlite_including_nulls`.
    refuses("SELECT a FROM t WHERE (a, b) = (SELECT id, a FROM t)");
    // An IN-subquery whose width disagrees with the probe.
    refuses("SELECT a FROM t WHERE (a, b) IN (SELECT id FROM t)");
}

/// A single `(expr)` stays plain grouping — the comma is what makes a row value.
/// This guards the parser change against swallowing ordinary parenthesization.
#[test]
fn single_parenthesized_expr_is_not_a_row_value() {
    let d = db();
    for s in KEYSET_SETUP {
        d.query(s, &[]).unwrap();
    }
    // `(created)` is grouping, not a 1-tuple; the query is an ordinary scalar
    // comparison and must return the expected ids.
    let got = mpedb_rows(&d, "SELECT id FROM page WHERE (created) > 150 ORDER BY id");
    let want = sqlite_rows(&{
        let mut s = String::new();
        for stmt in KEYSET_SETUP {
            s.push_str(stmt);
            s.push_str(";\n");
        }
        s.push_str("SELECT id FROM page WHERE (created) > 150 ORDER BY id;\n");
        s
    });
    assert_eq!(got, want);
    // And a grouped arithmetic expression still parses and evaluates.
    assert_eq!(
        mpedb_rows(&d, "SELECT (1 + 2) * 3"),
        vec![vec!["9".to_string()]]
    );
}

/// `(a, b) [NOT] IN ((x, y), …)` and its `VALUES` spelling, against sqlite.
///
/// The NULL cases are the point. `NOT IN` is the NEGATION of the disjunction,
/// not an AND of `<>`s — desugared the other way, a NULL in a non-matching row
/// answers TRUE where sqlite answers NULL, and an ORM's `not_in()` silently
/// gains rows.
#[test]
fn a_row_value_in_list_matches_sqlite_including_nulls() {
    const SETUP: &[&str] = &[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER)",
        "INSERT INTO t (id, x, y) VALUES (1, 5, 10)",
        "INSERT INTO t (id, x, y) VALUES (2, NULL, 10)",
        "INSERT INTO t (id, x, y) VALUES (3, 5, NULL)",
        "INSERT INTO t (id, x, y) VALUES (4, 9, 9)",
    ];
    let d = db();
    for s in SETUP {
        d.query(s, &[]).unwrap();
    }
    let mut script = String::new();
    for stmt in SETUP {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    for q in [
        "SELECT id FROM t WHERE (x, y) IN ((5, 10), (9, 9)) ORDER BY id",
        "SELECT id FROM t WHERE (x, y) IN (VALUES (5, 10), (9, 9)) ORDER BY id",
        "SELECT id FROM t WHERE (x, y) NOT IN ((5, 10), (9, 9)) ORDER BY id",
        "SELECT id FROM t WHERE (x, y) IN ((5, NULL)) ORDER BY id",
        "SELECT id FROM t WHERE (x, y) NOT IN ((5, NULL)) ORDER BY id",
        "SELECT id FROM t WHERE (x, y) IN () ORDER BY id",
        "SELECT id FROM t WHERE (x, y) NOT IN () ORDER BY id",
    ] {
        let got = mpedb_rows(&d, q);
        let want = sqlite_rows(&format!("{script}{q};\n"));
        assert_eq!(got, want, "`{q}`");
    }
}

/// `(a, b) [NOT] IN (SELECT x, y …)`, against sqlite.
///
/// Rewritten to the two correlated `EXISTS` that ARE its 3-valued definition —
/// TRUE if any row matches, else NULL if any row's comparison was NULL, else
/// FALSE. A single `EXISTS` would be a WRONG ANSWER: it collapses the NULL case
/// to FALSE, so a probe carrying a NULL would report "not a member" where
/// sqlite reports unknown. Rows 2 and 3 below are exactly that case.
#[test]
fn a_row_value_in_a_subquery_matches_sqlite_including_nulls() {
    const SETUP: &[&str] = &[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER)",
        "INSERT INTO t (id, x, y) VALUES (1, 5, 10)",
        "INSERT INTO t (id, x, y) VALUES (2, NULL, 10)",
        "INSERT INTO t (id, x, y) VALUES (3, 5, NULL)",
        "INSERT INTO t (id, x, y) VALUES (4, 9, 9)",
        "CREATE TABLE s (k INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
        "INSERT INTO s (k, a, b) VALUES (1, 5, 10)",
        "INSERT INTO s (k, a, b) VALUES (2, 9, 9)",
        "CREATE TABLE sn (k INTEGER PRIMARY KEY, a INTEGER, b INTEGER)",
        "INSERT INTO sn (k, a, b) VALUES (1, 5, 10)",
        "INSERT INTO sn (k, a, b) VALUES (2, NULL, 7)",
    ];
    let d = db();
    for s in SETUP {
        d.query(s, &[]).unwrap();
    }
    let mut script = String::new();
    for stmt in SETUP {
        script.push_str(stmt);
        script.push_str(";\n");
    }
    for q in [
        "SELECT id FROM t WHERE (x, y) IN (SELECT a, b FROM s) ORDER BY id",
        "SELECT id FROM t WHERE (x, y) NOT IN (SELECT a, b FROM s) ORDER BY id",
        // The subquery itself carries a NULL — the case a plain EXISTS gets wrong.
        "SELECT id FROM t WHERE (x, y) IN (SELECT a, b FROM sn) ORDER BY id",
        "SELECT id FROM t WHERE (x, y) NOT IN (SELECT a, b FROM sn) ORDER BY id",
        // Empty subquery: FALSE for every probe, NULLs included.
        "SELECT id FROM t WHERE (x, y) IN (SELECT a, b FROM s WHERE k < 0) ORDER BY id",
        "SELECT id FROM t WHERE (x, y) NOT IN (SELECT a, b FROM s WHERE k < 0) ORDER BY id",
        // The subquery's own WHERE survives the rewrite.
        "SELECT id FROM t WHERE (x, y) IN (SELECT a, b FROM s WHERE k = 2) ORDER BY id",
    ] {
        let got = mpedb_rows(&d, q);
        let want = sqlite_rows(&format!("{script}{q};\n"));
        assert_eq!(got, want, "`{q}`");
    }
}

// ---------------------------------------------- name capture in the desugar

/// The desugar SPLICES the outer probe into the subquery's scope, so an
/// unqualified column in it must be qualified against the OUTER scope first.
///
/// It was not, and the result was a shipped WRONG ANSWER — not a refusal.
/// `(a,b) IN (SELECT a,b FROM q)` became `q.a = q.a AND q.b = q.b`, trivially
/// true, so every outer row matched; `(a,b) IN (SELECT b,a FROM q)` became
/// `q.a = q.b AND q.b = q.a`, trivially false, so none did. Only the fully
/// qualified spelling escaped, which is why nothing noticed: every test written
/// for this feature happened to use distinct names.
#[test]
fn a_row_value_probe_is_not_captured_by_the_subquerys_own_columns() {
    let setup = [
        "CREATE TABLE r (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
        "CREATE TABLE q (a TEXT, b TEXT)",
        "INSERT INTO r (id,a,b) VALUES (1,'x','y'),(2,'y','z'),(3,'z','y'),(4,NULL,'y')",
        "INSERT INTO q (a,b) VALUES ('y','z'),('z','y')",
    ];
    let d = db();
    let mut script = String::new();
    for s in setup {
        d.query(s, &[]).unwrap();
        script.push_str(s);
        script.push_str(";\n");
    }
    for q in [
        // The colliding-name forms: `a` and `b` exist on BOTH sides.
        "SELECT id FROM r WHERE (a,b) IN (SELECT a,b FROM q) ORDER BY id",
        "SELECT id FROM r WHERE (a,b) IN (SELECT b,a FROM q) ORDER BY id",
        "SELECT id FROM r WHERE (a,b) NOT IN (SELECT a,b FROM q) ORDER BY id",
        // Reversed probe, same collision.
        "SELECT id FROM r WHERE (b,a) IN (SELECT a,b FROM q) ORDER BY id",
        // A literal beside a captured name, and an expression around one.
        "SELECT id FROM r WHERE (a,'y') IN (SELECT a,b FROM q) ORDER BY id",
        "SELECT id FROM r WHERE (upper(a),b) IN (SELECT upper(a),b FROM q) ORDER BY id",
        // Already qualified — the one spelling that was right before.
        "SELECT id FROM r WHERE (r.a,r.b) IN (SELECT q.a,q.b FROM q) ORDER BY id",
        // Aliased outer, colliding inner.
        "SELECT id FROM r r2 WHERE (r2.a,r2.b) IN (SELECT a,b FROM q) ORDER BY id",
        // The subquery's own WHERE still survives alongside the qualification.
        "SELECT id FROM r WHERE (a,b) IN (SELECT a,b FROM q WHERE a > 'a') ORDER BY id",
    ] {
        let got = mpedb_rows(&d, q);
        let want = sqlite_rows(&format!("{script}{q};\n"));
        assert_eq!(got, want, "`{q}`");
    }

    // The one shape qualification cannot save: the subquery reads a table
    // ADDRESSED BY THE SAME NAME as the outer one, so `r.a` is ambiguous on
    // both sides. A named refusal, because the alternative is a guess.
    let e = d
        .query("SELECT id FROM r WHERE (a,b) IN (SELECT a,b FROM r)", &[])
        .unwrap_err();
    assert!(
        format!("{e}").contains("SAME name"),
        "expected the same-name refusal, got {e}"
    );
}
