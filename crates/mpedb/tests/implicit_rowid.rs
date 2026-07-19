//! Implicit rowid (#94): a `CREATE TABLE` with NO declared PRIMARY KEY gets a
//! HIDDEN auto-increment integer `rowid` as its key, exactly like sqlite. Every
//! behavior below is differential-tested against the `sqlite3` CLI (3.45).
//!
//! What is verified vs sqlite: `SELECT *` shows only the declared columns; the
//! rowid is addressable by `rowid` / `_rowid_` / `oid`; INSERT with and without a
//! column list; the auto-increment 1,2,3,… sequence (reused after deleting the
//! top row, the plain non-AUTOINCREMENT rule); `WHERE rowid = ?`; `count(*)`,
//! ORDER BY / GROUP BY over the visible columns; a join between two PK-less
//! tables; delete/update by rowid; and that an explicit-PK table is unaffected.

use mpedb::{Config, Database, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

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

fn new_path() -> String {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-implicit-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    path
}

fn open_at(path: &str) -> Database {
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap()
}

/// A fresh mpedb database seeded with one throwaway table; the tables under test
/// are created live via `CREATE TABLE`, byte-identical to the sqlite3 script.
fn open() -> Tmp {
    let path = new_path();
    let db = open_at(&path);
    Tmp { db, path }
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("unexpected value in implicit-rowid test: {other:?}"),
    }
}

fn mpedb_state(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let t = open();
    for s in setup {
        t.db.query(s, &[])
            .unwrap_or_else(|e| panic!("mpedb setup `{s}` failed: {e}"));
    }
    match t.db.query(query, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect(),
        other => panic!("expected rows from `{query}`, got {other:?}"),
    }
}

fn sqlite_state(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push_str(";\n");

    sqlite_oracle::script_stdout(&script, "")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split('|').map(str::to_string).collect())
        .collect()
}

/// Assert mpedb and sqlite3 agree on the final result for a script.
fn assert_same(setup: &[&str], query: &str) {
    let m = mpedb_state(setup, query);
    let s = sqlite_state(setup, query);
    assert_eq!(m, s, "mpedb vs sqlite3 diverged for:\n{setup:?}\n{query}");
}

const DDL: &str = "CREATE TABLE t (a, b)";

#[test]
fn select_star_shows_only_declared_columns() {
    // `SELECT *` NEVER includes the hidden rowid — just `a`, `b`, in order.
    assert_same(
        &[DDL, "INSERT INTO t VALUES (1, 2)", "INSERT INTO t VALUES (3, 4)"],
        "SELECT * FROM t ORDER BY a",
    );
}

#[test]
fn insert_without_column_list_maps_to_visible_columns() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (1, 2)",
            "INSERT INTO t VALUES (10, 20)",
        ],
        "SELECT a, b FROM t ORDER BY a",
    );
}

#[test]
fn insert_with_partial_column_list_defaults_the_rest_to_null() {
    assert_same(
        &[DDL, "INSERT INTO t(a) VALUES (5)", "INSERT INTO t(b) VALUES (7)"],
        "SELECT a, b FROM t ORDER BY a",
    );
}

#[test]
fn rowid_auto_increments_one_two_three() {
    // The hidden rowid is 1,2,3,… — addressable by name, never in `*`.
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (10, 11)",
            "INSERT INTO t VALUES (20, 21)",
            "INSERT INTO t VALUES (30, 31)",
        ],
        "SELECT rowid, a, b FROM t ORDER BY rowid",
    );
}

#[test]
fn all_three_rowid_spellings_resolve() {
    // sqlite exposes the rowid under `rowid`, `_rowid_` and `oid`.
    assert_same(
        &[DDL, "INSERT INTO t VALUES (7, 8)"],
        "SELECT rowid, _rowid_, oid FROM t",
    );
}

#[test]
fn where_rowid_point_lookup() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (10, 100)",
            "INSERT INTO t VALUES (20, 200)",
            "INSERT INTO t VALUES (30, 300)",
        ],
        "SELECT a, b FROM t WHERE rowid = 2",
    );
}

#[test]
fn where_rowid_param() {
    // The rowid drives a parameterized point lookup through the facade.
    let t = open();
    t.db.query(DDL, &[]).unwrap();
    for (a, b) in [(10, 100), (20, 200), (30, 300)] {
        t.db.query("INSERT INTO t VALUES (?, ?)", &[Value::Int(a), Value::Int(b)])
            .unwrap();
    }
    match t
        .db
        .query("SELECT a, b FROM t WHERE rowid = ?", &[Value::Int(3)])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(30), Value::Int(300)]])
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn reuse_after_delete_of_top_rowid() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (1, 1)", // rowid 1
            "INSERT INTO t VALUES (2, 2)", // rowid 2
            "INSERT INTO t VALUES (3, 3)", // rowid 3
            "DELETE FROM t WHERE rowid = 3",
            "INSERT INTO t VALUES (9, 9)", // rowid 3 again
        ],
        "SELECT rowid, a FROM t ORDER BY rowid",
    );
}

#[test]
fn count_star() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (1, 2)",
            "INSERT INTO t VALUES (3, 4)",
            "INSERT INTO t VALUES (5, 6)",
        ],
        "SELECT count(*) FROM t",
    );
}

#[test]
fn group_by_over_visible_columns() {
    assert_same(
        &[
            "CREATE TABLE g (grp, n)",
            "INSERT INTO g VALUES ('x', 1)",
            "INSERT INTO g VALUES ('x', 2)",
            "INSERT INTO g VALUES ('y', 10)",
        ],
        "SELECT grp, sum(n) FROM g GROUP BY grp ORDER BY grp",
    );
}

