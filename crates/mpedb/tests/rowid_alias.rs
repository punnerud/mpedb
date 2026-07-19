//! INTEGER PRIMARY KEY as a rowid alias (sqlite semantics), differential-tested
//! against the `sqlite3` CLI (3.45).
//!
//! A table whose PRIMARY KEY is a SINGLE integer column makes that column an
//! alias for the rowid: a NULL or omitted value on INSERT auto-assigns
//! `max(existing rowid) + 1` (1 for an empty table). This is the plain,
//! non-AUTOINCREMENT rule — the CURRENT maximum plus one — so a deleted top
//! row's id can be reused. An explicit non-NULL id inserts at that id (a
//! duplicate is still a uniqueness error).
//!
//! Deliberate, documented deviations from sqlite (each a clean error, never a
//! wrong answer): a composite PK and a non-integer PK are NOT rowid aliases and
//! stay strict — mpedb rejects a NULL there, where sqlite's historical leniency
//! stores the NULL; and `AUTOINCREMENT` is refused by name (mpedb keeps no
//! persisted high-water counter, so it cannot promise the never-reuse guarantee
//! and will not silently downgrade it to the reuse-allowed behavior).

use mpedb::{Config, Database, Error, ExecResult, Value};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "sqlite_oracle/mod.rs"]
mod sqlite_oracle;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Self-deleting database, so a panicking test does not leak a `/dev/shm` file.
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

/// A fresh mpedb database seeded with one throwaway table; the tables under test
/// are created live via `CREATE TABLE`, exactly as they are in the sqlite3
/// script, so the two engines run byte-identical DDL.
fn open() -> Tmp {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm"
    } else {
        "/tmp"
    };
    let path = format!(
        "{dir}/mpedb-rowid-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 8\nmax_readers = 8\n\n\
         [[table]]\nname = \"seed\"\nprimary_key = [\"id\"]\n\
         [[table.column]]\nname = \"id\"\ntype = \"int64\"\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    Tmp { db, path }
}

/// Engine-agnostic cell rendering, matching the `sqlite3` CLI default list mode:
/// NULL empty, integers/text verbatim.
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        other => panic!("unexpected value in rowid test: {other:?}"),
    }
}

/// Run a script (setup DDL+DML) then a final SELECT against mpedb, returning the
/// rows rendered as strings. Every setup statement must succeed.
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

/// The same script + SELECT through the `sqlite3` CLI, parsed from list mode.
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

/// Assert mpedb and sqlite3 agree on the final table state for a script.
fn assert_same(setup: &[&str], query: &str) {
    let m = mpedb_state(setup, query);
    let s = sqlite_state(setup, query);
    assert_eq!(m, s, "mpedb vs sqlite3 diverged for:\n{setup:?}\n{query}");
}

const DDL: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)";
const SELECT: &str = "SELECT id, v FROM t ORDER BY id";

#[test]
fn null_value_auto_assigns() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (NULL, 'a')",
            "INSERT INTO t VALUES (NULL, 'b')",
            "INSERT INTO t VALUES (NULL, 'c')",
        ],
        SELECT,
    );
}

#[test]
fn omitted_column_auto_assigns() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t (v) VALUES ('a')",
            "INSERT INTO t (v) VALUES ('b')",
            // Multi-row, column omitted: consecutive ids in one statement.
            "INSERT INTO t (v) VALUES ('c'), ('d')",
        ],
        SELECT,
    );
}

#[test]
fn explicit_id_inserts_at_that_id() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (10, 'ten')",
            "INSERT INTO t VALUES (5, 'five')",
            "INSERT INTO t (id, v) VALUES (7, 'seven')",
        ],
        SELECT,
    );
}

#[test]
fn max_plus_one_tracks_the_current_maximum() {
    // After an explicit high id, an auto value is max+1 of the whole table,
    // not a running counter — and mixing explicit + auto in one multi-row
    // statement keeps ordering.
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (10, 'ten')",
            "INSERT INTO t VALUES (NULL, 'after10')",
            "INSERT INTO t VALUES (NULL, 'a'), (2, 'two'), (NULL, 'b')",
        ],
        SELECT,
    );
}

#[test]
fn negative_ids_still_max_plus_one() {
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (-3, 'neg')",
            "INSERT INTO t VALUES (NULL, 'next')",
        ],
        SELECT,
    );
}

