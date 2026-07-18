//! `CAST(x AS <type>)` differential test against sqlite3 3.45.
//!
//! mpedb matches sqlite's PERMISSIVE, AFFINITY-BASED casting: any type name is
//! accepted and folded to one of five affinities (INTEGER/TEXT/BLOB/REAL/
//! NUMERIC) by sqlite's substring rule, then the value is converted
//! permissively (leading-numeric-prefix parses, truncation, %!.15g text) rather
//! than rejected. This test drives `CAST(<value> AS <type>)` through both
//! engines over a matrix of type names × source values and asserts they agree.
//!
//! Reference: `/usr/bin/sqlite3`. Skipped (not failed) if it is absent.
//!
//! Deliberate, documented deviations (NOT diffed here; asserted separately in
//! `documented_deviations`):
//!   * A non-UTF-8 BLOB cast to TEXT has no mpedb representation (mpedb `Text`
//!     is a Rust `String`); mpedb refuses cleanly where sqlite keeps raw bytes.
//!   * An empty type name (`CAST(x AS "")`) is unreachable in mpedb's grammar
//!     (an empty quoted identifier is a tokenizer error), so sqlite's
//!     empty→NUMERIC quirk cannot be expressed and is not tested.

use mpedb::{Config, Database, ExecResult};
use mpedb_types::Value;
use std::process::Command;

const SQLITE3: &str = "/usr/bin/sqlite3";

