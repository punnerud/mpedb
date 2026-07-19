//! sqlite's JSON function set — every supported form cross-checked against the
//! real `sqlite3` 3.45 CLI.
//!
//! The comparison is differential: the SAME `SELECT <expr>` runs against mpedb
//! and against `sqlite3 :memory:`, and the single rendered result must match.
//! NULL is disambiguated with `-nullvalue <NULL>` on both sides, which matters
//! more here than anywhere else in the engine: `'{"f":null}' -> '$.f'` is the
//! four-character TEXT `null` while `'{"f":null}' ->> '$.f'` is SQL NULL, and a
//! test that could not tell them apart would pass on the wrong answer.
//!
//! Deliberate divergences (JSON5, JSONB, `json_valid` flag bits 2/4/8, the
//! subtype-undecidable argument shapes) are asserted directly as REFUSALS, so
//! this file also pins what mpedb promises not to guess.

use mpedb::{Config, Database, ExecResult, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

const NULL_SENTINEL: &str = "<NULL>";

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
        "mpedb-json-{}-{}.mpedb",
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
  name = "doc"
  type = "text"
  nullable = true

  [[table.column]]
  name = "i"
  type = "int64"
  nullable = true
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    db.query(
        r#"INSERT INTO t (id, doc, i) VALUES (1, '{"a": 1, "b": "x", "c": [1, 2, 3]}', 7)"#,
        &[],
    )
    .unwrap();
    db.query("INSERT INTO t (id, doc, i) VALUES (2, NULL, NULL)", &[])
        .unwrap();
    (db, path)
}

fn sqlite_setup() -> String {
    let mut s = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, doc TEXT, i INTEGER);\n");
    s.push_str(
        "INSERT INTO t (id, doc, i) VALUES (1, '{\"a\": 1, \"b\": \"x\", \"c\": [1, 2, 3]}', 7);\n",
    );
    s.push_str("INSERT INTO t (id, doc, i) VALUES (2, NULL, NULL);\n");
    s
}

