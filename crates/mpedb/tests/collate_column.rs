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
//! A collated UNIQUE / secondary index / PRIMARY KEY is now SUPPORTED: the engine
//! folds each value under the column's collation before it enters the keycode
//! tree, so `'Alice'` and `'ALICE'` share one on-disk key — a NOCASE UNIQUE
//! rejects the case-variant duplicate exactly as sqlite does, and a NOCASE probe
//! finds it. Those cases are verified differentially too. What stays refused: an
//! unknown collation name, and a non-BINARY collation on a non-text column.

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
    let dir = mpedb_testkit::scratch_base_str();
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

    sqlite_oracle::script_stdout(&script, "")
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
        // ---- f(DISTINCT name): the dedup INSIDE an aggregate -----------------
        // The same rung-2 rule as `SELECT DISTINCT`, and it was missing: the
        // accumulator keyed on the raw bytes, so `count(DISTINCT name)` said 8
        // where sqlite says 6.
        "SELECT count(DISTINCT name) FROM t",
        "SELECT count(DISTINCT code) FROM t",
        "SELECT count(DISTINCT plain) FROM t",
        "SELECT group_concat(DISTINCT name) FROM t WHERE name IS NOT NULL",
        // Per group, and with a FILTER in front of the dedup.
        "SELECT count(DISTINCT name) FROM t WHERE name IS NOT NULL GROUP BY code ORDER BY 1",
        // An EXPRESSION argument names no column and therefore carries no
        // collation — BINARY, as in sqlite, so the case variants come apart
        // again. (`trim`, not `lower`: mpedb's `lower()` casefolds Unicode
        // where sqlite's folds ASCII only, a divergence of its own.)
        "SELECT count(DISTINCT trim(name)) FROM t",
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
    let dir = mpedb_testkit::scratch_base_str();
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
    let want: Vec<Vec<String>> = sqlite_oracle::script_stdout(&script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect();
    assert_eq!(got, want, "ALTER ADD COLUMN COLLATE NOCASE mismatch");
}

