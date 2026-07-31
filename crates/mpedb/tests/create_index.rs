//! `CREATE [UNIQUE] INDEX … ON t (cols)` — builds a secondary index over the
//! existing rows. It never changes query ANSWERS (an index is an optimization);
//! it must build cleanly, enforce UNIQUE going forward, reject a build that
//! finds a duplicate, accept composite / ASC-DESC / IF NOT EXISTS forms, and be
//! idempotent by shape.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open(name: &str) -> (Database, PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!(
        "mpedb-createidx-{name}-{}-{}.mpedb",
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

#[test]
fn create_index_builds_over_existing_rows_and_queries_stay_correct() {
    let (db, path) = open("build");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b TEXT)", &[]).unwrap();
    for id in 1..=20 {
        db.query(&format!("INSERT INTO t (id, a, b) VALUES ({id}, {}, 'r{id}')", id % 5), &[])
            .unwrap();
    }
    // Build a non-unique index AFTER the data exists.
    db.query("CREATE INDEX idx_a ON t (a)", &[]).unwrap();
    // Equality and range over the indexed column return the right rows.
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE a = 3"), 4);
    let got = rows(db.query("SELECT id FROM t WHERE a = 0 ORDER BY id", &[]).unwrap());
    assert_eq!(got, vec![vec![Value::Int(5)], vec![Value::Int(10)], vec![Value::Int(15)], vec![Value::Int(20)]]);
    // A composite index with per-column ASC/DESC (direction ignored) also builds.
    db.query("CREATE INDEX idx_ba ON t (b, a DESC)", &[]).unwrap();
    assert_eq!(
        rows(db.query("SELECT id FROM t WHERE b = 'r7'", &[]).unwrap()),
        vec![vec![Value::Int(7)]]
    );
    // New inserts are indexed too.
    db.query("INSERT INTO t (id, a, b) VALUES (21, 3, 'r21')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t WHERE a = 3"), 5);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn unique_index_build_and_enforcement() {
    let (db, path) = open("unique");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT)", &[]).unwrap();
    db.query("INSERT INTO t (id, email) VALUES (1, 'a@x')", &[]).unwrap();
    db.query("INSERT INTO t (id, email) VALUES (2, 'b@x')", &[]).unwrap();
    // Building a UNIQUE index over distinct values succeeds and enforces going
    // forward.
    db.query("CREATE UNIQUE INDEX idx_email ON t (email)", &[]).unwrap();
    assert!(db.query("INSERT INTO t (id, email) VALUES (3, 'a@x')", &[]).is_err());
    db.query("INSERT INTO t (id, email) VALUES (3, 'c@x')", &[]).unwrap();
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 3);
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn unique_index_build_rejects_existing_duplicate() {
    let (db, path) = open("dupbuild");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)", &[]).unwrap();
    db.query("INSERT INTO t (id, v) VALUES (1, 7)", &[]).unwrap();
    db.query("INSERT INTO t (id, v) VALUES (2, 7)", &[]).unwrap(); // duplicate v
    // A UNIQUE index cannot be built over data that already violates it.
    assert!(db.query("CREATE UNIQUE INDEX idx_v ON t (v)", &[]).is_err());
    // The failed build left the table usable (both rows still there, no index).
    assert_eq!(scalar_i64(&db, "SELECT count(*) FROM t"), 2);
    db.query("INSERT INTO t (id, v) VALUES (3, 7)", &[]).unwrap(); // still allowed
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// `IF NOT EXISTS` is about the NAME, and a duplicate name is an error without
/// it — both MEASURED against sqlite 3.45.
///
/// This test used to assert that a second index with the same COLUMNS was "a
/// no-op, not an error", under two DIFFERENT names. That encoded a
/// misunderstanding: sqlite creates both (they are merely redundant), and the
/// no-op meant `CREATE INDEX also_a …` reported success while creating
/// nothing — so the matching `DROP INDEX also_a` then failed with "no such
/// index". Django's `remove_unique_together` on a unique field does exactly
/// that pair.
#[test]
fn create_index_name_rules_match_sqlite() {
    let (db, path) = open("idem");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b INT)", &[]).unwrap();
    db.query("INSERT INTO t (id, a, b) VALUES (1, 1, 1)", &[]).unwrap();
    db.query("CREATE INDEX idx_a ON t (a)", &[]).unwrap();

    // The SAME NAME with `IF NOT EXISTS` is the no-op — that is what
    // idempotence means here.
    db.query("CREATE INDEX IF NOT EXISTS idx_a ON t (a)", &[]).unwrap();
    // …and without it, a duplicate name is an error, as in sqlite.
    for sql in [
        "CREATE INDEX idx_a ON t (a)",
        // Same name, DIFFERENT shape — still just "already exists".
        "CREATE INDEX idx_a ON t (b)",
    ] {
        let e = db.query(sql, &[]).unwrap_err().to_string();
        assert!(e.contains("already exists"), "`{sql}`: {e}");
    }

    // A DIFFERENT name over the same columns is a distinct — merely redundant —
    // index, and sqlite builds it. This was a refusal while the C-API shim
    // keyed an index's name record by its SHAPE, because two same-shape
    // indexes then collided onto one record; the records are keyed by NAME
    // now, so it is legal here too.
    db.query("CREATE INDEX also_a ON t (a)", &[]).unwrap();
    // Both stand, and each drops under its own name.
    db.query("DROP INDEX also_a", &[]).unwrap();
    db.query("DROP INDEX idx_a", &[]).unwrap();
    db.query("CREATE INDEX idx_a ON t (a)", &[]).unwrap();

    // Unknown table / column errors.
    assert!(db.query("CREATE INDEX x ON nope (a)", &[]).is_err());
    assert!(db.query("CREATE INDEX x ON t (nope)", &[]).is_err());
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// The same rules INSIDE a write session, which is a second, separate applier.
///
/// This is the path that actually mattered: Django runs migrations under
/// `atomic=True`, so its `CREATE INDEX` never reaches the autocommit applier
/// above. The session arm was still idempotent-by-shape long after the
/// autocommit one was fixed, so a redundant index reported success and created
/// nothing and the migration's own `DROP INDEX` then failed.
#[test]
fn create_index_name_rules_hold_inside_a_transaction() {
    let (db, path) = open("idem_txn");
    db.query("CREATE TABLE t (id INTEGER PRIMARY KEY, a INT, b INT)", &[]).unwrap();
    db.query("INSERT INTO t (id, a, b) VALUES (1, 1, 1)", &[]).unwrap();

    let mut s = db.begin().unwrap();
    s.query("CREATE INDEX idx_a ON t (a)", &[]).unwrap();
    // Redundant shape, new name: legal, exactly as in autocommit.
    s.query("CREATE INDEX also_a ON t (a)", &[]).unwrap();
    // Duplicate name: refused without IF NOT EXISTS, a no-op with it.
    let e = s.query("CREATE INDEX idx_a ON t (b)", &[]).unwrap_err().to_string();
    assert!(e.contains("already exists"), "{e}");
    s.query("CREATE INDEX IF NOT EXISTS idx_a ON t (a)", &[]).unwrap();
    s.commit().unwrap();

    // Both survive the commit under their own names.
    db.query("DROP INDEX also_a", &[]).unwrap();
    db.query("DROP INDEX idx_a", &[]).unwrap();
    db.verify().unwrap();
    let _ = std::fs::remove_file(&path);
}
