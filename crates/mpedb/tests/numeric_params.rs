//! Task #74 item 1 — **numeric parameters vs sqlite affinity**.
//!
//! sqlite has no parameter types: `sqlite3_bind_int64(1)` against
//! `WHERE real_col > ?` is compared numerically, and `sqlite3_bind_double(1.0)`
//! into an INTEGER column is stored as the integer 1 by INTEGER affinity.
//! Comparison uses ClassCmp + Numeric affinity so a float bind against an
//! INTEGER column (`year >= 1942.1`, Django annotate) is exact numeric
//! comparison, never truncated. Assignment still refuses an inexact float
//! into an int column BY NAME (store-time INTEGER affinity).
//!
//! Every "mpedb now accepts it" case is checked against the real `sqlite3`
//! binary; every "mpedb still refuses" case asserts the reason is named.

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

const DDL: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, i INTEGER, r REAL)";
const ROWS: &[&str] = &[
    "INSERT INTO t VALUES (1, 5, 2.5)",
    "INSERT INTO t VALUES (2, -8, -3.5)",
    "INSERT INTO t VALUES (3, NULL, NULL)",
];

fn open() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-numparam-{}-{}.mpedb",
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

/// One value as the sqlite3 CLI prints it.
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
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

fn rows_of(r: ExecResult) -> Vec<Vec<String>> {
    match r {
        ExecResult::Rows { rows, .. } => {
            rows.iter().map(|row| row.iter().map(render).collect()).collect()
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Run a script through the `sqlite3` CLI over the same schema and rows.
fn sqlite_rows(extra: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut script = format!("{DDL};\n");
    for r in ROWS.iter().chain(extra.iter()) {
        script.push_str(r);
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

/// `sql` bound with `params` must return exactly what sqlite returns for
/// `sqlite_sql` (the same statement with the parameter written as a literal —
/// sqlite's own way of spelling "this value, whatever its type").
fn same_as_sqlite(sql: &str, params: &[Value], sqlite_sql: &str) {
    let t = open();
    let got = rows_of(
        t.db.query(sql, params).unwrap_or_else(|e| panic!("mpedb `{sql}` {params:?}: {e}")),
    );
    assert_eq!(got, sqlite_rows(&[], sqlite_sql), "diverged for `{sql}` with {params:?}");
}

fn refuses(sql: &str, params: &[Value], needle: &str) {
    let t = open();
    let e = t.db.query(sql, params).expect_err("must refuse").to_string();
    assert!(e.contains(needle), "expected `{needle}` in: {e}");
}

// ---- the Django shape: an int bound where the plan inferred float64 --------

#[test]
fn int_parameter_satisfies_a_float64_slot() {
    same_as_sqlite(
        "SELECT id FROM t WHERE r > ? ORDER BY id",
        &[Value::Int(1)],
        "SELECT id FROM t WHERE r > 1 ORDER BY id",
    );
    same_as_sqlite(
        "SELECT id FROM t WHERE r < ? ORDER BY id",
        &[Value::Int(0)],
        "SELECT id FROM t WHERE r < 0 ORDER BY id",
    );
    // Arithmetic, not just comparison: `? + r` pins `$1` to float64 too.
    same_as_sqlite(
        "SELECT ? + r FROM t WHERE id = 1",
        &[Value::Int(1)],
        "SELECT 1 + r FROM t WHERE id = 1",
    );
    // And the write side.
    let t = open();
    t.db.query("UPDATE t SET r = ? WHERE id = 1", &[Value::Int(3)]).unwrap();
    assert_eq!(
        rows_of(t.db.query("SELECT r FROM t WHERE id = 1", &[]).unwrap()),
        sqlite_rows(&["UPDATE t SET r = 3 WHERE id = 1"], "SELECT r FROM t WHERE id = 1"),
    );
}

#[test]
fn float_parameter_satisfies_an_int64_slot_when_integral() {
    same_as_sqlite(
        "SELECT id FROM t WHERE i > ? ORDER BY id",
        &[Value::Float(1.0)],
        "SELECT id FROM t WHERE i > 1.0 ORDER BY id",
    );
    same_as_sqlite(
        "SELECT id FROM t WHERE i = ? ORDER BY id",
        &[Value::Float(-8.0)],
        "SELECT id FROM t WHERE i = -8.0 ORDER BY id",
    );
    let t = open();
    t.db.query("INSERT INTO t (id, i) VALUES (9, ?)", &[Value::Float(7.0)]).unwrap();
    assert_eq!(
        rows_of(t.db.query("SELECT i, typeof(i) FROM t WHERE id = 9", &[]).unwrap()),
        sqlite_rows(
            &["INSERT INTO t (id, i) VALUES (9, 7.0)"],
            "SELECT i, typeof(i) FROM t WHERE id = 9"
        ),
    );
}

// ---- NULL on either side is untouched by the bridge -----------------------

#[test]
fn null_parameters_still_bind_to_either_slot() {
    // A NULL parameter has no type, so `fits` already passed it through; the
    // bridge must not have changed that, and the 3VL answer must still match.
    same_as_sqlite(
        "SELECT id FROM t WHERE r > ? ORDER BY id",
        &[Value::Null],
        "SELECT id FROM t WHERE r > NULL ORDER BY id",
    );
    same_as_sqlite(
        "SELECT id FROM t WHERE i > ? ORDER BY id",
        &[Value::Null],
        "SELECT id FROM t WHERE i > NULL ORDER BY id",
    );
    // The NULL rows themselves: a bound value never matches them.
    same_as_sqlite(
        "SELECT id FROM t WHERE i IS NULL ORDER BY id",
        &[],
        "SELECT id FROM t WHERE i IS NULL ORDER BY id",
    );
}

// ---- the refusals, each named --------------------------------------------

#[test]
fn inexact_conversions_are_refused_by_name() {
    // Comparison: float bind vs INTEGER column is numeric (sqlite), not refused.
    // Truncating the param would be a wrong answer — ClassCmp+Numeric keeps it.
    same_as_sqlite(
        "SELECT id FROM t WHERE i > ? ORDER BY id",
        &[Value::Float(1.5)],
        "SELECT id FROM t WHERE i > 1.5 ORDER BY id",
    );
    // Django annotate shape: year >= 1942.1.
    same_as_sqlite(
        "SELECT id FROM t WHERE i >= ? ORDER BY id",
        &[Value::Float(5.1)],
        "SELECT id FROM t WHERE i >= 5.1 ORDER BY id",
    );
    // The INSERT half: store-time INTEGER affinity refuses a non-integral float.
    refuses(
        "INSERT INTO t (id, i) VALUES (9, ?)",
        &[Value::Float(7.5)],
        "INTEGER affinity left it a float64",
    );

    // Extreme floats still compare (sqlite); NaN/inf stay reals under Numeric.
    same_as_sqlite(
        "SELECT id FROM t WHERE i > ? ORDER BY id",
        &[Value::Float(1e300)],
        "SELECT id FROM t WHERE i > 1e300 ORDER BY id",
    );

    // Int bind vs REAL column: ClassCmp+Numeric compares without coercing the
    // parameter to f64 first (sqlite's exact int/real compare).
    let t = open();
    for n in [0i64, 1, -1, 1 << 52, 1 << 53, -(1 << 53), 1 << 62, i64::MIN, i64::MAX, i64::MIN + 1] {
        t.db.query("SELECT id FROM t WHERE r > ?", &[Value::Int(n)])
            .unwrap_or_else(|e| panic!("{n} should compare: {e}"));
    }
    // 2^53 + 1 also compares without a float bridge.
    t.db
        .query("SELECT id FROM t WHERE r > ?", &[Value::Int((1i64 << 53) + 1)])
        .unwrap();
    // Keep a refuse probe that still hits the float64 slot pin: CAST forces it.
    refuses(
        "SELECT id FROM t WHERE CAST(r AS REAL) > ?",
        &[Value::Int((1i64 << 53) + 1)],
        "too large to convert to float64",
    );
}

// ---- the assignment half: a float64 CONSTANT into an int64 column ---------

#[test]
fn integral_float_constant_assigns_like_sqlite_strict() {
    // sqlite STRICT stores 8.0 in an INT column as the integer 8 and refuses
    // 8.5 ("cannot store REAL value in INT column"). mpedb now matches both.
    let t = open();
    t.db.query("INSERT INTO t (id, i) VALUES (11, 8.0)", &[]).unwrap();
    t.db.query("UPDATE t SET i = 9.0 WHERE id = 1", &[]).unwrap();
    assert_eq!(
        rows_of(t.db.query("SELECT id, i, typeof(i) FROM t WHERE id IN (1, 11) ORDER BY id", &[]).unwrap()),
        sqlite_rows(
            &["INSERT INTO t (id, i) VALUES (11, 8.0)", "UPDATE t SET i = 9.0 WHERE id = 1"],
            "SELECT id, i, typeof(i) FROM t WHERE id IN (1, 11) ORDER BY id"
        ),
    );

    let e = t
        .db
        .query("INSERT INTO t (id, i) VALUES (12, 8.5)", &[])
        .expect_err("8.5 must not fit an int64 column")
        .to_string();
    assert!(e.contains("not exactly an integer in the int64 range"), "{e}");

    // A NON-constant float expression is where task #113 moved the line: this
    // `i` is DDL-declared, so it carries sqlite's INTEGER affinity and applies
    // it per value at store time — which is where losslessness finally becomes
    // checkable. `SET i = <real>` therefore stores the integer when the real is
    // exactly one (Django's `SET i = POWER(i, ?)` shape) …
    t.db.query("UPDATE t SET i = r * 2 WHERE id = 2", &[]).unwrap();
    assert_eq!(
        rows_of(t.db.query("SELECT i, typeof(i) FROM t WHERE id = 2", &[]).unwrap()),
        sqlite_rows(&["UPDATE t SET i = r * 2 WHERE id = 2"], "SELECT i, typeof(i) FROM t WHERE id = 2"),
    );
    // … and REFUSES, per row, when it is not — sqlite would have stored the
    // real, which a rigid int64 cannot hold. Narrower, never different.
    let e = t
        .db
        .query("UPDATE t SET i = r WHERE id = 1", &[])
        .expect_err("a non-integral float64 must not truncate into an int64 column")
        .to_string();
    assert!(e.contains("INTEGER affinity left it a float64"), "{e}");
    // A CONFIG-declared `type = "int64"` column has no affinity to apply and
    // keeps the compile-time refusal (`bind_assign`'s constant-only rule).
    let cfg = config_int_db();
    let e = cfg
        .0
        .query("UPDATE c SET i = r WHERE id = 1", &[])
        .expect_err("a config int64 column stays rigid")
        .to_string();
    assert!(e.contains("only a constant whose value is exactly an integer converts"), "{e}");
}

/// A TOML-config table with the same shape as `DDL`, for the provenance half:
/// `type = "int64"` never converts, whatever `INTEGER` does.
fn config_int_db() -> (Database, String) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-numparams-cfg-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"c\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\
         [[table.column]]\nname = \"i\"\ntype = \"int64\"\nnullable = true\n\
         [[table.column]]\nname = \"r\"\ntype = \"float64\"\nnullable = true\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    db.query("INSERT INTO c (id, i, r) VALUES (1, 5, 2.5)", &[]).unwrap();
    (db, path)
}

// ---- nothing that already worked changed ---------------------------------

#[test]
fn the_bool_bridge_and_exact_matches_are_unchanged() {
    let t = open();
    // An exactly-typed parameter still takes the Cow::Borrowed fast path.
    assert_eq!(
        rows_of(t.db.query("SELECT id FROM t WHERE i = ?", &[Value::Int(5)]).unwrap()),
        vec![vec!["1".to_string()]]
    );
    assert_eq!(
        rows_of(t.db.query("SELECT id FROM t WHERE r = ?", &[Value::Float(2.5)]).unwrap()),
        vec![vec!["1".to_string()]]
    );
    // Text bind vs INTEGER column: full integer text converts (`'5'` → 5).
    same_as_sqlite(
        "SELECT id FROM t WHERE i = ? ORDER BY id",
        &[Value::Text("5".into())],
        "SELECT id FROM t WHERE i = 5 ORDER BY id",
    );
    // Non-numeric text cannot fill an int equality slot (pinned for PkPoint).
    let e = t
        .db
        .query("SELECT id FROM t WHERE i = ?", &[Value::Text("x".into())])
        .expect_err("non-numeric text must not fill int equality")
        .to_string();
    assert!(e.contains("text") && e.contains("int64"), "{e}");
}
