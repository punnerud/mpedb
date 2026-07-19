//! `printf()` / `format()` — sqlite's C-printf-style string formatter, every
//! supported specifier / flag / width / precision cross-checked against the real
//! `sqlite3` 3.45 CLI.
//!
//! The comparison is differential: the SAME `SELECT <expr>` string runs against
//! mpedb and against `sqlite3 :memory:`, and the single rendered result must
//! match. NULL is disambiguated with `-nullvalue <NULL>` on the sqlite side and
//! the identical sentinel on the mpedb side, so `printf('')` (NULL) is
//! distinguished from `printf('%s','')` (the empty string) — a real sqlite
//! distinction this function has to reproduce.
//!
//! `format()` is exercised as an exact alias for `printf()`, and a handful of
//! deliberate deviations (a NULL/empty format is NULL; the format must be text;
//! an untyped bare parameter in a data slot is refused) are asserted directly.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

const NULL_SENTINEL: &str = "<NULL>";

fn mpedb_db() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-printf-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    // A single-row table carries one value of each type (plus NULLs), so the
    // per-specifier argument coercion is exercised over real typed columns, not
    // only literals.
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
  name = "i"
  type = "int64"
  nullable = true

  [[table.column]]
  name = "f"
  type = "float64"
  nullable = true

  [[table.column]]
  name = "s"
  type = "text"
  nullable = true
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    db.query(
        "INSERT INTO t (id, i, f, s) VALUES (1, 255, 3.14159, 'O''Brien')",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO t (id, i, f, s) VALUES (2, NULL, NULL, NULL)", &[])
        .unwrap();
    (db, path)
}

fn sqlite_setup() -> String {
    let mut s = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, i INTEGER, f REAL, s TEXT);\n");
    s.push_str("INSERT INTO t (id, i, f, s) VALUES (1, 255, 3.14159, 'O''Brien');\n");
    s.push_str("INSERT INTO t (id, i, f, s) VALUES (2, NULL, NULL, NULL);\n");
    s
}