#[test]
fn reuse_after_delete_of_max_row() {
    // sqlite (plain INTEGER PK, NOT autoincrement) reuses a freed top id: the
    // next auto value is max+1 of the CURRENT rows, so deleting the highest row
    // makes its id available again.
    assert_same(
        &[
            DDL,
            "INSERT INTO t VALUES (NULL, 'a')", // 1
            "INSERT INTO t VALUES (NULL, 'b')", // 2
            "INSERT INTO t VALUES (NULL, 'c')", // 3
            "DELETE FROM t WHERE id = 3",
            "INSERT INTO t VALUES (NULL, 'reused')", // 3 again
        ],
        SELECT,
    );
}

#[test]
fn empty_table_starts_at_one() {
    assert_same(&[DDL, "INSERT INTO t (v) VALUES ('first')"], SELECT);
}

#[test]
fn duplicate_explicit_id_errors() {
    // Explicit id collision is a uniqueness error in BOTH engines.
    let t = open();
    t.db.query(DDL, &[]).unwrap();
    t.db.query("INSERT INTO t VALUES (5, 'a')", &[]).unwrap();
    let err = t
        .db
        .query("INSERT INTO t VALUES (5, 'b')", &[])
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::PrimaryKeyViolation { .. } | Error::UniqueViolation { .. }
        ),
        "expected a uniqueness error, got {err}"
    );

    // sqlite agrees: the duplicate makes the script fail.
    let mut script = String::from("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);\n");
    script.push_str("INSERT INTO t VALUES (5, 'a');\nINSERT INTO t VALUES (5, 'b');\n");
    assert!(
        sqlite_oracle::try_script_stdout(&script, "").is_err(),
        "sqlite should reject the duplicate explicit id"
    );
}

#[test]
fn composite_pk_is_not_a_rowid_alias() {
    // A composite PK is not a rowid alias; mpedb stays strict and rejects a NULL
    // key column (a documented, deliberate deviation — sqlite's historical
    // leniency stores the NULL; mpedb never answers wrongly, it refuses).
    let t = open();
    t.db.query(
        "CREATE TABLE c (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        &[],
    )
    .unwrap();
    let err = t
        .db
        .query("INSERT INTO c VALUES (NULL, 1)", &[])
        .unwrap_err();
    assert!(
        matches!(err, Error::NotNullViolation { .. } | Error::Bind(_)),
        "composite PK must stay strict, got {err}"
    );
    // An explicit composite key still works and does not auto-assign anything.
    t.db.query("INSERT INTO c VALUES (2, 3)", &[]).unwrap();
    match t.db.query("SELECT a, b FROM c", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(2), Value::Int(3)]])
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn non_integer_pk_is_not_a_rowid_alias() {
    // A TEXT primary key is not a rowid alias; a NULL is a strict violation in
    // mpedb (deliberate deviation from sqlite's leniency).
    let t = open();
    t.db.query("CREATE TABLE tx (id TEXT PRIMARY KEY, v TEXT)", &[])
        .unwrap();
    let err = t
        .db
        .query("INSERT INTO tx VALUES (NULL, 'a')", &[])
        .unwrap_err();
    assert!(
        matches!(err, Error::NotNullViolation { .. } | Error::Bind(_)),
        "text PK must stay strict, got {err}"
    );
    // An omitted text PK is likewise not defaultable → refused at bind time.
    let err = t
        .db
        .query("INSERT INTO tx (v) VALUES ('a')", &[])
        .unwrap_err();
    assert!(matches!(err, Error::Bind(_)), "got {err}");
}

#[test]
fn autoincrement_is_refused_by_name() {
    let t = open();
    let err = t
        .db
        .query(
            "CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
            &[],
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("AUTOINCREMENT"),
        "AUTOINCREMENT should be refused by name, got: {msg}"
    );
}

#[test]
fn returning_sees_the_assigned_id() {
    // RETURNING must observe the auto-assigned rowid, not NULL.
    let t = open();
    t.db.query(DDL, &[]).unwrap();
    match t
        .db
        .query("INSERT INTO t VALUES (NULL, 'a') RETURNING id", &[])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        other => panic!("{other:?}"),
    }
    match t
        .db
        .query("INSERT INTO t (v) VALUES ('b') RETURNING id", &[])
        .unwrap()
    {
        ExecResult::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(2)]]),
        other => panic!("{other:?}"),
    }
}
