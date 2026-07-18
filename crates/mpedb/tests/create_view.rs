//! `CREATE VIEW` (#73, DESIGN-VIEW.md) — a view is flattened onto its base
//! table at reference time. Cross-checked against sqlite 3.45: a `SELECT` over
//! the view returns exactly the base rows the view's projection/filter admit,
//! an outer filter AND-merges with the view's, `SELECT *` returns the view's
//! columns (not the base's), view-over-view chains, and complex views are
//! refused (never answered wrongly).

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open(name: &str) -> (Database, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-view-{name}-{}-{}.mpedb",
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
name = "seed"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"
"#,
        path.display()
    );
    (Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap(), path)
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar_i64(db: &Database, sql: &str) -> i64 {
    match &rows(db.query(sql, &[]).unwrap())[0][0] {
        Value::Int(i) => *i,
        other => panic!("{other:?}"),
    }
}

fn setup(db: &Database) {
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT, c INT)", &[]).unwrap();
    for id in 1..=10 {
        db.query(
            &format!("INSERT INTO t (id, a, b, c) VALUES ({id}, {}, 'r{id}', {})", id, id * 10),
            &[],
        )
        .unwrap();
    }
}

#[test]
fn projection_filter_view_flattens_correctly() {
    let (db, path) = open("proj");
    setup(&db);
    // View exposes a subset of columns with a filter.
    db.query("CREATE VIEW v AS SELECT id, a FROM t WHERE a > 5", &[]).unwrap();

    // `SELECT * FROM v` returns exactly the view's columns (id, a), only rows a>5.
    let got = rows(db.query("SELECT * FROM v ORDER BY id", &[]).unwrap());
    assert_eq!(got.len(), 5); // a = 6..10
    assert_eq!(got[0], vec![Value::Int(6), Value::Int(6)]); // only id,a — not b,c

    // An outer filter AND-merges with the view's.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM v WHERE a < 9"), 3); // a in 6,7,8
    assert_eq!(scalar_i64(&db, "SELECT id FROM v WHERE a = 7"), 7);

    // A column the view hides is not selectable via `*`, but the base filter
    // still applies. count over the view = admitted rows.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM v"), 5);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn star_view_and_view_over_view() {
    let (db, path) = open("star");
    setup(&db);
    db.query("CREATE VIEW allrows AS SELECT * FROM t WHERE c >= 30", &[]).unwrap();
    // `*` view exposes every base column.
    let got = rows(db.query("SELECT id, b FROM allrows WHERE id = 4", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(4), Value::Text("r4".into())]]);
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM allrows"), 8); // c=30..100

    // A view over a view chains (recursion), merging both filters.
    db.query("CREATE VIEW hi AS SELECT id, c FROM allrows WHERE c > 70", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM hi"), 3); // c=80,90,100
    assert_eq!(scalar_i64(&db, "SELECT id FROM hi WHERE c = 90"), 9);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn view_with_aggregate_over_it_and_group_by() {
    // The OUTER query may aggregate/group over a simple view — only the VIEW
    // body is constrained.
    let (db, path) = open("agg");
    setup(&db);
    db.query("CREATE VIEW v AS SELECT a, c FROM t WHERE a >= 3", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT sum(c) FROM v"), (3..=10).map(|i| i * 10).sum());
    let got = rows(
        db.query("SELECT count(*) FROM v WHERE c > 50", &[]).unwrap(),
    );
    assert_eq!(got, vec![vec![Value::Int(5)]]); // c=60..100
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn drop_view_and_refusals() {
    let (db, path) = open("refuse");
    setup(&db);
    db.query("CREATE VIEW v AS SELECT id FROM t", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM v"), 10);
    // DROP VIEW removes it.
    db.query("DROP VIEW v", &[]).unwrap();
    assert!(db.query("SELECT * FROM v", &[]).is_err());
    assert!(matches!(
        db.query("DROP VIEW IF EXISTS v", &[]).unwrap(),
        ExecResult::Affected(0)
    ));

    // Name collision with a table is refused.
    assert!(db.query("CREATE VIEW t AS SELECT id FROM t", &[]).is_err());
    // A complex view (aggregate body) is stored but refused at reference time.
    db.query("CREATE VIEW agg AS SELECT count(*) AS n FROM t", &[]).unwrap();
    assert!(db.query("SELECT * FROM agg", &[]).is_err());
    // Writing through a view is refused.
    db.query("CREATE VIEW w AS SELECT id, a FROM t", &[]).unwrap();
    assert!(db.query("INSERT INTO w (id, a) VALUES (99, 1)", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn view_persists_and_second_process_sees_it() {
    let (cfg, path) = {
        let dir = if Path::new("/dev/shm").is_dir() {
            PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let path = dir.join(format!(
            "mpedb-view-mp-{}-{}.mpedb",
            std::process::id(),
            UNIQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let toml = format!(
            "[database]\npath = \"{}\"\nsize_mb = 16\nmax_readers = 16\n\n[[table]]\nname = \"t\"\nprimary_key = [\"id\"]\n  [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n  [[table.column]]\n  name = \"a\"\n  type = \"int64\"\n",
            path.display()
        );
        (Config::from_toml_str(&toml).unwrap(), path)
    };
    let a = Database::open_with_config(cfg.clone()).unwrap();
    let b = Database::open_with_config(cfg).unwrap();
    for id in 1..=5 {
        a.query(&format!("INSERT INTO t (id, a) VALUES ({id}, {})", id * 2), &[]).unwrap();
    }
    a.query("CREATE VIEW ev AS SELECT id, a FROM t WHERE a >= 6", &[]).unwrap();
    // B — cached schema from before — must see the view on its next statement.
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM ev"), 3); // a=6,8,10
    a.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
