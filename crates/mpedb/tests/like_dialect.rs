//! Config-selectable LIKE dialect (`[compat] bare_group_by`, COMPAT.md).
//!
//! LIKE strictness rides the SAME compat dialect signal as GROUP BY (#87): the
//! sqlite default is lenient (case-INsensitive for ASCII, and a numeric operand
//! is coerced to text), while `bare_group_by = "postgres"` — the dialect a PG
//! `mirror import` produces — is strict (case-SENSITIVE, and a numeric operand
//! is refused). The dialect is baked into the compiled plan: sqlite emits
//! `Instr::Like`, postgres emits `Instr::LikeCs`.
//!
//! This differential test opens one database of each dialect, loads mixed-case
//! text plus a numeric column, and pins the four behaviors from the spec.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

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

/// (id, text). `id` is the numeric operand exercised by `id LIKE '1%'`; `s`
/// carries the mixed-case values that separate case-sensitive from -insensitive.
const DATA: &[(i64, &str)] = &[
    (1, "Ab"),   // upper A: matched by sqlite `LIKE 'ab%'`, NOT by postgres
    (2, "ab"),   // lower: matched by both dialects
    (3, "xyz"),  // never matched by `LIKE 'ab%'`
    (10, "abc"), // id starts with '1' (coercion), s matched by both dialects
];

fn open(name: &str, compat: Option<&str>) -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-like-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let compat_section = match compat {
        Some(mode) => format!("\n[compat]\nbare_group_by = \"{mode}\"\n"),
        None => String::new(),
    };
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 16\n{}{}",
        path.display(),
        compat_section,
        SCHEMA
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for (id, s) in DATA {
        db.query(&format!("INSERT INTO t (id, s) VALUES ({id}, '{s}')"), &[])
            .unwrap();
    }
    (db, path)
}

/// The set of `id`s a query returns, sorted.
fn ids(db: &Database, query: &str) -> Vec<i64> {
    match db.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            let mut out: Vec<i64> = rows
                .iter()
                .map(|r| match &r[0] {
                    Value::Int(i) => *i,
                    other => panic!("expected int id, got {other:?}"),
                })
                .collect();
            out.sort_unstable();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn sqlite_mode_like_is_case_insensitive_and_coerces_numeric() {
    let (db, path) = open("sqlite", Some("sqlite"));
    // Case-INsensitive: 'ab%' matches 'Ab' (1), 'ab' (2) and 'abc' (10).
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE s LIKE 'ab%' ORDER BY id"),
        vec![1, 2, 10]
    );
    // A numeric operand is coerced to text, so `id LIKE '1%'` matches 1 and 10.
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE id LIKE '1%' ORDER BY id"),
        vec![1, 10]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn postgres_mode_like_is_case_sensitive() {
    let (db, path) = open("pg-case", Some("postgres"));
    // Case-SENSITIVE: 'ab%' matches 'ab' (2) and 'abc' (10) but NOT 'Ab' (1).
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE s LIKE 'ab%' ORDER BY id"),
        vec![2, 10]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn postgres_mode_like_refuses_numeric_operand() {
    let (db, path) = open("pg-rigid", Some("postgres"));
    // Rigid: a numeric operand is refused rather than silently stringified.
    let err = db
        .query("SELECT id FROM t WHERE id LIKE '1%'", &[])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("requires text"),
        "postgres mode should refuse a numeric LIKE operand, got: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn config_default_is_sqlite_like() {
    // No [compat] section → sqlite LIKE: case-insensitive and coercing.
    let (db, path) = open("default", None);
    assert_eq!(
        ids(&db, "SELECT id FROM t WHERE s LIKE 'ab%' ORDER BY id"),
        vec![1, 2, 10]
    );
    assert!(db.query("SELECT id FROM t WHERE id LIKE '1%'", &[]).is_ok());
    let _ = std::fs::remove_file(path);
}
