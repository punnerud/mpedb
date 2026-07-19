//! Mixed CASE / COALESCE arm types — differential test against sqlite3 3.45.
//!
//! sqlite types a CASE/COALESCE result per ROW: the arm actually taken keeps
//! its own type, so `COALESCE(NULL, 1, 2.5)` is the INTEGER 1, never 1.0.
//! mpedb reproduces that exactly: arms keep their own types and values (no
//! widening cast on any arm), and the expression's static type is `any`,
//! decided per value at runtime. Widening instead was measured at 82 wrong
//! answers in the sqllogictest expr corpus — this test is the guard that the
//! per-arm semantics hold in every consumer.
//!
//! Two parts:
//!  * a FROM-less matrix — every arm-value pair {int, real, NULL, text, blob}
//!    x {COALESCE, 3-arg COALESCE, IFNULL, NULLIF, iif taken/not-taken,
//!    searched CASE both branches, simple CASE both branches}, compared on
//!    typeof() AND quote() (value + type must both match sqlite). Pairs
//!    within {int, real, NULL} must never be refused (that is the fix);
//!    non-numeric mixes MAY refuse (mpedb's documented deviation) but must
//!    agree whenever they are accepted.
//!  * a table battery — per-row winners, aggregates as arms, aggregates OVER
//!    mixed arms, and the `any` result feeding WHERE / arithmetic / ORDER BY
//!    / DISTINCT / GROUP BY, cell-compared against sqlite (an integer cell
//!    must be an integer in both engines — sqlite prints reals with a '.').
//!
//! Reference: `/usr/bin/sqlite3`. Skipped (not failed) if it is absent.

use mpedb::{Config, Database, ExecResult};
use mpedb_types::Value;
use std::process::Command;

const SQLITE3: &str = "/usr/bin/sqlite3";

/// The sqlite side of the shared table: same rows mpedb is seeded with.
/// Row 5 holds the deliberate int/real collision (i=1 vs f=1.0) so DISTINCT
/// and GROUP BY dedup across storage classes is exercised.
const SQLITE_SEED: &str = "CREATE TABLE t(pk INTEGER PRIMARY KEY, i INT, f REAL, s TEXT);\n\
     INSERT INTO t VALUES (1, 10, 2.5, 'a'), (2, NULL, 0.25, 'b'),\n\
     (3, 7, NULL, NULL), (4, NULL, NULL, 'd'), (5, 1, 1.0, 'e');";

fn open(name: &str) -> (Database, String) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir
        .join(format!("mpedb-mixedarms-{name}-{}.mpedb", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"t\"\nprimary_key = [\"pk\"]\n\n\
         [[table.column]]\nname = \"pk\"\ntype = \"int64\"\n\n\
         [[table.column]]\nname = \"i\"\ntype = \"int64\"\nnullable = true\n\n\
         [[table.column]]\nname = \"f\"\ntype = \"float64\"\nnullable = true\n\n\
         [[table.column]]\nname = \"s\"\ntype = \"text\"\nnullable = true\n"
    );
    let cfg = Config::from_toml_str(&toml).expect("config");
    let db = Database::open_with_config(cfg).expect("open");
    db.query(
        "INSERT INTO t VALUES (1, 10, 2.5, 'a'), (2, NULL, 0.25, 'b'), \
         (3, 7, NULL, NULL), (4, NULL, NULL, 'd'), (5, 1, 1.0, 'e')",
        &[],
    )
    .expect("seed");
    (db, path)
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}

fn sqlite_present() -> bool {
    std::path::Path::new(SQLITE3).exists()
}

