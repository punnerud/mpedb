//! `quote(X)` — the SQL literal denoting `X` — cross-checked value-by-value
//! against the real `sqlite3` CLI 3.45.
//!
//! `quote()` is what Django's `last_executed_query` calls (`SELECT QUOTE(?)`
//! once per bound parameter), so it has to be right for every storage class,
//! and the REAL case has to be right to the last digit: sqlite renders a real
//! with `%!.15g` and, when that text does not parse back to the SAME double,
//! falls back to `%!.20e`. `quote(0.1+0.2)` is therefore
//! `3.00000000000000044e-01` where `CAST(0.1+0.2 AS TEXT)` is `0.3`.
//!
//! The float sweep deliberately builds its doubles by ARITHMETIC over an
//! integer column (`n/7.0`, `sqrt(n)`, `exp(n/10.0)`, …) rather than by writing
//! 17-digit literals: both engines then start from the same short, exactly
//! representable literals and do the same IEEE operations, so any difference in
//! the output is a difference in `quote()` and not in either engine's
//! decimal-to-binary parser.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// 1..=120 in `n`, plus a text column carrying the awkward strings.
fn rows() -> Vec<String> {
    let mut v = Vec::new();
    for n in 1..=120i64 {
        v.push(format!("INSERT INTO t (id, n) VALUES ({n}, {n})"));
    }
    v
}

fn mpedb_db() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-quote-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
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
  name = "n"
  type = "int64"
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for r in rows() {
        db.query(&r, &[]).unwrap();
    }
    (db, path)
}

fn sqlite_setup() -> String {
    let mut s = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER);\n");
    for r in rows() {
        s.push_str(&r);
        s.push_str(";\n");
    }
    s
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn mpedb_rows(db: &Database, query: &str) -> Vec<String> {
    match db.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect(),
        other => panic!("expected rows for `{query}`, got {other:?}"),
    }
}

fn sqlite_rows(query: &str) -> Vec<String> {
    let mut input = sqlite_setup();
    input.push_str(query);
    input.push_str(";\n");
    sqlite_oracle::script_stdout(&input, "NULL")
        .lines()
        .map(|l| l.to_string())
        .collect()
}

fn cross_check(db: &Database, query: &str) {
    let m = mpedb_rows(db, query);
    let s = sqlite_rows(query);
    assert_eq!(m, s, "mpedb vs sqlite disagree on `{query}`");
}

#[test]
fn quote_matches_sqlite_for_every_storage_class() {
    let (db, path) = mpedb_db();

    // --- NULL, text, integers -------------------------------------------
    // A single row's worth of directed cases, all on one line so the CLI's
    // line-oriented output stays aligned. The newline case is folded through
    // replace() in BOTH engines for the same reason.
    cross_check(
        &db,
        "SELECT quote(NULL), quote(''), quote('it''s'), quote('a''''b'), quote('æøå')",
    );
    cross_check(
        &db,
        "SELECT replace(quote('a' || char(10) || 'b'), char(10), '<NL>')",
    );
    cross_check(&db, "SELECT quote(0), quote(1), quote(-1), quote(42)");
    cross_check(
        &db,
        "SELECT quote(9223372036854775807), quote(-9223372036854775807)",
    );
    // Every integer in the table, so the digit rendering is swept too.
    cross_check(&db, "SELECT quote(n), quote(-n) FROM t ORDER BY id");

    // --- BLOB ------------------------------------------------------------
    cross_check(
        &db,
        "SELECT quote(x''), quote(x'00'), quote(x'00ff10'), quote(x'deadBEEF')",
    );

    // --- REAL: every value whose `%!.15g` rendering round-trips ------------
    // 0.1, an integral-valued real (the `%!` flag's trailing `.0`), -0.0
    // (sqlite reports no sign), zero, and the exponent forms.
    cross_check(
        &db,
        "SELECT quote(0.1), quote(3.0), quote(-0.0), quote(0.0), quote(-2.5)",
    );
    cross_check(
        &db,
        "SELECT quote(1e300), quote(-1e300), quote(1e-300), quote(2.5e-10), quote(1e17)",
    );
    // All 15 significant digits in use, on both sides of the decimal point.
    cross_check(
        &db,
        "SELECT quote(0.000123456789012345), quote(123456789012345.0), quote(1.23456789012345)",
    );
    // Infinity: sqlite's `%!.15g` prints `Inf`, and quote() does NOT wrap it.
    cross_check(&db, "SELECT quote(1e308*10), quote(-1e308*10)");

    let _ = std::fs::remove_file(&path);
}

