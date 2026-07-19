//! COLUMN-DECLARED collation — `CREATE TABLE t(name TEXT COLLATE NOCASE)` and
//! `ALTER TABLE … ADD COLUMN … COLLATE …`. A column's declared collation is the
//! DEFAULT collating sequence for `= <> < <= > >= IN BETWEEN`, `ORDER BY`,
//! `GROUP BY` and `DISTINCT` on that column (sqlite's precedence rung 2), unless
//! an explicit `COLLATE` operand overrides it.
//!
//! Every semantic case is DIFFERENTIALLY verified against the `sqlite3` CLI 3.45:
//! mpedb runs the query, sqlite runs the identical `CREATE TABLE` + `INSERT`s +
//! query, and the two outputs must match exactly. Because a bare grouped/distinct
//! column has a representative sqlite may pick arbitrarily, the GROUP BY case
//! compares the COUNT multiset (representative-free) and the ORDER BY / DISTINCT
//! cases add a stable `id` tiebreak.
//!
//! What is DEFERRED (refused cleanly, never a wrong answer): a collated
//! UNIQUE/index/PRIMARY KEY — mpedb keys are memcmp-ordered and collated on-disk
//! keys are not built yet. Those refusals are mpedb-only (sqlite accepts them),
//! so they are asserted directly, not differentially.

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
        let _ = std::fs::remove_file(format!("{}-wal", self.path));
    }
}

/// The table under test, created at RUNTIME so its column collations are exercised
/// end-to-end (the config/TOML path declares none). `name` is NOCASE, `code` is
/// RTRIM, `plain` has no declared collation (BINARY — the control).
const CREATE: &str = "CREATE TABLE t (\
    id INTEGER PRIMARY KEY, \
    name TEXT COLLATE NOCASE, \
    code TEXT COLLATE RTRIM, \
    plain TEXT)";

