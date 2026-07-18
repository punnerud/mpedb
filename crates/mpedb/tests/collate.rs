//! `COLLATE` collating sequences — BINARY (default), NOCASE (ASCII-only
//! case-insensitive), RTRIM (ignore trailing spaces). Every case here is
//! cross-checked against the `sqlite3` CLI (3.45), whose three built-in
//! collations mpedb matches exactly.
//!
//! Scope of this stage (see COMPAT.md):
//!   * explicit `COLLATE` on a comparison operand (`= <> < <= > >=`, `IN`,
//!     `BETWEEN`) with the sqlite precedence rule (left operand's collation
//!     wins, else the right's, else BINARY);
//!   * `ORDER BY <expr> COLLATE <coll>` (collated sort);
//!   * NOCASE folds ONLY ASCII A–Z (Unicode is left byte-for-byte, as in sqlite).
//!
//! Column-declared `COLLATE` in `CREATE TABLE` is refused (stage 1b); a
//! `COLLATE` anywhere it could not change a comparison or a sort is a clean
//! error, never a silently-wrong answer.
//!
//! Determinism note: collated ORDER BY has ties sqlite may order arbitrarily
//! ('abc' == 'ABC' under NOCASE), so every ordering query adds `, id` as a
//! stable tiebreak — the same order mpedb's stable sort and sqlite then produce.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database, so a panicking test does not leak a `/dev/shm` file.
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

/// `id` is the PK (never NULL, always the ORDER BY tiebreak); `s` is a nullable
/// text column with NO declared collation — so both engines compare it BINARY
/// unless an explicit `COLLATE` overrides.
const SCHEMA: &str = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "s"
  type = "text"
  nullable = true
"#;

/// Seed rows: mixed ASCII case, trailing spaces (for RTRIM), a NULL, and two
/// accented rows ('héllo' / 'HÉLLO') that prove NOCASE folds ONLY ASCII.
const ROWS: &[(i64, Option<&'static str>)] = &[
    (1, Some("abc")),
    (2, Some("ABC")),
    (3, Some("Abc")),
    (4, Some("abc  ")),  // two trailing spaces
    (5, Some("ABC   ")), // three trailing spaces, uppercase
    (6, Some("abcd")),
    (7, Some("xyz")),
    (8, Some("XYZ")),
    (9, Some("Hello")),
    (10, Some("hello")),
    (11, Some("héllo")), // lowercase e-acute (non-ASCII)
    (12, Some("HÉLLO")), // uppercase E-acute (non-ASCII) — NOCASE must NOT fold it
    (13, None),          // NULL
];

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, s)| {
            let t = s.map_or("NULL".to_string(), |x| format!("'{x}'"));
            format!("INSERT INTO t (id, s) VALUES ({id}, {t})")
        })
        .collect()
}

fn db() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-collate-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!("[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n{SCHEMA}");
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
    Tmp { db, path }
}

/// Canonical cell rendering matching sqlite's default "list" mode: NULL empty,
/// bool as 1/0, text verbatim.
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        other => panic!("unexpected value in COLLATE test: {other:?}"),
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

