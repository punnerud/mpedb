//! Task #74 item 6 — a **binder PANIC** on a scalar function over a joined
//! column, and the class of bug behind it.
//!
//! ```sql
//! SELECT a.id FROM a INNER JOIN b ON (a.id = b.a_id) WHERE ABS(b.id) = 1
//! ```
//!
//! used to abort in `Scope::only()` ("this path has not been taught about
//! joins"). A panic in the binder is a crash for every embedder, so it is
//! covered here rather than left to the SQL crate's unit tests.
//!
//! **The real cause was not `only()`.** It was `Binder::static_type`, the
//! helper that answers "what type is this already-bound expression" for the
//! handful of functions whose RETURN TYPE IS THEIR ARGUMENT'S — `abs`, `round`,
//! `ceil`, `floor`, `trunc` and `hex`. For a `BExpr::Col(i)` it looked the slot
//! up in `scope.only().columns`, i.e. it resolved a TUPLE slot against ONE
//! table. That is right for a single-table scope and simply wrong for a join,
//! where slot `i` may belong to any of the scoped tables. The scope was never
//! single-table on this path; only the lookup assumed it was, and `only()` was
//! the assertion that caught the lie.
//!
//! So the fix is a real resolution, not an error at the call site:
//! `Scope::column_shape(slot)` walks the scoped tables in slot order — the same
//! walk `Scope::resolve` used to hand the slot out in the first place. Making
//! `only()` return an error instead would have turned the crash into a refusal
//! of a query sqlite answers, and left the wrong-table read latent for any
//! scope where the slot happened to be in range of the first table.
//!
//! Every case below is checked against the real `sqlite3` binary: the panic
//! being gone is only half of it, the answer has to be right.

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

/// `b.id` is deliberately NEGATIVE in one row, so `ABS(b.id)` is not the
/// identity and a wrong-table read would show up as a wrong answer rather than
/// an accidentally-equal one. `b` is also WIDER than `a`, so a slot from `b`
/// indexes past `a`'s columns — the exact condition the old lookup got wrong.
const DDL: &[&str] = &[
    "CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER, t TEXT)",
    "CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER, w REAL, u TEXT, z BLOB)",
];
const ROWS: &[&str] = &[
    "INSERT INTO a VALUES (1, 10, 'x')",
    "INSERT INTO a VALUES (2, -20, 'y')",
    "INSERT INTO a VALUES (3, NULL, NULL)",
    "INSERT INTO b VALUES (1, 1, -1.5, 'p', x'01')",
    "INSERT INTO b VALUES (-2, 2, 2.5, 'q', x'02')",
];