/// Count the significant digits in one sqlite float literal (`3.0e+17`,
/// `0.000125`, `1.23456789012345678e+14`): the digits of the mantissa, leading
/// zeros of a pure fraction excluded.
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), allow(dead_code))]
fn sig_digits(s: &str) -> usize {
    let mantissa = s.split(['e', 'E']).next().unwrap_or("");
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let trimmed = digits.trim_start_matches('0');
    // A trailing `.0` that `%!` added is not a significant digit.
    let t = if mantissa.contains('.') {
        trimmed.trim_end_matches('0')
    } else {
        trimmed
    };
    t.len().max(1)
}

/// The float sweep: 120 rows × 8 expressions = 960 doubles, all built by
/// ARITHMETIC over an integer column so both engines start from the same short,
/// exactly-parsed literals and run the same IEEE operations.
///
/// Where mpedb answers, the answer must equal sqlite's byte for byte. Where
/// mpedb REFUSES, sqlite must have taken its `%!.20e` fallback — i.e. printed
/// MORE than 15 significant digits — which is exactly the case mpedb declines
/// to guess (see `printf::quote_float`).
#[test]
// Calibration-build only: the sweep's premise is that mpedb and the bundled
// sqlite hold the SAME f64 for each literal — measured false on arm64, where
// sqlite's text->double conversion is not correctly rounded at extreme
// exponents (no 80-bit long double): quote(1.5e301) answers
// 1.500000000000000156e+301 there, and (14*1e300/7.0) = 2e300 evaluates TRUE
// on macOS sqlite while false on Linux. Both directions of the premise fail,
// so neither the answered nor the refused arm can be asserted off-calibration.
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64")), ignore = "sqlite text->f64 parse is arch-dependent at extreme exponents")]
fn quote_float_sweep_is_exact_or_a_refusal_of_the_unportable_branch() {
    let (db, path) = mpedb_db();
    let mut answered = 0usize;
    let mut refused = 0usize;
    for expr in [
        "n / 7.0",
        "1.0 / n",
        "sqrt(n)",
        "exp(n / 10.0)",
        "n * 1.0000001",
        "n * 1e300 / 7.0",
        "n * 1e-300 / 3.0",
        "n / 8.0",
    ] {
        let want = sqlite_rows(&format!("SELECT quote({expr}) FROM t ORDER BY id"));
        // The doubles themselves, straight out of mpedb's own arithmetic.
        let vals = match db
            .query(&format!("SELECT {expr} FROM t ORDER BY id"), &[])
            .unwrap()
        {
            ExecResult::Rows { rows, .. } => rows.into_iter().map(|r| r[0].clone()).collect(),
            other => panic!("{other:?}"),
        };
        let vals: Vec<Value> = vals;
        assert_eq!(vals.len(), want.len(), "row count for `{expr}`");
        for (v, w) in vals.iter().zip(&want) {
            match db.query("SELECT quote($1)", std::slice::from_ref(v)) {
                Ok(ExecResult::Rows { rows, .. }) => {
                    answered += 1;
                    assert_eq!(&render(&rows[0][0]), w, "quote({v:?}) over `{expr}`");
                }
                Ok(other) => panic!("{other:?}"),
                Err(e) => {
                    refused += 1;
                    let msg = e.to_string();
                    assert!(msg.contains("15 significant digits"), "message: {msg}");
                    // The strict converse — "a refusal implies the LOCAL
                    // rendering needs >15 digits" — is only sound on the
                    // build the refusal set was calibrated against. The whole
                    // reason these reals are refused is that sqlite's
                    // rendering of them is BUILD-DEPENDENT, and that cannot
                    // be judged from one machine: measured, the same double
                    // renders as `2.0e+300` (bundled 3.45.0/arm64),
                    // `1.999999999999999807e+300` (3.45.1/x86_64) and
                    // `1.999999999999999315e+300` (3.51/arm64). A refusal
                    // that looks over-conservative locally may be exactly
                    // right globally — so the strict check runs only where
                    // it was calibrated; elsewhere a refusal is accepted
                    // (a refusal is never a wrong answer).
                    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                    assert!(
                        sig_digits(w) > 15,
                        "mpedb refused quote({v:?}) but sqlite's `{w}` needs only {} digits — \
                         that is a real disagreement, not the unportable branch",
                        sig_digits(w)
                    );
                }
            }
        }
    }
    // Both arms must actually have been exercised.
    assert!(answered > 200, "only {answered} values answered");
    assert!(refused > 50, "only {refused} values refused");
    eprintln!("quote() float sweep: {answered} exact, {refused} refused (unportable %!.20e branch)");
    let _ = std::fs::remove_file(&path);
}