/// Run a full script (schema + data + one query) through the `sqlite3` CLI and
/// parse its default list-mode output. The sqlite `CREATE TABLE` declares `s`
/// with NO collation, matching the mpedb schema, so both compare BINARY by
/// default.
fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT);\n");
    for stmt in insert_statements() {
        script.push_str(&stmt);
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
        .expect("the sqlite3 CLI (3.45) must be on PATH for this cross-check");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// The whole differential battery: comparisons (all six operators), precedence,
/// IN, BETWEEN, ORDER BY, DISTINCT/GROUP BY interactions, and Unicode-not-folded
/// — each identical to sqlite 3.45.
#[test]
fn collate_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // ---- NOCASE equality/inequality in the projection ------------------
        "SELECT id, s = 'abc' COLLATE NOCASE FROM t ORDER BY id",
        "SELECT id, s <> 'abc' COLLATE NOCASE FROM t ORDER BY id",
        "SELECT id, 'ABC' = s COLLATE NOCASE FROM t ORDER BY id",
        // Explicit COLLATE on the LEFT operand (precedence rung 1).
        "SELECT id, s COLLATE NOCASE = 'abc' FROM t ORDER BY id",
        // ---- NOCASE ordering comparisons -----------------------------------
        "SELECT id FROM t WHERE s < 'abd' COLLATE NOCASE ORDER BY id",
        "SELECT id FROM t WHERE s >= 'ABC' COLLATE NOCASE ORDER BY id",
        // ---- RTRIM: trailing spaces ignored --------------------------------
        "SELECT id, s = 'abc' COLLATE RTRIM FROM t ORDER BY id",
        "SELECT id FROM t WHERE s = 'ABC' COLLATE RTRIM ORDER BY id",
        "SELECT id FROM t WHERE 'abc   ' = s COLLATE RTRIM ORDER BY id",
        // ---- BINARY (default and explicit): case- and space-sensitive ------
        "SELECT id, s = 'abc' FROM t ORDER BY id",
        "SELECT id, s = 'abc' COLLATE BINARY FROM t ORDER BY id",
        // ---- Precedence: left explicit beats right explicit ----------------
        // Left NOCASE wins over right BINARY → case-insensitive.
        "SELECT id, s COLLATE NOCASE = 'ABC' COLLATE BINARY FROM t ORDER BY id",
        // Left BINARY wins over right NOCASE → case-sensitive.
        "SELECT id, s COLLATE BINARY = 'ABC' COLLATE NOCASE FROM t ORDER BY id",
        // ---- IN (list), collated on the probe ------------------------------
        "SELECT id FROM t WHERE s COLLATE NOCASE IN ('abc', 'xyz') ORDER BY id",
        "SELECT id FROM t WHERE s IN ('abc', 'xyz') ORDER BY id",
        "SELECT id FROM t WHERE s COLLATE RTRIM IN ('abc', 'xyz') ORDER BY id",
        // ---- BETWEEN (desugars to >= AND <=), collated ---------------------
        "SELECT id FROM t WHERE s COLLATE NOCASE BETWEEN 'abc' AND 'abd' ORDER BY id",
        // ---- ORDER BY COLLATE ---------------------------------------------
        "SELECT id, s FROM t ORDER BY s COLLATE NOCASE, id",
        "SELECT id, s FROM t ORDER BY s COLLATE NOCASE DESC, id",
        "SELECT id, s FROM t ORDER BY s COLLATE RTRIM, id",
        "SELECT id, s FROM t ORDER BY s COLLATE BINARY, id",
        "SELECT id, s FROM t ORDER BY s, id", // plain, unchanged
        // ---- DISTINCT / GROUP BY (binary dedup) with collated ORDER BY -----
        // The column has no declared collation, so dedup/grouping is BINARY in
        // both engines; only the sort is collated. (`s IS NOT NULL` keeps the
        // NULL row out — sqlite's list mode prints it as a blank line the
        // harness cannot tell from end-of-output.)
        "SELECT DISTINCT s FROM t WHERE s IS NOT NULL ORDER BY s COLLATE NOCASE, s",
        "SELECT s, count(*) FROM t WHERE s IS NOT NULL GROUP BY s ORDER BY s COLLATE NOCASE, s",
        // ---- Unicode is NOT folded by NOCASE -------------------------------
        // 'héllo' (id 11) and 'HÉLLO' (id 12) differ only by accented-letter
        // case, which NOCASE does not fold → they are unequal.
        "SELECT id FROM t WHERE s = 'héllo' COLLATE NOCASE ORDER BY id",
        "SELECT id, 'héllo' = 'HÉLLO' COLLATE NOCASE FROM t WHERE id = 1",
        // The ASCII prefix still folds, the accented byte still distinguishes.
        "SELECT id, s = 'HÉLLO' COLLATE NOCASE FROM t ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// Direct `Value` assertions for the cases the task calls out explicitly, so a
/// regression shows up as a semantic failure and not only a CLI string diff.
#[test]
fn collate_semantics_direct() {
    let d = db();
    let one = |sql: &str| -> Value {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };

    // `'ABC' = 'abc' COLLATE NOCASE` → 1 (the task's headline case), constant-folded.
    assert_eq!(one("SELECT 'ABC' = 'abc' COLLATE NOCASE"), Value::Bool(true));
    // Without COLLATE it is BINARY → 0.
    assert_eq!(one("SELECT 'ABC' = 'abc'"), Value::Bool(false));
    // RTRIM: trailing spaces ignored.
    assert_eq!(one("SELECT 'abc' = 'abc   ' COLLATE RTRIM"), Value::Bool(true));
    assert_eq!(one("SELECT 'abc' = 'abc   '"), Value::Bool(false));
    // RTRIM is not NOCASE: case still matters.
    assert_eq!(one("SELECT 'abc' = 'ABC   ' COLLATE RTRIM"), Value::Bool(false));

    // Precedence: LEFT explicit collation wins over RIGHT.
    assert_eq!(
        one("SELECT 'ABC' COLLATE NOCASE = 'abc' COLLATE BINARY"),
        Value::Bool(true)
    );
    assert_eq!(
        one("SELECT 'ABC' COLLATE BINARY = 'abc' COLLATE NOCASE"),
        Value::Bool(false)
    );

    // Collation is irrelevant to numeric comparison (a stray COLLATE on a
    // non-text comparison degrades to the ordinary compare).
    assert_eq!(one("SELECT 2 < 10"), Value::Bool(true));

    // Unicode: NOCASE folds only ASCII, so accented case is NOT equalized.
    assert_eq!(one("SELECT 'é' = 'É' COLLATE NOCASE"), Value::Bool(false));
    // But an all-ASCII pair still folds.
    assert_eq!(one("SELECT 'e' = 'E' COLLATE NOCASE"), Value::Bool(true));
}

/// EXPLAIN surfaces the collation, and only for a non-BINARY key/comparison.
#[test]
fn collate_explain_renders_collation() {
    let d = db();
    let explain = |sql: &str| -> String {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Explain(text) => text,
            other => panic!("expected EXPLAIN output, got {other:?}"),
        }
    };
    let sorted = explain("EXPLAIN SELECT id FROM t ORDER BY s COLLATE NOCASE, id");
    assert!(
        sorted.contains("COLLATE NOCASE"),
        "collated ORDER BY should surface the collation:\n{sorted}"
    );
    let plain = explain("EXPLAIN SELECT id FROM t ORDER BY s, id");
    assert!(
        !plain.contains("COLLATE"),
        "a plain ORDER BY must not mention COLLATE:\n{plain}"
    );
}

