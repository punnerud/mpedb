//! Math scalar functions (`exp`, `ln`, `log`/`log10`/`log2`/`log(b,x)`, the
//! trig and hyperbolic family, `atan2`, `radians`/`degrees`, `pi()`, `mod`,
//! `trunc`) cross-checked against the real `sqlite3` CLI 3.45 over a shared
//! table of rows spanning each function's domain (positive, fractional,
//! negative, out-of-domain, and a NULL row).
//!
//! The comparison is differential and NUMERIC: the SAME `SELECT … FROM t ORDER
//! BY id` runs against mpedb and against an in-memory sqlite loaded with the
//! same rows, and each result cell is compared value-by-value. Floats are
//! compared with a relative tolerance because sqlite renders ~15 significant
//! digits while mpedb keeps the full f64 — the underlying doubles agree, their
//! text does not. NULL matches sqlite's `NULL` (driven with `-nullvalue NULL`),
//! and `Inf` (sqlite's overflow rendering) is accepted for a mpedb infinity.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Rows spanning the domains the functions care about: `x` is a float with a
/// positive, a fractional, a negative and a large case (plus a NULL); `n` is a
/// positive integer used where a strictly-positive argument is needed.
const ROWS: &[&str] = &[
    "INSERT INTO t (id, x, n) VALUES (1, 2.0, 8)",
    "INSERT INTO t (id, x, n) VALUES (2, 0.5, 100)",
    "INSERT INTO t (id, x, n) VALUES (3, -1.5, 3)",
    "INSERT INTO t (id, x, n) VALUES (4, 90.0, 45)",
    "INSERT INTO t (id, x, n) VALUES (5, NULL, NULL)",
];

fn sqlite_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn mpedb_db() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-math-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
max_readers = 8

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "x"
  type = "float64"

  [[table.column]]
  name = "n"
  type = "int64"
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for r in ROWS {
        db.query(r, &[]).unwrap();
    }
    (db, path)
}

/// The sqlite schema + data, as a script for the `sqlite3` CLI.
fn sqlite_setup() -> String {
    let mut s = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, x REAL, n INTEGER);\n");
    for r in ROWS {
        s.push_str(r);
        s.push_str(";\n");
    }
    s
}

/// The first-column values of every row mpedb returns for `query`.
fn mpedb_vals(db: &Database, query: &str) -> Vec<Value> {
    match db.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            rows.into_iter().map(|mut r| r.swap_remove(0)).collect()
        }
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

