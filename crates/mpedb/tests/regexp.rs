//! `x REGEXP 'pat'` / `x NOT REGEXP 'pat'` — sqlite's bundled `ext/misc/regexp.c`
//! matcher: case-SENSITIVE, unanchored substring match with `.`, `* + ?`,
//! `{p,q}`, `[...]`, `^`/`$`, `|`, `(...)`, `\d`/`\w`/`\s`/`\b` and escapes.
//! Modeled on the GLOB integration test, and every case here is cross-checked
//! against the `sqlite3` CLI (3.45), whose `REGEXP` operator ships that engine.
//!
//! The left operand is a text column that may be NULL, so the NULL-propagation
//! rule (`NULL REGEXP p` and `NULL NOT REGEXP p` are both NULL) is exercised
//! too.
//!
//! Since #74 item 3 the PATTERN no longer has to be a literal — Django always
//! binds it. `bound_pattern_matches_the_literal_form` re-runs the whole battery
//! below with the pattern bound as a parameter and asserts the two forms agree
//! row-for-row, which is the statement that matters: the dynamic form is the
//! same matcher, not a second implementation.

//! Task #108 adds the OTHER half of the operator: in real sqlite `x REGEXP y`
//! has no built-in meaning at all — it desugars to `regexp(y, x)` (pattern
//! first) and errors unless the consumer registered that 2-argument function.
//! When a host `regexp/2` IS registered (CPython/Django always do), the
//! operator dispatches to it and the host dialect wins; the native matcher
//! above remains as a documented EXTENSION for connections that registered
//! none. The host-dispatch tests differential against the BUNDLED library
//! (`sqlite_oracle::script_stdout_with` registers the same Rust closure as the
//! oracle's `regexp()`), so only the native-dialect battery still needs the
//! shell binary.

use mpedb::{Config, Database, Error, ExecResult, Value};
use std::io::Write;
use std::ops::Deref;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

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

/// `id` is the PK; `s` is a nullable text column the operand REGEXP matches
/// against. The strings deliberately cover case, digits, spaces, a literal `.`
/// and `-` in the DATA, an empty string, and a NULL, so a query can distinguish
/// anchors, classes, escapes, case-sensitivity and the 3VL NULL path.
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