/// One throwaway database (FROM-less SELECT needs no user tables, but a config
/// needs at least one, so we declare a trivial one and never touch it).
fn open() -> (Database, String) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir
        .join(format!("mpedb-cast-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"txt\"\ntype = \"text\"\nnullable = true\n"
    );
    let cfg = Config::from_toml_str(&toml).expect("config");
    let db = Database::open_with_config(cfg).expect("open");
    (db, path)
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

/// mpedb's single-value result for `SELECT <expr>`, or an error string.
fn mpedb_eval(db: &Database, expr: &str) -> Result<Value, String> {
    match db.query(&format!("SELECT {expr}"), &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .ok_or_else(|| "no row".to_string()),
        Ok(other) => Err(format!("not rows: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// sqlite's `typeof|quote` for `SELECT <expr>`, or `None` if sqlite errored.
fn sqlite_eval(expr: &str) -> Option<String> {
    let out = Command::new(SQLITE3)
        .arg(":memory:")
        .arg(format!("SELECT typeof({expr}) || '|' || quote({expr});"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim_end().to_string())
}

/// Strip sqlite's `quote()` text form: `'ab''c'` → `ab'c`.
fn unquote_text(rep: &str) -> String {
    let inner = rep
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .unwrap_or(rep);
    inner.replace("''", "'")
}

/// Compare mpedb's `Value` to sqlite's `typeof|quote` string. `Ok(())` on
/// agreement, `Err(reason)` otherwise.
fn agree(v: &Value, sqlite: &str) -> Result<(), String> {
    let (ty, rep) = sqlite
        .split_once('|')
        .ok_or_else(|| format!("bad sqlite output {sqlite:?}"))?;
    match v {
        Value::Null => (ty == "null")
            .then_some(())
            .ok_or_else(|| format!("mpedb NULL vs sqlite {ty} {rep}")),
        Value::Int(i) => (ty == "integer" && rep == i.to_string())
            .then_some(())
            .ok_or_else(|| format!("mpedb integer {i} vs sqlite {ty} {rep}")),
        Value::Float(f) => {
            if ty != "real" {
                return Err(format!("mpedb real {f} vs sqlite {ty} {rep}"));
            }
            // Compare the VALUE numerically — sqlite's 15-digit text may not
            // round-trip a divided real exactly, but every value in the matrix
            // is a clean literal/conversion. (Real → TEXT exactness is checked
            // separately via the TEXT-affinity rows, which compare strings.)
            let r: f64 = rep.parse().map_err(|_| format!("unparseable real {rep}"))?;
            let tol = 1e-9 * f.abs().max(1.0);
            ((f - r).abs() <= tol)
                .then_some(())
                .ok_or_else(|| format!("mpedb real {f} vs sqlite real {r}"))
        }
        Value::Text(s) => {
            if ty != "text" {
                return Err(format!("mpedb text {s:?} vs sqlite {ty} {rep}"));
            }
            let want = unquote_text(rep);
            (*s == want)
                .then_some(())
                .ok_or_else(|| format!("mpedb text {s:?} vs sqlite text {want:?}"))
        }
        Value::Blob(b) => {
            let want = format!(
                "X'{}'",
                b.iter().map(|x| format!("{x:02X}")).collect::<String>()
            );
            (ty == "blob" && rep == want)
                .then_some(())
                .ok_or_else(|| format!("mpedb blob {want} vs sqlite {ty} {rep}"))
        }
        other => Err(format!("mpedb produced {other:?} (unexpected for a CAST)")),
    }
}

/// The full type-name × value matrix.
fn matrix() -> Vec<String> {
    // Type names: the five affinities reached by both canonical and unknown/
    // multi-word/parenthesized names.
    let types = [
        "INTEGER",
        "INT",
        "SIGNED",   // -> NUMERIC (unknown name)
        "DECIMAL",  // -> NUMERIC
        "NUMERIC",
        "BOOLEAN",  // -> NUMERIC (sqlite has no bool affinity)
        "REAL",
        "DOUBLE",           // -> REAL ("DOUB")
        "DOUBLE PRECISION", // -> REAL, multi-word
        "TEXT",
        "VARCHAR",
        "VARCHAR(10)",       // size dropped
        "CHARACTER VARYING", // -> TEXT ("CHAR"), multi-word
        "CLOB",
        "BLOB",
        "TINYBLOB", // -> BLOB (contains "blob")
    ];
    // Source values: int, real, text-with-prefix, text-non-numeric, text-real,
    // text-exp, whitespace text, NULL, and a UTF-8 blob.
    let values = [
        "90", "2.9", "-1.9", "'12ab'", "'abc'", "'3.0'", "'1e3'", "'  12 '", "NULL", "x'41'",
    ];
    let mut out = Vec::new();
    for t in types {
        for v in values {
            out.push(format!("CAST({v} AS {t})"));
        }
    }
    // Real → TEXT stress cases: these produce TEXT, so the strings are compared
    // exactly, validating mpedb's %!.15g formatter against sqlite's.
    for v in [
        "1.0",
        "100.0",
        "1e20",
        "1e-20",
        "0.1",
        "1234567.5",
        "123456789012345.0",
        "1e15",
        "-2.5",
    ] {
        out.push(format!("CAST({v} AS TEXT)"));
        out.push(format!("CAST({v} AS BLOB)"));
    }
    // NUMERIC int/real decision over text (the 2^51 boundary and exp forms).
    for v in [
        "'3.5'",
        "'1e15'",
        "'1e16'",
        "'9999999999999999'",
        "'2251799813685247.0'",
        "'2251799813685248.0'",
        // i64 magnitude boundaries: MAX (int), MIN (int — u64 magnitude), and
        // one past MAX (overflow → real).
        "'9223372036854775807'",
        "'-9223372036854775808'",
        "'9223372036854775808'",
    ] {
        out.push(format!("CAST({v} AS NUMERIC)"));
    }
    out
}

#[test]
fn cast_matches_sqlite_across_affinities() {
    if !std::path::Path::new(SQLITE3).exists() {
        eprintln!("skipping: {SQLITE3} not found");
        return;
    }
    let (db, path) = open();
    let mut mismatches = Vec::new();
    for expr in matrix() {
        let Some(sqlite) = sqlite_eval(&expr) else {
            mismatches.push(format!("{expr}: sqlite errored (unexpected)"));
            continue;
        };
        match mpedb_eval(&db, &expr) {
            Ok(v) => {
                if let Err(why) = agree(&v, &sqlite) {
                    mismatches.push(format!("{expr}: {why}"));
                }
            }
            Err(e) => mismatches.push(format!("{expr}: mpedb error `{e}` (sqlite: {sqlite})")),
        }
    }
    cleanup(&path);
    assert!(
        mismatches.is_empty(),
        "CAST diverged from sqlite in {} case(s):\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The runtime (non-constant-folded) path: a NUMERIC cast of a text parameter
/// is decided PER VALUE — integer when integral, real otherwise. NUMERIC does
/// not pin its parameter (it has no single storage type), so the same plan
/// serves any input.
#[test]
fn numeric_affinity_is_per_value_at_runtime() {
    let (db, path) = open();
    let eval = |expr: &str, p: Value| match db.query(&format!("SELECT {expr}"), &[p]) {
        Ok(ExecResult::Rows { rows, .. }) => Ok(rows[0][0].clone()),
        Ok(other) => Err(format!("{other:?}")),
        Err(e) => Err(e.to_string()),
    };
    // NUMERIC: same plan, different runtime type per input value.
    assert_eq!(eval("CAST($1 AS NUMERIC)", Value::Text("42".into())), Ok(Value::Int(42)));
    assert_eq!(eval("CAST($1 AS NUMERIC)", Value::Text("3.5".into())), Ok(Value::Float(3.5)));
    assert_eq!(eval("CAST($1 AS NUMERIC)", Value::Text("1e3".into())), Ok(Value::Int(1000)));
    // The four fixed affinities PIN a bare parameter to their storage type (PG's
    // canonical way to type a `?`): a mismatched param value is refused, not
    // silently coerced — the cast is the identity on a correctly-typed param.
    assert!(eval("CAST($1 AS INTEGER)", Value::Text("12ab".into())).is_err());
    assert_eq!(eval("CAST($1 AS INTEGER)", Value::Int(7)), Ok(Value::Int(7)));
    assert_eq!(eval("CAST($1 AS TEXT)", Value::Text("hi".into())), Ok(Value::Text("hi".into())));
    cleanup(&path);
}

/// The corpus scenario: `CAST(text_col AS NUMERIC)` — the result column is
/// `Any` (int-or-real per row). It must bind, run, and produce the same
/// per-value types as sqlite over a real column of data.
#[test]
fn numeric_cast_of_a_text_column_matches_sqlite() {
    let (db, path) = open();
    for (id, txt) in [(1, "42"), (2, "3.5"), (3, "1e3"), (4, "abc"), (5, "12ab")] {
        db.query(
            "INSERT INTO t (id, txt) VALUES ($1, $2)",
            &[Value::Int(id), Value::Text(txt.into())],
        )
        .expect("insert");
    }
    let rows = match db.query("SELECT CAST(txt AS NUMERIC) FROM t ORDER BY id", &[]) {
        Ok(ExecResult::Rows { rows, .. }) => rows,
        other => panic!("{other:?}"),
    };
    let got: Vec<Value> = rows.into_iter().map(|r| r.into_iter().next().unwrap()).collect();
    assert_eq!(
        got,
        vec![
            Value::Int(42),
            Value::Float(3.5),
            Value::Int(1000),
            Value::Int(0),
            Value::Int(12),
        ],
        "per-value int/real from a NUMERIC cast of a text column"
    );
    cleanup(&path);
}

#[test]
fn documented_deviations() {
    let (db, path) = open();

    // A non-UTF-8 BLOB → TEXT is refused cleanly (sqlite keeps the raw bytes;
    // mpedb's TEXT is a Rust String and cannot hold them). Never a wrong answer.
    let r = mpedb_eval(&db, "CAST(x'ff' AS TEXT)");
    assert!(
        r.is_err(),
        "non-UTF-8 blob→text must refuse cleanly, got {r:?}"
    );

    // A valid-UTF-8 blob → TEXT still works (the common case).
    assert_eq!(
        mpedb_eval(&db, "CAST(x'41' AS TEXT)"),
        Ok(Value::Text("A".into()))
    );

    // An empty type name is a parse error (empty quoted identifier), so the
    // sqlite empty→NUMERIC quirk is simply not expressible.
    assert!(mpedb_eval(&db, "CAST(90 AS \"\")").is_err());

    cleanup(&path);
}
