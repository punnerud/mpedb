//! `DROP INDEX [IF EXISTS] <name>` — and the one thing that can go silently
//! wrong when it does.
//!
//! mpedb indexes are POSITIONAL: `index_no` is the position in
//! `TableDef::indexes` plus one, and that number is what plans and B-trees key
//! on. So dropping index #2 of three has to move #3 down to #2 — schema entry,
//! catalog tree-root and in-memory cache together. Get that wrong and a query
//! that used to read index #3 now reads index #2's B-tree and returns the
//! WRONG ROWS with no error at all. That is what the bulk of this file is
//! about; parsing and `IF EXISTS` are the easy half.
//!
//! Index NAMES only exist on the wire from canonical-bytes v11. Before that,
//! `DROP INDEX` could not be parsed and `REINDEX <typo>` could not be refused,
//! because nothing could tell a real index name from a made-up one.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open(name: &str) -> (Database, PathBuf) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!(
        "mpedb-dropidx-{name}-{}-{}.mpedb",
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

fn run(db: &Database, sql: &str) {
    db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
}

fn ints(db: &Database, sql: &str) -> Vec<i64> {
    match db.query(sql, &[]).unwrap_or_else(|e| panic!("`{sql}` failed: {e}")) {
        ExecResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(i) => i,
                ref other => panic!("expected an int, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Drop the MIDDLE of three indexes and check that the one above it still
/// answers for its own column.
///
/// Each index covers a different column with deliberately different values, so
/// a plan left pointing at the wrong B-tree cannot coincidentally agree: `b`,
/// `c` and `d` never share a value for the same row.
#[test]
fn dropping_the_middle_index_renumbers_the_one_above_it() {
    let (db, path) = open("renumber");
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, b INTEGER, c INTEGER, d INTEGER)");
    for i in 1..=20i64 {
        run(
            &db,
            &format!("INSERT INTO t (id, b, c, d) VALUES ({i}, {}, {}, {})", i, i + 100, i + 200),
        );
    }
    run(&db, "CREATE INDEX ib ON t (b)");
    run(&db, "CREATE INDEX ic ON t (c)");
    run(&db, "CREATE INDEX id_ ON t (d)");

    // Every index answers before the drop.
    assert_eq!(ints(&db, "SELECT id FROM t WHERE b = 7"), vec![7]);
    assert_eq!(ints(&db, "SELECT id FROM t WHERE c = 107"), vec![7]);
    assert_eq!(ints(&db, "SELECT id FROM t WHERE d = 207"), vec![7]);

    run(&db, "DROP INDEX ic");

    // `ib` was below the dropped one and did not move; `id_` was above it and
    // did. If the renumbering missed either the catalog root or the cache,
    // `d = 207` reads `ic`'s freed tree and answers nothing — or worse, reads a
    // live tree keyed on `c` and answers a row that does not match.
    assert_eq!(ints(&db, "SELECT id FROM t WHERE b = 7"), vec![7]);
    assert_eq!(ints(&db, "SELECT id FROM t WHERE d = 207"), vec![7]);
    // And the dropped index's column still answers correctly — by scan now.
    assert_eq!(ints(&db, "SELECT id FROM t WHERE c = 107"), vec![7]);

    // Writes after the drop keep every surviving index consistent.
    run(&db, "INSERT INTO t (id, b, c, d) VALUES (21, 21, 121, 221)");
    assert_eq!(ints(&db, "SELECT id FROM t WHERE b = 21"), vec![21]);
    assert_eq!(ints(&db, "SELECT id FROM t WHERE c = 121"), vec![21]);
    assert_eq!(ints(&db, "SELECT id FROM t WHERE d = 221"), vec![21]);
    run(&db, "DELETE FROM t WHERE id = 3");
    assert_eq!(ints(&db, "SELECT id FROM t WHERE d = 203"), Vec::<i64>::new());
    let _ = std::fs::remove_file(&path);
}

/// The survivors are still there, and the name is really gone.
#[test]
fn a_dropped_name_is_gone_and_the_others_remain() {
    let (db, path) = open("names");
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, b INTEGER, c INTEGER)");
    run(&db, "CREATE INDEX ib ON t (b)");
    run(&db, "CREATE INDEX ic ON t (c)");
    run(&db, "DROP INDEX ib");

    // Gone: a second drop of the same name is an error, not a silent success.
    let e = db.query("DROP INDEX ib", &[]).unwrap_err().to_string();
    assert!(e.contains("no such index"), "expected a named refusal, got: {e}");
    // …but IF EXISTS still succeeds, as sqlite does.
    assert_eq!(db.query("DROP INDEX IF EXISTS ib", &[]).unwrap(), ExecResult::Affected(0));
    // The survivor kept its name: dropping it works, and only once.
    run(&db, "DROP INDEX ic");
    assert!(db.query("DROP INDEX ic", &[]).unwrap_err().to_string().contains("no such index"));
    let _ = std::fs::remove_file(&path);
}

/// `REINDEX <name>` must ERROR on a name that identifies nothing. It was
/// accepted before index names existed, which made it the corpus's only error
/// mismatch — invisible, because a lenient no-op is indistinguishable from
/// success.
#[test]
fn reindex_refuses_a_name_that_identifies_nothing() {
    let (db, path) = open("reindex");
    run(&db, "CREATE TABLE t (id INTEGER PRIMARY KEY, b INTEGER)");
    run(&db, "CREATE INDEX t1i1 ON t (b)");

    // The three things sqlite accepts: no target, a table, an index.
    run(&db, "REINDEX");
    run(&db, "REINDEX t");
    run(&db, "REINDEX t1i1");
    // …and a collation name.
    run(&db, "REINDEX NOCASE");

    let e = db.query("REINDEX tXiX", &[]).unwrap_err().to_string();
    assert!(
        e.contains("unable to identify the object to be reindexed"),
        "expected sqlite's message, got: {e}"
    );
    // A dropped index's name stops identifying anything, too.
    run(&db, "DROP INDEX t1i1");
    assert!(db.query("REINDEX t1i1", &[]).unwrap_err().to_string().contains("unable to identify"));
    let _ = std::fs::remove_file(&path);
}

/// A flag-derived index (declared in the config, not by `CREATE INDEX`) has no
/// name to point at, and the refusal says so rather than pretending the index
/// is absent.
#[test]
fn a_flag_derived_index_has_no_name_to_drop() {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!(
        "mpedb-dropidx-flag-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "b"
  type = "int64"
  indexed = true
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    // There IS an index on `b`, but it has no name, so nothing names it.
    let e = db.query("DROP INDEX b", &[]).unwrap_err().to_string();
    assert!(e.contains("no such index"), "got: {e}");
    let _ = std::fs::remove_file(&path);
}
