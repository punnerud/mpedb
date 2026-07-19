//! `LIKE … ESCAPE c` (Django gap #6, first half) cross-checked row-by-row
//! against the real `sqlite3` CLI 3.45.
//!
//! Django emits `ESCAPE '\'` on EVERY `startswith` / `contains` / `endswith` /
//! `icontains` lookup, so this clause is on the hot path of the ORM rather than
//! being an exotic corner. It is also a clause where a near-miss is invisible:
//! it changes which rows come back, not whether the statement runs.
//!
//! The rules being pinned are sqlite's `patternCompare` + `likeFunc`, and every
//! expectation in this file was READ OFF the binary:
//!
//! - the escape character makes the NEXT character a literal — whatever it is,
//!   not only `%`/`_`/itself (`'ab' LIKE 'a\b' ESCAPE '\'` is TRUE);
//! - a DANGLING escape at the end of the pattern makes the comparison fail
//!   against every subject, including the empty string and the pattern's own
//!   text;
//! - an escape character that IS `%` or `_` DISABLES that wildcard for the
//!   whole pattern (`likeFunc` clears `matchAll`/`matchOne`), so
//!   `'axb' LIKE 'a%b' ESCAPE '%'` is FALSE;
//! - an escaped literal still folds ASCII case under the sqlite dialect.
//!
//! The PostgreSQL dialect (`bare_group_by = "postgres"`, `Instr::LikeCsEsc`)
//! cannot be cross-checked against sqlite — its whole point is that it does NOT
//! fold case — so it is pinned directly, and the case-agnostic subset is
//! asserted to agree with the sqlite-dialect database as well.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// (id, s). Deliberately full of the characters an escape clause is about:
/// literal `%`, literal `_`, literal backslashes, and mixed case.
const DATA: &[(i64, &str)] = &[
    (1, "ab"),
    (2, "a_b"),
    (3, "a%b"),
    (4, "axb"),
    (5, "AB"),
    (6, "A_B"),
    (7, r"a\b"),
    (8, "100%"),
    (9, "100x"),
    (10, "xx%fooyy"),
    (11, "xxfooyy"),
    (12, "_"),
    (13, "%"),
    (14, ""),
    (15, r"ab\"),
    (16, "aéb"),
];

const SCHEMA: &str = r#"
[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "s"
  type = "text"
"#;

fn sqlite_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn inserts() -> Vec<String> {
    DATA.iter()
        .map(|(id, s)| format!("INSERT INTO t (id, s) VALUES ({id}, '{}')", s.replace('\'', "''")))
        .collect()
}

fn open(compat: Option<&str>) -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-likeesc-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let compat_section = match compat {
        Some(mode) => format!("\n[compat]\nbare_group_by = \"{mode}\"\n"),
        None => String::new(),
    };
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 8\n{compat_section}{SCHEMA}",
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for i in inserts() {
        db.query(&i, &[]).unwrap();
    }
    (db, path)
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn mpedb_rows(db: &Database, query: &str, params: &[Value]) -> Vec<String> {
    match db.query(query, params).unwrap_or_else(|e| panic!("mpedb `{query}`: {e}")) {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect(),
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<String> {
    let mut input = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT);\n");
    for i in inserts() {
        input.push_str(&i);
        input.push_str(";\n");
    }
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

fn cross_check(db: &Database, query: &str) {
    let m = mpedb_rows(db, query, &[]);
    let s = sqlite_rows(query);
    assert_eq!(m, s, "mpedb vs sqlite3 disagree on `{query}`");
}

/// Every ESCAPE corner, differentially. The projection is
/// `id, <predicate>` over the whole table rather than a filtered row set, so a
/// disagreement names the row AND shows the boolean both engines produced.
#[test]
fn like_escape_matches_sqlite() {
    if !sqlite_available() {
        eprintln!("skipping: sqlite3 CLI not found");
        return;
    }
    let (db, path) = open(None);

    let preds = [
        // Escaped wildcards are literal.
        r"s LIKE '100\%' ESCAPE '\'",
        r"s LIKE 'a\_b' ESCAPE '\'",
        r"s LIKE '\%' ESCAPE '\'",
        r"s LIKE '\_' ESCAPE '\'",
        // The escape before a non-wildcard, non-escape character.
        r"s LIKE 'a\b' ESCAPE '\'",
        r"s LIKE 'A\B' ESCAPE '\'",
        // A doubled escape is a literal escape character.
        r"s LIKE 'a\\b' ESCAPE '\'",
        // A DANGLING escape: never matches anything, with and without a
        // preceding wildcard (the `%` backtracking must not rescue it).
        r"s LIKE 'ab\' ESCAPE '\'",
        r"s LIKE '%a\' ESCAPE '\'",
        r"s LIKE '\' ESCAPE '\'",
        r"s LIKE '%\' ESCAPE '\'",
        // Unescaped wildcards still work next to an escape clause.
        r"s LIKE 'a%b' ESCAPE '\'",
        r"s LIKE 'a_b' ESCAPE '\'",
        r"s LIKE '%' ESCAPE '\'",
        // The escape IS `%` — `likeFunc` clears matchAll for the whole pattern.
        "s LIKE 'a%%b' ESCAPE '%'",
        "s LIKE 'a%b' ESCAPE '%'",
        "s LIKE 'a%_b' ESCAPE '%'",
        // The escape IS `_` — matchOne is cleared.
        "s LIKE 'a__b' ESCAPE '_'",
        "s LIKE 'a_%b' ESCAPE '_'",
        "s LIKE '__' ESCAPE '_'",
        // A single-CHARACTER but multi-BYTE escape.
        "s LIKE 'aéb' ESCAPE 'é'",
        // NOT LIKE … ESCAPE, which is the outer NOT over the same node (and so
        // is NULL-propagating in the same way).
        r"s NOT LIKE 'a\b' ESCAPE '\'",
        r"s NOT LIKE 'ab\' ESCAPE '\'",
        // Django's exact lookup shapes.
        r"s LIKE '%foo%' ESCAPE '\'",
        r"s LIKE '%\%foo%' ESCAPE '\'",
        r"s LIKE '100\%%' ESCAPE '\'",
    ];
    for p in preds {
        cross_check(&db, &format!("SELECT id, {p} FROM t ORDER BY id"));
        cross_check(&db, &format!("SELECT id FROM t WHERE {p} ORDER BY id"));
    }

    // A NULL subject stays NULL through LIKE … ESCAPE and through NOT LIKE.
    cross_check(
        &db,
        r"SELECT typeof(NULL LIKE 'a\b' ESCAPE '\'), typeof(NULL NOT LIKE 'a\b' ESCAPE '\')",
    );

    // Escaped-vs-unescaped is a REAL difference, not a no-op: `\%` must not
    // match the rows a bare `%` does.
    let esc = mpedb_rows(&db, r"SELECT id FROM t WHERE s LIKE '100\%' ESCAPE '\'", &[]);
    let bare = mpedb_rows(&db, "SELECT id FROM t WHERE s LIKE '100%'", &[]);
    assert_eq!(esc, vec!["8".to_string()]);
    assert_eq!(bare, vec!["8".to_string(), "9".to_string()]);

    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// Django's exact statement shape: the pattern is a BOUND parameter and the
/// escape is the literal `'\'`.
///
/// mpedb refuses a parameterised LIKE pattern (`LIKE pattern must be a literal
/// in Phase 1`) — a pre-existing, separate limit that ESCAPE does not change.
/// The refusal must still name the PATTERN, not the escape clause: an ORM user
/// reading it has to be sent to the right half of the statement.
#[test]
fn a_bound_pattern_is_refused_by_name_not_blamed_on_escape() {
    let (db, path) = open(None);
    let e = db
        .query(r"SELECT id FROM t WHERE s LIKE ? ESCAPE '\'", &[Value::Text("%foo%".into())])
        .expect_err("a bound LIKE pattern is not supported");
    let m = e.to_string();
    assert!(
        m.contains("pattern") && !m.contains("ESCAPE"),
        "the refusal must name the PATTERN, not ESCAPE: {m}"
    );
    // The same statement with a literal pattern — Django's value inlined — runs.
    assert_eq!(
        mpedb_rows(&db, r"SELECT id FROM t WHERE s LIKE '%foo%' ESCAPE '\' ORDER BY id", &[]),
        vec!["10".to_string(), "11".to_string()]
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The ESCAPE argument itself. sqlite raises `ESCAPE expression must be a
/// single character` at STEP time for an empty or multi-character argument;
/// mpedb refuses at PREPARE, by name. sqlite's coercions (`ESCAPE 5` ≡
/// `ESCAPE '5'`) and its acceptance of an arbitrary expression are DELIBERATE
/// divergences — refused cleanly rather than approximated.
#[test]
fn a_bad_escape_argument_is_refused_by_name() {
    let (db, path) = open(None);
    for sql in [
        "SELECT id FROM t WHERE s LIKE 'a' ESCAPE ''",
        "SELECT id FROM t WHERE s LIKE 'a' ESCAPE 'ab'",
        "SELECT id FROM t WHERE s LIKE 'a' ESCAPE 5",
        "SELECT id FROM t WHERE s LIKE 'a' ESCAPE NULL",
        "SELECT id FROM t WHERE s LIKE 'a' ESCAPE s",
    ] {
        let m = db.query(sql, &[]).expect_err(sql).to_string();
        assert!(m.contains("ESCAPE"), "the refusal must name ESCAPE: {m}\n  for {sql}");
    }
    // sqlite agrees that the first two are errors (it just raises later).
    if sqlite_available() {
        for pat in ["''", "'ab'"] {
            let out = Command::new("sqlite3")
                .args([":memory:", &format!("SELECT 'x' LIKE 'x' ESCAPE {pat}")])
                .output()
                .expect("spawn sqlite3");
            assert!(!out.status.success(), "sqlite must reject ESCAPE {pat}");
        }
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The PostgreSQL dialect compiles `Instr::LikeCsEsc`: same escape rules, no
/// ASCII case folding. Pinned directly (sqlite has no case-sensitive LIKE), and
/// the case-agnostic rows are asserted to agree with the sqlite dialect too —
/// so a bug in the shared matcher shows up as a disagreement between the two.
#[test]
fn like_escape_is_case_sensitive_under_the_postgres_dialect() {
    let (pg, pg_path) = open(Some("postgres"));
    let (lite, lite_path) = open(None);

    let ids = |db: &Database, p: &str| mpedb_rows(db, &format!("SELECT id FROM t WHERE {p} ORDER BY id"), &[]);

    // `a\b` = literal "ab". The sqlite dialect also matches row 5 ("AB").
    assert_eq!(ids(&pg, r"s LIKE 'a\b' ESCAPE '\'"), vec!["1".to_string()]);
    assert_eq!(ids(&lite, r"s LIKE 'a\b' ESCAPE '\'"), vec!["1".to_string(), "5".to_string()]);
    // `A\B` = literal "AB": only the upper-case row under postgres.
    assert_eq!(ids(&pg, r"s LIKE 'A\B' ESCAPE '\'"), vec!["5".to_string()]);

    // Case-agnostic subjects: both dialects must agree, character for
    // character, on every escape rule.
    for p in [
        r"s LIKE '100\%' ESCAPE '\'",
        r"s LIKE 'ab\' ESCAPE '\'",
        r"s LIKE '\_' ESCAPE '\'",
        "s LIKE 'a%%b' ESCAPE '%'",
        "s LIKE '__' ESCAPE '_'",
        r"s LIKE '%\%foo%' ESCAPE '\'",
    ] {
        assert_eq!(ids(&pg, p), ids(&lite, p), "dialects disagree on `{p}`");
    }

    drop(pg);
    drop(lite);
    let _ = std::fs::remove_file(&pg_path);
    let _ = std::fs::remove_file(&lite_path);
}
