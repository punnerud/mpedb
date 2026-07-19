//! #74 item 3, LIKE/GLOB half: NON-LITERAL patterns — bound parameters
//! (Django's wire shape), per-row COLUMN patterns, computed patterns — and
//! the `likeFunc` runtime operand rule they expose, all differential against
//! the bundled sqlite oracle.
//!
//! The rules being pinned were READ OFF the binary (3.45.1), not inferred:
//!
//! - a BLOB on either side of LIKE or GLOB yields FALSE, and the check comes
//!   BEFORE the NULL rule (`NULL LIKE x'41'` is 0, not NULL; `NOT LIKE` over
//!   it is 1);
//! - then NULL propagates;
//! - then a numeric renders as its sqlite text at runtime (`'12' LIKE 12` is
//!   1, `'12' LIKE 12.0` is 0 — `12.0` renders as `'12.0'`);
//! - a DANGLING escape in a runtime pattern matches nothing — a legal
//!   answer, not an error (unlike REGEXP, where an uncompilable pattern is a
//!   named error: sqlite's own `patternCompare` returns NOMATCH here).
//!
//! The `any` column `p` is what makes the runtime rule REACHABLE with typed
//! parameters: a bound parameter is pinned to text (a non-text bind refuses
//! by name, see like_escape.rs), but a column can deliver a blob or a number
//! to the opcode per row — which is also why the binder no longer wraps an
//! `any` LIKE/GLOB operand in a bind-time CAST (the CAST turned a runtime
//! blob into text and MATCHED it, where sqlite answers FALSE).

