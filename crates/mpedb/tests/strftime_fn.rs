//! `strftime(FORMAT, TIMESTRING)` cross-checked against the real `sqlite3` CLI
//! 3.45, specifier by specifier, over a wide spread of dates and times.
//!
//! Django reaches this through `Cast(…, DateTimeField)` / `Cast(…, TimeField)`,
//! which compile to `strftime('%Y-%m-%d %H:%M:%f', …)` and
//! `strftime('%H:%M:%f', …)` — those two format strings are asserted first, and
//! then every specifier sqlite 3.45 has is swept individually.
//!
//! The refusals are asserted directly, by message: sqlite answers NULL for a
//! time string it cannot parse, so an unsupported FORM (`'now'`, a Julian-day
//! number, a modifier) must not be allowed to fall into that same NULL — it
//! would be indistinguishable from sqlite's own NULL while actually being a
//! different answer. Every such input is a named error instead.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// The time strings both engines are loaded with: dates across leap years,
/// century boundaries and week boundaries; times with and without seconds and
/// with 1..6 fractional digits; the two timezone spellings; and a time-only
/// string (which sqlite dates to 2000-01-01).
const TIMES: &[&str] = &[
    "2010-01-01",
    "2010-06-05",
    "2010-12-31",
    "2000-02-29",
    "1900-03-01",
    "2024-02-29",
    "1970-01-01",
    "1969-12-31",
    "0001-01-01",
    "9999-12-31",
    "2012-12-31",
    "2010-01-03",
    "2010-01-04",
    "2021-01-01",
    "2010-01-01 00:00",
    "2010-06-05 07:08",
    "2010-06-05 07:08:09",
    "2010-06-05 07:08:09.5",
    "2010-06-05 07:08:09.789",
    "2010-06-05 07:08:09.789012",
    "2010-06-05 23:59:59.999",
    "2010-06-05T12:34:56.75",
    "2010-06-05 12:34:56Z",
    "2010-06-05 12:34:56z",
    "2010-06-05 12:34:56+02:00",
    "2010-06-05 12:34:56.5-05:30",
    "2010-01-01 00:30:00+02:00",
    "2010-12-31 23:00:00-02:00",
    "2010-06-05 12:34:56+00:00",
    "12:34",
    "12:34:56",
    "12:34:56.789",
    "00:00:00",
    "23:59:59.999",
    "2010-01-01 ",
    // Negative (BC) years: sqlite's parseYyyyMmDd accepts a leading '-'.
    "-0500-03-04",
    "-0001-12-31",
    "0000-01-01",
    // A fraction that rounds all the way up to the next second.
    "12:00:00.999999999999999999999999",
];

/// Every format specifier sqlite 3.45 implements.
const SPECIFIERS: &[&str] = &[
    "%d", "%e", "%f", "%F", "%H", "%I", "%j", "%J", "%k", "%l", "%m", "%M", "%p", "%P", "%R", "%s",
    "%S", "%T", "%u", "%w", "%W", "%Y", "%%",
];

fn mpedb_db() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-strftime-{}-{}.mpedb",
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
  name = "ts"
  type = "text"
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for (i, t) in TIMES.iter().enumerate() {
        db.query(
            &format!("INSERT INTO t (id, ts) VALUES ({}, '{}')", i + 1, t),
            &[],
        )
        .unwrap();
    }
    (db, path)
}