/// Everything mpedb does NOT support in this stage must be refused CLEANLY —
/// never a silently-wrong sort or comparison.
#[test]
fn collate_refusals_are_clean() {
    let d = db();
    let err = |sql: &str| -> String {
        match d.query(sql, &[]) {
            Ok(_) => panic!("expected `{sql}` to be refused, but it succeeded"),
            Err(e) => e.to_string(),
        }
    };

    // An unknown collation name.
    let e = err("SELECT id FROM t WHERE s = 'x' COLLATE NOSUCH ORDER BY id");
    assert!(
        e.to_lowercase().contains("collation"),
        "unknown collation should say so: {e}"
    );

    // Column-declared COLLATE in CREATE TABLE is stage 1b — refused by name.
    let e = err("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE)");
    assert!(
        e.to_uppercase().contains("COLLATE"),
        "column COLLATE should be refused by name: {e}"
    );

    // A COLLATE that could not change a comparison or a sort (here: buried in a
    // GROUP BY key, where honoring it would need a collated regroup we do not
    // do) is refused rather than dropped.
    let e = err("SELECT s FROM t GROUP BY s COLLATE NOCASE");
    assert!(
        e.to_uppercase().contains("COLLATE"),
        "unsupported COLLATE position should be refused by name: {e}"
    );

    // A bare projected COLLATE (`SELECT s COLLATE NOCASE`) changes no value, so
    // sqlite tolerates it — mpedb refuses to keep the surface honest.
    let e = err("SELECT s COLLATE NOCASE FROM t");
    assert!(
        e.to_uppercase().contains("COLLATE"),
        "a value-only COLLATE should be refused by name: {e}"
    );
}
