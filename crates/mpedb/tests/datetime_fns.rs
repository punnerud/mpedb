//! `date(X)` / `time(X)` / `datetime(X)` / `julianday(X)` and the literal
//! `'now'`, cross-checked against the BUNDLED sqlite 3.45 oracle.
//!
//! The four functions are asserted over the SAME time-string corpus
//! `strftime_fn.rs` sweeps, because they share one parser with `strftime` and a
//! divergence would otherwise only show up in one of them. `julianday` is
//! rendered through `printf('%.16g', …)` on BOTH engines so the comparison is
//! over sqlite's own float formatter rather than two Display impls.
//!
//! `'now'` cannot be diffed value-for-value (the two engines read the clock at
//! different instants), so it is pinned by its three CONTRACTS instead:
//!
//! 1. **One instant per statement** (sqlite's `iCurrentTime`): two `'now'`s in
//!    one statement agree — asserted at MILLISECOND resolution, which a
//!    per-call clock read would fail.
//! 2. **A fresh instant per statement**: two statements a sleep apart disagree.
//! 3. **UTC**: mpedb's `strftime('%s','now')` and the oracle's agree to within
//!    a few seconds — a local-time bug would show up as a whole-hour offset.
//!
//! The refusals are asserted by message, for the same reason `strftime_fn.rs`
//! does it: sqlite ANSWERS for a modifier and for a Julian-day number, so
//! returning NULL there would be a silently different answer, not a refusal.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// The same spread `strftime_fn.rs` uses: leap years, century boundaries,
/// fractional seconds, both timezone spellings, time-only strings, BC years,
/// and the two un-normalised quirks (`24:00`, day > month length).
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
    "2010-01-01 00:00",
    "2010-06-05 07:08",
    "2010-06-05 07:08:09",
    "2010-06-05 07:08:09.5",
    "2010-06-05 07:08:09.789",
    "2010-06-05 23:59:59.999",
    "2010-06-05T12:34:56.75",
    "2010-06-05 12:34:56Z",
    "2010-06-05 12:34:56+02:00",
    "2010-06-05 12:34:56.5-05:30",
    "2010-01-01 00:30:00+02:00",
    "2010-12-31 23:00:00-02:00",
    "12:34",
    "12:34:56",
    "12:34:56.789",
    "00:00:00",
    "23:59:59.999",
    "2010-01-01 24:00",
    "2010-02-30",
    "2010-02-30 12:00",
    "-0500-03-04",
    "0000-01-01",
];

fn mpedb_db() -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-datetimefn-{}-{}.mpedb",
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
        // mpedb has a first-class bool where sqlite has 1/0; render it the way
        // sqlite prints it so a comparison's ANSWER can be diffed directly.
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
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

fn one_text(db: &Database, sql: &str) -> String {
    let rows = mpedb_rows(db, sql);
    assert_eq!(rows.len(), 1, "expected one row from `{sql}`");
    rows.into_iter().next().unwrap()
}

#[test]
fn date_time_datetime_julianday_match_sqlite_over_every_time_form() {
    let (db, path) = mpedb_db();

    for f in ["date", "time", "datetime"] {
        cross_check(&db, &format!("SELECT {f}(ts) FROM t ORDER BY id"));
        // NULL propagates, exactly as sqlite does.
        cross_check(&db, &format!("SELECT {f}(NULL)"));
    }
    // julianday returns a REAL: render it through sqlite's OWN `%.16g` on both
    // sides so the diff is over the value, not over two float Displays.
    cross_check(
        &db,
        "SELECT printf('%.16g', julianday(ts)) FROM t ORDER BY id",
    );
    cross_check(&db, "SELECT julianday(NULL)");

    // The identities sqlite's implementation is built on — asserted so a future
    // divergence between the family and `strftime` cannot hide.
    cross_check(
        &db,
        "SELECT date(ts) = strftime('%Y-%m-%d', ts) FROM t ORDER BY id",
    );
    cross_check(
        &db,
        "SELECT time(ts) = strftime('%H:%M:%S', ts) FROM t ORDER BY id",
    );
    cross_check(
        &db,
        "SELECT datetime(ts) = strftime('%Y-%m-%d %H:%M:%S', ts) FROM t ORDER BY id",
    );

    // Django's exact wire shape for `EscapingChecks.test_parameter_escaping`.
    cross_check(&db, "SELECT date('2020-02-29'), datetime('2020-02-29')");
    // Nesting one family member inside another.
    cross_check(&db, "SELECT strftime('%s', date('2020-02-29'))");
    cross_check(&db, "SELECT date(datetime('2010-06-05 12:34:56'))");

    let _ = std::fs::remove_file(&path);
}

