//! #47 stage 5: `ALTER TABLE ... RENAME` end to end — RENAME TO (table) and
//! RENAME [COLUMN] (column) are pure schema metadata: the id, columns, keys,
//! indexes, and every row are untouched, only the name changes. The old name
//! stops binding, the new name works for read and write, the change persists
//! across reopen, and a second process sees it on its next statement. sqlite/PG
//! equivalent (both refuse a rename to a colliding name / of an unknown target).

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn config(name: &str) -> (Config, PathBuf) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-altertable-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 32

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "name"
  type = "text"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), path)
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

#[test]
fn rename_table_keeps_data_and_reroutes_the_name() {
    let (cfg, path) = config("rename-table");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE accounts (id INTEGER PRIMARY KEY, bal INT NOT NULL)", &[]).unwrap();
    for (id, bal) in [(1, 10), (2, 20), (3, 30)] {
        db.query(&format!("INSERT INTO accounts (id, bal) VALUES ({id}, {bal})"), &[]).unwrap();
    }

    db.query("ALTER TABLE accounts RENAME TO ledger", &[]).unwrap();
    // Old name no longer binds; new name reads the SAME rows (no data moved).
    assert!(db.query("SELECT count(*) FROM accounts", &[]).is_err());
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM ledger"), 3);
    assert_eq!(scalar_i64(&db, "SELECT bal FROM ledger WHERE id = 2"), 20);
    // Writes to the new name land in the same tree.
    db.query("INSERT INTO ledger (id, bal) VALUES (4, 40)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT sum(bal) FROM ledger"), 100);
    // NOT NULL still enforced (the column definition survived the rename).
    assert!(db.query("INSERT INTO ledger (id, bal) VALUES (5, NULL)", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rename_column_both_syntaxes_and_data_intact() {
    let (cfg, path) = config("rename-col");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, qty INT, note TEXT)", &[]).unwrap();
    db.query("INSERT INTO t (id, qty, note) VALUES (1, 7, 'a')", &[]).unwrap();

    // `RENAME COLUMN a TO b`.
    db.query("ALTER TABLE t RENAME COLUMN qty TO amount", &[]).unwrap();
    assert!(db.query("SELECT qty FROM t", &[]).is_err(), "old column gone");
    assert_eq!(scalar_i64(&db, "SELECT amount FROM t WHERE id = 1"), 7);

    // The bare `RENAME a TO b` shorthand (sqlite accepts it too).
    db.query("ALTER TABLE t RENAME note TO memo", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT memo FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Text("a".into())]]
    );
    // Writes use the new column name; the row image never changed.
    db.query("INSERT INTO t (id, amount, memo) VALUES (2, 9, 'b')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT sum(amount) FROM t"), 16);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rename_refusals_match_sqlite() {
    let (cfg, path) = config("refuse");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE a (id INTEGER PRIMARY KEY, x INT, y INT)", &[]).unwrap();
    db.query("CREATE TABLE b (id INTEGER PRIMARY KEY)", &[]).unwrap();

    // Rename an unknown table.
    assert!(db.query("ALTER TABLE nope RENAME TO whatever", &[]).is_err());
    // Rename a table to a name that already exists (collision with `b`).
    assert!(db.query("ALTER TABLE a RENAME TO b", &[]).is_err());
    // The seed table `users` still exists — `a` was not half-renamed.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM a"), 0);
    // Rename an unknown column.
    assert!(db.query("ALTER TABLE a RENAME COLUMN nope TO z", &[]).is_err());
    // Rename a column onto an existing sibling name (x -> y collides).
    assert!(db.query("ALTER TABLE a RENAME COLUMN x TO y", &[]).is_err());
    // A valid rename still works after the refusals (no half-applied state).
    db.query("ALTER TABLE a RENAME COLUMN x TO z", &[]).unwrap();
    db.query("INSERT INTO a (id, z, y) VALUES (1, 5, 6)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT z FROM a WHERE id = 1"), 5);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rename_persists_and_second_process_sees_it() {
    let (cfg, path) = config("persist-mp");
    {
        let a = Database::open_with_config(cfg.clone()).unwrap();
        let b = Database::open_with_config(cfg.clone()).unwrap();
        a.query("CREATE TABLE widget (id INTEGER PRIMARY KEY, kind TEXT)", &[]).unwrap();
        a.query("INSERT INTO widget (id, kind) VALUES (1, 'gear')", &[]).unwrap();
        // B warms its schema on the original name.
        assert_eq!(scalar_i64(&b, "SELECT count(*) FROM widget"), 1);

        // A renames both the table and a column.
        a.query("ALTER TABLE widget RENAME TO gadget", &[]).unwrap();
        a.query("ALTER TABLE gadget RENAME COLUMN kind TO sort", &[]).unwrap();

        // B — stale schema — must pick up both on its next statement.
        assert!(b.query("SELECT kind FROM widget WHERE id = 1", &[]).is_err());
        assert_eq!(
            rows(b.query("SELECT sort FROM gadget WHERE id = 1", &[]).unwrap()),
            vec![vec![Value::Text("gear".into())]]
        );
        a.verify().unwrap();
    }
    // Reopen: the renames are durable.
    {
        let db = Database::open_with_config(cfg).unwrap();
        assert!(db.query("SELECT count(*) FROM widget", &[]).is_err());
        assert_eq!(
            rows(db.query("SELECT sort FROM gadget WHERE id = 1", &[]).unwrap()),
            vec![vec![Value::Text("gear".into())]]
        );
        db.verify().unwrap();
    }
    let _ = std::fs::remove_file(&path);
}
