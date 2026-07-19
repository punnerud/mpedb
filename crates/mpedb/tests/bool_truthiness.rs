//! sqlite TRUTHINESS in a boolean context — differential test against
//! sqlite3 3.45, plus the int/bool value bridge (Django gap #5).
//!
//! sqlite has no boolean type. `sqlite3VdbeBooleanValue` is: NULL stays
//! unknown, an integer is `!= 0`, everything else is `RealValue(x) != 0.0` —
//! the leading-float-prefix parse, over text AND over a blob's raw bytes.
//! mpedb keeps a rigid `Bool` internally but must be observably identical, so
//! the binder desugars a non-boolean in a boolean position into
//! `x <> 0` / `CAST(x AS REAL) <> 0.0` (see `Binder::coerce_bool_ctx`).
//!
//! This drives the same matrix through both engines in every boolean position
//! sqlite has — `WHERE`, `NOT`, `CASE WHEN`, `AND`, `OR` — and asserts they
//! agree. Reference: `/usr/bin/sqlite3`; skipped (not failed) if absent.
//!
//! Deliberate, documented NON-follows (asserted in `documented_refusals`):
//!   * A non-0/1 integer written INTO a `bool` column is refused. sqlite would
//!     store `2` and read `2` back; mpedb's rigid `Bool` cannot represent it,
//!     and truthy-testing it to TRUE would be a wrong answer on read-back.
//!   * A non-constant int64 expression assigned to a `bool` column, same
//!     reason: nothing proves it is 0/1.

use mpedb::{Config, Database, ExecResult};
use mpedb_types::Value;
use std::process::Command;

const SQLITE3: &str = "/usr/bin/sqlite3";

/// Every value the matrix drives through a boolean position. Written so the
/// literal text is legal in BOTH dialects.
const VALUES: &[&str] = &[
    "0",
    "1",
    "2",
    "-1",
    "0.0",
    "0.5",
    "-0.0",
    "1.0",
    "''",
    "'0'",
    "'1'",
    "'abc'",
    "'3abc'",
    "'0abc'",
    "'1e3'",
    "'.5'",
    "'0x1'",
    "' 1 '",
    "x'00'",
    "x'01'",
    "x'30'", // the byte "0"
    "x'31'", // the byte "1"
    "x''",
    "NULL",
];

