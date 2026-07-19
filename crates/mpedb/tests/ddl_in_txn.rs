//! #95: DDL inside a `WriteSession`'s transaction.
//!
//! CPython's `sqlite3` opens an implicit transaction on the first DML, so a
//! `CREATE TABLE` after an `INSERT` (and every `executescript`) runs inside a
//! transaction. These tests prove mpedb now applies table DDL THROUGH the open
//! session's txn: the schema change is visible to later statements in the same
//! session, commits/rolls back atomically with the session's DML, and stays
//! invisible to other handles until commit.

use mpedb::{Config, Database, Error, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Seed with ONE table (`base`, id 0) — mpedb refuses a schema with no live
/// tables. Everything else is created live inside the tests.
fn config(name: &str) -> (Config, FileGuard) {
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    let path = dir.join(format!(
        "mpedb-ddltxn-{name}-{}-{}.mpedb",
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
name = "base"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "n"
  type = "int64"
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), FileGuard(path))
}

fn one_i64(res: ExecResult) -> i64 {
    match res {
        ExecResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Int(i) => *i,
            other => panic!("expected int, got {other:?}"),
        },
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The headline flow the whole feature exists for: create → insert → select,
/// all inside one session, then commit — atomically.
#[test]
fn create_insert_select_commit_in_one_session() {
    let (cfg, _g) = config("headline");
    let db = Database::open_with_config(cfg).unwrap();

    let mut s = db.begin().unwrap();
    s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(1, 10)", &[]).unwrap();
    // The SELECT compiles + executes against the session's OWN schema view.
    assert_eq!(one_i64(s.query("SELECT v FROM t WHERE id = 1", &[]).unwrap()), 10);
    s.commit().unwrap();

    // Persisted and visible in autocommit after commit.
    assert_eq!(one_i64(db.query("SELECT v FROM t WHERE id = 1", &[]).unwrap()), 10);
    db.verify().unwrap();
}

/// DML and in-session DDL commit atomically as ONE unit.
#[test]
fn ddl_and_dml_commit_atomically() {
    let (cfg, _g) = config("atomic_commit");
    let db = Database::open_with_config(cfg).unwrap();

    let mut s = db.begin().unwrap();
    s.query("INSERT INTO base VALUES(1, 100)", &[]).unwrap(); // pre-existing table
    s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(1, 10)", &[]).unwrap();
    s.commit().unwrap();

    assert_eq!(one_i64(db.query("SELECT n FROM base WHERE id = 1", &[]).unwrap()), 100);
    assert_eq!(one_i64(db.query("SELECT v FROM t WHERE id = 1", &[]).unwrap()), 10);
    db.verify().unwrap();
}

/// Dropping the session without committing undoes the DDL (it lives in the
/// txn's COW pages, which the abort discards) AND the DML.
#[test]
fn rollback_undoes_ddl_and_dml() {
    let (cfg, _g) = config("rollback");
    let db = Database::open_with_config(cfg).unwrap();

    {
        let mut s = db.begin().unwrap();
        s.query("INSERT INTO base VALUES(1, 100)", &[]).unwrap();
        s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]).unwrap();
        s.query("INSERT INTO t VALUES(1, 10)", &[]).unwrap();
        s.rollback();
    }

    // The table never committed: a query naming it fails to bind.
    match db.query("SELECT v FROM t", &[]) {
        Err(Error::Bind(_)) => {}
        other => panic!("expected unknown-table bind error, got {other:?}"),
    }
    // The DML rolled back too.
    assert_eq!(one_i64(db.query("SELECT count(*) FROM base", &[]).unwrap()), 0);

    // The handle is still fully usable after the aborted DDL session.
    let mut s = db.begin().unwrap();
    s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(7, 70)", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(one_i64(db.query("SELECT v FROM t WHERE id = 7", &[]).unwrap()), 70);
    db.verify().unwrap();
}

/// A drop dropped inside a session that then aborts leaves the table; a drop
/// that commits removes it — both atomic with the session.
#[test]
fn drop_table_in_session_is_atomic() {
    let (cfg, _g) = config("drop");
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]).unwrap();
    db.query("INSERT INTO t VALUES(1, 10)", &[]).unwrap();

    // DROP then rollback: the table survives with its data.
    {
        let mut s = db.begin().unwrap();
        s.query("DROP TABLE t", &[]).unwrap();
        // Inside the session the table is already gone.
        assert!(s.query("SELECT v FROM t", &[]).is_err());
        s.rollback();
    }
    assert_eq!(one_i64(db.query("SELECT v FROM t WHERE id = 1", &[]).unwrap()), 10);

    // DROP then commit: the table is gone.
    {
        let mut s = db.begin().unwrap();
        s.query("DROP TABLE t", &[]).unwrap();
        s.commit().unwrap();
    }
    assert!(db.query("SELECT v FROM t", &[]).is_err());
    db.verify().unwrap();
}

/// A second handle on the same file does NOT see the uncommitted table while
/// the session is open, and DOES see it after commit.
#[test]
fn other_handle_sees_ddl_only_after_commit() {
    let (cfg, _g) = config("isolation");
    let a = Database::open_with_config(cfg.clone()).unwrap();
    let b = Database::open_with_config(cfg).unwrap();

    // Warm B's cached schema on the seed table.
    assert_eq!(one_i64(b.query("SELECT count(*) FROM base", &[]).unwrap()), 0);

    let mut s = a.begin().unwrap();
    s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(1, 10)", &[]).unwrap();

    // B (a lock-free reader) must NOT see the uncommitted table.
    match b.query("SELECT v FROM t", &[]) {
        Err(Error::Bind(_)) => {}
        other => panic!("B saw an uncommitted table: {other:?}"),
    }

    s.commit().unwrap();

    // After commit B picks it up on its next statement (schema-gen reload).
    assert_eq!(one_i64(b.query("SELECT v FROM t WHERE id = 1", &[]).unwrap()), 10);
}