/// The refusal is by name, carries the value, and names WHY: sqlite's `%!.20e`
/// fallback is chosen per build by `sqlite3Config.bUseLongDouble`, so there is
/// no single sqlite answer to match.
#[test]
fn quote_refuses_the_reals_whose_sqlite_rendering_is_build_dependent() {
    let (db, path) = mpedb_db();
    for sql in [
        "SELECT quote(0.1+0.2)",
        "SELECT quote(1.0/3.0)",
        "SELECT quote(123456789012345.6)",
        "SELECT quote(1.23456789012345678)",
    ] {
        let e = db.query(sql, &[]).expect_err(sql);
        let msg = e.to_string();
        assert!(msg.contains("quote()"), "{sql}: {msg}");
        assert!(msg.contains("15 significant digits"), "{sql}: {msg}");
        assert!(msg.contains("bUseLongDouble"), "{sql}: {msg}");
    }
    // …while CAST(x AS TEXT), sqlite's portable %!.15g rendering, still answers.
    assert_eq!(
        match db.query("SELECT CAST(0.1+0.2 AS TEXT)", &[]).unwrap() {
            ExecResult::Rows { rows, .. } => render(&rows[0][0]),
            other => panic!("{other:?}"),
        },
        "0.3"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn quote_of_a_bound_parameter_matches_sqlite() {
    let (db, path) = mpedb_db();
    // Django's `last_executed_query` emits exactly `SELECT QUOTE(?)`, so the
    // argument is an UNTYPED parameter: quote() must not pin it to a type.
    let one = |sql: &str, p: &[Value]| match db.query(sql, p).unwrap() {
        ExecResult::Rows { rows, .. } => render(&rows[0][0]),
        other => panic!("{other:?}"),
    };
    // Django writes it UPPERCASE (`SELECT QUOTE(?), QUOTE(?)`), so the name
    // must fold — asserted here rather than assumed.
    assert_eq!(one("SELECT QUOTE($1)", &[Value::Text("x".into())]), "'x'");
    assert_eq!(one("SELECT quote($1)", &[Value::Null]), "NULL");
    assert_eq!(one("SELECT quote($1)", &[Value::Int(-7)]), "-7");
    assert_eq!(
        one("SELECT quote($1)", &[Value::Text("it's".into())]),
        "'it''s'"
    );
    assert_eq!(
        one("SELECT quote($1)", &[Value::Blob(vec![0, 0xff, 0x10])]),
        "X'00FF10'"
    );
    assert_eq!(one("SELECT quote($1)", &[Value::Float(0.1)]), "0.1");
    // i64::MIN cannot be written as a literal (the tokenizer sees the positive
    // magnitude first), so it is bound — and cross-checked against sqlite's own
    // literal rendering of the same value.
    assert_eq!(
        one("SELECT quote($1)", &[Value::Int(i64::MIN)]),
        sqlite_rows("SELECT quote(-9223372036854775808)")[0]
    );
    // Two parameters in one statement, the Django shape.
    assert_eq!(
        match db
            .query("SELECT quote($1), quote($2)", &[Value::Int(1), Value::Null])
            .unwrap()
        {
            ExecResult::Rows { rows, .. } => rows[0].iter().map(render).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        },
        vec!["1".to_string(), "NULL".to_string()]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn quote_refuses_text_with_an_embedded_nul() {
    let (db, path) = mpedb_db();
    // sqlite reaches the text through a NUL-terminated C string, so
    // `quote(char(97,0,98))` silently TRUNCATES to `'a'`. mpedb's TEXT can hold
    // the NUL, and a quoting function that drops the tail of its input is a
    // quiet wrong answer — so it is a named refusal instead.
    let e = db
        .query("SELECT quote(char(97, 0, 98))", &[])
        .expect_err("embedded NUL must be refused");
    let msg = e.to_string();
    assert!(msg.contains("embedded NUL"), "message was: {msg}");
    assert!(msg.contains("quote()"), "message was: {msg}");
    // Arity is a compile error.
    assert!(db.query("SELECT quote()", &[]).is_err());
    assert!(db.query("SELECT quote('a', 'b')", &[]).is_err());
    let _ = std::fs::remove_file(&path);
}