fn sqlite_setup() -> String {
    let mut s = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, ts TEXT);\n");
    for (i, t) in TIMES.iter().enumerate() {
        s.push_str(&format!(
            "INSERT INTO t (id, ts) VALUES ({}, '{}');\n",
            i + 1,
            t
        ));
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

fn sqlite_rows(query: &str) -> Vec<String> {
    let mut input = sqlite_setup();
    input.push_str(query);
    input.push_str(";\n");
    sqlite_oracle::script_stdout(&input, "NULL")
        .lines()
        .map(|l| l.to_string())
        .collect()
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

fn cross_check(db: &Database, query: &str) {
    let m = mpedb_rows(db, query);
    let s = sqlite_rows(query);
    assert_eq!(m, s, "mpedb vs sqlite disagree on `{query}`");
}

#[test]
fn strftime_matches_sqlite_for_every_specifier_and_time_form() {
    let (db, path) = mpedb_db();

    // The two formats Django's `Cast` emits (functions/comparison.py).
    cross_check(
        &db,
        "SELECT strftime('%Y-%m-%d %H:%M:%f', ts) FROM t ORDER BY id",
    );
    cross_check(&db, "SELECT strftime('%H:%M:%f', ts) FROM t ORDER BY id");

    // Every specifier, one at a time, over every time form.
    for spec in SPECIFIERS {
        cross_check(
            &db,
            &format!("SELECT strftime('[{spec}]', ts) FROM t ORDER BY id"),
        );
    }
    // …and all of them at once, so the literal-run copying between specifiers
    // is exercised too (including a leading and a trailing literal).
    let all = SPECIFIERS.join("|");
    cross_check(
        &db,
        &format!("SELECT strftime('<{all}>', ts) FROM t ORDER BY id"),
    );
    // A format with no specifier at all, and one that is entirely literal
    // text with multi-byte characters around the '%'.
    cross_check(&db, "SELECT strftime('plain', ts) FROM t ORDER BY id");
    cross_check(&db, "SELECT strftime('', ts) FROM t ORDER BY id");
    cross_check(&db, "SELECT strftime('æ%Yø%må', ts) FROM t ORDER BY id");

    // NULL propagates on both arguments, exactly as sqlite does.
    cross_check(&db, "SELECT strftime('%Y', NULL)");
    cross_check(&db, "SELECT strftime(NULL, '2010-01-01')");

    // The seconds quirk: `%S` truncates the parsed double while `%f` rounds it
    // to milliseconds, so the same value reports 56 and 57.000.
    cross_check(
        &db,
        "SELECT strftime('%S %f %H %M', '2010-01-01 12:34:56.9999')",
    );
    cross_check(&db, "SELECT strftime('%f %S', '2010-01-01 12:34:59.9999')");
    // Hour 24 is accepted by sqlite's parser and NOT renormalised.
    cross_check(
        &db,
        "SELECT strftime('%Y-%m-%d %H:%M:%S %j %w %s', '2010-01-01 24:00')",
    );
    // …but a day past the end of the month IS renormalised (sqlite's isDate()
    // invalidates the parsed Y/M/D when D > 28).
    cross_check(
        &db,
        "SELECT strftime('%Y-%m-%d %H:%M:%S', '2010-02-30 12:00')",
    );
    cross_check(&db, "SELECT strftime('%Y-%m-%d', '2010-02-30')");

    let _ = std::fs::remove_file(&path);
}

/// A dense sweep: every day of four whole years, formatted with the specifiers
/// whose arithmetic is easy to get subtly wrong (day-of-year, week-of-year,
/// weekday, unix epoch, Julian day).
#[test]
fn strftime_calendar_arithmetic_matches_sqlite_over_four_years() {
    let (db, path) = mpedb_db();
    // A recursive CTE would be neater, but generating the dates in Rust keeps
    // both engines reading the SAME literal text.
    let mut days: Vec<String> = Vec::new();
    for (y, leap) in [(1999, false), (2000, true), (2001, false), (2024, true)] {
        let lens = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        for (mi, len) in lens.iter().enumerate() {
            for d in 1..=*len {
                days.push(format!("{y:04}-{:02}-{d:02}", mi + 1));
            }
        }
    }
    assert_eq!(days.len(), 365 + 366 + 365 + 366);
    // 200 dates per query keeps the CLI command line and the plan small.
    for chunk in days.chunks(200) {
        let list = chunk
            .iter()
            .map(|d| format!("strftime('%j %W %w %u %s %J %F %T', '{d}')"))
            .collect::<Vec<_>>()
            .join(", ");
        cross_check(&db, &format!("SELECT {list}"));
    }
    let _ = std::fs::remove_file(&path);
}

/// Every refusal, asserted by message. None of these may be NULL: sqlite
/// ANSWERS for `'now'`, for a Julian-day number and for a modifier, so a NULL
/// here would be a silently different answer rather than a refusal.
#[test]
fn strftime_refuses_by_name_what_it_cannot_reproduce() {
    let (db, path) = mpedb_db();
    let err = |sql: &str| -> String {
        db.query(sql, &[])
            .map(|r| panic!("`{sql}` should have been refused, got {r:?}"))
            .unwrap_err()
            .to_string()
    };

    // --- unsupported format specifiers, named ---------------------------
    for (sql, want) in [
        ("SELECT strftime('%q', '2010-01-01')", "'%q'"),
        ("SELECT strftime('%G', '2010-01-01')", "'%G'"),
        ("SELECT strftime('%V', '2010-01-01')", "'%V'"),
        ("SELECT strftime('%U', '2010-01-01')", "'%U'"),
        ("SELECT strftime('%n', '2010-01-01')", "'%n'"),
        ("SELECT strftime('a%Zb', '2010-01-01')", "'%Z'"),
    ] {
        let m = err(sql);
        assert!(m.contains("unsupported format specifier"), "{sql}: {m}");
        assert!(m.contains(want), "{sql}: {m}");
        assert!(m.contains("mpedb supports"), "{sql}: {m}");
    }
    // A bare trailing '%'.
    let m = err("SELECT strftime('%Y-%', '2010-01-01')");
    assert!(m.contains("bare '%'"), "{m}");

    // --- unsupported time strings, named --------------------------------
    for sql in [
        "SELECT strftime('%Y', 'now')",
        "SELECT strftime('%Y', '2455352.5')",
        "SELECT strftime('%Y', 'garbage')",
        "SELECT strftime('%Y', '')",
        "SELECT strftime('%Y', '2010-1-1')",
        "SELECT strftime('%Y', '2010-13-01')",
        "SELECT strftime('%Y', '2010-01-32')",
        "SELECT strftime('%Y', '2010-01-01 25:00')",
        "SELECT strftime('%Y', '2010-01-01 12:60')",
        "SELECT strftime('%Y', '2010-01-01 12:00:60')",
        "SELECT strftime('%Y', '2010-01-01 12:34:56.')",
        "SELECT strftime('%Y', '2010-01-01Z')",
        "SELECT strftime('%Y', '2010-01-01 12:34:56+99:00')",
        // Before the Julian epoch: sqlite's validJulianDay() rejects it.
        "SELECT strftime('%Y', '-4713-01-02')",
    ] {
        let m = err(sql);
        assert!(m.contains("unsupported time string"), "{sql}: {m}");
        assert!(m.contains("ISO-8601"), "{sql}: {m}");
    }

    // A fractional part long enough to overflow the seconds accumulator: the
    // NaN that produces is a NULL in sqlite (an undefined C cast lands the
    // Julian day out of range) and must never become an mpedb answer.
    let long = format!("SELECT strftime('%f', '00:00:00.{}')", "9".repeat(400));
    let m = err(&long);
    assert!(m.contains("unsupported time string"), "{m}");

    // --- modifiers ------------------------------------------------------
    let m = err("SELECT strftime('%Y-%m-%d', '2010-01-01', '+1 day')");
    assert!(m.contains("modifiers are not supported"), "{m}");
    let m = err("SELECT strftime('%Y', '2010-01-01', 'utc', 'start of month')");
    assert!(m.contains("modifiers are not supported"), "{m}");

    // --- a numeric time value (sqlite's Julian-day form) is a COMPILE error
    let m = err("SELECT strftime('%Y', 2455352.5)");
    assert!(m.contains("strftime()"), "{m}");
    // …and so is a wrong arity.
    assert!(db.query("SELECT strftime('%Y')", &[]).is_err());
    assert!(db.query("SELECT strftime()", &[]).is_err());

    let _ = std::fs::remove_file(&path);
}

/// Task #74 item 4 — `strftime(f, 'now')`: **re-examined and REFUSED BY
/// DESIGN**, and this test is the decision written down so it is not quietly
/// reversed.
///
/// The argument is determinism, not effort:
///
///  * mpedb has NO non-deterministic expression today. `'now'` would be the
///    first, and `CURRENT_TIMESTAMP`/`CURRENT_DATE`/`CURRENT_TIME` are refused
///    by name in DDL for the same reason (asserted below, so the two refusals
///    cannot drift apart).
///  * Compiled plans are content-hashed and published to a registry SHARED
///    ACROSS PROCESSES. `strftime('%Y','now')` has all-constant arguments, so
///    the day `fold` starts folding `Call` — which its own comment says is
///    foldable "in principle" — the plan bytes would carry a COMPILE-TIME
///    timestamp that every later process reuses. A wrong answer that outlives
///    the process that made it, in a shared file.
///  * sqlite fixes `'now'` ONCE PER STATEMENT (`iCurrentTime`). Reproducing
///    that needs a statement-start instant threaded through every
///    `ExprProgram::eval` call site; reading the clock inside `eval` would
///    drift within one statement instead — a different wrong answer, and a
///    syscall per row in a crate that has no clock dependency by design.
///  * A CHECK body holding `'now'` would pass at INSERT and fail on any later
///    re-validation, and the mirror's convergence criterion (replay reproduces
///    the source EXACTLY) has no meaning for a time-dependent statement.
///  * It would not close the shape Django actually uses: `'now'` is only useful
///    with the modifier language (`'now','start of day'`), which stays refused.
#[test]
fn now_is_refused_by_design_and_says_why() {
    let (db, path) = mpedb_db();

    // The refusal names `'now'` AND the reason, so the next reader does not
    // have to re-derive the argument above.
    let m = db
        .query("SELECT strftime('%Y', 'now')", &[])
        .map(|r| panic!("'now' must be refused, got {r:?}"))
        .unwrap_err()
        .to_string();
    assert!(m.contains("unsupported time string"), "{m}");
    assert!(m.contains("\"now\""), "{m}");
    assert!(m.contains("non-deterministic"), "{m}");
    assert!(m.contains("content-hashed"), "{m}");

    // Case and whitespace variants take the same path — sqlite accepts them all
    // (`'NOW'`, `' now '`), so none may fall through to a different message or,
    // worse, to a value.
    for t in ["'NOW'", "'Now'", "' now '", "'now '"] {
        let m = db
            .query(&format!("SELECT strftime('%Y', {t})"), &[])
            .map(|r| panic!("{t} must be refused, got {r:?}"))
            .unwrap_err()
            .to_string();
        assert!(m.contains("unsupported time string"), "{t}: {m}");
    }

    // The neighbouring refusals that make this one consistent rather than
    // arbitrary: mpedb has no non-deterministic expression ANYWHERE.
    let m = db
        .query("SELECT current_timestamp", &[])
        .map(|r| panic!("current_timestamp must not resolve, got {r:?}"))
        .unwrap_err()
        .to_string();
    assert!(m.contains("unknown column `current_timestamp`"), "{m}");
    for f in ["current_timestamp()", "now()", "datetime('now')", "date('now')", "time('now')"] {
        assert!(
            db.query(&format!("SELECT {f}"), &[]).is_err(),
            "{f} must not resolve to a value"
        );
    }
    for kw in ["CURRENT_TIMESTAMP", "CURRENT_DATE", "CURRENT_TIME"] {
        let m = db
            .query(&format!("CREATE TABLE nowtest_{kw} (a INT PRIMARY KEY, b TEXT DEFAULT {kw})"), &[])
            .map(|r| panic!("DEFAULT {kw} must be refused, got {r:?}"))
            .unwrap_err()
            .to_string();
        assert!(m.contains(kw), "{kw}: {m}");
    }

    // And the deterministic time strings still work, so this is a refusal of
    // non-determinism and not of `strftime`.
    assert!(db.query("SELECT strftime('%Y-%m-%d', '2020-01-02')", &[]).is_ok());

    let _ = std::fs::remove_file(&path);
}