/// One seed row: `(id, s)`.
const ROWS: &[(i64, Option<&'static str>)] = &[
    (1, Some("abc")),
    (2, Some("Abc")),         // case: uppercase A
    (3, Some("aXc")),         // middle char varies
    (4, Some("a1c")),         // an embedded digit
    (5, Some("a.c")),         // a literal '.' in the DATA
    (6, Some("hello world")), // a space + word boundary
    (7, Some("123")),         // all digits
    (8, Some("a-c")),         // a literal '-' in the DATA
    (9, Some("foobar")),      // no boundary before "bar"
    (10, Some("foo bar")),    // a boundary before "bar"
    (11, Some("")),           // empty string
    (12, None),               // NULL → every REGEXP/NOT REGEXP is NULL
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
        "{dir}/mpedb-regexp-{}-{}.mpedb",
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

/// Canonical, engine-agnostic cell rendering: must match how the `sqlite3` CLI's
/// default "list" mode prints the same value — NULL as empty, a boolean (all
/// REGEXP ever produces) as sqlite's 1/0, text verbatim.
fn render(v: Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => if b { "1" } else { "0" }.to_string(),
        Value::Text(s) => s,
        other => panic!("unexpected value in REGEXP test: {other:?}"),
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
/// parse its default list-mode output into rows.
///
/// DELIBERATELY the system binary, not the bundled oracle
/// (`tests/sqlite_oracle/mod.rs`) every other differential test uses: the
/// `regexp()` function is NOT part of the sqlite library — it is
/// `ext/misc/regexp.c`, compiled into the SHELL — so the bundled library
/// cannot answer REGEXP queries at all. This file is the one exemption, and
/// the only differential left that still requires a `sqlite3` on PATH.
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
    // Default list mode: one row per line, columns joined by '|', NULL empty.
    // Every query below selects `id` first (never NULL), so no wanted row is a
    // blank line.
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// A battery of REGEXP / NOT REGEXP queries, in the SELECT list and as a WHERE
/// predicate, exercising anchors, `.`, quantifiers, counts, classes,
/// alternation, the Perl escapes and `\b`, escaped metacharacters,
/// case-sensitivity and NULL — each must match sqlite 3.45.
#[test]
fn regexp_matches_sqlite_3_45() {
    let d = db();
    let queries = [
        // Unanchored substring, then `^`/`$` anchors in the projection.
        "SELECT id, s REGEXP 'bc' FROM t ORDER BY id",
        "SELECT id, s REGEXP '^a' FROM t ORDER BY id",
        "SELECT id, s REGEXP 'c$' FROM t ORDER BY id",
        // `.` = any one char; a `.*` run; a `{p}` count.
        "SELECT id, s REGEXP '^a.c$' FROM t ORDER BY id",
        "SELECT id, s REGEXP '^a.*c$' FROM t ORDER BY id",
        "SELECT id, s REGEXP '^.{3}$' FROM t ORDER BY id",
        // Character classes: range, and a negated class with a space.
        "SELECT id, s REGEXP '^[a-c]+$' FROM t ORDER BY id",
        "SELECT id, s REGEXP '[^a-z ]' FROM t ORDER BY id",
        // Alternation + grouping.
        "SELECT id, s REGEXP '^(abc|123)$' FROM t ORDER BY id",
        // Perl classes and word boundary.
        "SELECT id, s REGEXP '\\d' FROM t ORDER BY id",
        "SELECT id, s REGEXP '\\bworld\\b' FROM t ORDER BY id",
        "SELECT id, s REGEXP '\\bbar' FROM t ORDER BY id",
        // An escaped metacharacter: literal `.` matches only the "a.c" row.
        "SELECT id, s REGEXP '^a\\.c$' FROM t ORDER BY id",
        // Case-SENSITIVE: '^abc$' must NOT match the "Abc" row.
        "SELECT id, s REGEXP '^abc$' FROM t ORDER BY id",
        // NOT REGEXP in the projection (NULL row stays NULL → empty cell).
        "SELECT id, s NOT REGEXP '^a' FROM t ORDER BY id",
        // As a WHERE predicate: NULL denies, so the NULL row drops out.
        "SELECT id FROM t WHERE s REGEXP '^a.c$' ORDER BY id",
        "SELECT id FROM t WHERE s NOT REGEXP '\\d' ORDER BY id",
        "SELECT id FROM t WHERE s REGEXP '^[a-c]+$' ORDER BY id",
        // Combined with other logic.
        "SELECT id FROM t WHERE s REGEXP 'a' AND id < 6 ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), sqlite_rows(q), "mismatch on `{q}`");
    }
}

/// The properties asserted directly on the `Value` (not only via the string
/// cross-check): case-sensitivity, unanchored substring matching, that a NULL
/// operand propagates through both REGEXP and NOT REGEXP, and that `NOT REGEXP`
/// is the exact negation on non-NULL rows.
#[test]
fn regexp_null_and_case_direct() {
    let d = db();

    let one = |sql: &str| -> Value {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };

    // Case-SENSITIVE: "abc" matches '^abc$'; "Abc" does not.
    assert_eq!(one("SELECT s REGEXP '^abc$' FROM t WHERE id = 1"), Value::Bool(true));
    assert_eq!(one("SELECT s REGEXP '^abc$' FROM t WHERE id = 2"), Value::Bool(false));

    // Unanchored: 'bar' matches anywhere inside "foobar".
    assert_eq!(one("SELECT s REGEXP 'bar' FROM t WHERE id = 9"), Value::Bool(true));
    // `\b` requires a boundary: "foobar" has none before "bar", "foo bar" does.
    assert_eq!(one("SELECT s REGEXP '\\bbar' FROM t WHERE id = 9"), Value::Bool(false));
    assert_eq!(one("SELECT s REGEXP '\\bbar' FROM t WHERE id = 10"), Value::Bool(true));

    // An escaped `.` is a literal dot: matches "a.c", not "aXc".
    assert_eq!(one("SELECT s REGEXP '^a\\.c$' FROM t WHERE id = 5"), Value::Bool(true));
    assert_eq!(one("SELECT s REGEXP '^a\\.c$' FROM t WHERE id = 3"), Value::Bool(false));
    // An UNescaped `.` is any char: matches "aXc".
    assert_eq!(one("SELECT s REGEXP '^a.c$' FROM t WHERE id = 3"), Value::Bool(true));

    // NOT REGEXP is the exact negation on a non-NULL row.
    assert_eq!(one("SELECT s NOT REGEXP '^a' FROM t WHERE id = 1"), Value::Bool(false));
    assert_eq!(one("SELECT s NOT REGEXP '^a' FROM t WHERE id = 6"), Value::Bool(true));

    // Row 12 is NULL: both REGEXP and NOT REGEXP propagate NULL (NOT of NULL is
    // NULL), exactly as GLOB/LIKE do.
    assert_eq!(one("SELECT s REGEXP '^a' FROM t WHERE id = 12"), Value::Null);
    assert_eq!(one("SELECT s NOT REGEXP '^a' FROM t WHERE id = 12"), Value::Null);
}

/// #74 item 3 — the pattern bound as a PARAMETER must give exactly what the
/// same pattern written as a literal gives (and therefore what sqlite gives).
///
/// The old restriction ("REGEXP pattern must be a literal") was STRUCTURAL, not
/// a performance guard: the pattern lived in the plan's const pool because LIKE
/// and GLOB put it there, and `regexp_match` recompiled it on every row anyway.
/// It now memoizes the last pattern per thread, so the bound form costs what the
/// literal form costs.
#[test]
fn bound_pattern_matches_the_literal_form() {
    let d = db();
    let patterns = [
        "bc", "^a", "c$", "^a.c$", "a.*c", "^a{2}b", "[abc]", "[^abc]", "[0-9]+", "a|z", "(ab)+",
        "\\d", "\\w+", "\\s", "\\bbar", "^a\\.c$", "", "zzz", "A",
    ];
    for pat in patterns {
        // The same predicate, once with the pattern inline and once bound.
        let lit = format!("SELECT id, s REGEXP '{pat}', s NOT REGEXP '{pat}' FROM t ORDER BY id");
        let literal_rows = mpedb_rows(&d, &lit);
        // sqlite must agree with the literal form first — otherwise the
        // comparison below would be against a wrong baseline.
        assert_eq!(literal_rows, sqlite_rows(&lit), "literal form diverged for /{pat}/");

        let bound = match d
            .query(
                "SELECT id, s REGEXP ?, s NOT REGEXP ? FROM t ORDER BY id",
                &[Value::Text(pat.into()), Value::Text(pat.into())],
            )
            .unwrap()
        {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| r.into_iter().map(render).collect::<Vec<String>>())
                .collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(bound, literal_rows, "bound pattern diverged for /{pat}/");
    }
}

/// The corners of the dynamic form that the literal form cannot reach.
#[test]
fn dynamic_pattern_nulls_columns_and_refusals() {
    let d = db();
    let one = |sql: &str, params: &[Value]| -> Value {
        match d.query(sql, params).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };

    // A NULL PATTERN propagates, on both sides and through NOT — the literal
    // form could never express this, and sqlite answers NULL for it.
    assert_eq!(one("SELECT s REGEXP ? FROM t WHERE id = 1", &[Value::Null]), Value::Null);
    assert_eq!(one("SELECT s NOT REGEXP ? FROM t WHERE id = 1", &[Value::Null]), Value::Null);
    // A NULL subject with a bound pattern, and NULL on both sides.
    assert_eq!(
        one("SELECT s REGEXP ? FROM t WHERE id = 12", &[Value::Text("a".into())]),
        Value::Null
    );
    assert_eq!(one("SELECT s REGEXP ? FROM t WHERE id = 12", &[Value::Null]), Value::Null);

    // A COLUMN as the pattern: a string matches itself as a pattern only when
    // it has no metacharacters, so this is asserted per row against sqlite
    // rather than assumed.
    let q = "SELECT id, s REGEXP s FROM t ORDER BY id";
    assert_eq!(mpedb_rows(&d, q), sqlite_rows(q));

    // A statically non-text pattern is refused by name rather than coerced.
    let e = d
        .query("SELECT s REGEXP 3 FROM t", &[])
        .expect_err("an integer pattern must not bind")
        .to_string();
    assert!(e.contains("REGEXP pattern must be text"), "{e}");
    let e = d
        .query("SELECT s REGEXP ? FROM t", &[Value::Int(3)])
        .expect_err("an integer parameter must not bind to the pattern slot")
        .to_string();
    assert!(e.contains("statement requires text"), "{e}");

    // The memo is keyed on the pattern VALUE, not on the statement: alternating
    // patterns through one prepared plan must not answer from a stale program.
    for (pat, id, want) in [
        ("^abc$", 1, true),
        ("^zzz$", 1, false),
        ("^abc$", 1, true),
        ("^a", 6, false),
        ("^abc$", 1, true),
    ] {
        assert_eq!(
            one(&format!("SELECT s REGEXP ? FROM t WHERE id = {id}"), &[Value::Text(pat.into())]),
            Value::Bool(want),
            "/{pat}/ against row {id}"
        );
    }
}

// ================== host regexp() dispatch (task #108, W3 part 2) ==================

/// The consumer dialect these tests register on BOTH engines: a `(?i)` prefix
/// means case-insensitive, and the REST of the pattern is a LITERAL substring —
/// no metacharacters at all. Deliberately divergent from the native NFA on
/// patterns that are valid in both dialects (`a.c` here is the three characters
/// `a.c`; the NFA reads `a<any>c`, `^a` here is the two characters `^a`; the
/// NFA reads an anchor) — the exact wrongness mode W3 named: whenever the
/// dialects diverge, the answer must come from the HOST.
fn consumer_regexp(pattern: &str, text: &str) -> bool {
    match pattern.strip_prefix("(?i)") {
        Some(p) => text.to_lowercase().contains(&p.to_lowercase()),
        None => text.contains(pattern),
    }
}

/// Register `consumer_regexp` on the mpedb connection, with sqlite's UDF NULL
/// convention (Django's `_sqlite_regexp` returns None on a NULL argument).
fn register_consumer_regexp(db: &Database) {
    db.register_host_function("regexp", 2, |args: &[Value]| match (&args[0], &args[1]) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(p), Value::Text(s)) => Ok(Value::Bool(consumer_regexp(p, s))),
        (p, s) => Err(Error::TypeMismatch(format!(
            "test regexp() wants (text pattern, text subject), got ({p:?}, {s:?}) — \
             the operator's argument order is (pattern, text)"
        ))),
    });
}

/// The same query against the BUNDLED library with the same closure registered
/// as its `regexp()` — the consumer contract `x REGEXP y` desugars into.
fn oracle_rows_with_regexp(query: &str) -> Vec<Vec<String>> {
    let mut script = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT);\n");
    for stmt in insert_statements() {
        script.push_str(&stmt);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");
    let out = sqlite_oracle::script_stdout_with(&script, "", |conn| {
        use rusqlite::functions::FunctionFlags;
        conn.create_scalar_function(
            "regexp",
            2,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let p = ctx.get::<Option<String>>(0)?;
                let s = ctx.get::<Option<String>>(1)?;
                Ok(match (p, s) {
                    (Some(p), Some(s)) => Some(consumer_regexp(&p, &s)),
                    _ => None,
                })
            },
        )
        .expect("register regexp() on the oracle");
    });
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// With a host `regexp/2` registered, `x REGEXP y` IS that call — argument
/// order `(pattern, text)`, host dialect beating the native NFA, `NOT REGEXP`
/// as sqlite's 3VL NOT over the call's truthiness — differentially against the
/// bundled library running the identical closure.
#[test]
fn host_regexp_dispatch_matches_bundled_sqlite() {
    let d = db();
    register_consumer_regexp(&d);

    let queries = [
        // Patterns VALID IN BOTH dialects with DIVERGENT answers — the W3
        // mode. `a.c` (host: literal dot → row 5 only; NFA would say rows
        // 1,3,4,5,8) and `^a` (host: literal caret → nothing; NFA: anchor).
        "SELECT id, s REGEXP 'a.c' FROM t ORDER BY id",
        "SELECT id, s REGEXP '^a' FROM t ORDER BY id",
        "SELECT id FROM t WHERE s REGEXP 'a.c' ORDER BY id",
        // The host-only surface: `(?i)…` — every Django `__iregex`.
        "SELECT id, s REGEXP '(?i)ABC' FROM t ORDER BY id",
        "SELECT id FROM t WHERE s REGEXP '(?i)fo' ORDER BY id",
        // Plain hit through the host engine + the empty-string row (an
        // argument swap would flip row 11: '' ⊆ 'bc' but 'bc' ⊄ '').
        "SELECT id, s REGEXP 'bc' FROM t ORDER BY id",
        // The COLUMN as the pattern — asymmetric on purpose: the host must
        // receive (s, 'abc'), i.e. "is s a substring of 'abc'".
        "SELECT id, 'abc' REGEXP s FROM t ORDER BY id",
        // NOT REGEXP: 3VL NOT over the call (NULL row stays NULL), in the
        // projection and as a predicate.
        "SELECT id, s NOT REGEXP 'a.c' FROM t ORDER BY id",
        "SELECT id FROM t WHERE s NOT REGEXP '(?i)b' ORDER BY id",
        // Combined with other logic in a boolean position.
        "SELECT id FROM t WHERE s REGEXP 'o' AND id > 6 ORDER BY id",
    ];
    for q in queries {
        assert_eq!(mpedb_rows(&d, q), oracle_rows_with_regexp(q), "mismatch on `{q}`");
    }

    // The bound form Django actually emits: the pattern as a parameter takes
    // the identical host path (the oracle sees the same pattern inline —
    // sqlite makes no distinction).
    let bound = |pat: &str| -> Vec<Vec<String>> {
        match d
            .query(
                "SELECT id FROM t WHERE s REGEXP ? ORDER BY id",
                &[Value::Text(pat.into())],
            )
            .unwrap()
        {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| r.into_iter().map(render).collect())
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    for pat in ["(?i)ABC", "a.c", "bc", "(?i)FO"] {
        let q = format!("SELECT id FROM t WHERE s REGEXP '{pat}' ORDER BY id");
        assert_eq!(bound(pat), oracle_rows_with_regexp(&q), "bound /{pat}/ diverged");
    }

    // Direct 3VL pins (not only via rendering): a NULL subject propagates
    // through the call and through NOT.
    let one = |sql: &str| -> Value {
        match d.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => {
                rows.into_iter().next().unwrap().into_iter().next().unwrap()
            }
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(one("SELECT s REGEXP 'a' FROM t WHERE id = 12"), Value::Null);
    assert_eq!(one("SELECT s NOT REGEXP 'a' FROM t WHERE id = 12"), Value::Null);

    // Truthiness is the UDF-in-boolean-position rule, not `= 1`: re-register
    // regexp() (replacement drops the plan cache) to return an occurrence
    // COUNT — sqlite passes WHERE on any non-zero — and differential again.
    d.register_host_function("regexp", 2, |args: &[Value]| match (&args[0], &args[1]) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(p), Value::Text(s)) => {
            Ok(Value::Int(s.matches(p.as_str()).count() as i64))
        }
        _ => Err(Error::TypeMismatch("count regexp() wants text".into())),
    });
    let count_oracle = |query: &str| -> Vec<Vec<String>> {
        let mut script = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT);\n");
        for stmt in insert_statements() {
            script.push_str(&stmt);
            script.push_str(";\n");
        }
        script.push_str(query);
        script.push_str(";\n");
        let out = sqlite_oracle::script_stdout_with(&script, "", |conn| {
            use rusqlite::functions::FunctionFlags;
            conn.create_scalar_function(
                "regexp",
                2,
                FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
                |ctx| {
                    let p = ctx.get::<Option<String>>(0)?;
                    let s = ctx.get::<Option<String>>(1)?;
                    Ok(match (p, s) {
                        (Some(p), Some(s)) => Some(s.matches(p.as_str()).count() as i64),
                        _ => None,
                    })
                },
            )
            .expect("register counting regexp() on the oracle");
        });
        out.lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('|').map(str::to_string).collect())
            .collect()
    };
    for q in [
        // "hello world"/"foobar"/"foo bar" hold 2 o's: a `= 1` bridge would
        // drop them, truthiness keeps them.
        "SELECT id, s REGEXP 'o' FROM t ORDER BY id",
        "SELECT id FROM t WHERE s REGEXP 'o' ORDER BY id",
        "SELECT id FROM t WHERE s NOT REGEXP 'o' ORDER BY id",
    ] {
        assert_eq!(mpedb_rows(&d, q), count_oracle(q), "count-truthiness mismatch on `{q}`");
    }
}