/// The statements the Django/backends gaps in `C-API-COMPAT.md`'s run-4 table
/// name, VERBATIM. Each was a recorded refusal; each must now answer, with the
/// shape stock sqlite gives.
#[test]
fn the_named_django_statements_answer() {
    let (db, path) = mpedb_db();

    // Gap 2 — `test_parameter_escaping` (EscapingChecks, EscapingChecksDebug).
    // Was: `bind error: unknown function 'date()'`.
    let s = one_text(&db, "SELECT strftime('%s', date('now'))");
    let epoch: i64 = s.parse().unwrap_or_else(|e| panic!("not an epoch: {s:?} ({e})"));
    // date('now') truncates to midnight UTC, so this is a whole number of days.
    assert_eq!(epoch % 86_400, 0, "date('now') must be midnight UTC, got {epoch}");

    // Gap 3 — `test_no_interpolation`. Was:
    // `strftime(): unsupported time string "now"`.
    let y = one_text(&db, "SELECT strftime('%Y', 'now')");
    assert_eq!(y.len(), 4, "a four-digit year, got {y:?}");
    assert!(y.chars().all(|c| c.is_ascii_digit()), "got {y:?}");

    // The `date()` family gap in the D-list (`backends`). Was: `date()` unknown.
    for sql in [
        "SELECT date('now')",
        "SELECT time('now')",
        "SELECT datetime('now')",
        "SELECT julianday('now')",
    ] {
        assert!(db.query(sql, &[]).is_ok(), "`{sql}` must answer");
    }
    // The two spellings agree with each other, which is the whole point of
    // one instant per statement.
    assert_eq!(
        one_text(&db, "SELECT date('now') = strftime('%Y-%m-%d','now')"),
        "1"
    );

    let _ = std::fs::remove_file(&path);
}

/// Contract 1: sqlite fixes `'now'` ONCE PER STATEMENT, so two `'now'`s in one
/// statement are the SAME instant. Asserted at millisecond resolution (`%f`),
/// which is the resolution sqlite's own `'now'` has — a per-call clock read
/// would fail this test, not merely be slower.
#[test]
fn two_nows_in_one_statement_are_the_same_instant() {
    let (db, path) = mpedb_db();

    // The strictest form: the full millisecond timestamp, twice, compared.
    let eq = one_text(
        &db,
        "SELECT strftime('%Y-%m-%d %H:%M:%f','now') = strftime('%Y-%m-%d %H:%M:%f','now')",
    );
    assert_eq!(eq, "1", "two 'now's in one statement must agree");

    // …and across the whole family, mixed, in one statement: every one of them
    // resolves to the same reserved slot.
    let eq = one_text(
        &db,
        "SELECT datetime('now') = (strftime('%Y-%m-%d','now') || ' ' || time('now'))",
    );
    assert_eq!(eq, "1", "the family must share one statement instant");

    // Per ROW, too: one instant for the statement, not one per row.
    let rows = mpedb_rows(
        &db,
        "SELECT strftime('%Y-%m-%d %H:%M:%f','now') FROM t ORDER BY id",
    );
    assert!(rows.len() > 1, "need several rows to make this meaningful");
    assert!(
        rows.windows(2).all(|w| w[0] == w[1]),
        "'now' must be one instant for the whole statement, got {rows:?}"
    );

    let _ = std::fs::remove_file(&path);
}

