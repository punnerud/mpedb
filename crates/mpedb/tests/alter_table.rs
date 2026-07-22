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
fn add_column_rewrites_existing_rows_with_null() {
    let (cfg, path) = config("add-col");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT)", &[]).unwrap();
    for id in 1..=5 {
        db.query(&format!("INSERT INTO t (id, a, b) VALUES ({id}, {}, 'row{id}')", id * 10), &[])
            .unwrap();
    }

    // Add a nullable column. Existing rows gain it as NULL; the OTHER columns
    // must survive the row rewrite byte-for-byte.
    db.query("ALTER TABLE t ADD COLUMN c REAL", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT a FROM t WHERE id = 3"), 30);
    assert_eq!(
        rows(db.query("SELECT b FROM t WHERE id = 3", &[]).unwrap()),
        vec![vec![Value::Text("row3".into())]]
    );
    // The new column is NULL for every pre-existing row.
    assert_eq!(
        rows(db.query("SELECT c FROM t WHERE id = 3", &[]).unwrap()),
        vec![vec![Value::Null]]
    );
    // count(c) counts non-NULLs → 0 so far; count(*) is unchanged.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 5);
    assert_eq!(scalar_i64(&db, "SELECT count(c) FROM t"), 0);

    // New rows can set the new column; old rows still read back intact.
    db.query("INSERT INTO t (id, a, b, c) VALUES (6, 60, 'row6', 1.5)", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT c FROM t WHERE id = 6", &[]).unwrap()),
        vec![vec![Value::Float(1.5)]]
    );
    assert_eq!(scalar_i64(&db, "SELECT count(c) FROM t"), 1);
    assert_eq!(scalar_i64(&db, "SELECT sum(a) FROM t"), 10 + 20 + 30 + 40 + 50 + 60);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn add_column_refusals_and_persistence() {
    let (cfg, path) = config("add-col-refuse");
    {
        let db = Database::open_with_config(cfg.clone()).unwrap();
        db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT)", &[]).unwrap();
        db.query("INSERT INTO t (id, a) VALUES (1, 100)", &[]).unwrap();

        // v1 refusals: NOT NULL (no default), UNIQUE, PRIMARY KEY on ADD.
        assert!(db.query("ALTER TABLE t ADD COLUMN x INT NOT NULL", &[]).is_err());
        assert!(db.query("ALTER TABLE t ADD COLUMN x INT UNIQUE", &[]).is_err());
        assert!(db.query("ALTER TABLE t ADD COLUMN x INT PRIMARY KEY", &[]).is_err());
        // Duplicate column name.
        assert!(db.query("ALTER TABLE t ADD COLUMN a INT", &[]).is_err());
        // Unknown table.
        assert!(db.query("ALTER TABLE nope ADD COLUMN x INT", &[]).is_err());
        // After the refusals a valid ADD still works (no half-applied state).
        db.query("ALTER TABLE t ADD COLUMN note TEXT", &[]).unwrap();
        db.query("UPDATE t SET note = 'hi' WHERE id = 1", &[]).unwrap();
        assert_eq!(
            rows(db.query("SELECT note FROM t WHERE id = 1", &[]).unwrap()),
            vec![vec![Value::Text("hi".into())]]
        );
        db.verify().unwrap();
    }
    // The added column and its data are durable across reopen.
    {
        let db = Database::open_with_config(cfg).unwrap();
        assert_eq!(
            rows(db.query("SELECT note FROM t WHERE id = 1", &[]).unwrap()),
            vec![vec![Value::Text("hi".into())]]
        );
        assert_eq!(scalar_i64(&db, "SELECT a FROM t WHERE id = 1"), 100);
        db.verify().unwrap();
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn drop_column_removes_it_and_keeps_the_rest() {
    let (cfg, path) = config("drop-col");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT, c INT)", &[]).unwrap();
    for id in 1..=4 {
        db.query(
            &format!("INSERT INTO t (id, a, b, c) VALUES ({id}, {}, 'r{id}', {})", id, id * 100),
            &[],
        )
        .unwrap();
    }

    // Drop a middle column. The surviving columns (including `c`, which shifts
    // down one index) must read back intact.
    db.query("ALTER TABLE t DROP COLUMN a", &[]).unwrap();
    assert!(db.query("SELECT a FROM t", &[]).is_err(), "dropped column gone");
    assert_eq!(
        rows(db.query("SELECT b FROM t WHERE id = 3", &[]).unwrap()),
        vec![vec![Value::Text("r3".into())]]
    );
    assert_eq!(scalar_i64(&db, "SELECT c FROM t WHERE id = 3"), 300);
    assert_eq!(scalar_i64(&db, "SELECT sum(c) FROM t"), 100 + 200 + 300 + 400);
    // New inserts use the narrowed column list.
    db.query("INSERT INTO t (id, b, c) VALUES (5, 'r5', 500)", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT c FROM t WHERE id = 5"), 500);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn drop_column_renumbers_a_surviving_index() {
    // A UNIQUE index on a column that sits AFTER the dropped one must keep
    // working: its stored column reference shifts down by one, the value→pk
    // tree is untouched, and uniqueness is still enforced.
    let (cfg, path) = config("drop-col-idx");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, email TEXT, UNIQUE (email))", &[])
        .unwrap();
    db.query("INSERT INTO t (id, a, email) VALUES (1, 10, 'x@a')", &[]).unwrap();
    db.query("INSERT INTO t (id, a, email) VALUES (2, 20, 'y@a')", &[]).unwrap();

    db.query("ALTER TABLE t DROP COLUMN a", &[]).unwrap();
    // The unique index on `email` still enforces and still serves lookups.
    assert!(
        db.query("INSERT INTO t (id, email) VALUES (3, 'x@a')", &[]).is_err(),
        "UNIQUE(email) must still reject a duplicate after the drop"
    );
    assert_eq!(
        rows(db.query("SELECT id FROM t WHERE email = 'y@a'", &[]).unwrap()),
        vec![vec![Value::Int(2)]]
    );
    db.query("INSERT INTO t (id, email) VALUES (3, 'z@a')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 3);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn drop_column_refusals() {
    let (cfg, path) = config("drop-col-refuse");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, email TEXT, UNIQUE (email))", &[])
        .unwrap();
    db.query("CREATE TABLE one (id INTEGER PRIMARY KEY)", &[]).unwrap();

    // Cannot drop a PK column, an indexed column, an unknown column, or the
    // last remaining column.
    assert!(db.query("ALTER TABLE t DROP COLUMN id", &[]).is_err());
    assert!(db.query("ALTER TABLE t DROP COLUMN email", &[]).is_err());
    assert!(db.query("ALTER TABLE t DROP COLUMN nope", &[]).is_err());
    assert!(db.query("ALTER TABLE one DROP COLUMN id", &[]).is_err());
    // A droppable column still works after the refusals.
    db.query("INSERT INTO t (id, a, email) VALUES (1, 5, 'e')", &[]).unwrap();
    db.query("ALTER TABLE t DROP COLUMN a", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT email FROM t WHERE id = 1", &[]).unwrap()),
        vec![vec![Value::Text("e".into())]]
    );
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

/// `ADD COLUMN` on an IMPLICIT-ROWID table — the shape the C-API shim creates by
/// default (`CREATE TABLE` with no declared primary key, #94).
///
/// The synthetic `rowid` column must stay LAST and sole PK (`Schema::validate`
/// enforces it), so the new column is inserted BEFORE it, in both the schema and
/// the row rewrite. Appending past the rowid produced a schema that failed its
/// own validator — "table has implicit_rowid set but its last column is not a
/// NOT-NULL Int64 `rowid` sole primary key" — which made migrations impossible
/// on exactly the tables the shim produces. Found by measurement, not by a test.
#[test]
fn add_column_on_an_implicit_rowid_table_keeps_the_rowid_last() {
    let (cfg, path) = config("implicit-rowid-add");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE alpha (a int64, b text)", &[]).unwrap();
    db.query("INSERT INTO alpha (a, b) VALUES (1, 'x')", &[]).unwrap();
    // The failing step: this used to fail the schema validator outright.
    db.query("ALTER TABLE alpha ADD COLUMN c int64", &[]).unwrap();
    db.query("INSERT INTO alpha (a, b, c) VALUES (2, 'y', 7)", &[]).unwrap();
    // The pre-existing row reads back with its old values intact and NULL for
    // the new column — not the rowid shifted into `c`'s slot.
    let got = rows(db.query("SELECT a, b, c FROM alpha ORDER BY a", &[]).unwrap());
    assert_eq!(
        got,
        vec![
            vec![Value::Int(1), Value::Text("x".into()), Value::Null],
            vec![Value::Int(2), Value::Text("y".into()), Value::Int(7)],
        ],
        "old row keeps its values, new column is NULL, rowid never leaks into c"
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}