/// Precedence is LIVE, not a compile-time fork of the connection: without a
/// registration the native NFA answers (mpedb's documented extension — plain
/// sqlite would error `no such function: regexp`), registering flips the SAME
/// query to the host meaning, unregistering flips it back. Each flip crosses
/// the plan cache, so this also pins that registration changes invalidate
/// cached REGEXP plans.
#[test]
fn host_registration_flips_the_operator_and_back() {
    let d = db();
    let q = "SELECT id FROM t WHERE s REGEXP 'a.c' ORDER BY id";
    // Native NFA: `.` is any char, so rows 1,3,4,5,8.
    let native: Vec<Vec<String>> = ["1", "3", "4", "5", "8"]
        .iter()
        .map(|id| vec![id.to_string()])
        .collect();
    // Host literal-substring dialect: only the actual "a.c" row.
    let host: Vec<Vec<String>> = vec![vec!["5".to_string()]];

    assert_eq!(mpedb_rows(&d, q), native, "pre-registration must be the native dialect");
    register_consumer_regexp(&d);
    assert_eq!(mpedb_rows(&d, q), host, "a registered regexp/2 must own the operator");
    assert!(d.unregister_host_function("regexp", 2));
    assert_eq!(mpedb_rows(&d, q), native, "unregistering must restore the native dialect");

    // The W3 part-1 contract holds on each side of the flip: a pattern outside
    // the native dialect is a NAMED error without a host regexp, and the
    // host's answer with one.
    let e = d
        .query("SELECT id FROM t WHERE s REGEXP '(?i)fo' ORDER BY id", &[])
        .expect_err("(?i) is not in the native dialect")
        .to_string();
    assert!(e.contains("not valid in mpedb's regexp dialect"), "{e}");
    register_consumer_regexp(&d);
    assert_eq!(
        mpedb_rows(&d, "SELECT id FROM t WHERE s REGEXP '(?i)fo' ORDER BY id"),
        vec![vec!["9".to_string()], vec!["10".to_string()]],
        "with a host regexp the (?i) pattern must answer rows"
    );
}