use mpedb::{params, Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// (id, s TEXT, p ANY) — `p` is the per-row pattern (and sometimes the
/// subject). Chosen so every likeFunc arm fires at least once: matching and
/// non-matching text patterns, case-folding, escape-relevant `%`, numeric
/// patterns of both flavors, NULLs on each side, blobs against text AND
/// against NULL, a GLOB-flavored pattern, and a dangling escape.
const DATA: &[(i64, Option<&str>, SqlAny)] = &[
    (1, Some("abc"), SqlAny::T("a%")),
    (2, Some("abc"), SqlAny::T("b%")),
    (3, Some("ABC"), SqlAny::T("a_c")),
    (4, Some("a%c"), SqlAny::T(r"a\%c")),
    (5, Some("12"), SqlAny::I(12)),
    (6, Some("12.0"), SqlAny::F(12.0)),
    (7, Some("12"), SqlAny::F(12.0)),
    (8, None, SqlAny::T("a")),
    (9, Some("a"), SqlAny::Null),
    (10, None, SqlAny::B(&[0x61, 0x25])),
    (11, Some("a%"), SqlAny::B(&[0x61, 0x25])),
    (12, Some("xyz"), SqlAny::T("x*z")),
    (13, Some(""), SqlAny::T("%")),
    (14, Some(r"ab\"), SqlAny::T(r"ab\")),
    (15, Some("axc"), SqlAny::T("a_c")),
    (16, Some("100%"), SqlAny::T(r"100\%")),
];

/// A value that can be written both as an mpedb param and a sqlite literal.
enum SqlAny {
    T(&'static str),
    I(i64),
    F(f64),
    B(&'static [u8]),
    Null,
}

impl SqlAny {
    fn mpedb(&self) -> Value {
        match self {
            SqlAny::T(s) => Value::Text((*s).into()),
            SqlAny::I(i) => Value::Int(*i),
            SqlAny::F(f) => Value::Float(*f),
            SqlAny::B(b) => Value::Blob(b.to_vec()),
            SqlAny::Null => Value::Null,
        }
    }
    fn sqlite(&self) -> String {
        match self {
            SqlAny::T(s) => format!("'{}'", s.replace('\'', "''")),
            SqlAny::I(i) => i.to_string(),
            SqlAny::F(f) => format!("{f:?}"), // 12.0 → "12.0"
            SqlAny::B(b) => {
                let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                format!("x'{hex}'")
            }
            SqlAny::Null => "NULL".into(),
        }
    }
}

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
  nullable = true
  [[table.column]]
  name = "p"
  type = "any"
  nullable = true
"#;

fn open(compat: Option<&str>) -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-likebound-{}-{}.mpedb",
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
    let ins = db.prepare("INSERT INTO t (id, s, p) VALUES ($1, $2, $3)").unwrap();
    for (id, s, p) in DATA {
        let s = match s {
            Some(s) => Value::Text((*s).into()),
            None => Value::Null,
        };
        db.execute(&ins, &params![*id, s, p.mpedb()]).unwrap();
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
    let mut input = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, p);\n");
    for (id, s, p) in DATA {
        let s = match s {
            Some(s) => format!("'{}'", s.replace('\'', "''")),
            None => "NULL".into(),
        };
        input.push_str(&format!("INSERT INTO t VALUES ({id}, {s}, {});\n", p.sqlite()));
    }
    input.push_str(query);
    input.push_str(";\n");
    sqlite_oracle::script_stdout(&input, "NULL")
        .lines()
        .map(|l| l.to_string())
        .collect()
}

fn cross_check(db: &Database, query: &str) {
    let m = mpedb_rows(db, query, &[]);
    let s = sqlite_rows(query);
    assert_eq!(m, s, "mpedb vs sqlite3 disagree on `{query}`");
}

/// The pattern is a COLUMN, evaluated per row — two different text patterns
/// on adjacent rows (ids 1/2) prove the one-slot memo re-keys correctly, and
/// the mixed-type rows drive every likeFunc arm: numeric patterns (5/6/7),
/// NULL on either side (8/9), a blob against NULL (10 — FALSE, not NULL) and
/// against text (11), a dangling escape arriving at runtime (14, under
/// ESCAPE).
#[test]
fn per_row_column_pattern_matches_sqlite() {
    let (db, path) = open(None);
    for q in [
        "SELECT id, s LIKE p, s NOT LIKE p FROM t ORDER BY id",
        r"SELECT id, s LIKE p ESCAPE '\', s NOT LIKE p ESCAPE '\' FROM t ORDER BY id",
        "SELECT id, s GLOB p, s NOT GLOB p FROM t ORDER BY id",
        "SELECT id FROM t WHERE s LIKE p ORDER BY id",
        r"SELECT id FROM t WHERE s LIKE p ESCAPE '\' ORDER BY id",
        "SELECT id FROM t WHERE s GLOB p ORDER BY id",
    ] {
        cross_check(&db, q);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The `any` column as the SUBJECT, against literal patterns — the corner the
/// old bind-time CAST got WRONG: a runtime blob subject was cast to text and
/// matched, where sqlite's likeFunc answers FALSE (row 10/11: `p` holds
/// x'6125' = the bytes of "a%", and `p LIKE 'a%'` must be 0, not 1). Numeric
/// subjects coerce (`p LIKE '1%'` is 1 for 12 and 12.0).
#[test]
fn any_subject_follows_the_blob_and_coercion_rules() {
    let (db, path) = open(None);
    for q in [
        "SELECT id, p LIKE 'a%', p NOT LIKE 'a%' FROM t ORDER BY id",
        "SELECT id, p LIKE '1%', p LIKE '12._' FROM t ORDER BY id",
        "SELECT id, p GLOB '1*', p GLOB 'a*' FROM t ORDER BY id",
        r"SELECT id, p LIKE 'a\%' ESCAPE '\' FROM t ORDER BY id",
        // both sides `any`: blob LIKE blob is still FALSE, numerics self-match
        "SELECT id, p LIKE p, p GLOB p FROM t ORDER BY id",
        // the same subjects through the DYN opcodes (pattern is a column too)
        "SELECT id, p LIKE s FROM t ORDER BY id",
    ] {
        cross_check(&db, q);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// Constant NON-TEXT patterns: a numeric literal pattern now binds and gives
/// sqlite's answer (it used to refuse with "must be a string literal").
/// A computed CONSTANT text pattern folds and rejoins the literal opcode; a
/// computed PER-ROW pattern stays dynamic — both must match sqlite.
#[test]
fn constant_and_computed_patterns_match_sqlite() {
    let (db, path) = open(None);
    for q in [
        "SELECT id, s LIKE 12, s NOT LIKE 12 FROM t ORDER BY id",
        "SELECT id, s LIKE 12.0 FROM t ORDER BY id",
        "SELECT id, s GLOB 12 FROM t ORDER BY id",
        // folds to the literal 'a%' at bind — the const-pool opcode
        "SELECT id, s LIKE ('a' || '%') FROM t ORDER BY id",
        // per-row computed pattern: substr(s,1,1) || '%' differs row to row
        "SELECT id, s LIKE (substr(s, 1, 1) || '%') FROM t ORDER BY id",
        r"SELECT id, s LIKE (substr(s, 1, 1) || '%') ESCAPE '\' FROM t ORDER BY id",
    ] {
        cross_check(&db, q);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// GLOB with a BOUND pattern — the same restriction existed for GLOB and is
/// closed in the same style: the bound form must agree with the literal form
/// row-for-row, and the literal form with sqlite first.
#[test]
fn bound_glob_matches_the_literal_form() {
    let (db, path) = open(None);
    let patterns =
        ["a*", "*c", "a?c", "[ax]*", "[^a]*", "x[!*]z", "*", "", "abc", "A*", "1*", "12.*"];
    for pat in patterns {
        let lit = format!("SELECT id, s GLOB '{pat}', s NOT GLOB '{pat}' FROM t ORDER BY id");
        let literal_rows = mpedb_rows(&db, &lit, &[]);
        assert_eq!(literal_rows, sqlite_rows(&lit), "literal GLOB diverged for `{pat}`");
        let bound = mpedb_rows(
            &db,
            "SELECT id, s GLOB ?, s NOT GLOB ? FROM t ORDER BY id",
            &[Value::Text(pat.into()), Value::Text(pat.into())],
        );
        assert_eq!(bound, literal_rows, "bound GLOB diverged for `{pat}`");
    }
    // NULL pattern propagates; non-text binds refuse by name (pinned to text).
    assert_eq!(
        mpedb_rows(&db, "SELECT s GLOB ? FROM t WHERE id = 1", &[Value::Null]),
        vec!["NULL".to_string()]
    );
    let m = db
        .query("SELECT s GLOB ? FROM t WHERE id = 1", &[Value::Int(1)])
        .expect_err("a non-text GLOB pattern bind must refuse")
        .to_string();
    assert!(m.contains("statement requires text"), "{m}");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The PostgreSQL dialect stays RIGID for the dyn pattern exactly as for the
/// subject: an `any` or numeric pattern is a bind refusal naming the PATTERN
/// half, a bound TEXT pattern works and is case-sensitive.
#[test]
fn postgres_dialect_refuses_nontext_patterns_by_name() {
    let (db, path) = open(Some("postgres"));
    for (q, want) in [
        ("SELECT s LIKE p FROM t", "LIKE pattern requires text"),
        ("SELECT s LIKE 12 FROM t", "LIKE pattern requires text"),
        ("SELECT s GLOB p FROM t", "GLOB pattern requires text"),
    ] {
        let m = db.query(q, &[]).expect_err(q).to_string();
        assert!(m.contains(want), "`{q}` must refuse naming the pattern: {m}");
    }
    // Bound text works, case-SENSITIVELY: every lowercase-`a`-initial row
    // matches (1, 2, 4 'a%c', 9 'a', 11 'a%', 14 'ab\', 15 'axc') — and
    // 'ABC' (3) does NOT, which is the dialect's whole point.
    assert_eq!(
        mpedb_rows(
            &db,
            "SELECT id FROM t WHERE s LIKE ? ORDER BY id",
            &[Value::Text("a%".into())]
        ),
        ["1", "2", "4", "9", "11", "14", "15"].map(String::from).to_vec()
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}
