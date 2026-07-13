//! Minimized reproductions of REAL engine bugs found by this testkit.
//! Each test asserts the CORRECT (SQL-standard, sqlite-confirmed) behavior,
//! is `#[ignore]`d while the bug exists, and doubles as a ready-made
//! regression test: when the engine is fixed, un-ignore it (and update the
//! corpus records that pin the buggy behavior — they are cross-referenced).

use mpedb::{Config, Database, ExecResult, Value};

/// ENGINE BUG (mpedb-types `expr::like_match`): the two-pointer loop tries
/// the literal/`_` branch BEFORE the `%`-wildcard branch, so when the
/// SUBJECT string contains a literal `%` at the position where the PATTERN
/// has `%`, the wildcard is consumed as a ONE-CHARACTER literal match
/// instead of starting a wildcard run, and no backtrack point is recorded
/// for it.
///
/// Minimal reproductions (sqlite 3.45 says TRUE for all three):
///
/// ```text
/// '%%'  LIKE '%'    -- mpedb: FALSE   correct: TRUE
/// 'a%c' LIKE 'a%'   -- mpedb: FALSE   correct: TRUE
/// 'x%'  LIKE 'x%'   -- mpedb: TRUE  (matches by accident: both end together)
/// ```
///
/// The fix is to check `p[pi] == '%'` before the literal-equality branch in
/// `like_match`. Found by the sqllogictest corpus
/// (`tests/slt/like_patterns.test`, which pins the CURRENT buggy output for
/// the two affected records — update those together with this test).
#[test]
fn engine_bug_like_percent_wildcard_consumed_as_literal() {
    let dir = mpedb_testkit::TempDir::new("engine-bug-like").unwrap();
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 8

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "s"
  type = "text"
"#,
        dir.db_path("bug").display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    db.query("INSERT INTO t (id, s) VALUES (1, '%%'), (2, 'a%c')", &[])
        .unwrap();

    let ids = |sql: &str| -> Vec<i64> {
        match db.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| match &r[0] {
                    Value::Int(i) => *i,
                    other => panic!("expected int id, got {other:?}"),
                })
                .collect(),
            other => panic!("expected rows, got {other:?}"),
        }
    };

    // '%' matches ANY string, including one made of percent signs.
    assert_eq!(
        ids("SELECT id FROM t WHERE s LIKE '%'"),
        vec![1, 2],
        "'%' must match every non-NULL string"
    );
    // 'a%' matches every string starting with 'a' -- including 'a%c'.
    assert_eq!(
        ids("SELECT id FROM t WHERE s LIKE 'a%'"),
        vec![2],
        "'a%' must match 'a%c'"
    );
}
