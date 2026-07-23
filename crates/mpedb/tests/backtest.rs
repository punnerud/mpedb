//! Trigger backtesting: replay a trigger over the CURRENT rows in an
//! always-aborted transaction and report what it would have done — firing
//! counts, WHEN skips, RAISE outcomes, and net row effects — while provably
//! changing nothing.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn open(name: &str) -> (Database, FileGuard) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-btest-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 16

[[table]]
name = "orders"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "total"
  type = "int64"
  nullable = false

[[table]]
name = "audit"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "oid"
  type = "int64"
  nullable = false
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (db, FileGuard(path))
}

fn count(db: &Database, sql: &str) -> i64 {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(v) => v,
            ref v => panic!("count is {v:?}"),
        },
        other => panic!("unexpected {other:?}"),
    }
}

fn seed_orders(db: &Database, n: i64) {
    for id in 1..=n {
        db.query(
            &format!("INSERT INTO orders (id, total) VALUES ({id}, {})", id * 100),
            &[],
        )
        .unwrap();
    }
}

#[test]
fn backtest_reports_fires_and_effects_and_changes_nothing() {
    let (db, _p) = open("fires");
    seed_orders(&db, 5);
    db.query(
        "CREATE TRIGGER a AFTER INSERT ON orders FOR EACH ROW \
         BEGIN INSERT INTO audit (id, oid) VALUES (NEW.id, NEW.id); END",
        &[],
    )
    .unwrap();

    let r = db.backtest_trigger("a", 0).unwrap();
    assert_eq!(r.event, "AFTER INSERT");
    assert_eq!(r.table, "orders");
    assert_eq!((r.table_rows, r.rows_scanned), (5, 5));
    assert_eq!((r.fired, r.skipped_when, r.ignored, r.vetoed), (5, 0, 0, 0));
    assert_eq!(r.net_rows, vec![("audit".to_string(), 5)]);
    assert!(r.assumption.is_none());

    // The replay committed NOTHING.
    assert_eq!(count(&db, "SELECT count(*) FROM audit"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM orders"), 5);
}

#[test]
fn backtest_splits_when_gate_from_fires_and_honours_limit() {
    let (db, _p) = open("when");
    seed_orders(&db, 6); // totals 100..600
    db.query(
        "CREATE TRIGGER big AFTER INSERT ON orders FOR EACH ROW WHEN (NEW.total > 300) \
         BEGIN INSERT INTO audit (id, oid) VALUES (NEW.id, NEW.total); END",
        &[],
    )
    .unwrap();

    let r = db.backtest_trigger("big", 0).unwrap();
    assert_eq!((r.fired, r.skipped_when), (3, 3));
    assert_eq!(r.net_rows, vec![("audit".to_string(), 3)]);

    // A limit caps the replay corpus and says so.
    let r = db.backtest_trigger("big", 2).unwrap();
    assert_eq!((r.table_rows, r.rows_scanned), (6, 2));
}

#[test]
fn backtest_counts_raise_vetoes_with_messages_and_ignores() {
    let (db, _p) = open("raise");
    seed_orders(&db, 4); // totals 100, 200, 300, 400
    db.query(
        "CREATE TRIGGER floor_chk BEFORE INSERT ON orders FOR EACH ROW \
         BEGIN SELECT RAISE(ABORT, 'total too small') WHERE NEW.total < 250; END",
        &[],
    )
    .unwrap();
    let r = db.backtest_trigger("floor_chk", 0).unwrap();
    assert_eq!((r.fired, r.vetoed), (2, 2));
    assert_eq!(r.veto_examples, vec!["total too small"; 2]);
    assert!(r.net_rows.is_empty());

    db.query("DROP TRIGGER floor_chk", &[]).unwrap();
    db.query(
        "CREATE TRIGGER skip_small BEFORE INSERT ON orders FOR EACH ROW \
         BEGIN SELECT RAISE(IGNORE) WHERE NEW.total < 250; END",
        &[],
    )
    .unwrap();
    let r = db.backtest_trigger("skip_small", 0).unwrap();
    assert_eq!((r.fired, r.ignored, r.vetoed), (2, 2, 0));
}

/// The dry-run form: a full CREATE TRIGGER statement backtests WITHOUT being
/// stored — the analyse-before-it-goes-live workflow.
#[test]
fn backtest_dry_runs_an_uncreated_trigger() {
    let (db, _p) = open("dry");
    seed_orders(&db, 3);
    let r = db
        .backtest_trigger(
            "CREATE TRIGGER would_be AFTER DELETE ON orders FOR EACH ROW \
             BEGIN INSERT INTO audit (id, oid) VALUES (OLD.id, OLD.total); END",
            0,
        )
        .unwrap();
    assert_eq!(r.event, "AFTER DELETE");
    assert_eq!(r.fired, 3);
    assert_eq!(r.net_rows, vec![("audit".to_string(), 3)]);
    // Nothing was stored, nothing was written.
    assert!(db.list_triggers().unwrap().is_empty());
    assert_eq!(count(&db, "SELECT count(*) FROM audit"), 0);

    // And a broken dry spec refuses like CREATE would.
    let e = db
        .backtest_trigger(
            "CREATE TRIGGER x AFTER INSERT ON nosuch BEGIN DELETE FROM audit; END",
            0,
        )
        .unwrap_err();
    assert!(e.to_string().contains("no such table"), "{e}");

    let e = db.backtest_trigger("nosuch_trigger", 0).unwrap_err();
    assert!(e.to_string().contains("no trigger named"), "{e}");
}

#[test]
fn backtest_update_states_the_identity_assumption() {
    let (db, _p) = open("upd");
    seed_orders(&db, 3);
    db.query(
        "CREATE TRIGGER u AFTER UPDATE OF total ON orders FOR EACH ROW \
         WHEN (NEW.total = OLD.total) \
         BEGIN INSERT INTO audit (id, oid) VALUES (NEW.id, OLD.total); END",
        &[],
    )
    .unwrap();
    let r = db.backtest_trigger("u", 0).unwrap();
    // Identity replay: OLD = NEW, so the WHEN passes for every row, and the
    // UPDATE OF column counts as assigned.
    assert_eq!(r.fired, 3);
    assert!(r.assumption.as_deref().unwrap_or("").contains("identity"), "{:?}", r.assumption);
}