/// Open a fresh mpedb (seed table only) and run `stmts` in order, returning the
/// rows of the LAST statement — or the first error as `Err`, so a constraint
/// violation is observable and comparable to sqlite's.
fn run_mpedb(stmts: &[&str]) -> Result<Vec<Vec<String>>, String> {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-collate-idx-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    struct G(String);
    impl Drop for G {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(format!("{}-wal", self.0));
        }
    }
    let _g = G(path.clone());
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let mut last = Vec::new();
    for s in stmts {
        match db.query(s, &[]) {
            Ok(ExecResult::Rows { rows, .. }) => {
                last = rows
                    .into_iter()
                    .map(|r| r.into_iter().map(render).collect())
                    .collect();
            }
            Ok(_) => last = Vec::new(),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(last)
}

/// The same script through the bundled sqlite: `Ok(rows)` on success, `Err` if
/// any statement failed (so a UNIQUE / PK violation surfaces as `Err`, like
/// mpedb; `try_script_stdout` is fail-fast, the old script's `.bail on`).
fn run_sqlite(stmts: &[&str]) -> Result<Vec<Vec<String>>, String> {
    let mut script = String::new();
    for s in stmts {
        script.push_str(s);
        script.push_str(";\n");
    }
    Ok(sqlite_oracle::try_script_stdout(&script, "")?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect())
}

/// Assert mpedb and sqlite AGREE on `stmts` — same rows on success, and both
/// error on a constraint violation (the category must match; error text need
/// not). This is the collated-index contract: fold-into-the-key must reject a
/// case-variant duplicate and resolve a case-variant probe exactly as sqlite.
fn agree(stmts: &[&str]) {
    let m = run_mpedb(stmts);
    let s = run_sqlite(stmts);
    match (&m, &s) {
        (Ok(mr), Ok(sr)) => assert_eq!(mr, sr, "row mismatch on {stmts:?}"),
        (Err(_), Err(_)) => {}
        _ => panic!("outcome category differs on {stmts:?}\n  mpedb={m:?}\n  sqlite={s:?}"),
    }
}

/// WHICH member of a collated group the KEY reports: sqlite reads it from the
/// min/max WITNESS row, exactly as it reads a bare column.
///
/// Under BINARY every member is byte-identical, so the choice is unobservable
/// — under NOCASE/RTRIM it is not, and mpedb reported the group's FIRST row.
/// `SELECT k, max(v) … GROUP BY k` on a NOCASE `k` answered 'a' where sqlite
/// answers 'A', which is a wrong answer with nothing to hint at it. With no
/// min/max there is no witness and BOTH engines report the first row, so the
/// aggregate set is what decides.
#[test]
fn a_collated_group_key_reports_the_witness_row() {
    let d = db();
    for q in [
        // A single min/max: the key follows the extremum's row.
        "SELECT name, max(id) FROM t GROUP BY name ORDER BY name",
        "SELECT name, min(id) FROM t GROUP BY name ORDER BY name",
        "SELECT code, max(id) FROM t GROUP BY code ORDER BY code",
        "SELECT code, min(id) FROM t GROUP BY code ORDER BY code",
        // Two keys at once, and a key next to a bare column — one capture, so
        // they cannot come from different rows.
        "SELECT name, code, max(id) FROM t GROUP BY name, code ORDER BY name, code",
        "SELECT name, max(id), plain FROM t GROUP BY name ORDER BY name",
        // No min/max ⇒ no witness ⇒ the first row, in both engines.
        "SELECT name, count(*) FROM t GROUP BY name ORDER BY name",
        "SELECT name, sum(id) FROM t GROUP BY name ORDER BY name",
        // TWO min/max ⇒ sqlite's pick is order-dependent and it falls back to
        // the first row; mpedb must do the same rather than pick one extremum.
        "SELECT name, max(id), min(id) FROM t GROUP BY name ORDER BY name",
        // The witness follows a FILTERED extremum.
        "SELECT name, max(id) FILTER (WHERE id > 2) FROM t GROUP BY name ORDER BY name",
        // …and it survives HAVING, DISTINCT and a window over the group.
        "SELECT name, max(id) FROM t GROUP BY name HAVING max(id) > 1 ORDER BY name",
        "SELECT DISTINCT name, max(id) FROM t GROUP BY name ORDER BY name",
        "SELECT name, max(id), rank() OVER (ORDER BY max(id)) FROM t GROUP BY name ORDER BY name",
        // The BINARY control column: unobservable either way, and must not move.
        "SELECT plain, max(id) FROM t GROUP BY plain ORDER BY plain",
    ] {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// A WINDOW's `PARTITION BY` and `ORDER BY` compare under the column's declared
/// collation — sqlite's precedence rung 2 reaches them like every other clause.
///
/// They did not, and it was a WRONG ANSWER rather than a refusal: `PARTITION BY
/// name` on a NOCASE column put 'Alice' and 'alice' in DIFFERENT partitions and
/// `ORDER BY name` ranked them as distinct, so `row_number`, `rank`,
/// `group_concat` over the partition and every frame that spans peers all
/// disagreed with sqlite while looking perfectly plausible.
#[test]
fn window_clauses_use_the_declared_collation() {
    let d = db();
    for q in [
        // PARTITION BY: the NOCASE classes are ONE partition each.
        "SELECT id, count(*) OVER (PARTITION BY name) FROM t ORDER BY id",
        "SELECT id, group_concat(id) OVER (PARTITION BY name) FROM t ORDER BY id",
        // RTRIM likewise — 'x', 'x ', 'x  ' are one class.
        "SELECT id, count(*) OVER (PARTITION BY code) FROM t ORDER BY id",
        // The BINARY control column must NOT collapse.
        "SELECT id, count(*) OVER (PARTITION BY plain) FROM t ORDER BY id",
        // ORDER BY: the classes are PEERS, which `rank`/`dense_rank` see and
        // `row_number` does not.
        "SELECT id, rank() OVER (ORDER BY name) FROM t ORDER BY id",
        "SELECT id, dense_rank() OVER (ORDER BY name) FROM t ORDER BY id",
        "SELECT id, row_number() OVER (ORDER BY name) FROM t ORDER BY id",
        "SELECT id, rank() OVER (ORDER BY code) FROM t ORDER BY id",
        "SELECT id, rank() OVER (ORDER BY plain) FROM t ORDER BY id",
        // Peer-based frames read the same peer groups.
        "SELECT id, group_concat(id) OVER (ORDER BY name \
         RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, group_concat(id) OVER (ORDER BY name \
         GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        // …including the exclusion, whose peer group is the collated one.
        "SELECT id, count(*) OVER (ORDER BY name \
         ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) \
         FROM t ORDER BY id",
        // Both at once, and the offset functions that read peer order.
        "SELECT id, lag(id) OVER (PARTITION BY code ORDER BY name) FROM t ORDER BY id",
        "SELECT id, first_value(id) OVER (PARTITION BY name ORDER BY id) FROM t ORDER BY id",
        // A COMPUTED key carries no declared collation — BINARY, in both engines.
        "SELECT id, count(*) OVER (PARTITION BY substr(name, 1, 3)) FROM t ORDER BY id",
    ] {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// A collated UNIQUE / secondary index / PRIMARY KEY now builds folded on-disk
/// keys — verified to behave exactly like sqlite: case-variant duplicates are
/// rejected, case-variant probes resolve, and ordering collapses the classes.
#[test]
fn collated_unique_index_pk_match_sqlite() {
    // NOCASE UNIQUE: 'ALICE' is a duplicate of 'Alice' → both engines reject.
    agree(&[
        "CREATE TABLE a (id INTEGER PRIMARY KEY, s TEXT COLLATE NOCASE UNIQUE)",
        "INSERT INTO a VALUES (1, 'Alice')",
        "INSERT INTO a VALUES (2, 'ALICE')",
        "SELECT id FROM a ORDER BY id",
    ]);
    // NOCASE UNIQUE, non-conflicting inserts + a case-variant equality probe.
    agree(&[
        "CREATE TABLE a (id INTEGER PRIMARY KEY, s TEXT COLLATE NOCASE UNIQUE)",
        "INSERT INTO a VALUES (1, 'Alice')",
        "INSERT INTO a VALUES (2, 'Bob')",
        "SELECT id FROM a WHERE s = 'aLiCe'",
    ]);
    // Table-level UNIQUE over a NOCASE column.
    agree(&[
        "CREATE TABLE b (id INTEGER PRIMARY KEY, s TEXT COLLATE NOCASE, UNIQUE (s))",
        "INSERT INTO b VALUES (1, 'x')",
        "INSERT INTO b VALUES (2, 'X')",
        "SELECT id FROM b ORDER BY id",
    ]);
    // NOCASE PRIMARY KEY: 'BOB' duplicates 'Bob'.
    agree(&[
        "CREATE TABLE c (s TEXT COLLATE NOCASE PRIMARY KEY)",
        "INSERT INTO c VALUES ('Bob')",
        "INSERT INTO c VALUES ('BOB')",
        "SELECT s FROM c",
    ]);
    // A NOCASE PK point-lookup resolves the case-variant.
    agree(&[
        "CREATE TABLE c (s TEXT COLLATE NOCASE PRIMARY KEY)",
        "INSERT INTO c VALUES ('Bob')",
        "INSERT INTO c VALUES ('carol')",
        "SELECT s FROM c WHERE s = 'BOB'",
    ]);
    // CREATE INDEX on a NOCASE column, then a case-variant equality lookup and
    // an ordering that collapses case.
    agree(&[
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE)",
        "INSERT INTO t VALUES (1,'Alice'),(2,'alice'),(3,'Bob'),(4,'ALICE')",
        "CREATE INDEX ix ON t (name)",
        "SELECT id FROM t WHERE name = 'ALICE' ORDER BY id",
    ]);
    // RTRIM UNIQUE: 'x ' duplicates 'x'.
    agree(&[
        "CREATE TABLE r (id INTEGER PRIMARY KEY, s TEXT COLLATE RTRIM UNIQUE)",
        "INSERT INTO r VALUES (1, 'x')",
        "INSERT INTO r VALUES (2, 'x  ')",
        "SELECT id FROM r ORDER BY id",
    ]);
    // Updating a NOCASE-unique value to a case-variant of ITSELF is a no-op for
    // the index (no phantom self-conflict), so the UPDATE succeeds in both.
    agree(&[
        "CREATE TABLE u (id INTEGER PRIMARY KEY, s TEXT COLLATE NOCASE UNIQUE)",
        "INSERT INTO u VALUES (1, 'Bob')",
        "UPDATE u SET s = 'bob' WHERE id = 1",
        "SELECT id, s FROM u",
    ]);
}

/// Still refused (mpedb-only, sqlite accepts the first but we don't): an unknown
/// collation name, and a non-BINARY collation on a non-text column.
#[test]
fn unsupported_collation_forms_refused() {
    let err = |sql: &str| -> String {
        match run_mpedb(&[sql, "SELECT 1"]) {
            Ok(_) => panic!("expected `{sql}` to be refused, but it succeeded"),
            Err(e) => e,
        }
    };
    let e = err("CREATE TABLE d (id INTEGER PRIMARY KEY, s TEXT COLLATE NOSUCH)");
    assert!(
        e.to_lowercase().contains("collation") || e.to_uppercase().contains("COLLATE"),
        "unknown collation refusal: {e}"
    );
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

// ------------------------------- collation across a SUBQUERY boundary ------

/// A declared collation must survive a comparison that crosses into a subquery.
///
/// It did not, and that was a shipped WRONG ANSWER in entirely ordinary SQL: a
/// correlated reference is rewritten to a bare `Param` before the inner binder
/// sees it, and the binder's rung-2 lookup read `BExpr::Col` only — so
/// `nc.a COLLATE NOCASE` compared BYTEWISE inside the subquery and matched
/// nothing. The correlation slots carry the outer column's collation now.
///
/// The FLIPPED case is the control, and it is why the fix could not simply wrap
/// the parameter in an explicit `COLLATE`: sqlite's precedence is SIDE-SENSITIVE
/// — the left operand's column collation wins — so `nq.x = nc.a` legitimately
/// answers nothing, and an explicit `COLLATE` on the right would have turned
/// that correct answer into a wrong one.
#[test]
fn a_declared_collation_crosses_a_subquery_boundary_like_sqlite() {
    agree(&[
        "CREATE TABLE nc (id INTEGER PRIMARY KEY, a TEXT COLLATE NOCASE)",
        "CREATE TABLE nq (x TEXT)",
        "INSERT INTO nc (id,a) VALUES (1,'AB'),(2,'cd')",
        "INSERT INTO nq (x) VALUES ('ab')",
        // Correlated, outer column on the LEFT: NOCASE governs, one row.
        "SELECT id FROM nc WHERE EXISTS (SELECT 1 FROM nq WHERE nc.a = nq.x) ORDER BY id",
    ]);
    // The control: outer column on the RIGHT, so the INNER column's (BINARY)
    // collation governs and the answer is empty in both engines.
    agree(&[
        "CREATE TABLE nc (id INTEGER PRIMARY KEY, a TEXT COLLATE NOCASE)",
        "CREATE TABLE nq (x TEXT)",
        "INSERT INTO nc (id,a) VALUES (1,'AB'),(2,'cd')",
        "INSERT INTO nq (x) VALUES ('ab')",
        "SELECT id FROM nc WHERE EXISTS (SELECT 1 FROM nq WHERE nq.x = nc.a) ORDER BY id",
    ]);
    // A scalar subquery on the right takes the outer column's collation too.
    agree(&[
        "CREATE TABLE nc (id INTEGER PRIMARY KEY, a TEXT COLLATE NOCASE)",
        "CREATE TABLE nq (x TEXT)",
        "INSERT INTO nc (id,a) VALUES (1,'AB'),(2,'cd')",
        "INSERT INTO nq (x) VALUES ('ab')",
        "SELECT id FROM nc WHERE a = (SELECT x FROM nq) ORDER BY id",
    ]);
}
