//! Task #74 item 1 — **rigid numeric parameter typing**.
//!
//! sqlite has no parameter types: `sqlite3_bind_int64(1)` against
//! `WHERE real_col > ?` is compared numerically, and `sqlite3_bind_double(1.0)`
//! into an INTEGER column is stored as the integer 1 by INTEGER affinity. mpedb
//! infers a type per parameter slot, so the DRIVER's choice of bind function —
//! which for Django/CPython follows the Python value's type, not the column's —
//! decided whether the statement ran at all.
//!
//! The fix bridges at BIND (`coerce_params`), exactly like the pre-existing
//! int↔bool bridge, and **only when the round trip is exact**. The inexact
//! cases stay refused BY NAME, which is the point: rounding a parameter before
//! a comparison would be a wrong answer, not a wider one.
//!
//! Every "mpedb now accepts it" case is checked against the real `sqlite3`
//! binary; every "mpedb still refuses" case asserts the reason is named.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

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
    let mut child = Command::new("sqlite3")
        .arg(":memory:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the sqlite3 CLI must be on PATH for this cross-check");
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success() && stderr.is_empty(), "sqlite3 failed: {stderr}\n{script}");
    String::from_utf8(out.stdout)
        .unwrap()
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
    // 1.5 into an int64 slot: sqlite compares `i > 1.5` exactly. Truncating to
    // 1 (or rounding to 2) would be a WRONG answer, so it is refused instead.
    refuses("SELECT id FROM t WHERE i > ?", &[Value::Float(1.5)], "not an exact integer");
    refuses("INSERT INTO t (id, i) VALUES (9, ?)", &[Value::Float(7.5)], "not an exact integer");

    // Outside the i64 range — a different reason, named differently.
    refuses("SELECT id FROM t WHERE i > ?", &[Value::Float(1e300)], "outside the int64 range");
    refuses("SELECT id FROM t WHERE i > ?", &[Value::Float(f64::NAN)], "not an exact integer");
    refuses("SELECT id FROM t WHERE i > ?", &[Value::Float(f64::INFINITY)], "not an exact integer");

    // An integer too large to be an f64 exactly. sqlite compares an integer
    // against a real EXACTLY (`sqlite3IntFloatCompare`), so widening the
    // parameter first could flip the `>`.
    refuses(
        "SELECT id FROM t WHERE r > ?",
        &[Value::Int(i64::MAX)],
        "too large to convert to float64",
    );
    refuses(
        "SELECT id FROM t WHERE r > ?",
        &[Value::Int(i64::MIN + 1)],
        "too large to convert to float64",
    );
    // …but the boundary values that ARE exact still go through: i64::MIN is
    // exactly -2^63, and every value up to 2^53 is exact.
    let t = open();
    for n in [0i64, 1, -1, 1 << 52, 1 << 53, -(1 << 53), 1 << 62, i64::MIN] {
        t.db.query("SELECT id FROM t WHERE r > ?", &[Value::Int(n)])
            .unwrap_or_else(|e| panic!("{n} should bridge exactly: {e}"));
    }
    // 2^53 + 1 is the first integer an f64 cannot hold.
    refuses(
        "SELECT id FROM t WHERE r > ?",
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

    // Losslessness has to be CHECKABLE, so only a constant converts: a column
    // of reals stays refused rather than truncated.
    let e = t
        .db
        .query("UPDATE t SET i = r WHERE id = 1", &[])
        .expect_err("a float64 column must not truncate into an int64 column")
        .to_string();
    assert!(e.contains("only a constant whose value is exactly an integer converts"), "{e}");
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
    // A text parameter in a numeric slot is still a plain, unqualified refusal
    // — the message the Python SDK matches on must not have grown a suffix.
    let e = t
        .db
        .query("SELECT id FROM t WHERE i = ?", &[Value::Text("5".into())])
        .expect_err("text must not bridge")
        .to_string();
    assert!(e.ends_with("parameter $1 is text, statement requires int64"), "{e}");
}
