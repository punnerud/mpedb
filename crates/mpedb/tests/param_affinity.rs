//! Comparison affinity for BOUND PARAMETERS, in an index range bound.
//!
//! sqlite applies the column's affinity to the other side of a comparison
//! before comparing (`OP_Affinity`/`applyAffinity`): `lat BETWEEN '59.8' AND
//! '59.9'` on a REAL column compares numbers, not text. Every client that
//! binds through a string-typed protocol depends on it — PDO binds every
//! `execute([...])` parameter as TEXT unless told otherwise, which is what
//! `api.php` does on every request.
//!
//! mpedb applies it to the residual filter and to plan-time constants
//! (`EXPLAIN` prints `affinity(lat, REAL)`), but an index range bound built
//! from a parameter kept the parameter's own type. A TEXT bound over a numeric
//! index orders above every number in `keycode`, so the scan matched nothing
//! and the query answered ZERO ROWS — no error, no warning, just an empty
//! result where 45 915 rows exist.
//!
//! Found by the politihelikopter.com shadow run: sqlite 6207 rows, mpedb 0,
//! on the site's own area query. Every other shape agreed, which is what made
//! it dangerous — a PK lookup, an index EQUALITY probe and an unindexed column
//! all coerce correctly, so nothing short of this exact shape shows it.

use mpedb::{Database, ExecResult, Value};
use mpedb_types::Config;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};

static UNIQ: AtomicU32 = AtomicU32::new(0);

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

fn open(tag: &str) -> Tmp {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-affin-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for d in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, lat REAL NOT NULL, n INTEGER NOT NULL, \
         s TEXT NOT NULL)",
        "CREATE INDEX t_lat ON t (lat)",
        "CREATE INDEX t_n ON t (n)",
    ] {
        db.query(d, &[]).unwrap();
    }
    let mut w = db.begin().unwrap();
    for i in 1..=20i64 {
        w.query(
            "INSERT INTO t (id, lat, n, s) VALUES ($1, $2, $3, $4)",
            &[
                Value::Int(i),
                Value::Float(59.0 + i as f64 * 0.01),
                Value::Int(i * 10),
                Value::Text(format!("r{i}")),
            ],
        )
        .unwrap();
    }
    w.commit().unwrap();
    Tmp { db, path }
}

fn n_of(r: ExecResult) -> i64 {
    match r {
        ExecResult::Rows { rows, .. } => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(i)) => *i,
            other => panic!("expected a count, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The case the shadow run found: a range over a REAL index, bounds bound as
/// TEXT. This is exactly what PDO sends.
#[test]
fn a_text_parameter_bounds_a_real_index_range() {
    let d = open("realrange");
    let want = n_of(d.query("SELECT COUNT(*) FROM t WHERE lat BETWEEN 59.05 AND 59.15", &[]).unwrap());
    assert_eq!(want, 11, "the literal form, as the fixture is built");

    let got = n_of(
        d.query(
            "SELECT COUNT(*) FROM t WHERE lat BETWEEN $1 AND $2",
            &[Value::Text("59.05".into()), Value::Text("59.15".into())],
        )
        .unwrap(),
    );
    assert_eq!(got, want, "a TEXT bound over a REAL index must compare as a number");
}

/// The same for an INTEGER index — the other numeric affinity.
#[test]
fn a_text_parameter_bounds_an_integer_index_range() {
    let d = open("intrange");
    let want = n_of(d.query("SELECT COUNT(*) FROM t WHERE n BETWEEN 50 AND 150", &[]).unwrap());
    assert_eq!(want, 11);
    let got = n_of(
        d.query(
            "SELECT COUNT(*) FROM t WHERE n BETWEEN $1 AND $2",
            &[Value::Text("50".into()), Value::Text("150".into())],
        )
        .unwrap(),
    );
    assert_eq!(got, want, "a TEXT bound over an INTEGER index must compare as a number");
}

/// The shapes that already agreed. They are here so a fix cannot buy the
/// broken case by breaking a working one.
#[test]
fn the_shapes_that_already_worked_still_work() {
    let d = open("others");
    // PK equality and PK range.
    assert_eq!(n_of(d.query("SELECT COUNT(*) FROM t WHERE id = $1", &[Value::Text("7".into())]).unwrap()), 1);
    assert_eq!(
        n_of(d.query("SELECT COUNT(*) FROM t WHERE id BETWEEN $1 AND $2",
            &[Value::Text("3".into()), Value::Text("9".into())]).unwrap()),
        7
    );
    // Index equality on a numeric column.
    assert_eq!(n_of(d.query("SELECT COUNT(*) FROM t WHERE n = $1", &[Value::Text("50".into())]).unwrap()), 1);
    // A genuinely textual column keeps text semantics — affinity must not turn
    // a TEXT column's comparison into a numeric one.
    assert_eq!(n_of(d.query("SELECT COUNT(*) FROM t WHERE s = $1", &[Value::Text("r5".into())]).unwrap()), 1);
}

/// Affinity CONVERTS ONLY WHEN LOSSLESS — it is not a cast. A bound that is
/// not a number stays text and matches nothing numeric, rather than becoming
/// 0 and matching the low end of the range.
#[test]
fn an_unconvertible_bound_is_not_cast_to_zero() {
    let d = open("nocast");
    let got = n_of(
        d.query(
            "SELECT COUNT(*) FROM t WHERE lat BETWEEN $1 AND $2",
            &[Value::Text("abc".into()), Value::Text("59.15".into())],
        )
        .unwrap(),
    );
    assert_eq!(got, 0, "'abc' is not 0; a text low bound leaves nothing below the high one");
}

/// Which SHAPE loses the pinning? `coerce_params` already converts a numeric
/// text into a typed slot — the question is whether the binder pins the slot.
/// Equality does; this asks the two range spellings separately, because
/// `BETWEEN` and `>= AND <=` are the same predicate written twice and a gap in
/// one of them is a gap in the binder, not in the comparison.
#[test]
fn which_range_spelling_pins_its_parameter() {
    let d = open("spelling");
    let p = [Value::Text("59.05".into()), Value::Text("59.15".into())];
    let between = n_of(
        d.query("SELECT COUNT(*) FROM t WHERE lat BETWEEN $1 AND $2", &p).unwrap(),
    );
    let spelled = n_of(
        d.query("SELECT COUNT(*) FROM t WHERE lat >= $1 AND lat <= $2", &p).unwrap(),
    );
    assert_eq!((between, spelled), (11, 11), "both spellings of the same range");

    // The one-sided form was the loose end when this test was written: it
    // answered 20 — every row — where the numeric reading is 16 and the
    // all-text-sorts-high reading is 0, a third number nobody could explain,
    // so it was left unasserted rather than pinned on a guess. The domain fix
    // settled it: an unbounded-below range with a text `lo` had been dropping
    // its bound and keeping none, which is every row. It is 16 now, and
    // asserted.
    let one_sided = n_of(d.query("SELECT COUNT(*) FROM t WHERE lat >= $1", &p[..1]).unwrap());
    assert_eq!(one_sided, 16, "lat >= 59.05 over 59.01..59.20");
}