#[test]
fn order_by_visible_and_ordinal() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (3, 'c')",
            "INSERT INTO t VALUES (1, 'a')",
            "INSERT INTO t VALUES (2, 'b')",
        ],
        "SELECT a, b FROM t ORDER BY 1 DESC",
    );
}

#[test]
fn select_star_order_by_star_ordinal_matches() {
    // `SELECT *` output has two columns; ORDER BY 2 sorts by the 2nd VISIBLE one.
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (1, 30)",
            "INSERT INTO t VALUES (2, 10)",
            "INSERT INTO t VALUES (3, 20)",
        ],
        "SELECT * FROM t ORDER BY 2",
    );
}

#[test]
fn join_between_two_pk_less_tables() {
    // `SELECT *` over the join shows both tables' VISIBLE columns, no rowids.
    assert_same(
        &[
            "CREATE TABLE l (a, x)",
            "CREATE TABLE r (a, y)",
            "INSERT INTO l VALUES (1, 'l1')",
            "INSERT INTO l VALUES (2, 'l2')",
            "INSERT INTO r VALUES (1, 'r1')",
            "INSERT INTO r VALUES (2, 'r2')",
        ],
        "SELECT * FROM l JOIN r ON l.a = r.a ORDER BY l.a",
    );
}

#[test]
fn join_can_address_each_rowid() {
    assert_same(
        &[
            "CREATE TABLE l (a, x)",
            "CREATE TABLE r (a, y)",
            "INSERT INTO l VALUES (1, 'l1')",
            "INSERT INTO r VALUES (1, 'r1')",
        ],
        "SELECT l.rowid, r.rowid, l.x, r.y FROM l JOIN r ON l.a = r.a",
    );
}

#[test]
fn update_by_rowid() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (1, 1)",
            "INSERT INTO t VALUES (2, 2)",
            "UPDATE t SET a = 99 WHERE rowid = 2",
        ],
        "SELECT rowid, a FROM t ORDER BY rowid",
    );
}

#[test]
fn delete_by_rowid() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (1, 1)",
            "INSERT INTO t VALUES (2, 2)",
            "INSERT INTO t VALUES (3, 3)",
            "DELETE FROM t WHERE rowid = 2",
        ],
        "SELECT a FROM t ORDER BY a",
    );
}

#[test]
fn returning_sees_the_assigned_rowid() {
    let t = open();
    t.db.query(DDL, &[]).unwrap();
    match t
        .db
        .query("INSERT INTO t VALUES (5, 6) RETURNING rowid", &[])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("{other:?}"),
    }
    // RETURNING * shows only the visible columns.
    match t
        .db
        .query("INSERT INTO t VALUES (7, 8) RETURNING *", &[])
        .unwrap()
    {
        ExecResult::Rows { columns, rows } => {
            assert_eq!(columns, vec!["a".to_string(), "b".to_string()]);
            assert_eq!(rows, vec![vec![Value::Int(7), Value::Int(8)]]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn explicit_pk_table_is_unaffected() {
    // A table WITH an explicit INTEGER PRIMARY KEY keeps its rowid alias VISIBLE
    // in `SELECT *` — no hidden column, unchanged behavior.
    assert_same(
        &[
            "CREATE TABLE e (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO e VALUES (1, 'a')",
            "INSERT INTO e VALUES (2, 'b')",
        ],
        "SELECT * FROM e ORDER BY id",
    );
    // And `*` over an explicit-PK table has the id column present.
    let t = open();
    t.db.query("CREATE TABLE e (id INTEGER PRIMARY KEY, v TEXT)", &[])
        .unwrap();
    t.db.query("INSERT INTO e VALUES (1, 'a')", &[]).unwrap();
    match t.db.query("SELECT * FROM e", &[]).unwrap() {
        ExecResult::Rows { columns, .. } => {
            assert_eq!(columns, vec!["id".to_string(), "v".to_string()])
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn declaring_a_rowid_column_without_pk_is_refused_cleanly() {
    // A PK-less table that also names a `rowid` column collides with the implicit
    // rowid — refuse cleanly (never answer differently than sqlite).
    let t = open();
    let err = t
        .db
        .query("CREATE TABLE bad (rowid INTEGER, v TEXT)", &[])
        .unwrap_err();
    assert!(
        format!("{err}").contains("rowid"),
        "expected a clean rowid-collision error, got: {err}"
    );
}

#[test]
fn a_second_handle_sees_the_table_and_rows() {
    // The implicit-rowid table is durable in the catalog: a second Database handle
    // on the same file sees the table and its rows (with correct rowids).
    let path = new_path();
    {
        let db = open_at(&path);
        db.query("CREATE TABLE t (a, b)", &[]).unwrap();
        db.query("INSERT INTO t VALUES (1, 2)", &[]).unwrap();
        db.query("INSERT INTO t VALUES (3, 4)", &[]).unwrap();
    }
    // Fresh handle, same file.
    let db2 = open_at(&path);
    match db2
        .query("SELECT rowid, a, b FROM t ORDER BY rowid", &[])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(1), Value::Int(2)],
                vec![Value::Int(2), Value::Int(3), Value::Int(4)],
            ]
        ),
        other => panic!("{other:?}"),
    }
    // `SELECT *` on the reopened handle still hides the rowid.
    match db2.query("SELECT * FROM t ORDER BY a", &[]).unwrap() {
        ExecResult::Rows { columns, .. } => {
            assert_eq!(columns, vec!["a".to_string(), "b".to_string()])
        }
        other => panic!("{other:?}"),
    }
    drop(db2);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
}