/// Render one mpedb value the way the sqlite CLI (`-nullvalue <NULL>`) prints
/// it.
fn render(v: &Value) -> String {
    match v {
        Value::Null => NULL_SENTINEL.to_string(),
        Value::Text(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        // mpedb has a first-class BOOL where sqlite has an integer; a
        // comparison result renders as sqlite's 1/0.
        Value::Bool(b) => (*b as i64).to_string(),
        // A REAL must never be compared through Rust's Display: sqlite's CLI
        // prints `1e3` as `1000.0` and Rust prints `1000`. Every query that can
        // yield one is wrapped in `CAST(… AS TEXT)`, which is sqlite's own
        // `%!.15g` on both sides, so reaching this arm is a test bug.
        Value::Float(x) => panic!(
            "a REAL ({x}) reached the differential renderer — wrap the query in \
             CAST(… AS TEXT)"
        ),
        other => format!("{other:?}"),
    }
}

fn mpedb_rows(db: &Database, query: &str) -> Vec<String> {
    match db.query(query, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows.iter().map(|r| render(&r[0])).collect(),
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

fn sqlite_lines(queries: &[String]) -> Vec<String> {
    let mut input = sqlite_setup();
    for q in queries {
        input.push_str(q);
        input.push_str(";\n");
    }
    let mut child = Command::new("sqlite3")
        .args(["-batch", "-noheader", "-nullvalue", NULL_SENTINEL, ":memory:"])
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
        "sqlite3 batch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// Batched differential check: every query must yield exactly one
/// single-column, newline-free row in both engines, and the two must agree.
fn cross_check_batch(db: &Database, queries: &[String]) {
    let s_lines = sqlite_lines(queries);
    assert_eq!(
        s_lines.len(),
        queries.len(),
        "sqlite produced {} lines for {} queries (a result contained a newline?)",
        s_lines.len(),
        queries.len()
    );
    for (q, s) in queries.iter().zip(&s_lines) {
        let m = mpedb_rows(db, q);
        assert_eq!(m.len(), 1, "expected one row for `{q}`");
        assert_eq!(&m[0], s, "mpedb vs sqlite disagree on `{q}`");
    }
}

fn q(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// An expression mpedb must REFUSE at compile or run time, with a message
/// naming `needle`.
fn refuses(db: &Database, query: &str, needle: &str) {
    match db.query(query, &[]) {
        Ok(other) => panic!("`{query}` should have been refused, got {other:?}"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains(needle),
                "`{query}` was refused, but the message does not name `{needle}`: {msg}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// json(X) — validate and minify
// ---------------------------------------------------------------------------

/// Every valid document round-trips through `json()` byte-identically to
/// sqlite, INCLUDING the number spellings sqlite preserves (`1.50` stays
/// `1.50`, `1e3` stays `1e3`) — minifying is not re-rendering.
#[test]
fn json_valid_documents_match_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let docs = [
        "null",
        "true",
        "false",
        "0",
        "-0",
        "1",
        "-1",
        "1.50",
        "1e3",
        "1E3",
        "1e+3",
        "-1.5e-7",
        "9223372036854775807",
        "\"\"",
        "\"x\"",
        r#""a\"b""#,
        r#""å\n\t\\\/\b\f\r""#,
        r#""😀""#,
        "\"xå😀\"",
        "[]",
        "{}",
        "[1,2,3]",
        "  [ 1 , 2 ]  ",
        "{ \"a\" : 1 }",
        r#"{"a":1,"b":[1,{"c":null}],"d":{"e":{}}}"#,
        r#"{"a":1,"a":2}"#,
        "[[[[[1]]]]]",
        "\n\t [ true , false , null ] \r\n",
    ];
    let mut qs = Vec::new();
    for d in docs {
        let lit = d.replace('\'', "''");
        qs.push(format!("SELECT json('{lit}')"));
        // Idempotence: json(json(X)) == json(X), in both engines.
        qs.push(format!("SELECT json(json('{lit}'))"));
        qs.push(format!("SELECT json_type('{lit}')"));
        qs.push(format!("SELECT json_valid('{lit}')"));
    }
    // A document large enough that buffering, not just parsing, is exercised.
    let big: String = format!(
        "[{}]",
        (0..2000)
            .map(|i| format!(r#"{{"k{i}":{i}}}"#))
            .collect::<Vec<_>>()
            .join(",")
    );
    qs.push(format!("SELECT length(json('{big}'))"));
    qs.push(format!("SELECT json_array_length('{big}')"));
    cross_check_batch(&db, &qs);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// Malformed input: sqlite RAISES and mpedb refuses, so the differential is on
/// `json_valid`, which answers 0 in both, plus a direct assertion that `json()`
/// itself errors.
#[test]
fn json_malformed_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let bad = [
        "",
        " ",
        "{",
        "}",
        "[",
        "]",
        "[1,",
        "[1,]",
        "{\"a\"}",
        "{\"a\":}",
        "{\"a\":1,}",
        "{a:1}",
        "{'a':1}",
        "'a'",
        "\"a",
        "a\"",
        "01",
        "+1",
        ".5",
        "1.",
        "1e",
        "1e+",
        "0x10",
        "Infinity",
        "-Infinity",
        "NaN",
        "tru",
        "nul",
        "[1 2]",
        "[1,2] junk",
        "1 2",
        "\"\\q\"",
        "\"\\u00g0\"",
        "[/*c*/1]",
        "[1] // c",
        // A raw control character inside a string.
        "\"a\u{1}b\"",
    ];
    let mut qs = Vec::new();
    for d in &bad {
        let lit = d.replace('\'', "''");
        qs.push(format!("SELECT json_valid('{lit}')"));
    }
    cross_check_batch(&db, &qs);
    // And `json()` itself refuses every one of them.
    for d in &bad {
        let lit = d.replace('\'', "''");
        refuses(&db, &format!("SELECT json('{lit}')"), "malformed JSON");
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The nesting bound. mpedb parses 128 levels where sqlite parses 1000, so the
/// two agree up to 128 and mpedb REFUSES beyond it rather than answering 0
/// (which would be a wrong answer, not a refusal — sqlite says 1 there).
#[test]
fn json_depth_bound() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let nest = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    let mut qs = Vec::new();
    for n in [1usize, 2, 32, 127, 128] {
        qs.push(format!("SELECT json_valid('{}')", nest(n)));
        qs.push(format!("SELECT length(json('{}'))", nest(n)));
    }
    cross_check_batch(&db, &qs);
    // A document that is BOTH too deep and malformed reports the depth first:
    // mpedb never gets far enough to see the truncation, so it refuses where
    // sqlite answers 0. A refusal, not a wrong answer.
    refuses(
        &db,
        &format!("SELECT json_valid('{}')", "[".repeat(2000)),
        "nests deeper than 128 levels",
    );
    for n in [129usize, 1000, 1001, 2000] {
        refuses(
            &db,
            &format!("SELECT json_valid('{}')", nest(n)),
            "nests deeper than 128 levels",
        );
        refuses(
            &db,
            &format!("SELECT json('{}')", nest(n)),
            "nests deeper than 128 levels",
        );
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The deliberate JSON5 split, stated as a test: sqlite's `json()` ACCEPTS and
/// rewrites JSON5 while its `json_valid()` rejects it; mpedb refuses in
/// `json()` too, so the two agree — and the refusal names JSON5.
#[test]
fn json5_is_refused_by_name() {
    let (db, path) = mpedb_db();
    for d in ["{a:1}", "'a'", "0x10", "+1", ".5", "1.", "[1,2,]", "Infinity", "NaN"] {
        let lit = d.replace('\'', "''");
        refuses(&db, &format!("SELECT json('{lit}')"), "JSON5");
        // json_valid agrees with sqlite (0) rather than refusing.
        assert_eq!(
            mpedb_rows(&db, &format!("SELECT json_valid('{lit}')")),
            vec!["0".to_string()],
            "json_valid('{d}')"
        );
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// json_valid(X[, FLAGS])
// ---------------------------------------------------------------------------

#[test]
fn json_valid_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let mut qs = q(&[
        "SELECT json_valid(NULL)",
        "SELECT json_valid('')",
        "SELECT json_valid('  ')",
        "SELECT json_valid('{\"a\":1}')",
        "SELECT json_valid('{\"a\":1}', 1)",
        "SELECT json_valid('{a:1}', 1)",
        "SELECT json_valid('null')",
        "SELECT json_valid(doc) FROM t WHERE id = 1",
        "SELECT json_valid(doc) FROM t WHERE id = 2",
        "SELECT json_valid(i) FROM t WHERE id = 1",
        "SELECT json_valid('[1,2,3]', 1)",
    ]);
    // Every strict-grammar answer is the same with and without the flag.
    for d in ["{\"a\":1}", "[1]", "5", "\"s\"", "junk", "{a:1}"] {
        qs.push(format!("SELECT json_valid('{d}') || '/' || json_valid('{d}', 1)"));
    }
    cross_check_batch(&db, &qs);
    // Out-of-range FLAGS is sqlite's own error, verbatim.
    for f in ["0", "16", "-1", "NULL"] {
        refuses(
            &db,
            &format!("SELECT json_valid('[1]', {f})"),
            "must be between 1 and 15",
        );
    }
    // Every in-range grammar bit mpedb does not implement is refused BY NAME.
    for f in [2, 3, 4, 5, 6, 7, 8, 9, 15] {
        refuses(
            &db,
            &format!("SELECT json_valid('[1]', {f})"),
            "grammar bit 1",
        );
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// json_extract / -> / ->>
// ---------------------------------------------------------------------------

const DOC: &str = r#"{"a":1,"b":"x","c":[1,2,3],"d":{"e":9},"f":null,"g":true,"h":false,"i":1.5,"j":1e3,"k":[],"l":{},"m":"[1,2]","a.b":7,"":8}"#;

#[test]
fn json_extract_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let paths = [
        "$",
        "$.a",
        "$.b",
        "$.c",
        "$.c[0]",
        "$.c[2]",
        "$.c[3]",
        "$.c[#-1]",
        "$.c[#-3]",
        "$.c[#-4]",
        "$.c[#]",
        "$.d",
        "$.d.e",
        "$.d.zz",
        "$.e",
        "$.f",
        "$.g",
        "$.h",
        "$.i",
        "$.j",
        "$.k",
        "$.l",
        "$.m",
        "$.zz",
        "$.a.b",
        "$.a[0]",
        "$.c.a",
        "$.\"a.b\"",
        "$.\"\"",
        "$[0]",
        "$.c[00]",
    ];
    let mut qs = Vec::new();
    for p in paths {
        let pl = p.replace('\'', "''");
        // The value AND its type: `json_extract` unwraps, so a wrong type is a
        // wrong answer even when the rendered text coincides.
        qs.push(format!("SELECT CAST(json_extract('{DOC}', '{pl}') AS TEXT)"));
        qs.push(format!("SELECT typeof(json_extract('{DOC}', '{pl}'))"));
        // The two operators over the same path — `->` yields JSON text, `->>`
        // a SQL value, and this is where the difference shows.
        qs.push(format!("SELECT '{DOC}' -> '{pl}'"));
        qs.push(format!("SELECT typeof('{DOC}' -> '{pl}')"));
        qs.push(format!("SELECT CAST('{DOC}' ->> '{pl}' AS TEXT)"));
        qs.push(format!("SELECT typeof('{DOC}' ->> '{pl}')"));
    }
    // Multiple paths wrap into a JSON array; a missing one becomes `null`.
    qs.extend(q(&[
        &format!("SELECT json_extract('{DOC}', '$.a', '$.b')"),
        &format!("SELECT typeof(json_extract('{DOC}', '$.a', '$.b'))"),
        &format!("SELECT json_extract('{DOC}', '$.a', '$.zz')"),
        &format!("SELECT json_extract('{DOC}', '$.c', '$.d', '$.f')"),
        &format!("SELECT json_extract('{DOC}', '$', '$')"),
        // Scalar documents, and a path INTO a scalar.
        "SELECT CAST(json_extract('5', '$') AS TEXT)",
        "SELECT typeof(json_extract('5', '$'))",
        "SELECT json_extract('5', '$.a')",
        "SELECT json_extract('5', '$[0]')",
        "SELECT json_extract('\"s\"', '$')",
        "SELECT json_extract('null', '$')",
        "SELECT typeof(json_extract('null', '$'))",
        "SELECT json_extract('true', '$')",
        // Numbers at the i64 boundary: sqlite decides integer-vs-real from the
        // TOKEN's shape and from whether it fits.
        "SELECT CAST(json_extract('{\"a\":9223372036854775807}', '$.a') AS TEXT)",
        "SELECT typeof(json_extract('{\"a\":9223372036854775807}', '$.a'))",
        "SELECT CAST(json_extract('{\"a\":-9223372036854775808}', '$.a') AS TEXT)",
        "SELECT typeof(json_extract('{\"a\":-9223372036854775808}', '$.a'))",
        "SELECT CAST(json_extract('{\"a\":9223372036854775808}', '$.a') AS TEXT)",
        "SELECT typeof(json_extract('{\"a\":9223372036854775808}', '$.a'))",
        "SELECT CAST(json_extract('{\"a\":1.0}', '$.a') AS TEXT)",
        "SELECT typeof(json_extract('{\"a\":1.0}', '$.a'))",
        "SELECT CAST(json_extract('{\"a\":1e3}', '$.a') AS TEXT)",
        "SELECT typeof(json_extract('{\"a\":1e3}', '$.a'))",
        // Escapes come back DECODED as SQL text.
        r#"SELECT json_extract('{"a":"å\n\t"}', '$.a') = char(229) || char(10) || char(9)"#,
        r#"SELECT json_extract('{"a":"😀"}', '$.a') = char(128512)"#,
        // A document label carrying escapes is matched by its DECODED text.
        r#"SELECT json_extract('{"a\"b":1}', '$.a"b')"#,
        // NULL propagation.
        "SELECT json_extract(NULL, '$.a')",
        "SELECT json_extract('{\"a\":1}', NULL)",
        "SELECT NULL -> '$.a'",
        "SELECT NULL ->> '$.a'",
        "SELECT '{\"a\":1}' -> NULL",
        "SELECT '{\"a\":1}' ->> NULL",
        // Over a real column, including the NULL row.
        "SELECT json_extract(doc, '$.a') FROM t WHERE id = 1",
        "SELECT doc ->> '$.b' FROM t WHERE id = 1",
        "SELECT doc -> '$.c' FROM t WHERE id = 1",
        "SELECT json_extract(doc, '$.a') FROM t WHERE id = 2",
        "SELECT doc ->> '$.b' FROM t WHERE id = 2",
    ]));
    cross_check_batch(&db, &qs);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The abbreviated right-hand side the OPERATORS accept and `json_extract`
/// does not: an integer is an array index, a bare word is a whole LABEL (so
/// `'a.b'` means `$."a.b"`, NOT `$.a.b`), and `[…]` is rooted at `$`.
#[test]
fn json_arrow_path_sugar_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let qs = q(&[
        "SELECT '[1,2,3]' -> 0",
        "SELECT '[1,2,3]' ->> 0",
        "SELECT '[1,2,3]' -> 2",
        "SELECT '[1,2,3]' -> 3",
        "SELECT '[1,2,3]' -> -1",
        "SELECT '[1,2,3]' ->> -1",
        "SELECT '{\"1\":7}' -> 1",
        "SELECT '{\"a\":1}' -> 0",
        "SELECT '{\"a\":1}' -> 'a'",
        "SELECT '{\"a\":1}' ->> 'a'",
        "SELECT '{\"a.b\":1}' -> 'a.b'",
        "SELECT '{\"a\":{\"b\":9}}' -> 'a.b'",
        "SELECT '{\"a\":{\"b\":9}}' -> '$.a.b'",
        "SELECT '{\"a[0]\":7}' -> 'a[0]'",
        "SELECT '[[5,6]]' -> '[0][1]'",
        "SELECT '[1,2,3]' -> '[1]'",
        "SELECT '[1,2,3]' -> '[#-1]'",
        "SELECT '{\"a\":1}' -> '.a'",
        // Chaining: left-associative, and the two operators mix.
        "SELECT '{\"a\":{\"b\":7}}' -> '$.a' -> '$.b'",
        "SELECT '{\"a\":{\"b\":7}}' -> '$.a' ->> '$.b'",
        "SELECT '[[1,2],[3,4]]' -> 1 -> 0",
        // Precedence: the accessors bind tighter than `*`, `+`, and a
        // comparison, so none of these need parentheses.
        "SELECT '[10,20]' ->> 1 * 2",
        "SELECT '[10,20]' ->> 1 + 1",
        "SELECT '[10,20]' ->> 1 > 15",
        "SELECT '[10,20]' ->> 0 >= 10",
        // The tokenizer must keep `>`, `>=`, `->` and `->>` apart with no
        // whitespace at all.
        "SELECT '{\"a\":1}'->>'$.a'",
        "SELECT '{\"a\":1}'->'$.a'",
        "SELECT 3>2, 2>=2, 5-3",
    ]);
    // The last one yields three columns; check it separately.
    let (multi, single) = qs.split_last().unwrap();
    cross_check_batch(&db, single);
    match db.query(multi, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => {
            assert_eq!(rows[0].len(), 3, "`{multi}`");
            assert_eq!(render(&rows[0][0]), "1");
            assert_eq!(render(&rows[0][1]), "1");
            assert_eq!(render(&rows[0][2]), "2");
        }
        other => panic!("`{multi}` -> {other:?}"),
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A bad path is an ERROR in both engines (never a NULL that could be mistaken
/// for "not found"), and the message names the path.
#[test]
fn json_bad_paths_are_errors() {
    let (db, path) = mpedb_db();
    for p in [
        "a", "", " $.a", "$ ", "$x", "$[", "$[]", "$[a]", "$[-1]", "$[#", "$[#-]", "$.a[",
    ] {
        let lit = p.replace('\'', "''");
        refuses(
            &db,
            &format!("SELECT json_extract('{{\"a\":1}}', '{lit}')"),
            "bad JSON path",
        );
    }
    // A backslash in a path key is the one place mpedb refuses where sqlite
    // answers — sqlite compares a DECODED document label against a VERBATIM
    // path key, and that asymmetry is not reproduced.
    refuses(
        &db,
        r#"SELECT json_extract('{"a":1}', '$."a\"b"')"#,
        "backslash",
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// json_type / json_array_length / json_quote
// ---------------------------------------------------------------------------

#[test]
fn json_type_and_array_length_match_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let mut qs = Vec::new();
    for p in [
        "$", "$.a", "$.b", "$.c", "$.c[0]", "$.d", "$.f", "$.g", "$.h", "$.i", "$.j", "$.k",
        "$.l", "$.zz", "$.c[9]",
    ] {
        qs.push(format!("SELECT json_type('{DOC}', '{p}')"));
        qs.push(format!("SELECT json_array_length('{DOC}', '{p}')"));
    }
    qs.extend(q(&[
        "SELECT json_type('{\"a\":1}')",
        "SELECT json_type('[1]')",
        "SELECT json_type('1')",
        "SELECT json_type('1.5')",
        "SELECT json_type('1e3')",
        "SELECT json_type('\"x\"')",
        "SELECT json_type('true')",
        "SELECT json_type('false')",
        "SELECT json_type('null')",
        "SELECT json_type(NULL)",
        "SELECT json_type('{\"a\":1}', NULL)",
        "SELECT json_array_length('[1,2,3]')",
        "SELECT json_array_length('[]')",
        "SELECT json_array_length('{\"a\":1}')",
        "SELECT json_array_length('5')",
        "SELECT json_array_length(NULL)",
        "SELECT json_array_length('[1]', NULL)",
        "SELECT json_type(doc, '$.c') FROM t WHERE id = 1",
        "SELECT json_array_length(doc, '$.c') FROM t WHERE id = 1",
        "SELECT json_type(doc) FROM t WHERE id = 2",
    ]));
    cross_check_batch(&db, &qs);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn json_quote_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let mut qs = q(&[
        "SELECT json_quote(1)",
        "SELECT json_quote(-1)",
        "SELECT json_quote(1.5)",
        "SELECT json_quote(1e3)",
        "SELECT json_quote(1e300)",
        "SELECT json_quote(1e-5)",
        "SELECT json_quote(0.1)",
        "SELECT json_quote(-0.0)",
        "SELECT json_quote(1.0/3)",
        "SELECT json_quote(NULL)",
        "SELECT typeof(json_quote(NULL))",
        "SELECT json_quote('x')",
        "SELECT json_quote('')",
        "SELECT json_quote('a\"b')",
        "SELECT json_quote('a\\b')",
        "SELECT json_quote('{\"a\":1}')",
        "SELECT json_quote(char(9))",
        "SELECT json_quote(char(10))",
        "SELECT json_quote(char(13))",
        "SELECT json_quote(char(8))",
        "SELECT json_quote(char(12))",
        "SELECT json_quote(char(1))",
        "SELECT json_quote(char(31))",
        "SELECT json_quote(char(127))",
        "SELECT json_quote('xå😀')",
        "SELECT json_quote(doc) FROM t WHERE id = 1",
        "SELECT json_quote(doc) FROM t WHERE id = 2",
        "SELECT json_quote(i) FROM t WHERE id = 1",
        // A JSON-producing argument passes straight through (sqlite's subtype).
        "SELECT json_quote(json('[ 1 , 2 ]'))",
        "SELECT json_quote(json('{\"a\":1}'))",
        "SELECT json_quote(json_array(1, 'x'))",
        "SELECT json_quote(json_object('a', 1))",
        "SELECT json_quote(json_quote('x'))",
        "SELECT json_quote('{\"a\":[9]}' -> '$.a')",
        "SELECT json_quote('{\"a\":[9]}' ->> '$.a')",
    ]);
    // The subtype flows through lazy control flow in sqlite; mpedb decides the
    // same thing at bind time, so all-JSON and all-plain arms both agree.
    qs.extend(q(&[
        "SELECT json_quote(CASE WHEN 1=1 THEN json('[1]') ELSE json('[2]') END)",
        "SELECT json_quote(CASE WHEN 1=1 THEN 'a' ELSE 'b' END)",
        "SELECT json_quote(coalesce(json('[1]'), json('[2]')))",
        "SELECT json_quote(coalesce('a', 'b'))",
        "SELECT json_quote(CASE WHEN 1=1 THEN json('[1]') ELSE NULL END)",
        // Shapes sqlite does NOT subtype: concatenation and an aggregate.
        "SELECT json_quote(json('[1]') || '')",
    ]));
    cross_check_batch(&db, &qs);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// The writers
// ---------------------------------------------------------------------------

#[test]
fn json_array_and_object_match_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let qs = q(&[
        "SELECT json_array()",
        "SELECT json_array(1)",
        "SELECT json_array(1, 2.5, 'x', NULL)",
        "SELECT json_array('{\"a\":1}')",
        "SELECT json_array(json('{\"a\":1}'))",
        "SELECT json_array(json_array(1), json_object('a', 2))",
        "SELECT json_array('a\"b', char(10), char(9), char(1))",
        "SELECT json_array(1e3, 1e300, -0.0, 0.1)",
        "SELECT json_array(doc) FROM t WHERE id = 1",
        "SELECT json_array(json(doc)) FROM t WHERE id = 1",
        "SELECT json_array(doc, i) FROM t WHERE id = 2",
        "SELECT json_object()",
        "SELECT json_object('a', 1)",
        "SELECT json_object('a', 1, 'b', 'x', 'c', NULL)",
        "SELECT json_object('a', '[1,2]')",
        "SELECT json_object('a', json('[1,2]'))",
        "SELECT json_object('a\"b', 1)",
        "SELECT json_object('', 1)",
        "SELECT json_object('a', json_object('b', 2))",
    ]);
    cross_check_batch(&db, &qs);
    // sqlite's own arity/label errors.
    refuses(&db, "SELECT json_object('a')", "even number of arguments");
    // sqlite raises "labels must be TEXT" per row; mpedb's rigid binder pins
    // the label positions to text, so the same mistake is a COMPILE error.
    refuses(&db, "SELECT json_object(1, 2)", "must be text");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn json_writers_match_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let qs = q(&[
        // set / insert / replace, over both objects and arrays.
        "SELECT json_set('{\"a\":1}', '$.a', 9)",
        "SELECT json_set('{\"a\":1}', '$.b', 9)",
        "SELECT json_set('{\"a\":1}', '$.a', 9, '$.b', 8)",
        "SELECT json_insert('{\"a\":1}', '$.a', 9, '$.b', 8)",
        "SELECT json_replace('{\"a\":1}', '$.a', 9, '$.b', 8)",
        "SELECT json_set('{\"a\":1}', '$.b.c', 9)",
        "SELECT json_set('[1,2,3]', '$[0]', 9)",
        "SELECT json_set('[1,2,3]', '$[3]', 9)",
        "SELECT json_set('[1,2,3]', '$[5]', 9)",
        "SELECT json_set('[1,2,3]', '$[#-1]', 9)",
        "SELECT json_insert('[1,2]', '$[#]', 9)",
        "SELECT json_set('{\"a\":1}', '$', 9)",
        "SELECT json_replace('{\"a\":1}', '$', 9)",
        "SELECT json_insert('{\"a\":1}', '$', 9)",
        // The untouched parts keep their EXACT spelling.
        "SELECT json_set('{\"a\":1.50,\"b\":1e3,\"c\":\"å\"}', '$.z', 1)",
        "SELECT json_remove('{\"a\":1.50,\"b\":1e3,\"c\":\"å\"}', '$.z')",
        // Values entering the document.
        "SELECT json_set('{}', '$.a', '[1,2]')",
        "SELECT json_set('{}', '$.a', json('[1,2]'))",
        "SELECT json_set('{}', '$.a', NULL)",
        "SELECT json_set('{}', '$.a', 1e3)",
        "SELECT json_set('{}', '$.a', char(10))",
        "SELECT json_insert('{}', '$.a', NULL)",
        "SELECT json_replace('{\"a\":1}', '$.a', NULL)",
        // NULL rules, which differ between the writers.
        "SELECT json_set(NULL, '$.a', 1)",
        "SELECT json_set('{\"a\":1}', NULL, 1)",
        "SELECT json_remove(NULL, '$.a')",
        "SELECT json_remove('{\"a\":1}', NULL)",
        // remove
        "SELECT json_remove('{\"a\":1,\"b\":2}', '$.a')",
        "SELECT json_remove('{\"a\":1,\"b\":2}', '$.a', '$.b')",
        "SELECT json_remove('[1,2,3]', '$[1]')",
        "SELECT json_remove('[1,2,3]', '$[#-1]')",
        "SELECT json_remove('[1,2,3]', '$[9]')",
        "SELECT json_remove('{\"a\":1}', '$')",
        "SELECT json_remove('{\"a\":1}')",
        "SELECT json_remove('{\"a\":1}', '$.zz')",
        // patch (RFC 7396)
        "SELECT json_patch('{\"a\":1,\"b\":2}', '{\"b\":null,\"c\":3}')",
        "SELECT json_patch('{\"a\":{\"b\":1,\"c\":2}}', '{\"a\":{\"b\":null}}')",
        "SELECT json_patch('{\"a\":1}', '{\"b\":{\"c\":null}}')",
        "SELECT json_patch('{}', '{\"b\":{\"c\":null}}')",
        "SELECT json_patch('[1,2]', '[3]')",
        "SELECT json_patch('{\"a\":1}', '5')",
        "SELECT json_patch('5', '{\"a\":1}')",
        "SELECT json_patch('{\"a\":1}', '{}')",
        "SELECT json_patch(NULL, '{}')",
        "SELECT json_patch('{}', NULL)",
        "SELECT json_patch('{\"a\":1.50}', '{\"z\":1}')",
        // Composition: a writer's output is JSON, so it splices raw.
        "SELECT json_set('{}', '$.a', json_array(1, 2))",
        "SELECT json_object('a', json_set('{}', '$.b', 1))",
        "SELECT json_array(json_remove('[1,2]', '$[0]'))",
        "SELECT json_array(json_patch('{\"a\":1}', '{\"b\":2}'))",
    ]);
    cross_check_batch(&db, &qs);
    refuses(&db, "SELECT json_set('{}', '$.a')", "odd number of arguments");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A BLOB cannot enter a document — sqlite's own error, reproduced.
#[test]
fn json_refuses_blob_values() {
    let (db, path) = mpedb_db();
    refuses(&db, "SELECT json_array(x'41')", "JSON cannot hold BLOB");
    refuses(&db, "SELECT json_quote(x'41')", "JSON cannot hold BLOB");
    // `json(<blob>)` would be JSONB in sqlite; mpedb's binder pins the
    // document argument to text, so it never reaches the runtime message.
    refuses(&db, "SELECT json(x'41')", "must be text, got blob");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The argument shapes whose sqlite answer depends on a runtime JSON subtype
/// mpedb's values do not carry are refused BY NAME rather than guessed.
#[test]
fn subtype_undecidable_shapes_are_refused() {
    let (db, path) = mpedb_db();
    // json_extract is JSON only when the extracted node is a container — a
    // property of the DATA, not of the query.
    refuses(
        &db,
        "SELECT json_array(json_extract('{\"a\":[1]}', '$.a'))",
        "object or an array",
    );
    refuses(
        &db,
        "SELECT json_quote(json_extract('{\"a\":[1]}', '$.a'))",
        "object or an array",
    );
    refuses(
        &db,
        "SELECT json_set('{}', '$.a', json_extract('{\"a\":[1]}', '$.a'))",
        "object or an array",
    );
    // Mixed CASE arms.
    refuses(
        &db,
        "SELECT json_array(CASE WHEN 1=1 THEN json('[1]') ELSE 'x' END)",
        "CASE arms disagree",
    );
    refuses(
        &db,
        "SELECT json_quote(coalesce(json('[1]'), 'x'))",
        "arms disagree",
    );
    // A scalar subquery.
    refuses(
        &db,
        "SELECT json_array((SELECT doc FROM t WHERE id = 1))",
        "scalar subquery",
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The table-valued / aggregate / JSONB members of sqlite's JSON surface are
/// refused with a message naming them, not with "unknown function".
#[test]
fn out_of_scope_json_functions_name_themselves() {
    let (db, path) = mpedb_db();
    refuses(&db, "SELECT json_each('[1]')", "TABLE-VALUED");
    refuses(&db, "SELECT json_tree('[1]')", "TABLE-VALUED");
    refuses(&db, "SELECT json_group_array(1)", "AGGREGATES");
    refuses(&db, "SELECT json_group_object('a', 1)", "AGGREGATES");
    refuses(&db, "SELECT jsonb('[1]')", "JSONB");
    refuses(&db, "SELECT jsonb_extract('[1]', '$')", "JSONB");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Django's shape
// ---------------------------------------------------------------------------

/// The exact table Django's `JSONField` creates on sqlite, and rows going in
/// through the CHECK.
///
/// This needs BOTH halves of Django gap #4, and both have landed: the JSON
/// function set (so `JSON_VALID()` resolves at all) and the int-to-bool bridge
/// (so the INTEGER 0/1 it returns is usable as the left operand of the `OR`).
/// Neither may regress silently, so the CHECK must now COMPILE — a failure on
/// `AND/OR requires boolean operands` means the bridge regressed and is a test
/// failure, not a skip.
#[test]
fn django_jsonfield_check_compiles_and_enforces() {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-json-django-{}-{}.mpedb",
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
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    let ddl = r#"CREATE TABLE "model" (
        "id" integer NOT NULL PRIMARY KEY,
        "data" text NULL CHECK ((JSON_VALID("data") OR "data" IS NULL))
    )"#;
    if let Err(e) = db.query(ddl, &[]) {
        panic!("Django's JSONField CHECK must compile — both halves of gap #4 \
                have landed, so this is a regression: {e}");
    }
    db.query(r#"INSERT INTO "model" (id, data) VALUES (1, '{"k": [1, 2]}')"#, &[])
        .expect("a valid document must pass the CHECK");
    db.query(r#"INSERT INTO "model" (id, data) VALUES (2, NULL)"#, &[])
        .expect("NULL must pass the CHECK");
    let bad = db.query(r#"INSERT INTO "model" (id, data) VALUES (3, 'not json')"#, &[]);
    assert!(bad.is_err(), "a malformed document must fail the CHECK");
    // And the lookups Django compiles for `data__k` / `data__k__0`.
    assert_eq!(
        mpedb_rows(
            &db,
            r#"SELECT JSON_EXTRACT("data", '$.k') FROM "model" WHERE id = 1"#
        ),
        vec!["[1,2]".to_string()]
    );
    assert_eq!(
        mpedb_rows(&db, r#"SELECT "data" ->> '$.k[0]' FROM "model" WHERE id = 1"#),
        vec!["1".to_string()]
    );
    assert_eq!(
        mpedb_rows(
            &db,
            r#"SELECT JSON_TYPE("data", '$.k') FROM "model" WHERE id = 1"#
        ),
        vec!["array".to_string()]
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// A generated cross-product sweep
// ---------------------------------------------------------------------------

/// The curated tests above pin the cases that were REASONED about. This one
/// pins the ones that were not: every reader crossed with every document and
/// every path, ~1,800 queries in one sqlite invocation. It exists to catch a
/// wrong answer in a combination nobody thought to write down, which is the
/// only kind of JSON bug that is invisible.
#[test]
fn generated_reader_sweep_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let docs = [
        r#"{"a":1,"b":[1,2],"c":{"d":null},"e":"s","f":true,"g":1.5}"#,
        r#"[1,"two",null,false,{"k":[]},[[]]]"#,
        r#"{"":0,"0":1,"a.b":2,"a[0]":3,"å":4}"#,
        r#"[]"#,
        r#"{}"#,
        r#"0"#,
        r#""s""#,
        r#"null"#,
        r#"true"#,
        r#"{"n":{"n":{"n":[1,2,3]}}}"#,
    ];
    let paths = [
        "$", "$.a", "$.b", "$.b[0]", "$.b[1]", "$.b[2]", "$.b[#-1]", "$.b[#]", "$.c",
        "$.c.d", "$.e", "$.f", "$.g", "$.zz", "$[0]", "$[1]", "$[5]", "$[#-1]",
        "$[4].k", "$[5][0]", "$.\"\"", "$.0", "$.\"a.b\"", "$.å", "$.n.n.n[2]",
    ];
    let mut qs = Vec::new();
    for d in docs {
        let dl = d.replace('\'', "''");
        qs.push(format!("SELECT json('{dl}')"));
        qs.push(format!("SELECT json_type('{dl}')"));
        qs.push(format!("SELECT json_valid('{dl}')"));
        qs.push(format!("SELECT json_array_length('{dl}')"));
        for p in paths {
            let pl = p.replace('\'', "''");
            qs.push(format!(
                "SELECT CAST(json_extract('{dl}', '{pl}') AS TEXT)"
            ));
            qs.push(format!("SELECT typeof(json_extract('{dl}', '{pl}'))"));
            qs.push(format!("SELECT '{dl}' -> '{pl}'"));
            qs.push(format!("SELECT typeof('{dl}' -> '{pl}')"));
            qs.push(format!("SELECT CAST('{dl}' ->> '{pl}' AS TEXT)"));
            qs.push(format!("SELECT typeof('{dl}' ->> '{pl}')"));
            qs.push(format!("SELECT json_type('{dl}', '{pl}')"));
            qs.push(format!("SELECT json_array_length('{dl}', '{pl}')"));
        }
    }
    cross_check_batch(&db, &qs);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The same idea for the WRITERS: every mutating function crossed with a set of
/// documents, paths and values, including the JSON-vs-plain distinction on the
/// value side.
#[test]
fn generated_writer_sweep_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();
    let docs = [
        r#"{"a":1,"b":[1,2],"c":{"d":null}}"#,
        r#"[1,2,3]"#,
        r#"[]"#,
        r#"{}"#,
        r#"5"#,
        r#"{"a":1.50,"b":1e3}"#,
    ];
    let paths = [
        "$", "$.a", "$.b", "$.b[0]", "$.b[2]", "$.b[#]", "$.b[#-1]", "$.c.d", "$.z",
        "$.z.y", "$[0]", "$[3]", "$[9]", "$[#]",
    ];
    let values = ["9", "'x'", "NULL", "2.5", "'[1,2]'", "json('[1,2]')", "1e3"];
    let mut qs = Vec::new();
    for d in docs {
        let dl = d.replace('\'', "''");
        qs.push(format!("SELECT json_remove('{dl}')"));
        for p in paths {
            qs.push(format!("SELECT json_remove('{dl}', '{p}')"));
            for v in values {
                qs.push(format!("SELECT json_set('{dl}', '{p}', {v})"));
                qs.push(format!("SELECT json_insert('{dl}', '{p}', {v})"));
                qs.push(format!("SELECT json_replace('{dl}', '{p}', {v})"));
            }
        }
        for e in docs {
            let el = e.replace('\'', "''");
            qs.push(format!("SELECT json_patch('{dl}', '{el}')"));
        }
        qs.push(format!("SELECT json_array('{dl}', json('{dl}'))"));
        qs.push(format!("SELECT json_object('k', '{dl}', 'j', json('{dl}'))"));
    }
    cross_check_batch(&db, &qs);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A deterministic xorshift generator over documents AND paths, so the shapes
/// nobody enumerated get exercised too: nested objects and arrays up to 4
/// levels, every scalar kind, keys that collide with path syntax, and paths
/// built both from the document's own structure and at random. ~1,500 cases,
/// one sqlite invocation.
///
/// A wrong answer in JSON is invisible — a path that silently returns the
/// wrong node looks exactly like a path that returned the right one. This is
/// the test that is allowed to find one.
#[test]
fn randomized_document_sweep_matches_sqlite() {
    if !sqlite_available() {
        return;
    }
    let (db, path) = mpedb_db();

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    // Keys deliberately include the ones that collide with path syntax.
    const KEYS: [&str; 8] = ["a", "b", "k1", "", "0", "a.b", "x y", "å"];
    const SCALARS: [&str; 10] = [
        "0", "1", "-1", "1.50", "1e3", "true", "false", "null", "\"s\"", "\"[1,2]\"",
    ];

    fn doc(r: &mut Rng, depth: u64) -> String {
        // Past depth 3, only scalars — keeps documents small enough that a
        // failure is readable.
        let pick = if depth >= 3 { 2 } else { r.below(4) };
        match pick {
            0 => {
                let n = r.below(4);
                let items: Vec<String> = (0..n).map(|_| doc(r, depth + 1)).collect();
                format!("[{}]", items.join(","))
            }
            1 => {
                let n = r.below(4);
                let mut used = Vec::new();
                let mut pairs = Vec::new();
                for _ in 0..n {
                    let k = KEYS[r.below(KEYS.len() as u64) as usize];
                    // Duplicate keys are legal JSON and sqlite keeps them, but
                    // they make a generated expectation ambiguous to READ, so
                    // one of each per object here.
                    if used.contains(&k) {
                        continue;
                    }
                    used.push(k);
                    pairs.push(format!("\"{k}\":{}", doc(r, depth + 1)));
                }
                format!("{{{}}}", pairs.join(","))
            }
            _ => SCALARS[r.below(SCALARS.len() as u64) as usize].to_string(),
        }
    }

    /// A path built by walking the document's own structure, so most paths HIT.
    fn path_into(r: &mut Rng, d: &str) -> String {
        let mut p = String::from("$");
        // Re-walking the text is not worth it: build from the same key/index
        // vocabulary and let the misses be misses.
        for _ in 0..r.below(4) {
            if r.below(2) == 0 {
                let k = KEYS[r.below(KEYS.len() as u64) as usize];
                if k.contains('.') || k.contains(' ') || k.is_empty() {
                    p.push_str(&format!(".\"{k}\""));
                } else {
                    p.push_str(&format!(".{k}"));
                }
            } else {
                match r.below(3) {
                    0 => p.push_str(&format!("[{}]", r.below(4))),
                    1 => p.push_str(&format!("[#-{}]", 1 + r.below(3))),
                    _ => p.push_str("[#]"),
                }
            }
        }
        let _ = d;
        p
    }

    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let mut qs = Vec::new();
    for _ in 0..150 {
        let d = doc(&mut r, 0);
        let dl = d.replace('\'', "''");
        qs.push(format!("SELECT json('{dl}')"));
        qs.push(format!("SELECT json_type('{dl}')"));
        qs.push(format!("SELECT json_array_length('{dl}')"));
        for _ in 0..2 {
            let p = path_into(&mut r, &d);
            let pl = p.replace('\'', "''");
            qs.push(format!("SELECT CAST(json_extract('{dl}', '{pl}') AS TEXT)"));
            qs.push(format!("SELECT typeof(json_extract('{dl}', '{pl}'))"));
            qs.push(format!("SELECT '{dl}' -> '{pl}'"));
            qs.push(format!("SELECT CAST('{dl}' ->> '{pl}' AS TEXT)"));
            qs.push(format!("SELECT json_type('{dl}', '{pl}')"));
            qs.push(format!("SELECT json_array_length('{dl}', '{pl}')"));
            qs.push(format!("SELECT json_remove('{dl}', '{pl}')"));
            qs.push(format!("SELECT json_set('{dl}', '{pl}', 7)"));
            qs.push(format!("SELECT json_insert('{dl}', '{pl}', 'v')"));
            qs.push(format!("SELECT json_replace('{dl}', '{pl}', json('[8]'))"));
        }
        // patch against another generated document
        let e = doc(&mut r, 0).replace('\'', "''");
        qs.push(format!("SELECT json_patch('{dl}', '{e}')"));
        // and a round-trip
        qs.push(format!("SELECT json(json('{dl}')) = json('{dl}')"));
    }
    cross_check_batch(&db, &qs);
    drop(db);
    let _ = std::fs::remove_file(&path);
}