/// A REGEXP that compiled into a host call is connection-local: it must never
/// reach the shared `plan/<hash>` registry, and a second handle (no UDFs) must
/// fail CLOSED on the hash — `UnknownPlan`, never the native dialect's answer.
/// The same SQL compiled WITHOUT a registration is an ordinary shareable plan,
/// so the containment above is not vacuous.
#[test]
fn host_regexp_plans_stay_connection_local() {
    let d = db();
    register_consumer_regexp(&d);

    let sql = "SELECT id FROM t WHERE s REGEXP ? ORDER BY id";
    let h = d.prepare(sql).unwrap();
    // The registering connection executes it from its local cache.
    match d.execute(&h, &[Value::Text("(?i)ABC".into())]).unwrap() {
        ExecResult::Rows { rows, .. } => assert_eq!(rows.len(), 2),
        other => panic!("{other:?}"),
    }

    // A fresh handle onto the same file, no UDFs: the hash must not resolve.
    let db2 = Database::open_from_file(std::path::Path::new(&d.path)).expect("second handle");
    match db2.execute(&h, &[Value::Text("(?i)ABC".into())]) {
        Err(Error::UnknownPlan(_)) => {}
        other => panic!("host-REGEXP plan leaked into the shared registry: {other:?}"),
    }
    drop(db2);

    // Without a host regexp the SAME text compiles to the native opcode and
    // shares normally — the extension keeps its multi-process contract.
    assert!(d.unregister_host_function("regexp", 2));
    let h_native = d.prepare(sql).unwrap();
    let db3 = Database::open_from_file(std::path::Path::new(&d.path)).expect("third handle");
    match db3.execute(&h_native, &[Value::Text("^a.c$".into())]) {
        // `^a.c$` in the NATIVE dialect: rows 1,3,4,5,8 (`.` = any char).
        Ok(ExecResult::Rows { rows, .. }) => {
            assert_eq!(rows.len(), 5, "native plan must be shared and answer");
        }
        other => panic!("a native REGEXP plan must still publish: {other:?}"),
    }
}
