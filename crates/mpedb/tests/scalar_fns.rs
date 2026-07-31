//! New scalar string functions (`replace`, `ltrim`, `rtrim`, `instr`), each
//! value cross-checked against sqlite 3.45. NULL propagates (any NULL arg →
//! NULL); `replace` with an empty search string is a no-op; `instr` is 1-based
//! and 0 when absent (1 for an empty needle).

use mpedb::{Config, Database, ExecResult, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn db() -> (Database, PathBuf) {
    let dir = mpedb_testkit::scratch_base();
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

/// `substr(X, Y[, Z])` — the two rules that were WRONG ANSWERS, not refusals.
///
/// A NEGATIVE START counts from the END (`substr('abcdef', -2)` is 'ef'); the
/// sign was ignored and the whole string came back. A NEGATIVE LENGTH takes
/// the |Z| characters BEFORE the start (`substr('abcdef', 2, -1)` is 'a'); the
/// empty string came back. Django compiles `Right(x, n)` to exactly
/// `SUBSTR(x, -n)`, which is how it surfaced.
///
/// Every value here is from a 105-combination sweep run against sqlite 3.45
/// BEFORE the rewrite. The clamp is the third rule and the least obvious: a
/// window starting left of position 1 is TRUNCATED there rather than sliding
/// right, so `substr('abcdef', 0, 2)` is 'a' and not 'ab'.
#[test]
fn substr_negative_start_and_length_match_sqlite() {
    let (db, path) = db();
    let cases: &[(&str, &str)] = &[
        // Negative start: from the end, 1-based, clamped at the front.
        ("SELECT substr('abcdef', -1)", "f"),
        ("SELECT substr('abcdef', -2)", "ef"),
        ("SELECT substr('abcdef', -5)", "bcdef"),
        ("SELECT substr('abcdef', -6)", "abcdef"),
        ("SELECT substr('abcdef', -8)", "abcdef"),
        ("SELECT substr('abc', -2)", "bc"),
        ("SELECT substr('', -2)", ""),
        // Zero and positive starts are unchanged.
        ("SELECT substr('abcdef', 0)", "abcdef"),
        ("SELECT substr('abcdef', 1)", "abcdef"),
        ("SELECT substr('abcdef', 3)", "cdef"),
        ("SELECT substr('abcdef', 7)", ""),
        // Negative LENGTH: the window moves left of the start.
        ("SELECT substr('abcdef', 2, -1)", "a"),
        ("SELECT substr('abcdef', 4, -2)", "bc"),
        ("SELECT substr('abcdef', -2, -1)", "d"),
        ("SELECT substr('abcdef', 1, -3)", ""),
        // The clamp: what falls left of position 1 is LOST, not shifted. These
        // two are the ones a hand-derived expectation gets wrong (`-8, 3` is
        // 'a', not '') — every value in this list came off a sweep against
        // sqlite, including after the engine was already passing it.
        ("SELECT substr('abcdef', 0, 2)", "a"),
        ("SELECT substr('abcdef', -8, 3)", "a"),
        ("SELECT substr('abcdef', -7, 3)", "ab"),
        // Both together, and the alias.
        ("SELECT substr('abcdef', -3, 2)", "de"),
        ("SELECT substr('abcdef', -3, 5)", "def"),
        ("SELECT substring('abcdef', -2)", "ef"),
        ("SELECT substring('abcdef', 2, -1)", "a"),
        // CHARACTERS, not bytes — the negative start counts codepoints.
        ("SELECT substr('Ж日本語', -2)", "本語"),
        ("SELECT substr('Ж日本語', -3, 2)", "日本"),
    ];
    for (sql, want) in cases {
        assert_eq!(one(&db, sql), txt(want), "{sql}");
    }
    // NULL in any argument propagates.
    for sql in [
        "SELECT substr(NULL, 1)",
        "SELECT substr('abc', NULL)",
        "SELECT substr('abc', 1, NULL)",
    ] {
        assert_eq!(one(&db, sql), Value::Null, "{sql}");
    }
    // A length far past the end is just "to the end". sqlite's own answer
    // DEGENERATES above 2^31 — 'cdef' up to 1e9, then 'ab' at 2^31, '' at
    // 2^62, 'b' at i64::MAX — which is 32-bit overflow inside its C, not a
    // rule. mpedb computes in i128 and keeps the arithmetic answer; the two
    // agree for every value a caller would actually pass.
    assert_eq!(one(&db, "SELECT substr('abcdef', 3, 1000000000)"), txt("cdef"));
    assert_eq!(one(&db, "SELECT substr('abcdef', 3, 9223372036854775807)"), txt("cdef"));

    drop(db);
    let _ = std::fs::remove_file(&path);
}