/// mpedb's rows for `sql`, or the error string.
fn mpedb_rows(db: &Database, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    match db.query(sql, &[]) {
        Ok(ExecResult::Rows { rows, .. }) => Ok(rows),
        Ok(other) => Err(format!("not rows: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// sqlite's list-mode rows for `sql` over the seeded table, or `None` if it
/// errored. Cells are the CLI's default `|`-separated text.
fn sqlite_rows(sql: &str) -> Option<Vec<Vec<String>>> {
    let out = Command::new(SQLITE3)
        .arg(":memory:")
        .arg(format!("{SQLITE_SEED}\n{sql};"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(
        s.lines()
            .map(|l| l.split('|').map(str::to_string).collect())
            .collect(),
    )
}

/// One cell: does mpedb's typed `Value` match sqlite's list-mode text — value
/// AND storage class? sqlite prints every REAL with a `.` (or exponent) and
/// every INTEGER without, so the class check needs no typeof() column.
fn cell_agree(v: &Value, s: &str) -> bool {
    match v {
        Value::Null => s.is_empty(),
        Value::Int(i) => !s.contains('.') && s.parse::<i64>() == Ok(*i),
        Value::Float(f) => {
            (s.contains('.') || s.contains('e') || s.contains('E'))
                && s.parse::<f64>()
                    .is_ok_and(|r| (f - r).abs() <= 1e-9 * f.abs().max(1.0))
        }
        Value::Text(t) => t == s,
        _ => false, // no bool/blob/timestamp is expected out of these queries
    }
}

fn rows_agree(mine: &[Vec<Value>], theirs: &[Vec<String>]) -> Result<(), String> {
    if mine.len() != theirs.len() {
        return Err(format!("{} rows vs sqlite {}", mine.len(), theirs.len()));
    }
    for (rn, (m, t)) in mine.iter().zip(theirs).enumerate() {
        if m.len() != t.len() {
            return Err(format!("row {rn}: {} cols vs sqlite {}", m.len(), t.len()));
        }
        for (cn, (v, s)) in m.iter().zip(t).enumerate() {
            if !cell_agree(v, s) {
                return Err(format!("row {rn} col {cn}: mpedb {v:?} vs sqlite {s:?}"));
            }
        }
    }
    Ok(())
}

/// sqlite's `typeof|quote` for a FROM-less `SELECT <expr>`, or `None` on error.
fn sqlite_eval(expr: &str) -> Option<String> {
    let out = Command::new(SQLITE3)
        .arg(":memory:")
        .arg(format!("SELECT typeof({expr}) || '|' || quote({expr});"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok().map(|s| s.trim_end().to_string())
}

/// mpedb's single value for a FROM-less `SELECT <expr>`, or the error string.
fn mpedb_eval(db: &Database, expr: &str) -> Result<Value, String> {
    mpedb_rows(db, &format!("SELECT {expr}"))?
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .ok_or_else(|| "no row".to_string())
}

/// Compare mpedb's `Value` against sqlite's `typeof|quote` text.
fn agree(v: &Value, sqlite: &str) -> Result<(), String> {
    let (ty, rep) = sqlite
        .split_once('|')
        .ok_or_else(|| format!("bad sqlite output {sqlite:?}"))?;
    let unquote = |r: &str| -> String {
        r.strip_prefix('\'')
            .and_then(|x| x.strip_suffix('\''))
            .unwrap_or(r)
            .replace("''", "'")
    };
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
            let r: f64 = rep.parse().map_err(|_| format!("unparseable real {rep}"))?;
            ((f - r).abs() <= 1e-9 * f.abs().max(1.0))
                .then_some(())
                .ok_or_else(|| format!("mpedb real {f} vs sqlite real {r}"))
        }
        Value::Text(s) => (ty == "text" && *s == unquote(rep))
            .then_some(())
            .ok_or_else(|| format!("mpedb text {s:?} vs sqlite {ty} {rep}")),
        Value::Blob(b) => {
            let want = format!(
                "X'{}'",
                b.iter().map(|x| format!("{x:02X}")).collect::<String>()
            );
            (ty == "blob" && rep == want)
                .then_some(())
                .ok_or_else(|| format!("mpedb blob vs sqlite {ty} {rep}"))
        }
        other => Err(format!("unexpected mpedb value {other:?}")),
    }
}

/// The FROM-less matrix: every arm-type pair through every polymorphic
/// construct. Numeric pairs must all bind and agree; non-numeric mixes may
/// refuse (documented deviation) but must agree whenever accepted.
#[test]
fn fromless_arm_matrix() {
    if !sqlite_present() {
        eprintln!("skipping: {SQLITE3} not present");
        return;
    }
    let (db, path) = open("matrix");

    // (literal, is-numeric-or-null) — bool/timestamp are mpedb-only types
    // with no sqlite literal, so they cannot appear in a differential matrix.
    let arms: &[(&str, bool)] = &[
        ("1", true),
        ("-60", true),
        ("2.5", true),
        ("50 / 84.0", true),
        ("NULL", true),
        ("'a'", false),
        ("x'ab'", false),
    ];
    let shapes: &[fn(&str, &str) -> String] = &[
        |a, b| format!("coalesce({a}, {b})"),
        |a, b| format!("coalesce(NULL, {a}, {b})"),
        |a, b| format!("ifnull({a}, {b})"),
        |a, b| format!("nullif({a}, {b})"),
        |a, b| format!("iif(1, {a}, {b})"),
        |a, b| format!("iif(0, {a}, {b})"),
        |a, b| format!("CASE WHEN 1 THEN {a} ELSE {b} END"),
        |a, b| format!("CASE WHEN 0 THEN {a} ELSE {b} END"),
        |a, b| format!("CASE 5 WHEN 5 THEN {a} ELSE {b} END"),
        |a, b| format!("CASE 5 WHEN 6 THEN {a} ELSE {b} END"),
    ];

    let (mut compared, mut refused) = (0u32, 0u32);
    for (a, a_num) in arms {
        for (b, b_num) in arms {
            for shape in shapes {
                let expr = shape(a, b);
                match mpedb_eval(&db, &expr) {
                    Ok(v) => {
                        let want = sqlite_eval(&expr)
                            .unwrap_or_else(|| panic!("sqlite errored on {expr}"));
                        if let Err(e) = agree(&v, &want) {
                            panic!("DISAGREE on {expr}: {e}");
                        }
                        compared += 1;
                    }
                    Err(e) => {
                        // The fix under test: a numeric (or NULL) arm pair may
                        // NEVER be refused. NULLIF over a cross-class pair is
                        // its a=b comparison refusing, which is allowed.
                        assert!(
                            !(*a_num && *b_num),
                            "numeric arms refused: {expr}: {e}"
                        );
                        refused += 1;
                    }
                }
            }
        }
    }
    // 5 numeric/NULL arms x 5 x 10 shapes = 250 cells must ALL have compared.
    assert!(compared >= 250, "only {compared} compared, {refused} refused");
    cleanup(&path);
}

/// The corpus classics, verbatim — the exact statements CORPUS-STATUS.md
/// hand-verified as refused before this change.
#[test]
fn corpus_classics() {
    if !sqlite_present() {
        eprintln!("skipping: {SQLITE3} not present");
        return;
    }
    let (db, path) = open("classics");
    for expr in [
        "coalesce(NULL, 1, 2.5)",
        "CASE WHEN 1=1 THEN 1 ELSE 2.5 END",
        "coalesce(-60, 50/84.0)",
        "coalesce(30, 1.5) / 35",     // integer division: the 82-wrong-answers shape
        "coalesce(NULL, 2.5, 1) / 2", // real wins -> real division
        "typeof(coalesce(NULL, 1, 2.5))",
        "typeof(coalesce(NULL, 2.5, 1))",
    ] {
        let v = mpedb_eval(&db, expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
        let want = sqlite_eval(expr).expect("sqlite");
        agree(&v, &want).unwrap_or_else(|e| panic!("DISAGREE on {expr}: {e}"));
    }
    cleanup(&path);
}

/// The table battery: per-row winners, aggregates as arms, aggregates over
/// mixed arms, and the `any` result flowing into WHERE, arithmetic, ORDER BY,
/// DISTINCT and GROUP BY. Identical SQL against both engines; every cell must
/// match in value AND storage class.
#[test]
fn table_battery() {
    if !sqlite_present() {
        eprintln!("skipping: {SQLITE3} not present");
        return;
    }
    let (db, path) = open("battery");
    let queries = [
        // Per-row winner keeps its own type, and typeof() sees it per row.
        "SELECT pk, coalesce(i, f), typeof(coalesce(i, f)) FROM t ORDER BY pk",
        "SELECT pk, CASE WHEN i IS NULL THEN f ELSE i END, \
                typeof(CASE WHEN i IS NULL THEN f ELSE i END) FROM t ORDER BY pk",
        "SELECT pk, ifnull(i, 0.5), typeof(ifnull(i, 0.5)) FROM t ORDER BY pk",
        "SELECT pk, iif(i IS NULL, 0.5, i), typeof(iif(i IS NULL, 0.5, i)) FROM t ORDER BY pk",
        "SELECT pk, CASE pk WHEN 1 THEN 2.5 ELSE pk END, \
                typeof(CASE pk WHEN 1 THEN 2.5 ELSE pk END) FROM t ORDER BY pk",
        // Aggregates as arms: the int arm must stay an INTEGER when it wins.
        "SELECT coalesce(NULL, 1, avg(i)), typeof(coalesce(NULL, 1, avg(i))) FROM t",
        "SELECT coalesce(30, avg(f)) / 35 FROM t",
        "SELECT coalesce(avg(f), 1), typeof(coalesce(avg(f), 1)) FROM t",
        "SELECT CASE WHEN count(*) > 100 THEN 1 ELSE avg(f) END FROM t",
        "SELECT CASE WHEN count(*) > 0 THEN sum(i) ELSE 2.5 END FROM t",
        // Aggregates OVER mixed arms: sum/avg accumulate int-and-real like
        // sqlite; min/max/count order across the classes.
        "SELECT sum(CASE WHEN pk > 2 THEN 1 ELSE 0.5 END), \
                avg(CASE WHEN pk > 2 THEN 1 ELSE 0.5 END), \
                min(coalesce(i, f)), max(coalesce(i, f)), count(coalesce(i, f)) FROM t",
        // WHERE: the `any` result is truthy-tested per value.
        "SELECT pk FROM t WHERE CASE WHEN pk < 3 THEN 1 ELSE 0.0 END ORDER BY pk",
        // Arithmetic over the `any` result settles per value: int stays int.
        "SELECT pk, coalesce(i, 2.5) * 2 FROM t ORDER BY pk",
        // ORDER BY an `any` key (NULL first, then numeric across classes).
        "SELECT pk FROM t ORDER BY coalesce(i, f), pk",
        // DISTINCT dedups integer 1 against real 1.0 (one value, as sqlite).
        "SELECT DISTINCT coalesce(i, f) FROM t ORDER BY 1",
        "SELECT DISTINCT coalesce(f, 1) FROM t ORDER BY 1",
        // GROUP BY an `any` key: 1 and 1.0 land in ONE group of 3.
        "SELECT coalesce(f, 1), count(*) FROM t GROUP BY coalesce(f, 1) ORDER BY 1",
        "SELECT coalesce(f, 1), count(*) FROM t GROUP BY coalesce(f, 1) \
         HAVING count(*) > 1 ORDER BY 1",
        // Comparisons against the `any` result settle per value — the
        // integer 1 and the real 1.0 both satisfy `= 1`.
        "SELECT pk FROM t WHERE coalesce(i, f) = 1 ORDER BY pk",
        "SELECT pk FROM t WHERE coalesce(i, f) > 2 ORDER BY pk",
        "SELECT pk FROM t WHERE coalesce(i, f) BETWEEN 0.5 AND 8 ORDER BY pk",
    ];
    for sql in queries {
        let mine = mpedb_rows(&db, sql).unwrap_or_else(|e| panic!("mpedb refused {sql}: {e}"));
        let theirs = sqlite_rows(sql).unwrap_or_else(|| panic!("sqlite errored on {sql}"));
        rows_agree(&mine, &theirs).unwrap_or_else(|e| panic!("DISAGREE on {sql}: {e}"));
    }
    cleanup(&path);
}

/// The `any`-typed result must survive the compiled-plan path too: encode,
/// re-validating decode, shared-registry execute — not just the direct query.
#[test]
fn prepared_plan_roundtrip() {
    let (db, path) = open("plan");
    let hash = db
        .prepare("SELECT coalesce(i, f) FROM t WHERE pk = $1")
        .expect("prepare an any-typed projection");
    let get = |pk: i64| match db.execute(&hash, &[Value::Int(pk)]) {
        Ok(ExecResult::Rows { rows, .. }) => rows[0][0].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(get(1), Value::Int(10)); // int arm wins, stays int
    assert_eq!(get(2), Value::Float(0.25)); // real arm wins, stays real
    assert_eq!(get(4), Value::Null);
    cleanup(&path);
}