/// ALTER TABLE ADD COLUMN inside a session is visible to a later statement and
/// commits atomically.
#[test]
fn alter_add_column_in_session() {
    let (cfg, _g) = config("alter_add");
    let db = Database::open_with_config(cfg).unwrap();

    let mut s = db.begin().unwrap();
    s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(1, 10)", &[]).unwrap();
    s.query("ALTER TABLE t ADD COLUMN w INTEGER DEFAULT 99", &[]).unwrap();
    // The new column resolves against the session's own view.
    assert_eq!(one_i64(s.query("SELECT w FROM t WHERE id = 1", &[]).unwrap()), 99);
    s.query("INSERT INTO t(id, v, w) VALUES(2, 20, 5)", &[]).unwrap();
    assert_eq!(one_i64(s.query("SELECT w FROM t WHERE id = 2", &[]).unwrap()), 5);
    s.commit().unwrap();

    assert_eq!(one_i64(db.query("SELECT w FROM t WHERE id = 1", &[]).unwrap()), 99);
    db.verify().unwrap();
}

/// CREATE INDEX naming a table this same session just created works, and the
/// index is usable after commit.
#[test]
fn create_index_in_session() {
    let (cfg, _g) = config("create_index");
    let db = Database::open_with_config(cfg).unwrap();

    let mut s = db.begin().unwrap();
    s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, k INTEGER)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(1, 5)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(2, 5)", &[]).unwrap();
    s.query("CREATE INDEX t_k ON t(k)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(3, 7)", &[]).unwrap();
    assert_eq!(one_i64(s.query("SELECT count(*) FROM t WHERE k = 5", &[]).unwrap()), 2);
    s.commit().unwrap();

    assert_eq!(one_i64(db.query("SELECT count(*) FROM t WHERE k = 5", &[]).unwrap()), 2);
    db.verify().unwrap();
}

/// DDL while a SAVEPOINT is open is refused cleanly (the engine savepoint does
/// not restore the captured schema bundle, so a `ROLLBACK TO` before the DDL
/// could not undo it) — and the session stays usable.
#[test]
fn ddl_after_savepoint_is_refused() {
    let (cfg, _g) = config("savepoint");
    let db = Database::open_with_config(cfg).unwrap();

    let mut s = db.begin().unwrap();
    s.query("SAVEPOINT sp", &[]).unwrap();
    match s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]) {
        Err(Error::Unsupported(_)) => {}
        other => panic!("expected refusal, got {other:?}"),
    }
    // The refusal left the session usable: release the savepoint and continue.
    s.query("RELEASE sp", &[]).unwrap();
    s.query("INSERT INTO base VALUES(1, 1)", &[]).unwrap();
    s.commit().unwrap();
    assert_eq!(one_i64(db.query("SELECT n FROM base WHERE id = 1", &[]).unwrap()), 1);
}

/// A DDL statement that fails (here: two columns each marked inline PRIMARY KEY)
/// does NOT poison the session — it had no side effect, so the session stays
/// fully usable and can commit its other work.
#[test]
fn failed_ddl_leaves_session_usable() {
    let (cfg, _g) = config("ddl_error");
    let db = Database::open_with_config(cfg).unwrap();

    let mut s = db.begin().unwrap();
    s.query("INSERT INTO base VALUES(1, 42)", &[]).unwrap();
    // Two inline PRIMARY KEY columns is refused (a composite must be declared
    // once at table level) — this CREATE fails with no side effect.
    assert!(s
        .query("CREATE TABLE bad (a INTEGER PRIMARY KEY, b INTEGER PRIMARY KEY)", &[])
        .is_err());
    // The session is not poisoned: a valid statement still runs and commits.
    s.query("CREATE TABLE good (id INTEGER PRIMARY KEY, v)", &[]).unwrap();
    s.query("INSERT INTO good VALUES(1, 7)", &[]).unwrap();
    s.commit().unwrap();

    assert_eq!(one_i64(db.query("SELECT n FROM base WHERE id = 1", &[]).unwrap()), 42);
    assert_eq!(one_i64(db.query("SELECT v FROM good WHERE id = 1", &[]).unwrap()), 7);
    db.verify().unwrap();
}

/// DDL BEFORE any savepoint is fine, and a `ROLLBACK TO` a savepoint opened
/// AFTER the DDL keeps the table (the DDL is outside the savepoint's scope).
#[test]
fn ddl_before_savepoint_then_rollback_to_keeps_table() {
    let (cfg, _g) = config("ddl_before_sp");
    let db = Database::open_with_config(cfg).unwrap();

    let mut s = db.begin().unwrap();
    s.query("CREATE TABLE t(id INTEGER PRIMARY KEY, v)", &[]).unwrap();
    s.query("INSERT INTO t VALUES(1, 10)", &[]).unwrap();
    s.query("SAVEPOINT sp", &[]).unwrap();
    s.query("INSERT INTO t VALUES(2, 20)", &[]).unwrap();
    s.query("ROLLBACK TO sp", &[]).unwrap();
    // The post-savepoint INSERT is undone; the table and its first row remain.
    assert_eq!(one_i64(s.query("SELECT count(*) FROM t", &[]).unwrap()), 1);
    assert_eq!(one_i64(s.query("SELECT v FROM t WHERE id = 1", &[]).unwrap()), 10);
    s.commit().unwrap();
    assert_eq!(one_i64(db.query("SELECT count(*) FROM t", &[]).unwrap()), 1);
    db.verify().unwrap();
}
