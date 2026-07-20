//! `Database::access_report` — what a statement touches, at column
//! granularity, without running it (the input an authorization gate,
//! an audit log or a policy layer consults at prepare time).
//!
//! The property under test is the one the module promises: EXACT columns for a
//! single-table statement, and for everything else a widening that
//! over-reports and never under-reports — with `exact_columns` telling the two
//! apart. A gate fed an under-reporting list would let a column through
//! unexamined, which is the failure mode worth a test.

use mpedb::{Access, Config, Database, ObjectKind, TxnOp};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct Db {
    db: Database,
    path: PathBuf,
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn open() -> Db {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-access-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 8
max_readers = 8

[[table]]
name = "t1"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "a"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "text"

[[table]]
name = "t2"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "c"
  type = "int64"
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Db { db, path }
}

/// The reads a report names, as `table.column` strings.
fn reads(db: &Database, sql: &str) -> Vec<String> {
    db.access_report(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .actions
        .iter()
        .filter_map(|a| match a {
            Access::Read { table, column } => Some(format!("{table}.{column}")),
            _ => None,
        })
        .collect()
}

fn exact(db: &Database, sql: &str) -> bool {
    db.access_report(sql).unwrap().exact_columns
}

#[test]
fn single_table_select_names_exactly_the_columns_it_touches() {
    let h = open();
    let db = &h.db;

    // Projection only.
    assert_eq!(reads(db, "SELECT b FROM t1"), ["t1.b"]);
    // Projection + WHERE: both, deduplicated and in schema order.
    assert_eq!(reads(db, "SELECT b FROM t1 WHERE a = 1"), ["t1.a", "t1.b"]);
    assert_eq!(reads(db, "SELECT a FROM t1 WHERE a = 1"), ["t1.a"]);
    // `*` is every column.
    assert_eq!(reads(db, "SELECT * FROM t1"), ["t1.id", "t1.a", "t1.b"]);
    // A PK point probe READS the key column even though nothing projects it.
    assert_eq!(reads(db, "SELECT b FROM t1 WHERE id = 1"), ["t1.id", "t1.b"]);
    // A computed projection reads what its expression reads.
    assert_eq!(reads(db, "SELECT a + 1 FROM t1"), ["t1.a"]);
    // GROUP BY / aggregate arguments are base-row reads. The PROJECTION of a
    // grouped plan is NOT: it indexes `[keys ‖ aggs]`, so slot 0 is the group
    // key, not table column 0 — reading it as a base column named `t1.id`,
    // which this statement never touches.
    assert_eq!(reads(db, "SELECT b, count(a) FROM t1 GROUP BY b"), ["t1.a", "t1.b"]);
    // ORDER BY over the base row.
    assert_eq!(reads(db, "SELECT b FROM t1 ORDER BY a"), ["t1.a", "t1.b"]);

    for sql in ["SELECT b FROM t1", "SELECT * FROM t1 WHERE a = 1"] {
        assert!(exact(db, sql), "{sql} should be exact");
    }
    // The statement action itself is a Select.
    assert_eq!(db.access_report("SELECT b FROM t1").unwrap().actions[0], Access::Select);
}

/// A join's plan indices address a concatenated tuple, so the report widens to
/// every column of every table read. The test that matters is the DIRECTION:
/// the widened list must be a strict SUPERSET of the columns the statement
/// really names, never a subset.
#[test]
fn a_join_widens_and_the_widening_only_over_reports() {
    let h = open();
    let db = &h.db;
    let sql = "SELECT t1.b FROM t1 JOIN t2 ON t1.id = t2.id WHERE t2.c = 1";
    assert!(!exact(db, sql), "a join cannot promise exact columns");
    let got = reads(db, sql);
    for named in ["t1.b", "t1.id", "t2.id", "t2.c"] {
        assert!(got.contains(&named.to_string()), "{named} missing from {got:?}");
    }
    // Widening means all of both tables — never fewer.
    assert_eq!(got.len(), 5, "{got:?}");
}

#[test]
fn writes_name_their_table_and_the_columns_they_assign() {
    let h = open();
    let db = &h.db;

    let ins = db.access_report("INSERT INTO t1 (id, a, b) VALUES (1, 2, 'x')").unwrap();
    assert_eq!(ins.actions, [Access::Insert { table: "t1".into() }]);

    let upd = db.access_report("UPDATE t1 SET b = 'y' WHERE a = 3").unwrap();
    assert!(upd.exact_columns);
    // Reads: the WHERE column and the assigned one; writes: only `b`.
    assert_eq!(reads(db, "UPDATE t1 SET b = 'y' WHERE a = 3"), ["t1.a", "t1.b"]);
    assert!(upd
        .actions
        .contains(&Access::Update { table: "t1".into(), column: "b".into() }));
    assert!(!upd
        .actions
        .contains(&Access::Update { table: "t1".into(), column: "a".into() }));

    let del = db.access_report("DELETE FROM t1 WHERE a = 3").unwrap();
    assert_eq!(reads(db, "DELETE FROM t1 WHERE a = 3"), ["t1.a"]);
    assert!(del.actions.contains(&Access::Delete { table: "t1".into() }));
}

#[test]
fn ddl_and_transaction_control_are_described_too() {
    let h = open();
    let db = &h.db;
    let only = |sql: &str| db.access_report(sql).unwrap().actions;

    assert_eq!(
        only("CREATE TABLE fresh (x INTEGER PRIMARY KEY)"),
        [Access::Create { kind: ObjectKind::Table, name: "fresh".into(), table: None }]
    );
    assert_eq!(
        only("CREATE INDEX ix ON t1 (a)"),
        [Access::Create {
            kind: ObjectKind::Index,
            name: "ix".into(),
            table: Some("t1".into())
        }]
    );
    assert_eq!(
        only("DROP TABLE t2"),
        [Access::Drop { kind: ObjectKind::Table, name: "t2".into(), table: None }]
    );
    assert_eq!(only("ALTER TABLE t1 RENAME TO t3"), [Access::Alter { table: "t1".into() }]);
    assert_eq!(only("BEGIN"), [Access::Transaction { op: TxnOp::Begin }]);
    assert_eq!(only("COMMIT"), [Access::Transaction { op: TxnOp::Commit }]);
    assert_eq!(
        only("SAVEPOINT sp"),
        [Access::Savepoint { op: TxnOp::Begin, name: "sp".into() }]
    );
    // A no-op maintenance statement touches nothing.
    assert!(only("ANALYZE").is_empty());
}

/// The report must describe a statement against the SESSION's schema when a
/// transaction has DDL in flight — otherwise a gate could not describe (and so
/// would have to refuse) every statement inside an open transaction.
#[test]
fn a_session_describes_its_own_uncommitted_ddl() {
    let h = open();
    let mut s = h.db.begin().unwrap();
    s.query("CREATE TABLE mid (id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
    // The committed view cannot see it…
    assert!(h.db.access_report("SELECT v FROM mid").is_err());
    // …the session's own view can.
    let r = s.access_report("SELECT v FROM mid").unwrap();
    assert!(r.actions.contains(&Access::Read { table: "mid".into(), column: "v".into() }));
    s.rollback();
}
