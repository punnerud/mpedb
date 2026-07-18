//! New scalar string functions (`replace`, `ltrim`, `rtrim`, `instr`), each
//! value cross-checked against sqlite 3.45. NULL propagates (any NULL arg →
//! NULL); `replace` with an empty search string is a no-op; `instr` is 1-based
//! and 0 when absent (1 for an empty needle).

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn db() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-scalar-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    // One tiny table; the functions are exercised over a FROM-less SELECT.
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
"#,
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn one(db: &Database, sql: &str) -> Value {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows.into_iter().next().unwrap().into_iter().next().unwrap(),
        other => panic!("{other:?}"),
    }
}

fn txt(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn replace_ltrim_rtrim_instr_match_sqlite() {
    let (db, path) = db();

    // replace: every occurrence; empty search string is a no-op (sqlite's rule).
    assert_eq!(one(&db, "SELECT replace('hello world', 'o', '0')"), txt("hell0 w0rld"));
    assert_eq!(one(&db, "SELECT replace('abc', '', 'X')"), txt("abc"));

    // ltrim / rtrim: whitespace by default, or a set of characters.
    assert_eq!(one(&db, "SELECT ltrim('   hi  ')"), txt("hi  "));
    assert_eq!(one(&db, "SELECT rtrim('   hi  ')"), txt("   hi"));
    assert_eq!(one(&db, "SELECT ltrim('xxabcxx', 'x')"), txt("abcxx"));
    assert_eq!(one(&db, "SELECT rtrim('xxabcxx', 'x')"), txt("xxabc"));

    // instr: 1-based, 0 when absent, 1 for an empty needle.
    assert_eq!(one(&db, "SELECT instr('hello', 'll')"), Value::Int(3));
    assert_eq!(one(&db, "SELECT instr('hello', 'z')"), Value::Int(0));
    assert_eq!(one(&db, "SELECT instr('hello', '')"), Value::Int(1));

    // NULL propagates through every one.
    assert_eq!(one(&db, "SELECT replace('a', 'a', NULL)"), Value::Null);
    assert_eq!(one(&db, "SELECT instr(NULL, 'x')"), Value::Null);
    assert_eq!(one(&db, "SELECT ltrim(NULL)"), Value::Null);

    // Character-based, not byte-based (consistent with length()).
    assert_eq!(one(&db, "SELECT instr('æøå', 'å')"), Value::Int(3));

    // Arity errors are compile errors.
    assert!(db.query("SELECT replace('a', 'b')", &[]).is_err());
    assert!(db.query("SELECT instr('a')", &[]).is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sqrt_pow_sign_match_sqlite() {
    let (db, path) = db();

    // sqrt / pow: always float; a non-real result (sqrt of a negative, a
    // fractional power of a negative base) is NULL, matching sqlite.
    assert_eq!(one(&db, "SELECT sqrt(9.0)"), Value::Float(3.0));
    assert_eq!(one(&db, "SELECT sqrt(9)"), Value::Float(3.0)); // int arg → float out
    assert_eq!(one(&db, "SELECT sqrt(-1)"), Value::Null);
    assert_eq!(one(&db, "SELECT pow(2, 10)"), Value::Float(1024.0));
    assert_eq!(one(&db, "SELECT pow(2, -1)"), Value::Float(0.5));
    assert_eq!(one(&db, "SELECT pow(-1, 0.5)"), Value::Null);
    assert_eq!(one(&db, "SELECT power(3, 2)"), Value::Float(9.0)); // alias

    // sign: always an integer, -1 / 0 / 1.
    assert_eq!(one(&db, "SELECT sign(-4)"), Value::Int(-1));
    assert_eq!(one(&db, "SELECT sign(0)"), Value::Int(0));
    assert_eq!(one(&db, "SELECT sign(2.5)"), Value::Int(1));
    assert_eq!(one(&db, "SELECT sign(-0.0)"), Value::Int(0));

    // NULL propagates; a non-number is a compile/runtime error.
    assert_eq!(one(&db, "SELECT sqrt(NULL)"), Value::Null);
    assert!(db.query("SELECT sqrt('x')", &[]).is_err());

    // ceil/floor preserve the argument's type: int stays int, float rounds.
    assert_eq!(one(&db, "SELECT ceil(5)"), Value::Int(5));
    assert_eq!(one(&db, "SELECT ceil(1.2)"), Value::Float(2.0));
    assert_eq!(one(&db, "SELECT ceiling(1.2)"), Value::Float(2.0)); // alias
    assert_eq!(one(&db, "SELECT floor(-1.5)"), Value::Float(-2.0));
    assert_eq!(one(&db, "SELECT floor(9)"), Value::Int(9));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn scalars_compose_and_filter_over_rows() {
    let (db, path) = db();
    for id in 1..=3 {
        db.query(&format!("INSERT INTO t (id) VALUES ({id})"), &[]).unwrap();
    }
    // Composed over real rows: build a padded label, trim it, find a marker.
    let res = db
        .query(
            "SELECT id, instr(rtrim(replace('a-b-x   ', '-', '_')), 'x') FROM t \
             WHERE id = 2",
            &[],
        )
        .unwrap();
    match res {
        ExecResult::Rows { rows, .. } => {
            // replace → 'a_b_x   ', rtrim → 'a_b_x', instr(..,'x') → 5
            assert_eq!(rows, vec![vec![Value::Int(2), Value::Int(5)]]);
        }
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_file(&path);
}
