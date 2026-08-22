//! ORDER BY that an equality-pinned index prefix already delivers.
//!
//! An index access yields index order: the key columns in order, then the pk
//! within one key. So a sort over one cannot generally be dropped — and the
//! planner refused to drop any, which was right but too broad. Pin the leading
//! column with an equality and every row the scan yields agrees on it, so what
//! remains IS the index's next key column.
//!
//! Two costs, one cause. `WHERE hex = ? ORDER BY ts LIMIT 20` over `(hex, ts)`
//! sorted 338 887 rows on politihelikopter.com to return twenty: 403 ms and
//! 483 MB peak, against sqlite's 1,5 ms. And because a cap cannot reach the
//! scan through a sort (`gather.rs`), the LIMIT could not help either. Eliding
//! the sort fixes both at once.
//!
//! The work meter is how that is asserted here rather than a stopwatch: one
//! charge per index entry visited, so a budget the sorting version cannot fit
//! is a precise statement that the scan stopped at the LIMIT.

use mpedb::{Database, ExecResult, Value};
use mpedb_sql::{AccessPath, PlanStmt};
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

/// `t (g, r)` composite-indexed, `g` low-cardinality so an equality on it
/// covers a large slice — the shape that made the sort expensive. `nn` is a
/// NULLABLE twin of `r` and `s` a NOCASE text, both there to be refused.
fn open(tag: &str, max_work_rows: u64, n: i64) -> Tmp {
    let dir = mpedb_testkit::scratch_base_str();
    let path = format!(
        "{dir}/mpedb-elide-{tag}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 32\nmax_readers = 8\n\n\
         [runtime]\nmax_work_rows = {max_work_rows}\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    for d in [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER NOT NULL, r INTEGER NOT NULL, \
         nn INTEGER, s TEXT NOT NULL COLLATE NOCASE)",
        "CREATE INDEX t_gr ON t (g, r)",
        "CREATE INDEX t_gn ON t (g, nn)",
        "CREATE INDEX t_gs ON t (g, s)",
    ] {
        db.query(d, &[]).unwrap();
    }
    let mut w = db.begin().unwrap();
    for i in 1..=n {
        w.query(
            "INSERT INTO t (id, g, r, nn, s) VALUES ($1, $2, $3, $4, $5)",
            &[
                Value::Int(i),
                Value::Int(i % 4),
                Value::Int(i),
                Value::Int(i),
                Value::Text(format!("v{i}")),
            ],
        )
        .unwrap();
    }
    w.commit().unwrap();
    Tmp { db, path }
}

fn rows(r: ExecResult) -> Vec<Vec<Value>> {
    match r {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The plan's remaining sort keys, and its access path.
fn plan_of(db: &Database, sql: &str) -> (Vec<u16>, AccessPath) {
    let schema = db.schema();
    let p = mpedb_sql::prepare(sql, &schema).expect("prepare");
    match &p.stmt {
        PlanStmt::Select(s) => {
            (s.order_by.iter().map(|(c, _, _)| *c).collect(), s.access.clone())
        }
        other => panic!("expected a select, got {other:?}"),
    }
}

/// The case from the site: equality on the leading key column, ORDER BY the
/// next one. The sort goes, and the LIMIT then reaches the scan — a budget of
/// 5 answers a query whose `g` matches 100 rows.
#[test]
fn an_equality_pinned_prefix_delivers_the_next_key_column() {
    let d = open("pinned", 5, 400);
    let sql = "SELECT id FROM t WHERE g = 1 ORDER BY r LIMIT 3";
    let (keys, access) = plan_of(&d, sql);
    assert!(matches!(access, AccessPath::IndexPoint { .. }), "{access:?}");
    assert!(keys.is_empty(), "the index already delivers r within g = 1: {keys:?}");

    let got = rows(d.query(sql, &[]).unwrap());
    assert_eq!(
        got,
        vec![vec![Value::Int(1)], vec![Value::Int(5)], vec![Value::Int(9)]],
        "and in the RIGHT order — a dropped sort that reorders rows is not an optimisation"
    );
}

/// The answer must be identical with and without the elision, over the whole
/// result rather than the first few. Compared against the same query forced to
/// sort by asking for a key the index does not deliver.
#[test]
fn the_elided_order_matches_the_sorted_one() {
    let d = open("same", 4000, 400);
    let elided = rows(d.query("SELECT r FROM t WHERE g = 2 ORDER BY r", &[]).unwrap());
    let sorted = rows(d.query("SELECT r FROM t WHERE g = 2 ORDER BY r + 0", &[]).unwrap());
    assert_eq!(elided.len(), 100);
    assert_eq!(elided, sorted, "same rows, same order, sort or no sort");
}

/// Every way the key can fail to be the value's order. Each of these must KEEP
/// its sort — an elision that fires here returns rows in the wrong order and
/// says nothing about it.
#[test]
fn the_refusals() {
    let d = open("refuse", 4000, 400);
    let keys = |sql: &str| plan_of(&d, sql).0;

    // DESC: the index walks ascending.
    assert!(!keys("SELECT id FROM t WHERE g = 1 ORDER BY r DESC").is_empty());
    // No equality pin — a RANGE on the leading column does not hold it
    // constant, so the second column's order is per-g, not global.
    assert!(!keys("SELECT id FROM t WHERE g > 1 ORDER BY r").is_empty());
    // No predicate at all.
    assert!(!keys("SELECT id FROM t ORDER BY r").is_empty());
    // A column the index does not have next.
    assert!(!keys("SELECT id FROM t WHERE g = 1 ORDER BY id").is_empty());
    // A NOCASE key stores the FOLDED text, so its byte order is not the
    // value's order.
    assert!(!keys("SELECT id FROM t WHERE g = 1 ORDER BY s").is_empty());
    // A NULLABLE key column: the NULL placement stops being moot.
    assert!(!keys("SELECT id FROM t WHERE g = 1 ORDER BY nn").is_empty());
}

/// Ordering by the pinned column itself is trivially satisfied — every row has
/// the same value — and must not confuse the prefix arithmetic.
#[test]
fn ordering_by_the_pinned_column_is_still_correct() {
    let d = open("bypinned", 4000, 400);
    let got = rows(d.query("SELECT DISTINCT g FROM t WHERE g = 1 ORDER BY g", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(1)]]);
}