fn open() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-joinscope-{}-{}.mpedb",
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
    for s in DDL.iter().chain(ROWS.iter()) {
        t.db.query(s, &[]).unwrap_or_else(|e| panic!("setup `{s}`: {e}"));
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

fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
    for s in DDL.iter().chain(ROWS.iter()) {
        script.push_str(s);
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

fn same(db: &Database, query: &str) {
    let got = match db.query(query, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| r.iter().map(render).collect::<Vec<String>>())
            .collect::<Vec<_>>(),
        Ok(other) => panic!("expected rows from `{query}`, got {other:?}"),
        Err(e) => panic!("mpedb `{query}` failed: {e}"),
    };
    assert_eq!(got, sqlite_rows(query), "mpedb vs sqlite3 diverged for:\n  {query}");
}

/// The exact Django statement that crashed, plus the shape of it that would
/// expose a WRONG-TABLE read rather than a panic.
#[test]
fn the_reported_statement_answers_correctly() {
    let d = open();
    same(&d, "SELECT a.id FROM a INNER JOIN b ON (a.id = b.a_id) WHERE ABS(b.id) = 1");
    // `b.id` is -2 in the other row, so ABS is doing real work — a lookup that
    // read `a`'s column at the same slot would answer differently.
    same(&d, "SELECT a.id FROM a INNER JOIN b ON (a.id = b.a_id) WHERE ABS(b.id) = 2");
    same(&d, "SELECT a.id, ABS(b.id) FROM a JOIN b ON a.id = b.a_id ORDER BY a.id");
}

/// Every function whose return type IS its argument's — the whole set that
/// routes through `static_type` — over a column from EACH side of the join.
#[test]
fn argument_typed_functions_work_over_either_side_of_a_join() {
    let d = open();
    // NOTE: `ROUND` is compared only over the FLOAT column here. mpedb's
    // `round(<int>)` returns an integer where sqlite's returns a real
    // (`round(10)` is `10.0` there) — a PRE-EXISTING, documented deviation
    // (COMPAT.md: "keep their argument's numeric type"), unrelated to this fix
    // and not silently papered over by comparing an int argument.
    for f in ["ABS", "ROUND", "CEIL", "FLOOR", "TRUNC"] {
        // Right table's float64 column: its slot lies PAST `a`'s width, which
        // is the exact condition the old single-table lookup got wrong.
        same(
            &d,
            &format!("SELECT a.id, {f}(b.w) FROM a JOIN b ON a.id = b.a_id ORDER BY a.id"),
        );
        if f != "ROUND" {
            // Integer columns from BOTH sides.
            same(
                &d,
                &format!(
                    "SELECT a.id, {f}(a.v), {f}(b.id) \
                     FROM a JOIN b ON a.id = b.a_id ORDER BY a.id"
                ),
            );
        }
        // In a predicate as well as a projection.
        same(
            &d,
            &format!("SELECT a.id FROM a JOIN b ON a.id = b.a_id WHERE {f}(b.w) > 2 ORDER BY a.id"),
        );
        // Nested, and mixed across the two sides.
        same(
            &d,
            &format!(
                "SELECT {f}({f}(b.w)) + {f}(b.w) FROM a JOIN b ON a.id = b.a_id ORDER BY a.id"
            ),
        );
    }
    // `hex` reads the same helper. mpedb refuses a numeric argument by name
    // (pre-existing, and identical for one table and for a join) — what matters
    // here is that it is a REFUSAL and not a panic, and that the text/blob
    // arguments it does accept resolve to the right side.
    // (Restricted to non-NULL rows: mpedb's `hex(NULL)` is NULL where sqlite's
    // is the empty TEXT — another pre-existing deviation, not this fix's.)
    same(
        &d,
        "SELECT HEX(a.t), HEX(b.u), HEX(b.z) FROM a JOIN b ON a.id = b.a_id          WHERE a.t IS NOT NULL ORDER BY a.id",
    );
    for sql in [
        "SELECT HEX(b.id) FROM a JOIN b ON a.id = b.a_id",
        "SELECT HEX(b.w) FROM a JOIN b ON a.id = b.a_id",
    ] {
        let e = d.query(sql, &[]).expect_err(sql).to_string();
        assert!(e.contains("hex() expects text or blob"), "`{sql}`: {e}");
    }
    // The type error must name the RIGHT column's type: `b.w` is float64 and
    // `a.v` is int64, and a lookup against the wrong table would say the wrong
    // one. (`a` has no text column at `b.w`'s slot, so this is a real probe.)
    let e = d
        .query("SELECT HEX(b.w) FROM a JOIN b ON a.id = b.a_id", &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("got float64"), "{e}");
}

/// Wider and stranger scopes: three tables, a self-join with aliases, an outer
/// join whose NULL-extension makes the argument NULL, and the joined-scope
/// paths that reach `static_type` through other binder entry points.
#[test]
fn wider_scopes_and_outer_joins() {
    let d = open();
    same(
        &d,
        "SELECT a.id FROM a JOIN b ON a.id = b.a_id JOIN a c ON c.id = a.id \
         WHERE ABS(b.id) = 1 ORDER BY a.id",
    );
    // A self-join with two aliases: both sides are the SAME table, so a lookup
    // that ignored the slot would still find a column — and the wrong one.
    same(
        &d,
        "SELECT x.id, ABS(x.v), ABS(y.v) FROM a x JOIN a y ON y.id = x.id + 1 ORDER BY x.id",
    );
    // LEFT JOIN: the unmatched rows NULL-extend, so the function argument is
    // NULL and must propagate rather than resolve to some other slot.
    same(
        &d,
        "SELECT a.id, ABS(b.id), ROUND(b.w) FROM a LEFT JOIN b ON a.id = b.a_id ORDER BY a.id",
    );
    same(&d, "SELECT a.id FROM a LEFT JOIN b ON a.id = b.a_id WHERE ABS(b.id) IS NULL");
    // Through the other clauses that bind over the joined scope.
    same(
        &d,
        "SELECT a.id FROM a JOIN b ON ABS(b.id) = a.id ORDER BY a.id",
    );
    same(
        &d,
        "SELECT a.id FROM a JOIN b ON a.id = b.a_id ORDER BY ABS(b.id) DESC",
    );
    same(&d, "SELECT SUM(ABS(b.id)), MAX(ABS(b.w)) FROM a JOIN b ON a.id = b.a_id");
    // The scalar max/min of item 5 over a joined scope reaches the same helper.
    same(&d, "SELECT a.id, max(a.v, b.id), min(a.v, b.id) FROM a JOIN b ON a.id = b.a_id ORDER BY a.id");
    same(
        &d,
        "SELECT ABS(b.id), COUNT(*) FROM a JOIN b ON a.id = b.a_id GROUP BY ABS(b.id) \
         ORDER BY 1",
    );
}

/// The single-table paths `Scope::only()` legitimately still serves must be
/// unchanged — in particular `excluded.<c>`, whose `Col(n + i)` slot is the one
/// case where the index deliberately runs PAST the scope's width.
#[test]
fn single_table_and_excluded_paths_are_unchanged() {
    // The plain single-table forms first, against sqlite — before the upsert
    // below changes a row the sqlite baseline does not replay.
    let d = open();
    same(&d, "SELECT ABS(v) FROM a ORDER BY id");
    same(&d, "SELECT HEX(t) FROM a WHERE t IS NOT NULL ORDER BY id");
    let e = d.query("SELECT ABS(t) FROM a", &[]).expect_err("text").to_string();
    assert!(e.contains("expects a number, got text"), "{e}");

    // `excluded.v` binds to Col(n + i) over [existing ‖ proposed]; `ABS` of it
    // must still report int64 and evaluate over the proposed row.
    d.query(
        "INSERT INTO a VALUES (1, -99, 'z') ON CONFLICT(id) DO UPDATE SET v = ABS(excluded.v)",
        &[],
    )
    .unwrap();
    // Asserted directly rather than through `same`: the sqlite baseline replays
    // only the setup, not this upsert.
    let got = match d.query("SELECT id, v FROM a WHERE id = 1", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows[0].iter().map(render).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(got, vec!["1".to_string(), "99".to_string()], "ABS(excluded.v) of -99");
}