/// Contract 2: a FRESH instant per statement. The same plan executed twice, a
/// sleep apart, must see the clock move — otherwise the slot would be baked
/// into the plan (the failure mode the whole design exists to prevent).
#[test]
fn now_advances_between_statements() {
    let (db, path) = mpedb_db();
    let sql = "SELECT strftime('%Y-%m-%d %H:%M:%f','now')";

    let a = one_text(&db, sql);
    std::thread::sleep(std::time::Duration::from_millis(30));
    let b = one_text(&db, sql);
    assert_ne!(
        a, b,
        "'now' must be re-read per execute; a baked instant would repeat"
    );

    // Same thing through the PREPARED path, where the plan really is reused
    // byte-for-byte from the registry.
    let h = db.prepare(sql).unwrap();
    let first = match db.execute(&h, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => render(&rows[0][0]),
        other => panic!("expected rows, got {other:?}"),
    };
    std::thread::sleep(std::time::Duration::from_millis(30));
    let second = match db.execute(&h, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => render(&rows[0][0]),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_ne!(
        first, second,
        "a REUSED plan must still read the clock per execute"
    );

    let _ = std::fs::remove_file(&path);
}

/// Contract 3: `'now'` is UTC, exactly as in sqlite (only the refused
/// `'localtime'` modifier shifts it). Compared against the oracle's own
/// `'now'`: a local-time bug shows up as a whole-hour offset, far outside the
/// seconds of slack the two clock reads need.
#[test]
fn now_is_utc_and_agrees_with_sqlites_own_now() {
    let (db, path) = mpedb_db();

    let mine: i64 = one_text(&db, "SELECT strftime('%s','now')").parse().unwrap();
    let theirs: i64 = sqlite_oracle::script_stdout("SELECT strftime('%s','now');", "NULL")
        .trim()
        .parse()
        .unwrap();
    assert!(
        (mine - theirs).abs() <= 5,
        "mpedb's 'now' ({mine}) and sqlite's ({theirs}) must be the same UTC instant; \
         a whole-hour gap would mean a local-time reading"
    );

    // The unix-epoch conversion, checked from the other end: `date('now')` must
    // name the same UTC day the epoch seconds do.
    let day = one_text(&db, "SELECT date('now')");
    let expect = sqlite_oracle::script_stdout(
        &format!("SELECT date({mine}, 'unixepoch');"),
        "NULL",
    );
    assert_eq!(day, expect.trim(), "date('now') must be the UTC calendar day");

    let _ = std::fs::remove_file(&path);
}

/// `'now'` is recognised as a bind-time LITERAL only, and everything sqlite
/// answers but mpedb does not reproduce is a NAMED error rather than sqlite's
/// NULL — the rule `strftime_fn.rs` states and this family inherits.
#[test]
fn the_refusals_are_named_never_a_silent_null() {
    let (db, path) = mpedb_db();

    // The whole modifier language, for every member of the family.
    for sql in [
        "SELECT date('2020-01-01','+1 day')",
        "SELECT time('2020-01-01','start of day')",
        "SELECT datetime('now','localtime')",
        "SELECT datetime('now','utc')",
        "SELECT julianday('2020-01-01','unixepoch')",
        "SELECT strftime('%Y','2020-01-01','+1 month')",
    ] {
        let e = db.query(sql, &[]).unwrap_err().to_string();
        assert!(
            e.contains("modifiers are not supported"),
            "`{sql}` should refuse the modifier language by name, got: {e}"
        );
    }

    // A time string outside the supported grammar — including a Julian-day
    // NUMBER, which sqlite answers and mpedb must not guess.
    for sql in [
        "SELECT date('nonsense')",
        "SELECT datetime('2010-13-01')",
        "SELECT julianday('2020-01-01 xx')",
    ] {
        let e = db.query(sql, &[]).unwrap_err().to_string();
        assert!(
            e.contains("unsupported time string"),
            "`{sql}` should refuse the time string by name, got: {e}"
        );
    }
    for sql in ["SELECT date(2455352.5)", "SELECT time(0)"] {
        let e = db.query(sql, &[]).unwrap_err().to_string();
        assert!(
            e.contains("must be text") || e.contains("must be timestamp"),
            "`{sql}` should refuse the Julian-day number form, got: {e}"
        );
    }

    // A RUNTIME 'now' — a column whose VALUE is the text `now` — is NOT the
    // bind-time literal and stays refused: resolving it would mean a clock read
    // per row, which drifts within one statement.
    db.query("INSERT INTO t (id, ts) VALUES (9001, 'now')", &[])
        .unwrap();
    let e = db
        .query("SELECT date(ts) FROM t WHERE id = 9001", &[])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("unsupported time string") && e.contains("bind-time"),
        "a runtime 'now' should be refused and say why, got: {e}"
    );

    // The reserved slot the literal binds to cannot be spelled by a caller.
    let e = db
        .query("SELECT current_setting('@statement_instant')", &[])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("reserved slot name"),
        "the instant slot must not be readable as a session setting, got: {e}"
    );

    let _ = std::fs::remove_file(&path);
}

/// `'now'` is refused where the expression is STORED and re-evaluated later —
/// a CHECK body, a DEFAULT, an index expression — because the answer would
/// silently change under it. A refusal, never a value that rots.
#[test]
fn now_is_refused_in_stored_expressions() {
    let (db, path) = mpedb_db();
    let e = db
        .query(
            "CREATE TABLE c (id integer PRIMARY KEY, d text CHECK (d > date('now')))",
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("'now' is not allowed") || e.contains("not allowed in this expression"),
        "a CHECK must not capture the statement instant, got: {e}"
    );
    let _ = std::fs::remove_file(&path);
}