/// Render one mpedb value the way the sqlite CLI (`-nullvalue <NULL>`) prints
/// it: NULL as the sentinel, everything else verbatim. printf always yields
/// text or NULL, so those two arms suffice.
fn render(v: &Value) -> String {
    match v {
        Value::Null => NULL_SENTINEL.to_string(),
        Value::Text(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => x.to_string(),
        other => format!("{other:?}"),
    }
}

fn mpedb_rows(db: &Database, query: &str) -> Vec<String> {
    match db.query(query, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows.iter().map(|r| render(&r[0])).collect(),
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<String> {
    let mut input = sqlite_setup();
    input.push_str(query);
    input.push_str(";\n");
    sqlite_oracle::script_stdout(&input, NULL_SENTINEL)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// The same query must produce the same rows in both engines.
fn cross_check(db: &Database, query: &str) {
    let m = mpedb_rows(db, query);
    let s = sqlite_rows(query);
    assert_eq!(m, s, "mpedb vs sqlite disagree on `{query}`");
}

/// Batched differential check: run every `SELECT <expr>` (each yielding one
/// single-column row whose text has no newline) in ONE sqlite invocation, and
/// each against mpedb, then compare line for line. Far fewer process spawns
/// than one-at-a-time, so a broad sweep stays fast.
fn cross_check_batch(db: &Database, queries: &[String]) {
    // sqlite: all SELECTs in one script; one output line per query.
    let mut input = sqlite_setup();
    for q in queries {
        input.push_str(q);
        input.push_str(";\n");
    }
    let s_lines: Vec<String> = sqlite_oracle::script_stdout(&input, NULL_SENTINEL)
        .lines()
        .map(|l| l.to_string())
        .collect();
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

#[test]
fn printf_matches_sqlite() {
    let (db, path) = mpedb_db();

    // Every case is a FROM-less SELECT (one synthetic row); the value is text or
    // NULL and rendered identically on both sides.
    let cases: &[&str] = &[
        // --- integers: %d %i %u %x %X %o ---
        "SELECT printf('%d', 42)",
        "SELECT printf('%i', 42)",
        "SELECT printf('%d', -42)",
        "SELECT printf('%d', 0)",
        "SELECT printf('%u', -1)",
        "SELECT printf('%x', 255)",
        "SELECT printf('%X', 255)",
        "SELECT printf('%o', 8)",
        "SELECT printf('%x', -1)",
        "SELECT printf('%o', -1)",
        "SELECT printf('%#x', 255)",
        "SELECT printf('%#X', 255)",
        "SELECT printf('%#o', 8)",
        "SELECT printf('%#x', 0)",
        "SELECT printf('%#o', 0)",
        "SELECT printf('%d', 9223372036854775807)",
        "SELECT printf('%d', -9223372036854775807)",
        // --- integer coercion: text / float / bool arguments ---
        "SELECT printf('%d', 'abc')",
        "SELECT printf('%d', '12abc')",
        "SELECT printf('%d', '  42xyz')",
        "SELECT printf('%d', '+42')",
        "SELECT printf('%d', '-5')",
        "SELECT printf('%d', '3.9')",
        "SELECT printf('%d', '1e3')",
        "SELECT printf('%d', '0x10')",
        "SELECT printf('%d', 2.9)",
        "SELECT printf('%d', -2.9)",
        "SELECT printf('%d', 1e3)",
        "SELECT printf('%d', '99999999999999999999999')",
        "SELECT printf('%d', 1=1)",
        "SELECT printf('%d', 1=2)",
        "SELECT printf('%x', 15.9)",
        "SELECT printf('%x', 'abc')",
        // --- widths, precisions, flags on integers ---
        "SELECT printf('%5d', 3)",
        "SELECT printf('%-5d|', 3)",
        "SELECT printf('%05d', 3)",
        "SELECT printf('%05d', -3)",
        "SELECT printf('[%-05d]', 3)",
        "SELECT printf('%+d', 3)",
        "SELECT printf('%+d', 0)",
        "SELECT printf('% d', 3)",
        "SELECT printf('%.5d', 42)",
        "SELECT printf('%.0d', 0)",
        "SELECT printf('%.5x', 255)",
        "SELECT printf('%8.5d', 42)",
        // --- the thousands separator ---
        "SELECT printf('%,d', 1000000)",
        "SELECT printf('%,d', -1000000)",
        "SELECT printf('%,d', 100)",
        "SELECT printf('%,d', 1234567)",
        // --- star width / precision ---
        "SELECT printf('%*d', 5, 3)",
        "SELECT printf('[%*d]', -5, 3)",
        "SELECT printf('%.*f', 2, 3.14159)",
        "SELECT printf('[%.*f]', -1, 3.14159)",
        "SELECT printf('%*.*f', 8, 2, 3.14159)",
        // --- floats: %f %e %E %g %G ---
        "SELECT printf('%f', 3.14159)",
        "SELECT printf('%.3f', 2)",
        "SELECT printf('%.3f', 3.14159)",
        "SELECT printf('%.0f', 0.5)",
        "SELECT printf('%.0f', 1.5)",
        "SELECT printf('%.0f', 2.5)",
        "SELECT printf('%.0f', 3.5)",
        "SELECT printf('%.0f', 4.5)",
        "SELECT printf('%.1f', 0.25)",
        "SELECT printf('%.1f', 0.35)",
        "SELECT printf('%.1f', 0.45)",
        "SELECT printf('%.2f', 0.125)",
        "SELECT printf('%.2f', 0.375)",
        "SELECT printf('%f', 'abc')",
        "SELECT printf('%+.2f', 3.0)",
        "SELECT printf('%08.2f', 3.14)",
        "SELECT printf('%08.2f', -3.14)",
        "SELECT printf('%e', 12345.678)",
        "SELECT printf('%E', 12345.678)",
        "SELECT printf('%.3e', 12345.678)",
        "SELECT printf('%e', 1000.0)",
        "SELECT printf('%e', 0.000123)",
        "SELECT printf('%g', 12345.678)",
        "SELECT printf('%G', 0.0000123)",
        "SELECT printf('%g', 0.0001)",
        "SELECT printf('%g', 0.00001)",
        "SELECT printf('%g', 123456)",
        "SELECT printf('%g', 1234567)",
        "SELECT printf('%g', 100.0)",
        "SELECT printf('%g', 100000000)",
        "SELECT printf('%#g', 100.0)",
        "SELECT printf('%g', 3.14159265358979)",
        "SELECT printf('%.10g', 0.3333333333333333)",
        "SELECT printf('%f', 1e20)",
        "SELECT printf('%.17f', 0.1)",
        "SELECT printf('%f', 0.0)",
        "SELECT printf('%f', -0.0)",
        // --- %c: first character of the argument's text ---
        "SELECT printf('%c', 65)",
        "SELECT printf('%c', 97)",
        "SELECT printf('%c', 'A')",
        "SELECT printf('%c', 'hello')",
        "SELECT printf('%c', 3.5)",
        "SELECT printf('[%5c]', 'x')",
        "SELECT printf('[%-5c]', 'x')",
        "SELECT printf('[%.3c]', 'ab')",
        // --- %s and precision ---
        "SELECT printf('%s', 'hi')",
        "SELECT printf('%s', 42)",
        "SELECT printf('%s', 3.14)",
        "SELECT printf('%s', 3.0)",
        "SELECT printf('%s', 1=1)",
        "SELECT printf('%s', NULL)",
        "SELECT printf('%.3s', 'hello')",
        "SELECT printf('[%10s]', 'hi')",
        "SELECT printf('[%-10s]', 'hi')",
        // --- %q %Q %w escapes ---
        "SELECT printf('%q', 'O''Brien')",
        "SELECT printf('%Q', 'O''Brien')",
        "SELECT printf('%q', NULL)",
        "SELECT printf('%Q', NULL)",
        "SELECT printf('%w', NULL)",
        "SELECT printf('%q', '')",
        "SELECT printf('%Q', '')",
        "SELECT printf('%w', 'a\"b')",
        "SELECT printf('%.3q', 'abcdef')",
        // --- literal percent and plain text ---
        "SELECT printf('100%%')",
        "SELECT printf('%5%')",
        "SELECT printf('no specifiers')",
        "SELECT printf('%d/%d/%d = %.2f%%', 3, 4, 12, 25.0)",
        // --- missing / extra arguments ---
        "SELECT printf('%d %d', 5)",
        "SELECT printf('%d')",
        "SELECT printf('%s %s', 'a')",
        "SELECT printf('%q')",
        "SELECT printf('%Q')",
        "SELECT printf('%d', 5, 6, 7)",
        // --- NULL / empty format ---
        "SELECT printf('')",
        "SELECT printf('%s', '')",
        "SELECT printf('%c', NULL)",
        // --- invalid / trailing specifiers ---
        "SELECT printf('a%yb')",
        "SELECT printf('abc%')",
        // --- long-flag no-ops ---
        "SELECT printf('%lld', 42)",
        "SELECT printf('%ld', 42)",
        // --- the `!` alt form: char-based width/precision for strings, and
        //     26 significant digits for floats ---
        "SELECT printf('[%!5s]', 'æ')",
        "SELECT printf('[%!.2s]', 'æøå')",
        "SELECT printf('[%!5c]', 'æ')",
        "SELECT printf('%!.3q', 'æøåxy')",
        "SELECT printf('%!d', 3)",
        "SELECT printf('%!.6f', 3.14159265358979)",
        "SELECT printf('%!g', 12345.678)",
        // --- format() is an exact alias ---
        "SELECT format('%d-%s', 5, 'x')",
        "SELECT format('%,d', 1000000)",
        "SELECT format('%.3f', 3.14159)",
        "SELECT format('')",
    ];
    for c in cases {
        cross_check(&db, c);
    }

    // Column-driven coercion: the format argument is a real typed column
    // (int64, float64, text), including the all-NULL row.
    cross_check(&db, "SELECT printf('%d|%.2f|%s', i, f, s) FROM t ORDER BY id");
    cross_check(&db, "SELECT printf('%x', i) FROM t ORDER BY id");
    cross_check(&db, "SELECT printf('%q', s) FROM t ORDER BY id");
    cross_check(&db, "SELECT printf('%Q', s) FROM t ORDER BY id");
    cross_check(&db, "SELECT printf('%08.3f', f) FROM t ORDER BY id");
    cross_check(&db, "SELECT printf('%c', s) FROM t ORDER BY id");
    cross_check(&db, "SELECT format('[%5d]', i) FROM t ORDER BY id");

    let _ = std::fs::remove_file(&path);
}

/// A broad deterministic sweep (no rand): every float format specifier over a
/// wide set of values × precisions, plus the integer specifiers over a set of
/// magnitudes and flags. This is the load-bearing coverage for the ported
/// `sqlite3FpDecode` — the double-double float decoder has to match sqlite's
/// long-double CLI bit for bit.
// The value set includes numbers near PI/E purely as formatting inputs.
#[allow(clippy::approx_constant)]
#[test]
fn printf_float_and_int_sweep_matches_sqlite() {
    let (db, path) = mpedb_db();

    // A deterministic value set: halves, unit fractions, sevenths, and a fixed
    // list of notable constants. Rendered with `{:?}` so the SQL literal is the
    // exact same double sqlite parses.
    let mut vals: Vec<f64> = Vec::new();
    for k in 0..40 {
        vals.push(k as f64 + 0.5);
        vals.push(-(k as f64) - 0.5);
    }
    for d in 1..=40 {
        vals.push(1.0 / d as f64);
        vals.push(d as f64 / 7.0);
    }
    for &c in &[
        3.14159265358979,
        2.718281828459045,
        0.1,
        0.2,
        0.3,
        123456.789,
        9999999.5,
        0.125,
        0.375,
        0.625,
        1234567.0,
        100000000.0,
        0.0001,
        0.00001,
        2.0 / 3.0,
        1.005,
        2.675,
        1e15,
        1e-15,
        0.0,
    ] {
        vals.push(c);
    }

    let float_specs = [
        "%f", "%.0f", "%.1f", "%.2f", "%.3f", "%.6f", "%.10f", "%.15f", "%e", "%.2e", "%.10e",
        "%E", "%g", "%.10g", "%.17g", "%G", "%+.4f", "%#g",
    ];
    let mut queries: Vec<String> = Vec::new();
    for &v in &vals {
        for spec in float_specs {
            // {v:?} yields a round-trippable literal (e.g. "0.5", "-0.5", "1e15")
            // that parses to the identical f64 in both engines.
            queries.push(format!("SELECT printf('{spec}', {v:?})"));
        }
    }

    // Integers: a set of magnitudes (incl. negatives and boundaries) over the
    // integer specifiers and flags.
    let ints: [i64; 14] = [
        0,
        1,
        -1,
        7,
        8,
        255,
        1000,
        -1000,
        1000000,
        -1000000,
        1234567,
        9223372036854775807,
        -42,
        123,
    ];
    let int_specs = [
        "%d", "%i", "%u", "%5d", "%-5d", "%05d", "%+d", "% d", "%,d", "%.5d", "%x", "%X", "%o",
        "%#x", "%#X", "%#o", "%8x", "%-8x|", "%08x",
    ];
    for &v in &ints {
        for spec in int_specs {
            queries.push(format!("SELECT printf('{spec}', {v})"));
        }
    }

    cross_check_batch(&db, &queries);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn printf_null_and_empty_format_return_null() {
    let (db, path) = mpedb_db();
    let val = |sql: &str| match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows[0][0].clone(),
        other => panic!("{other:?}"),
    };
    // A NULL format argument and an empty format string both yield NULL.
    assert_eq!(val("SELECT printf(NULL)"), Value::Null);
    assert_eq!(val("SELECT printf(NULL, 1, 2)"), Value::Null);
    assert_eq!(val("SELECT printf('')"), Value::Null);
    assert_eq!(val("SELECT format('')"), Value::Null);
    // A data argument that is NULL does NOT propagate: `%s` of NULL is the empty
    // string (text, not NULL), and `%d` of NULL is 0.
    assert_eq!(val("SELECT printf('%s', NULL)"), Value::Text(String::new()));
    assert_eq!(val("SELECT printf('%d', NULL)"), Value::Text("0".into()));

    // Documented deviation: the `!` alt form asks the float decoder for up to 26
    // significant digits, and beyond ~17 the portable double-double decoder (which
    // mpedb uses so its output is identical on every platform) can differ from a
    // long-double sqlite build in the last digit(s). mpedb's value is
    // deterministic; this asserts it directly rather than cross-checking, since a
    // long-double sqlite would print `0.666666666666666629`. Ordinary `%!`
    // usage (string char-width, floats up to ~17 significant digits) matches.
    assert_eq!(
        val("SELECT printf('%!.20g', 0.6666666666666666)"),
        Value::Text("0.6666666666666666296".into())
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn printf_binding_rules() {
    let (db, path) = mpedb_db();
    // The format string must be text (sqlite coerces a non-text format; mpedb is
    // rigid and refuses it — a clean compile error, never a wrong answer).
    assert!(db.query("SELECT printf(42, 'x')", &[]).is_err());
    // At least the format argument is required.
    assert!(db.query("SELECT printf()", &[]).is_err());
    assert!(db.query("SELECT format()", &[]).is_err());
    // An untyped bare parameter in a data slot cannot be typed (the specifier
    // that consumes it is only known at runtime) — refused, with a CAST as the
    // fix; a typed literal or column works.
    assert!(db.query("SELECT printf('%d', $1)", &[]).is_err());
    // Both spellings resolve to the same function.
    let same = |a: &str, b: &str| {
        let g = |sql: &str| match db.query(sql, &[]).unwrap() {
            ExecResult::Rows { rows, .. } => rows[0][0].clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(g(a), g(b));
    };
    same("SELECT printf('%05.2f', 3.14159)", "SELECT format('%05.2f', 3.14159)");
    let _ = std::fs::remove_file(&path);
}
