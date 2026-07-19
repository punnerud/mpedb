//! Task #74 item 5 — sqlite's **scalar** `max(a, b, …)` / `min(a, b, …)`.
//!
//! These are a different C function from the one-argument aggregates of the
//! same name (`minmaxFunc` vs `minmaxStep`) and sqlite resolves them on ARITY,
//! which is what mpedb's parser now does. Three things that must NOT change,
//! each asserted below: `max(x)` stays the aggregate, `max(DISTINCT x)` stays
//! the aggregate, and `max(x) OVER (…)` stays the window aggregate.
//!
//! The scalar form is a SELECTION, not a computation — the winning ARGUMENT is
//! returned unchanged — so `max(3, 2.5)` is the INTEGER 3 while `max(1, 2.5)`
//! is the REAL 2.5. That is why the result type of a mixed call is `any` rather
//! than a widened number: widening would turn the first into 3.0.
//!
//! The tie rule is sqlite's `minmaxFunc` loop verbatim: `max` keeps the EARLIER
//! of two equal arguments, `min` takes the LATER one — observable as
//! `typeof(max(1, 1.0)) = 'integer'` and `typeof(min(1, 1.0)) = 'real'`.

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
    }
}

/// `a` is mpedb's typeless column, so sqlite's must be a STRICT `ANY` — the
/// only sqlite column that likewise keeps a value in its own storage class.
const DDL: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, i INTEGER, r REAL, s TEXT, a ANY)";
const SQLITE_DDL: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, i INT, r REAL, s TEXT, a ANY) STRICT";
const ROWS: &[&str] = &[
    "INSERT INTO t VALUES (1, 5, 2.5, 'abc', 7)",
    "INSERT INTO t VALUES (2, -8, -3.5, 'B', 'zz')",
    "INSERT INTO t VALUES (3, NULL, NULL, NULL, NULL)",
];

fn open() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-minmax-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let t = Tmp { db, path };
    t.db.query(DDL, &[]).unwrap();
    for r in ROWS {
        t.db.query(r, &[]).unwrap();
    }
    t
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        Value::Text(s) => s.clone(),
        Value::Bool(b) => (*b as i32).to_string(),
        other => panic!("unexpected value: {other:?}"),
    }
}