/// One seed row: `(id, name, code, plain)`.
type Row = (i64, Option<&'static str>, Option<&'static str>, Option<&'static str>);

/// Mixed ASCII case for NOCASE, trailing spaces for RTRIM, two accented rows
/// (NOCASE must NOT fold Unicode), and a NULL row.
const ROWS: &[Row] = &[
    (1, Some("Alice"), Some("x"), Some("aa")),
    (2, Some("alice"), Some("x "), Some("AA")),
    (3, Some("ALICE"), Some("x  "), Some("aa")),
    (4, Some("Bob"), Some("y"), Some("bb")),
    (5, Some("bob"), Some("y "), Some("BB")),
    (6, Some("Carol"), Some("z"), Some("cc")),
    (7, Some("dave"), Some("zz"), Some("dd")),
    (8, Some("héllo"), Some("h"), Some("ee")), // lowercase é (non-ASCII)
    (9, Some("HÉLLO"), Some("h "), Some("ee")), // uppercase É — NOCASE must NOT fold
    (10, None, None, None),
];

fn lit(v: Option<&str>) -> String {
    v.map_or("NULL".to_string(), |s| format!("'{}'", s.replace('\'', "''")))
}

fn insert_statements() -> Vec<String> {
    ROWS.iter()
        .map(|(id, name, code, plain)| {
            format!(
                "INSERT INTO t (id, name, code, plain) VALUES ({id}, {}, {}, {})",
                lit(*name),
                lit(*code),
                lit(*plain)
            )
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
        "{dir}/mpedb-collate-col-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    // A throwaway seed table satisfies the config schema; the real table is
    // CREATEd at runtime so its COLLATE clauses are parsed and applied live.
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    db.query(CREATE, &[]).unwrap();
    for stmt in insert_statements() {
        db.query(&stmt, &[]).unwrap();
    }
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

/// Run `CREATE t` + inserts + the query through the sqlite3 CLI and parse the
/// default list-mode output. sqlite's `CREATE TABLE` declares the identical
/// column collations, so both engines resolve collation the same way.
fn sqlite_rows(query: &str) -> Vec<Vec<String>> {
    let mut script = format!("{CREATE};\n");
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
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "sqlite3 failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// The full differential battery: comparisons, precedence, IN/BETWEEN, ORDER BY,
/// DISTINCT, GROUP BY, RTRIM, the BINARY control, and Unicode-not-folded — each
/// identical to sqlite 3.45.
#[test]
fn column_collation_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // ---- NOCASE column: `=` is case-insensitive by default --------------
        "SELECT id FROM t WHERE name = 'ALICE' ORDER BY id",
        "SELECT id FROM t WHERE name = 'alice' ORDER BY id",
        "SELECT id FROM t WHERE 'ALICE' = name ORDER BY id",
        "SELECT id, name = 'bob' FROM t ORDER BY id",
        "SELECT id FROM t WHERE name <> 'bob' ORDER BY id",
        // ---- NOCASE column: ordering comparisons ----------------------------
        "SELECT id FROM t WHERE name < 'b' ORDER BY id",
        "SELECT id FROM t WHERE name >= 'BOB' ORDER BY id",
        // ---- Explicit COLLATE overrides the column default -------------------
        // BINARY on either operand (left wins) → only the exact-case row.
        "SELECT id FROM t WHERE name = 'ALICE' COLLATE BINARY ORDER BY id",
        "SELECT id FROM t WHERE name COLLATE BINARY = 'ALICE' ORDER BY id",
        "SELECT id FROM t WHERE 'ALICE' COLLATE BINARY = name ORDER BY id",
        // A redundant explicit NOCASE agrees with the column default.
        "SELECT id FROM t WHERE name = 'ALICE' COLLATE NOCASE ORDER BY id",
        // ---- IN / BETWEEN inherit the column collation -----------------------
        "SELECT id FROM t WHERE name IN ('alice', 'BOB') ORDER BY id",
        "SELECT id FROM t WHERE name NOT IN ('alice') AND name IS NOT NULL ORDER BY id",
        "SELECT id FROM t WHERE name BETWEEN 'a' AND 'c' ORDER BY id",
        // ---- RTRIM column: trailing spaces ignored ---------------------------
        "SELECT id FROM t WHERE code = 'x' ORDER BY id",
        "SELECT id FROM t WHERE code = 'y' ORDER BY id",
        "SELECT id FROM t WHERE 'h' = code ORDER BY id",
        // ---- ORDER BY name: case-insensitive, id tiebreak --------------------
        "SELECT id, name FROM t WHERE name IS NOT NULL ORDER BY name, id",
        "SELECT id, name FROM t WHERE name IS NOT NULL ORDER BY name DESC, id",
        // Explicit BINARY on ORDER BY overrides the column default.
        "SELECT id, name FROM t WHERE name IS NOT NULL ORDER BY name COLLATE BINARY, id",
        // ---- DISTINCT name: case-insensitive dedup ---------------------------
        "SELECT DISTINCT name FROM t WHERE name IS NOT NULL ORDER BY name, name COLLATE BINARY",
        // ---- GROUP BY name: case-insensitive grouping (count multiset) -------
        // Representative-free: the group COUNTS (sorted) prove the collapse.
        "SELECT count(*) FROM t WHERE name IS NOT NULL GROUP BY name ORDER BY count(*)",
        // ---- BINARY control column (no declared collation) -------------------
        "SELECT id FROM t WHERE plain = 'aa' ORDER BY id",
        // …but an explicit COLLATE still applies to a BINARY column.
        "SELECT id FROM t WHERE plain = 'AA' COLLATE NOCASE ORDER BY id",
        // ---- Unicode is NOT folded by NOCASE ---------------------------------
        // 'héllo'(8) and 'HÉLLO'(9) differ only by accented-letter case.
        "SELECT id FROM t WHERE name = 'héllo' ORDER BY id",
        "SELECT id FROM t WHERE name = 'HÉLLO' ORDER BY id",
        // The ASCII prefix still folds; the accented byte still distinguishes.
        "SELECT id FROM t WHERE name = 'HELLO' ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// `ALTER TABLE … ADD COLUMN … COLLATE NOCASE` — the added column's collation is
/// the default for comparisons on it, verified against sqlite.
#[test]
fn alter_add_column_collate_matches_sqlite() {
    let dir = if std::path::Path::new("/dev/shm").is_dir() { "/dev/shm" } else { "/tmp" };
    let path = format!(
        "{dir}/mpedb-collate-alter-{}-{}.mpedb",
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
    struct G(String);
    impl Drop for G {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(format!("{}-wal", self.0));
        }
    }
    let _g = G(path.clone());

    let setup = [
        "CREATE TABLE u (id INTEGER PRIMARY KEY, base TEXT)",
        "ALTER TABLE u ADD COLUMN tag TEXT COLLATE NOCASE",
        "INSERT INTO u (id, base, tag) VALUES (1, 'p', 'Red')",
        "INSERT INTO u (id, base, tag) VALUES (2, 'q', 'RED')",
        "INSERT INTO u (id, base, tag) VALUES (3, 'r', 'blue')",
    ];
    for s in setup {
        db.query(s, &[]).unwrap();
    }
    let query = "SELECT id FROM u WHERE tag = 'red' ORDER BY id";
    let got = mpedb_rows(&db, query);

    // sqlite: same DDL + data + query.
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
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
        .expect("sqlite3 CLI required");
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "sqlite3: {}", String::from_utf8_lossy(&out.stderr));
    let want: Vec<Vec<String>> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect();
    assert_eq!(got, want, "ALTER ADD COLUMN COLLATE NOCASE mismatch");
}

/// The DEFERRED half: a collated UNIQUE / index / PRIMARY KEY is refused cleanly
/// (mpedb keys are memcmp-ordered; collated on-disk keys are not built yet). Each
/// refusal names COLLATE so the gap is visible, never a wrong uniqueness answer.
#[test]
fn collated_unique_and_index_refused_cleanly() {
    let d = db(); // has table `t` with a NOCASE `name`
    let err = |sql: &str| -> String {
        match d.query(sql, &[]) {
            Ok(_) => panic!("expected `{sql}` to be refused, but it succeeded"),
            Err(e) => e.to_string(),
        }
    };

    // Inline UNIQUE on a NOCASE column.
    let e = err("CREATE TABLE a (id INTEGER PRIMARY KEY, s TEXT COLLATE NOCASE UNIQUE)");
    assert!(e.to_uppercase().contains("COLLATE"), "collated UNIQUE refusal: {e}");
    // Table-level UNIQUE over a NOCASE column.
    let e = err("CREATE TABLE b (id INTEGER PRIMARY KEY, s TEXT COLLATE NOCASE, UNIQUE (s))");
    assert!(e.to_uppercase().contains("COLLATE"), "collated table UNIQUE refusal: {e}");
    // A NOCASE PRIMARY KEY.
    let e = err("CREATE TABLE c (s TEXT COLLATE NOCASE PRIMARY KEY)");
    assert!(e.to_uppercase().contains("COLLATE"), "collated PK refusal: {e}");
    // CREATE INDEX on an existing NOCASE column.
    let e = err("CREATE INDEX ix ON t (name)");
    assert!(e.to_uppercase().contains("COLLATE"), "collated CREATE INDEX refusal: {e}");

    // An unknown collation name is a clean error, not a silent BINARY.
    let e = err("CREATE TABLE d (id INTEGER PRIMARY KEY, s TEXT COLLATE NOSUCH)");
    assert!(
        e.to_lowercase().contains("collation") || e.to_uppercase().contains("COLLATE"),
        "unknown collation refusal: {e}"
    );

    // A non-BINARY collation on a non-text column is refused (collation is a
    // text-only notion), matching a clean-refusal policy.
    let e = err("CREATE TABLE e (id INTEGER PRIMARY KEY, n INTEGER COLLATE NOCASE)");
    assert!(
        e.to_lowercase().contains("text") || e.to_uppercase().contains("COLLATE"),
        "non-text collation refusal: {e}"
    );
}

/// Direct semantic assertions for the headline cases, so a regression shows up as
/// a semantic failure and not only a CLI string diff.
#[test]
fn column_collation_semantics_direct() {
    let d = db();
    let ids = |sql: &str| -> Vec<i64> {
        match d.query(sql, &[]).unwrap() {
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
    // The task's headline: a NOCASE column matches a case-variant literal.
    assert_eq!(ids("SELECT id FROM t WHERE name = 'ALICE' ORDER BY id"), vec![1, 2, 3]);
    // Explicit COLLATE BINARY overrides the column default → exact case only.
    assert_eq!(
        ids("SELECT id FROM t WHERE name = 'ALICE' COLLATE BINARY ORDER BY id"),
        vec![3]
    );
    // RTRIM column ignores trailing spaces.
    assert_eq!(ids("SELECT id FROM t WHERE code = 'x' ORDER BY id"), vec![1, 2, 3]);
    // The BINARY control column is case-sensitive.
    assert_eq!(ids("SELECT id FROM t WHERE plain = 'aa' ORDER BY id"), vec![1, 3]);
    // NOCASE does not fold Unicode.
    assert_eq!(ids("SELECT id FROM t WHERE name = 'héllo' ORDER BY id"), vec![8]);
    assert_eq!(ids("SELECT id FROM t WHERE name = 'HÉLLO' ORDER BY id"), vec![9]);
    // DISTINCT collapses the NOCASE classes: {Alice/alice/ALICE, Bob/bob, Carol,
    // dave, héllo, HÉLLO} = 6 distinct names.
    let distinct = ids(
        "SELECT count(*) FROM t WHERE name IS NOT NULL GROUP BY name ORDER BY count(*) DESC",
    );
    // One group of 3 (alice*), one of 2 (bob*), four of 1.
    assert_eq!(distinct, vec![3, 2, 1, 1, 1, 1]);
}