/// The first-column text of every row sqlite returns for `query`.
fn sqlite_lines(query: &str) -> Vec<String> {
    let mut input = sqlite_setup();
    input.push_str(query);
    input.push_str(";\n");
    let mut child = Command::new("sqlite3")
        .args(["-batch", "-noheader", "-nullvalue", "NULL", ":memory:"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sqlite3");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write to sqlite3");
    let out = child.wait_with_output().expect("wait sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 failed for `{query}`: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// Does one mpedb value agree with sqlite's rendered cell? Floats compare with
/// a relative tolerance (sqlite prints ~15 digits); NULL and infinity are
/// special-cased.
fn value_matches(m: &Value, s: &str) -> bool {
    match m {
        Value::Null => s == "NULL",
        Value::Int(i) => s
            .parse::<i64>()
            .map(|y| y == *i)
            .unwrap_or_else(|_| s.parse::<f64>().map(|y| y == *i as f64).unwrap_or(false)),
        Value::Float(x) => {
            if x.is_nan() {
                return false; // a NaN must have become NULL before reaching here
            }
            if x.is_infinite() {
                return s.eq_ignore_ascii_case("inf") || s.eq_ignore_ascii_case("-inf");
            }
            match s.parse::<f64>() {
                Ok(y) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
                Err(_) => false,
            }
        }
        other => panic!("unexpected value type from a math function: {other:?}"),
    }
}

/// The same query must give the same values in both engines.
fn cross_check(db: &Database, query: &str) {
    let m = mpedb_vals(db, query);
    let s = sqlite_lines(query);
    assert_eq!(
        m.len(),
        s.len(),
        "row count differs for `{query}`: mpedb {m:?} vs sqlite {s:?}"
    );
    for (mv, sv) in m.iter().zip(&s) {
        assert!(
            value_matches(mv, sv),
            "mismatch on `{query}`: mpedb {mv:?} vs sqlite `{sv}`"
        );
    }
}

#[test]
fn math_fns_match_sqlite_over_a_table() {
    if !sqlite_available() {
        eprintln!("skipping: sqlite3 CLI not found");
        return;
    }
    let (db, path) = mpedb_db();

    // exp: e^x; overflow is kept as Inf on both engines (x = 90 is still finite).
    cross_check(&db, "SELECT exp(x) FROM t ORDER BY id");
    // ln / log10 / log2 / log: base checks — over the strictly-positive n, and
    // over x (which includes a negative and so exercises the x<=0 → NULL path).
    cross_check(&db, "SELECT ln(n) FROM t ORDER BY id");
    cross_check(&db, "SELECT ln(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT log(n) FROM t ORDER BY id"); // 1-arg log == log10
    cross_check(&db, "SELECT log10(n) FROM t ORDER BY id");
    cross_check(&db, "SELECT log2(n) FROM t ORDER BY id");
    cross_check(&db, "SELECT log10(x) FROM t ORDER BY id");
    // log(b, x): base b > 1 required — over x as the base this covers 2.0 (ok),
    // 0.5 (base<1 → NULL), -1.5 (→ NULL) and 90 (ok).
    cross_check(&db, "SELECT log(2, n) FROM t ORDER BY id");
    cross_check(&db, "SELECT log(x, n) FROM t ORDER BY id");
    // Trig and inverse trig. asin/acos over x exercise the out-of-[-1,1] → NULL
    // path (x = 2.0 / -1.5 / 90 are all out of domain).
    cross_check(&db, "SELECT sin(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT cos(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT tan(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT asin(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT acos(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT atan(x) FROM t ORDER BY id");
    // atan2(y, x): both columns, mixed float/int arguments.
    cross_check(&db, "SELECT atan2(x, n) FROM t ORDER BY id");
    // Hyperbolic.
    cross_check(&db, "SELECT sinh(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT cosh(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT tanh(x) FROM t ORDER BY id");
    // Angle conversions.
    cross_check(&db, "SELECT radians(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT degrees(x) FROM t ORDER BY id");
    // pi(): the constant, one row per table row.
    cross_check(&db, "SELECT pi() FROM t ORDER BY id");
    // mod: fmod sign-of-dividend, over int and float, plus a zero divisor
    // (→ NULL on both, not an error).
    cross_check(&db, "SELECT mod(n, 3) FROM t ORDER BY id");
    cross_check(&db, "SELECT mod(x, 2) FROM t ORDER BY id");
    cross_check(&db, "SELECT mod(n, 0) FROM t ORDER BY id");
    // trunc: type-preserving — a float truncates to a float, an int stays int.
    cross_check(&db, "SELECT trunc(x) FROM t ORDER BY id");
    cross_check(&db, "SELECT trunc(n) FROM t ORDER BY id");
    // A composed expression: degrees(atan2(x, n)) rounded toward zero.
    cross_check(&db, "SELECT trunc(degrees(atan2(x, n))) FROM t ORDER BY id");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn math_fn_domain_and_arity_edges() {
    let (db, path) = mpedb_db();
    let one = |sql: &str| match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows.into_iter().next().unwrap().swap_remove(0),
        other => panic!("{other:?}"),
    };

    // Out-of-domain → NULL, matching sqlite (verified above too, asserted here
    // directly so the intent is explicit).
    assert_eq!(one("SELECT ln(-1) FROM t WHERE id = 1"), Value::Null);
    assert_eq!(one("SELECT ln(0) FROM t WHERE id = 1"), Value::Null);
    assert_eq!(one("SELECT log2(0) FROM t WHERE id = 1"), Value::Null);
    assert_eq!(one("SELECT log(1, 5) FROM t WHERE id = 1"), Value::Null); // base 1
    assert_eq!(one("SELECT log(0.5, 8) FROM t WHERE id = 1"), Value::Null); // base<1
    assert_eq!(one("SELECT asin(2) FROM t WHERE id = 1"), Value::Null);
    assert_eq!(one("SELECT acos(2) FROM t WHERE id = 1"), Value::Null);
    assert_eq!(one("SELECT mod(5, 0) FROM t WHERE id = 1"), Value::Null); // NOT an error

    // trunc preserves the argument type (int stays int; a float truncates).
    assert_eq!(one("SELECT trunc(9) FROM t WHERE id = 1"), Value::Int(9));
    assert_eq!(one("SELECT trunc(2.9) FROM t WHERE id = 1"), Value::Float(2.0));

    // NULL propagates.
    assert_eq!(one("SELECT exp(NULL) FROM t WHERE id = 1"), Value::Null);
    assert_eq!(one("SELECT atan2(NULL, 1) FROM t WHERE id = 1"), Value::Null);
    assert_eq!(one("SELECT mod(NULL, 2) FROM t WHERE id = 1"), Value::Null);

    // Arity / type errors are compile errors.
    assert!(db.query("SELECT pi(1) FROM t", &[]).is_err()); // pi is nullary
    assert!(db.query("SELECT log() FROM t", &[]).is_err()); // log needs 1 or 2
    assert!(db.query("SELECT log(2, 3, 4) FROM t", &[]).is_err());
    assert!(db.query("SELECT sin('x') FROM t", &[]).is_err()); // non-number
    assert!(db.query("SELECT atan2(1) FROM t", &[]).is_err()); // needs 2

    let _ = std::fs::remove_file(&path);
}