fn mpedb_rows(db: &Database, sql: &str) -> Vec<Vec<String>> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => {
            rows.iter().map(|r| r.iter().map(render).collect()).collect()
        }
        Ok(other) => panic!("expected rows from `{sql}`, got {other:?}"),
        Err(e) => panic!("mpedb `{sql}` failed: {e}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = format!("{SQLITE_DDL};\n");
    for r in ROWS {
        script.push_str(r);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    sqlite_oracle::script_stdout(&script, "NULL")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

fn same(query: &str) {
    let t = open();
    assert_eq!(
        mpedb_rows(&t.db, query),
        sqlite_rows(query),
        "mpedb vs sqlite3 diverged for:\n  {query}"
    );
}

// ---- the scalar forms -----------------------------------------------------

#[test]
fn scalar_min_max_agree_with_sqlite() {
    same("SELECT max(1, 2), min(1, 2), max(2, 1), min(2, 1)");
    // Variadic beyond two.
    same("SELECT max(1, 2, 3, 4, 5), min(5, 4, 3, 2, 1), max(3, 1, 2), min(2, 3, 1)");
    // Over columns, per row, including the all-NULL row.
    same("SELECT id, max(i, 0), min(i, 0) FROM t ORDER BY id");
    same("SELECT id, max(i, id, 3), min(i, id, 3) FROM t ORDER BY id");
    // Text arguments, ordered by the BINARY collation ('B' = 0x42 < 'a' = 0x61).
    same("SELECT max('a', 'B'), min('a', 'B'), max('abc', 'abd'), min('', 'a')");
    same("SELECT id, max(s, 'M'), min(s, 'M') FROM t ORDER BY id");
}

#[test]
fn any_null_argument_yields_null() {
    // sqlite's `minmaxFunc` returns on the FIRST NULL it sees, at any position.
    same("SELECT max(1, NULL), min(NULL, 1), max(NULL, NULL), min(NULL, NULL)");
    same("SELECT max(1, NULL, 3), min(1, NULL, 3), max(NULL, 2, 3)");
    // A NULL COLUMN is the same thing, and it must not be confused with 0.
    same("SELECT id, max(i, -100), min(i, 100) FROM t ORDER BY id");
    same("SELECT max(0, NULL), min(0, NULL)");
}

#[test]
fn the_result_is_the_winning_argument_unchanged() {
    // A SELECTION: `max(3, 2.5)` is the INTEGER 3, not 3.0. Widening the mixed
    // call to float64 would change that value, which is why the bind-time type
    // of a mixed call is `any`.
    same("SELECT max(1, 2.5), typeof(max(1, 2.5)), max(3, 2.5), typeof(max(3, 2.5))");
    same("SELECT min(3, 2.5), typeof(min(3, 2.5)), min(1, 2.5), typeof(min(1, 2.5))");
    // The TIE rule, which is the observable half of sqlite's loop: `max` keeps
    // the EARLIER equal argument, `min` takes the LATER one.
    same("SELECT typeof(max(1, 1.0)), typeof(min(1, 1.0))");
    same("SELECT typeof(max(1.0, 1)), typeof(min(1.0, 1))");
    same("SELECT typeof(max(2, 2.0, 2)), typeof(min(2, 2.0, 2))");
    // Mixed numeric over columns.
    same("SELECT id, max(i, r), min(i, r) FROM t ORDER BY id");
    // A typeless column: the runtime orders by sqlite's storage class.
    same("SELECT id, max(a, 5), min(a, 5) FROM t ORDER BY id");
    same("SELECT id, max(a, 'm'), min(a, 'm') FROM t ORDER BY id");
}

// ---- the aggregate must not have moved ------------------------------------

#[test]
fn the_one_argument_aggregate_is_untouched() {
    same("SELECT max(i), min(i) FROM t");
    same("SELECT max(DISTINCT i), min(DISTINCT i) FROM t");
    same("SELECT max(s), min(s) FROM t");
    same("SELECT id, max(i) FROM t GROUP BY id ORDER BY id");
    same("SELECT max(i) FILTER (WHERE id = 1) FROM t");
    same("SELECT id, max(i) OVER (ORDER BY id) FROM t ORDER BY id");
    // Both in one statement — the arity really is what routes them.
    same("SELECT max(i), max(i, 100) FROM t");
}

// ---- parameters and the refusals ------------------------------------------

#[test]
fn parameters_adopt_the_single_concrete_type() {
    let t = open();
    // `max(?, i)` pins `$1` to int64, the way `? > i` does.
    let got = match t.db.query("SELECT max(?, 1), min(?, 1)", &[Value::Int(5), Value::Int(5)]) {
        Ok(ExecResult::Rows { rows, .. }) => {
            rows[0].iter().map(render).collect::<Vec<_>>()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(got, vec!["5".to_string(), "1".to_string()]);
    // A NULL parameter still propagates.
    let got = match t.db.query("SELECT max(?, 1)", &[Value::Null]) {
        Ok(ExecResult::Rows { rows, .. }) => rows[0].iter().map(render).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(got, vec!["NULL".to_string()]);
    // A wrongly-typed parameter is a clean type error, not a class ordering.
    let e = t
        .db
        .query("SELECT max(?, 1)", &[Value::Text("z".into())])
        .expect_err("text must not bind to an int64-pinned slot")
        .to_string();
    assert!(e.contains("statement requires int64"), "{e}");
}

#[test]
fn cross_type_and_aggregate_only_syntax_refuse_by_name() {
    let t = open();

    // sqlite ranks a number against a text by STORAGE CLASS. That is the same
    // cross-class comparison `sql_cmp` refuses everywhere else in mpedb, so it
    // is a named refusal rather than a guess.
    for sql in [
        "SELECT max(i, 'a') FROM t",
        "SELECT min(1, 'a')",
        "SELECT max(s, 1) FROM t",
        "SELECT max(i, x'41') FROM t",
    ] {
        let e = t.db.query(sql, &[]).expect_err(sql).to_string();
        assert!(
            e.contains("cannot order arguments of different types") && e.contains("CAST"),
            "`{sql}`: {e}"
        );
    }
    // …and the CAST the message names gives sqlite's answer.
    same("SELECT max(CAST(1 AS TEXT), 'a'), min(CAST(1 AS TEXT), 'a')");

    // DISTINCT is aggregate-only GRAMMAR, so a multi-argument call carrying it
    // is refused rather than silently answered (sqlite happens to answer, but
    // `max(DISTINCT a, b)` is not a shape any client emits on purpose).
    let e = t.db.query("SELECT max(DISTINCT i, 2) FROM t", &[]).expect_err("distinct").to_string();
    assert!(e.contains("takes exactly one argument"), "{e}");

    // FILTER and OVER belong to the aggregate. sqlite refuses the OVER form too
    // ("max() may not be used as a window function").
    for sql in ["SELECT max(i, 2) OVER () FROM t", "SELECT max(i, 2) FILTER (WHERE id = 1) FROM t"] {
        let e = t.db.query(sql, &[]).expect_err(sql).to_string();
        assert!(e.contains("is the SCALAR form"), "`{sql}`: {e}");
    }

    // Zero arguments is a parse error in both engines.
    assert!(t.db.query("SELECT max() FROM t", &[]).is_err());
}
