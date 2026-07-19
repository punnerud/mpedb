//! `typeof()` reports a sqlite STORAGE CLASS, never an mpedb type name.
//!
//! `typeof()` is a borrowed sqlite function and it borrows sqlite's contract:
//! its documented range is exactly `null`/`integer`/`real`/`text`/`blob`, and
//! every consumer switches on exactly those five. mpedb's own first-class
//! `Bool` and `Timestamp` used to answer `'boolean'`/`'timestamp'` — honest
//! about mpedb's type system, but a DIFFERENT ANSWER rather than an error to a
//! caller asking a sqlite question, and one that contradicted the C-API's
//! `sqlite3_column_type`, which has always mapped both onto `SQLITE_INTEGER`.
//!
//! Reference: `/usr/bin/sqlite3` (3.45); the differential is skipped, not
//! failed, if the binary is absent.

use mpedb::{params, Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::process::Command;

const SQLITE3: &str = "/usr/bin/sqlite3";

/// Takes its file with it when it dies (the `/dev/shm` leak discipline).
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

fn db(tag: &str, schema: &str) -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir
        .join(format!("mpedb-typeofcls-{tag}-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let db = Database::open_with_config(
        Config::from_toml_str(&format!(
            "[database]\npath = \"{path}\"\nsize_mb = 8\n{schema}"
        ))
        .unwrap(),
    )
    .unwrap();
    Tmp { db, path }
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &params![]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("not rows: {other:?}"),
    }
}

/// sqlite's answer for `script`, one line per row, columns joined by `|`.
fn sq(script: &str) -> Option<Vec<String>> {
    let out = Command::new(SQLITE3).arg(":memory:").arg(script).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8(out.stdout)
            .ok()?
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect(),
    )
}

const SCHEMA: &str = r#"[[table]]
name = "t"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "b"
  type = "bool"
  nullable = true
  [[table.column]]
  name = "ts"
  type = "timestamp"
  nullable = true
  [[table.column]]
  name = "a"
  type = "any"
  nullable = true
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
  [[table.column]]
  name = "l"
  type = "blob"
  nullable = true
"#;

/// The rows, as (bool, timestamp-micros, any-value) plus the four rigid
/// columns. Each row exercises a different class in the `any` column.
fn fixture() -> Vec<(Option<bool>, Option<i64>, Value)> {
    vec![
        (Some(true), Some(1_720_000_000_000_000), Value::Text("str".into())),
        (Some(false), Some(0), Value::Int(42)),
        (None, None, Value::Float(1.5)),
        (Some(true), Some(-1), Value::Blob(vec![1, 2])),
        (None, None, Value::Null),
    ]
}

#[test]
fn typeof_over_every_class_and_every_mpedb_type_matches_sqlite() {
    let d = db("all", SCHEMA);
    let ins = d
        .prepare("INSERT INTO t (id, b, ts, a, i, f, s, l) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
        .unwrap();
    for (n, (b, ts, a)) in fixture().into_iter().enumerate() {
        let id = n as i64 + 1;
        d.execute(
            &ins,
            &params![
                id,
                b.map(Value::Bool).unwrap_or(Value::Null),
                ts.map(Value::Timestamp).unwrap_or(Value::Null),
                a,
                Value::Int(7),
                Value::Float(2.5),
                Value::Text("txt".into()),
                Value::Blob(vec![0, 255])
            ],
        )
        .unwrap();
    }

    let q = "SELECT typeof(b), typeof(ts), typeof(a), typeof(i), typeof(f), \
             typeof(s), typeof(l) FROM t ORDER BY id";
    let got: Vec<String> = rows(&d, q)
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match v {
                    Value::Text(s) => s,
                    other => panic!("typeof returned a non-text {other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();

    // The range is CLOSED. This is the property the whole change is about: no
    // value of ANY mpedb column type may name a sixth string.
    for line in &got {
        for name in line.split('|') {
            assert!(
                matches!(name, "null" | "integer" | "real" | "text" | "blob"),
                "typeof answered {name:?}, outside sqlite's five storage classes: {line}"
            );
        }
    }

    // ...and specifically: bool and timestamp are `integer`, never their own
    // names. Pinned independently of the differential so the assertion survives
    // a box with no sqlite3 binary.
    assert_eq!(
        got,
        vec![
            "integer|integer|text|integer|real|text|blob",
            "integer|integer|integer|integer|real|text|blob",
            "null|null|real|integer|real|text|blob",
            "integer|integer|blob|integer|real|text|blob",
            "null|null|null|integer|real|text|blob",
        ]
    );

    // --- the differential. sqlite has no bool/timestamp/any types; it takes
    // the same declared words and applies its affinity rule, and a bool is its
    // integer 0/1 while a timestamp is its integer microseconds — which is
    // exactly the representation the C-API shim already surfaces for both.
    let script = "CREATE TABLE t (id integer PRIMARY KEY, b bool, ts timestamp, a any, \
                  i int64, f float64, s text, l blob);\n\
                  INSERT INTO t VALUES (1, 1, 1720000000000000, 'str', 7, 2.5, 'txt', x'00ff');\n\
                  INSERT INTO t VALUES (2, 0, 0, 42, 7, 2.5, 'txt', x'00ff');\n\
                  INSERT INTO t VALUES (3, NULL, NULL, 1.5, 7, 2.5, 'txt', x'00ff');\n\
                  INSERT INTO t VALUES (4, 1, -1, x'0102', 7, 2.5, 'txt', x'00ff');\n\
                  INSERT INTO t VALUES (5, NULL, NULL, NULL, 7, 2.5, 'txt', x'00ff');\n\
                  SELECT typeof(b), typeof(ts), typeof(a), typeof(i), typeof(f), \
                  typeof(s), typeof(l) FROM t ORDER BY id;";
    let Some(want) = sq(script) else {
        eprintln!("sqlite3 unavailable — differential skipped");
        return;
    };
    assert_eq!(got, want, "mpedb typeof() disagrees with sqlite 3.45");
}

#[test]
fn typeof_over_literals_and_expressions_matches_sqlite() {
    let d = db("lit", SCHEMA);
    // Every literal class plus the expression forms whose result class is not
    // its operands' (integer + integer stays integer; a real anywhere wins).
    let exprs = [
        "NULL", "1", "1.5", "'x'", "''", "x'00ff'", "2 + 3", "1.0 * 2", "2 / 1", "1 / 2.0",
        "'a' || 'b'", "length('abc')", "abs(-1.5)", "hex(x'ff')", "typeof(1)",
    ];
    let select = exprs
        .iter()
        .map(|e| format!("typeof({e})"))
        .collect::<Vec<_>>()
        .join(", ");

    let got = rows(&d, &format!("SELECT {select}"))
        .into_iter()
        .next()
        .unwrap()
        .into_iter()
        .map(|v| match v {
            Value::Text(s) => s,
            other => panic!("typeof returned a non-text {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("|");

    for name in got.split('|') {
        assert!(
            matches!(name, "null" | "integer" | "real" | "text" | "blob"),
            "typeof answered {name:?}, outside sqlite's five storage classes"
        );
    }

    let Some(want) = sq(&format!("SELECT {select};")) else {
        eprintln!("sqlite3 unavailable — differential skipped");
        return;
    };
    assert_eq!(vec![got], want, "mpedb typeof() disagrees with sqlite 3.45");
}