fn open(tag: &str) -> (Database, String) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir
        .join(format!("mpedb-bool-{tag}-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    // `t` holds exactly one row (id = 1) so `count(*) ... WHERE <expr>` is the
    // truth value of a constant expression. `flag` is a real bool column and
    // `n` a real int64 column, for the column-valued and Django-shaped cases.
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"flag\"\ntype = \"bool\"\nnullable = true\n\n\
         [[table.column]]\nname = \"n\"\ntype = \"int64\"\nnullable = true\n\n\
         [[table.column]]\nname = \"s\"\ntype = \"text\"\nnullable = true\n"
    );
    let cfg = Config::from_toml_str(&toml).expect("config");
    let db = Database::open_with_config(cfg).expect("open");
    db.query("INSERT INTO t (id, flag, n, s) VALUES (1, NULL, NULL, NULL)", &[])
        .expect("seed row");
    (db, path)
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

/// mpedb's single scalar for a one-row, one-column query, or an error string.
fn mp(db: &Database, sql: &str) -> Result<Value, String> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .ok_or_else(|| "no row".to_string()),
        Ok(other) => Err(format!("not rows: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// sqlite's answer for the same query, rendered as `typeof|quote`.
fn sq(sql: &str) -> Option<String> {
    let script = format!(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, flag BOOLEAN, n INTEGER, s TEXT);\n\
         INSERT INTO t VALUES (1, NULL, NULL, NULL);\n\
         WITH q(x) AS ({sql}) SELECT typeof(x) || '|' || quote(x) FROM q;"
    );
    let out = Command::new(SQLITE3)
        .arg(":memory:")
        .arg(script)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim_end().to_string())
}

/// Compare an mpedb `Value` to sqlite's `typeof|quote`, folding mpedb's `Bool`
/// onto sqlite's integer 0/1 — that IS sqlite's boolean representation, and the
/// C-API shim already surfaces `Value::Bool` as `SQLITE_INTEGER` 0/1.
fn agree(v: &Value, sqlite: &str) -> Result<(), String> {
    let (ty, rep) = sqlite
        .split_once('|')
        .ok_or_else(|| format!("bad sqlite output {sqlite:?}"))?;
    let got = match v {
        Value::Null => "null|NULL".to_string(),
        Value::Bool(b) => format!("integer|{}", *b as i64),
        Value::Int(i) => format!("integer|{i}"),
        Value::Float(f) => format!("real|{f}"),
        Value::Text(s) => format!("text|'{}'", s.replace('\'', "''")),
        other => format!("?|{other:?}"),
    };
    (got == format!("{ty}|{rep}"))
        .then_some(())
        .ok_or_else(|| format!("mpedb {got} vs sqlite {ty}|{rep}"))
}

/// The five boolean positions, as `(label, mpedb sql, sqlite sql)` templates.
/// `{}` is the value under test. mpedb and sqlite take identical SQL here.
fn positions(v: &str) -> Vec<(String, String)> {
    vec![
        // WHERE: does the single row survive?
        (
            format!("WHERE {v}"),
            format!("SELECT count(*) FROM t WHERE {v}"),
        ),
        (
            format!("WHERE NOT {v}"),
            format!("SELECT count(*) FROM t WHERE NOT {v}"),
        ),
        // NOT as a VALUE — this one is 3VL, so NULL must survive as NULL.
        (format!("NOT {v}"), format!("SELECT NOT {v} FROM t")),
        (
            format!("CASE WHEN {v}"),
            format!("SELECT CASE WHEN {v} THEN 'T' ELSE 'F' END FROM t"),
        ),
        (
            format!("{v} AND 1"),
            format!("SELECT ({v}) AND 1 FROM t"),
        ),
        (
            format!("{v} AND 0"),
            format!("SELECT ({v}) AND 0 FROM t"),
        ),
        (format!("{v} OR 1"), format!("SELECT ({v}) OR 1 FROM t")),
        (format!("{v} OR 0"), format!("SELECT ({v}) OR 0 FROM t")),
    ]
}

fn have_sqlite() -> bool {
    std::path::Path::new(SQLITE3).exists()
}

#[test]
fn truthiness_matches_sqlite_in_every_boolean_position() {
    if !have_sqlite() {
        eprintln!("skipping: {SQLITE3} not present");
        return;
    }
    let (db, path) = open("matrix");
    let mut bad = Vec::new();
    let mut checked = 0usize;
    for v in VALUES {
        for (label, sql) in positions(v) {
            let Some(want) = sq(&sql) else {
                bad.push(format!("{label}: sqlite itself errored on `{sql}`"));
                continue;
            };
            match mp(&db, &sql) {
                Ok(got) => {
                    checked += 1;
                    if let Err(why) = agree(&got, &want) {
                        bad.push(format!("{label}: {why}   [{sql}]"));
                    }
                }
                Err(e) => bad.push(format!("{label}: mpedb refused ({e})   [{sql}]")),
            }
        }
    }
    cleanup(&path);
    assert!(checked >= VALUES.len() * 8, "matrix under-ran: {checked}");
    assert!(bad.is_empty(), "{} divergence(s):\n{}", bad.len(), bad.join("\n"));
}

/// The same rule over COLUMN values rather than literals — the shape Django
/// actually emits (`WHERE "tbl"."flag"`), and the path where the value's type
/// is only known at runtime.
#[test]
fn column_values_are_truthy_tested_like_sqlite() {
    if !have_sqlite() {
        return;
    }
    let (db, path) = open("cols");
    // Rows: (n, s) covering the interesting truthiness classes.
    let rows: &[(&str, &str)] = &[
        ("0", "'0'"),
        ("1", "'abc'"),
        ("2", "'3abc'"),
        ("-1", "''"),
        ("NULL", "NULL"),
    ];
    let mut bad = Vec::new();
    for (i, (n, s)) in rows.iter().enumerate() {
        let id = i + 2;
        db.query(
            &format!("INSERT INTO t (id, flag, n, s) VALUES ({id}, NULL, {n}, {s})"),
            &[],
        )
        .expect("insert");
    }
    let inserts: String = rows
        .iter()
        .enumerate()
        .map(|(i, (n, s))| format!("INSERT INTO t VALUES ({}, NULL, {n}, {s});\n", i + 2))
        .collect();
    // `sum(id)` identifies the surviving SET without depending on row order.
    for q in [
        "SELECT sum(id) FROM t WHERE n",
        "SELECT sum(id) FROM t WHERE NOT n",
        "SELECT sum(id) FROM t WHERE s",
        "SELECT sum(id) FROM t WHERE NOT s",
        "SELECT sum(id) FROM t WHERE n AND s",
        "SELECT sum(id) FROM t WHERE n OR s",
        "SELECT sum(id) FROM t WHERE NOT (n OR s)",
        "SELECT count(*) FROM t WHERE CASE WHEN n THEN 1 ELSE 0 END",
        "SELECT sum(CASE WHEN s THEN 10 ELSE 1 END) FROM t",
    ] {
        let script = format!(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, flag BOOLEAN, n INTEGER, s TEXT);\n\
             INSERT INTO t VALUES (1, NULL, NULL, NULL);\n{inserts}\
             WITH r(x) AS ({q}) SELECT typeof(x) || '|' || quote(x) FROM r;"
        );
        let out = Command::new(SQLITE3)
            .arg(":memory:")
            .arg(&script)
            .output()
            .expect("sqlite3");
        let want = String::from_utf8(out.stdout).unwrap().trim_end().to_string();
        match mp(&db, q) {
            Ok(got) => {
                if let Err(why) = agree(&got, &want) {
                    bad.push(format!("{q}: {why}"));
                }
            }
            Err(e) => bad.push(format!("{q}: mpedb refused ({e})")),
        }
    }
    cleanup(&path);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// A real `bool` column must behave EXACTLY as it did before this change —
/// the regression guard for "zero wrong answers".
#[test]
fn bool_column_unchanged() {
    let (db, path) = open("boolcol");
    db.query("UPDATE t SET flag = true WHERE id = 1", &[]).unwrap();
    db.query("INSERT INTO t (id, flag) VALUES (2, false)", &[]).unwrap();
    db.query("INSERT INTO t (id, flag) VALUES (3, NULL)", &[]).unwrap();

    let one = |sql: &str| mp(&db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(one("SELECT count(*) FROM t WHERE flag"), Value::Int(1));
    assert_eq!(one("SELECT count(*) FROM t WHERE NOT flag"), Value::Int(1));
    assert_eq!(one("SELECT count(*) FROM t WHERE flag = true"), Value::Int(1));
    assert_eq!(one("SELECT count(*) FROM t WHERE flag = false"), Value::Int(1));
    assert_eq!(one("SELECT count(*) FROM t WHERE flag IS NULL"), Value::Int(1));
    assert_eq!(one("SELECT flag FROM t WHERE id = 1"), Value::Bool(true));
    assert_eq!(one("SELECT flag FROM t WHERE id = 2"), Value::Bool(false));
    assert_eq!(one("SELECT flag FROM t WHERE id = 3"), Value::Null);
    // A bool PARAMETER still binds as a bool.
    match db.query("SELECT count(*) FROM t WHERE flag = $1", &[Value::Bool(true)]) {
        Ok(ExecResult::Rows { rows, .. }) => assert_eq!(rows[0][0], Value::Int(1)),
        other => panic!("{other:?}"),
    }
    cleanup(&path);
}

/// Django's `BooleanField`: the column is declared `bool`, and every value
/// arrives as the integer 1/0 — as a literal, as a bound parameter, and back
/// out through comparison. `filter(flag=True)` / `exclude(flag=True)`.
#[test]
fn django_boolean_field_shape() {
    let (db, path) = open("django");
    // Django's INSERT binds True as 1.
    db.query("UPDATE t SET flag = 1 WHERE id = 1", &[]).unwrap();
    db.query("INSERT INTO t (id, flag) VALUES (2, 0)", &[]).unwrap();
    db.query("INSERT INTO t (id, flag) VALUES ($1, $2)", &[Value::Int(3), Value::Int(1)])
        .unwrap();

    let one = |sql: &str, p: &[Value]| match db.query(sql, p) {
        Ok(ExecResult::Rows { rows, .. }) => rows[0][0].clone(),
        other => panic!("{sql}: {other:?}"),
    };
    // filter(flag=True) — literal and bound forms.
    assert_eq!(one("SELECT count(*) FROM t WHERE t.flag = 1", &[]), Value::Int(2));
    assert_eq!(
        one("SELECT count(*) FROM t WHERE t.flag = $1", &[Value::Int(1)]),
        Value::Int(2)
    );
    // exclude(flag=True) — Django emits NOT (…) over the same predicate.
    assert_eq!(
        one("SELECT count(*) FROM t WHERE NOT (t.flag = $1)", &[Value::Int(1)]),
        Value::Int(1)
    );
    // filter(flag=False)
    assert_eq!(one("SELECT count(*) FROM t WHERE t.flag = 0", &[]), Value::Int(1));
    // The bare-column form Django uses for `filter(flag=True)` on some paths.
    assert_eq!(one("SELECT count(*) FROM t WHERE t.flag", &[]), Value::Int(2));
    // `flag = 2` is FALSE, not TRUE — the bool is bridged by its integer VALUE,
    // never by truthiness of the right-hand side.
    assert_eq!(one("SELECT count(*) FROM t WHERE t.flag = 2", &[]), Value::Int(0));
    assert_eq!(one("SELECT count(*) FROM t WHERE t.flag = -1", &[]), Value::Int(0));
    // Read-back is still a bool (SQLITE_INTEGER 0/1 through the C-API shim).
    assert_eq!(one("SELECT flag FROM t WHERE id = 3", &[]), Value::Bool(true));
    // An int64 column in a boolean position, the other half of the gap.
    db.query("UPDATE t SET n = 2 WHERE id = 1", &[]).unwrap();
    db.query("UPDATE t SET n = 0 WHERE id = 2", &[]).unwrap();
    assert_eq!(one("SELECT count(*) FROM t WHERE n", &[]), Value::Int(1));
    // A bool-typed expression assigned to an int64 column: sqlite stores 1/0.
    db.query("UPDATE t SET n = (id = 1)", &[]).unwrap();
    assert_eq!(one("SELECT n FROM t WHERE id = 1", &[]), Value::Int(1));
    assert_eq!(one("SELECT n FROM t WHERE id = 2", &[]), Value::Int(0));
    cleanup(&path);
}

/// A parameter bound as an integer where the plan wants a bool — exactly what
/// CPython's `sqlite3` does with `True`/`False` (`sqlite3_bind_int64` 1/0).
#[test]
fn int_parameter_in_a_bool_slot() {
    let (db, path) = open("param");
    db.query("UPDATE t SET flag = true WHERE id = 1", &[]).unwrap();
    let cnt = |sql: &str, p: &[Value]| match db.query(sql, p) {
        Ok(ExecResult::Rows { rows, .. }) => rows[0][0].clone(),
        other => panic!("{sql}: {other:?}"),
    };
    // `$1` unifies to Bool from the predicate position, then takes an Int.
    assert_eq!(cnt("SELECT count(*) FROM t WHERE $1", &[Value::Int(1)]), Value::Int(1));
    assert_eq!(cnt("SELECT count(*) FROM t WHERE $1", &[Value::Int(0)]), Value::Int(0));
    assert_eq!(
        cnt("SELECT count(*) FROM t WHERE NOT $1", &[Value::Int(0)]),
        Value::Int(1)
    );
    assert_eq!(
        cnt("SELECT count(*) FROM t WHERE flag AND $1", &[Value::Int(1)]),
        Value::Int(1)
    );
    // The reverse: a real Bool in an int64 slot is exact (TRUE -> 1).
    assert_eq!(
        cnt("SELECT count(*) FROM t WHERE id = $1", &[Value::Bool(true)]),
        Value::Int(1)
    );
    // A non-0/1 integer in a bool slot is REFUSED, not truthy-tested.
    let e = db
        .query("SELECT count(*) FROM t WHERE $1", &[Value::Int(2)])
        .unwrap_err()
        .to_string();
    assert!(e.contains("statement requires bool"), "{e}");
    cleanup(&path);
}

/// What mpedb deliberately does NOT follow, and why. These are clean refusals,
/// never wrong answers.
#[test]
fn documented_refusals() {
    let (db, path) = open("refuse");
    // 1. A non-0/1 integer INTO a bool column. sqlite stores 2 and reads 2 back;
    //    mpedb's rigid Bool cannot, so it refuses instead of guessing TRUE.
    let e = db
        .query("INSERT INTO t (id, flag) VALUES (9, 2)", &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("cannot be inserted into column"), "{e}");
    let e = db.query("UPDATE t SET flag = 2", &[]).unwrap_err().to_string();
    assert!(e.contains("only the literals 0 and 1"), "{e}");
    // 2. A non-constant int64 expression into a bool column — nothing proves
    //    the value is 0/1.
    let e = db
        .query("UPDATE t SET flag = n", &[])
        .unwrap_err()
        .to_string();
    assert!(e.contains("only the literals 0 and 1"), "{e}");
    // 3. 0 and 1 DO convert, in both statement shapes.
    db.query("INSERT INTO t (id, flag) VALUES (9, 1)", &[]).unwrap();
    db.query("UPDATE t SET flag = 0 WHERE id = 9", &[]).unwrap();
    assert_eq!(mp(&db, "SELECT flag FROM t WHERE id = 9").unwrap(), Value::Bool(false));
    cleanup(&path);
}
