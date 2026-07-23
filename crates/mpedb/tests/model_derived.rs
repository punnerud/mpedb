//! `[[model.derived]]` (DESIGN-MODEL-LANG §3's maintenance rung): the model
//! DECLARES a derived structure, `sync_model_derived` makes the database match
//! — engine-generated triggers keep it current, a changed declaration
//! regenerates it, an unclaimed one is dropped. The oracle throughout is
//! recomputation from the source table: after any write mix, the derived
//! content must equal what a fresh scan derives.

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
        "mpedb-drv-{name}-{}-{}.mpedb",
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
name = "edge"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "src"
  type = "int64"
  nullable = true

  [[table.column]]
  name = "dst"
  type = "int64"
  nullable = true

  [[table.index]]
  columns = ["src", "dst"]
  unique = true

[[table]]
name = "msg"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "author"
  type = "text"
  nullable = true
"#,
        path.display()
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (db, FileGuard(path))
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

const GRAPH_MODEL: &str = r#"
[model]
archetype = "graph-traversal"

[[model.table]]
name = "edge"
role = "edge"
  [[model.table.access]]
  kind = "traverse"
  columns = ["src", "dst"]

[[model.derived]]
name = "edge_rev"
kind = "reverse-edge"
source = "edge"
"#;

/// The derived mirror must equal a fresh recomputation from the source —
/// which excludes NULL-endpoint rows (index-membership discipline).
fn assert_rev_matches(db: &Database) {
    let derived = rows(db, "SELECT dst, src FROM edge_rev ORDER BY dst, src");
    let recomputed = rows(
        db,
        "SELECT dst, src FROM edge WHERE dst IS NOT NULL AND src IS NOT NULL \
         ORDER BY dst, src",
    );
    assert_eq!(derived, recomputed);
}

#[test]
fn reverse_edge_backfills_and_stays_current() {
    let (db, _g) = open("rev");
    for (id, s, d) in [(1i64, "1", "2"), (2, "1", "3"), (3, "2", "3"), (4, "7", "NULL")] {
        db.query(
            &format!("INSERT INTO edge (id, src, dst) VALUES ({id}, {s}, {d})"),
            &[],
        )
        .unwrap();
    }
    db.set_model(GRAPH_MODEL).unwrap();
    let r = db.sync_model_derived().unwrap();
    assert_eq!(r.installed, vec!["edge_rev"]);
    assert_rev_matches(&db);
    // The NULL-endpoint row has no mirror entry.
    assert_eq!(rows(&db, "SELECT count(*) FROM edge_rev"), vec![vec![Value::Int(3)]]);

    // Live maintenance: insert, delete, a key-moving update, a became-NULL
    // update (the _un trigger), and a NULL-healing update (the _u trigger
    // with a no-match OLD delete).
    db.query("INSERT INTO edge (id, src, dst) VALUES (5, 3, 1)", &[]).unwrap();
    assert_rev_matches(&db);
    db.query("DELETE FROM edge WHERE id = 2", &[]).unwrap();
    assert_rev_matches(&db);
    db.query("UPDATE edge SET dst = 9 WHERE id = 3", &[]).unwrap();
    assert_rev_matches(&db);
    db.query("UPDATE edge SET dst = NULL WHERE id = 5", &[]).unwrap();
    assert_rev_matches(&db);
    db.query("UPDATE edge SET dst = 8 WHERE id = 4", &[]).unwrap();
    assert_rev_matches(&db);

    // Idempotent: a second sync keeps, changes nothing.
    let r = db.sync_model_derived().unwrap();
    assert_eq!(r.kept, vec!["edge_rev"]);
    assert!(r.installed.is_empty() && r.dropped.is_empty());
}

const COUNTER_MODEL: &str = r#"
[model]
archetype = "oltp"

[[model.derived]]
name = "msgs_per_author"
kind = "counter"
source = "msg"
group_by = ["author"]
"#;

fn assert_counts_match(db: &Database) {
    let derived = rows(
        db,
        "SELECT author, n FROM msgs_per_author WHERE n > 0 ORDER BY author",
    );
    let recomputed = rows(
        db,
        "SELECT author, count(*) FROM msg WHERE author IS NOT NULL \
         GROUP BY author ORDER BY author",
    );
    assert_eq!(derived, recomputed);
}

#[test]
fn counter_backfills_counts_moves_and_rests_at_zero() {
    let (db, _g) = open("cnt");
    for (id, a) in [(1, "'ada'"), (2, "'ada'"), (3, "'bo'"), (4, "NULL")] {
        db.query(&format!("INSERT INTO msg (id, author) VALUES ({id}, {a})"), &[]).unwrap();
    }
    db.set_model(COUNTER_MODEL).unwrap();
    db.sync_model_derived().unwrap();
    assert_counts_match(&db);

    // NULL authors have no entry (index-membership discipline).
    assert_eq!(rows(&db, "SELECT count(*) FROM msgs_per_author"), vec![vec![Value::Int(2)]]);

    // Insert, author move, move to NULL, move from NULL, delete.
    db.query("INSERT INTO msg (id, author) VALUES (5, 'bo')", &[]).unwrap();
    assert_counts_match(&db);
    db.query("UPDATE msg SET author = 'ada' WHERE id = 3", &[]).unwrap();
    assert_counts_match(&db);
    db.query("UPDATE msg SET author = NULL WHERE id = 5", &[]).unwrap();
    assert_counts_match(&db);
    db.query("UPDATE msg SET author = 'cy' WHERE id = 4", &[]).unwrap();
    assert_counts_match(&db);
    db.query("DELETE FROM msg WHERE author = 'ada'", &[]).unwrap();
    assert_counts_match(&db);

    // A fully drained key RESTS at 0 (a count, not a membership set).
    assert_eq!(
        rows(&db, "SELECT n FROM msgs_per_author WHERE author = 'ada'"),
        vec![vec![Value::Int(0)]]
    );
}

#[test]
fn unclaimed_is_dropped_and_changed_is_regenerated() {
    let (db, _g) = open("sync");
    db.query("INSERT INTO msg (id, author) VALUES (1, 'ada')", &[]).unwrap();
    db.set_model(COUNTER_MODEL).unwrap();
    db.sync_model_derived().unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM msgs_per_author").len(), 1);

    // The model stops claiming it: triggers AND table go.
    db.set_model("[model]\narchetype = \"oltp\"\n[[model.table]]\nname = \"msg\"\n")
        .unwrap();
    let r = db.sync_model_derived().unwrap();
    assert_eq!(r.dropped, vec!["msgs_per_author"]);
    assert!(db.query("SELECT count(*) FROM msgs_per_author", &[]).is_err());
    assert!(db
        .list_triggers()
        .unwrap()
        .iter()
        .all(|(n, _, _)| !n.starts_with("__drv_")));
    // And the source is writable without ghost maintenance.
    db.query("INSERT INTO msg (id, author) VALUES (2, 'bo')", &[]).unwrap();

    // A CHANGED declaration (different key) regenerates under the same name.
    db.set_model(COUNTER_MODEL).unwrap();
    db.sync_model_derived().unwrap();
    let changed = COUNTER_MODEL.replace("group_by = [\"author\"]", "group_by = [\"id\"]");
    db.set_model(&changed).unwrap();
    let r = db.sync_model_derived().unwrap();
    assert_eq!(r.installed, vec!["msgs_per_author"]);
    // Now keyed by id: one row per message.
    assert_eq!(
        rows(&db, "SELECT count(*) FROM msgs_per_author"),
        vec![vec![Value::Int(2)]]
    );
}

#[test]
fn refusals_name_the_rule() {
    let (db, _g) = open("refuse");
    // reverse-edge needs the pair UNIQUE on the source: point it at `msg`
    // (no traverse declaration → that refusal fires first; then a model
    // whose traverse names a non-unique pair).
    db.set_model(
        "[model]\n[[model.derived]]\nname = \"r\"\nkind = \"reverse-edge\"\nsource = \"msg\"\n",
    )
    .unwrap();
    let e = db.sync_model_derived().unwrap_err();
    assert!(e.to_string().contains("traverse"), "{e}");

    // A traverse pair that is NOT unique on the source refuses by name: the
    // mirror is keyed on the pair.
    db.set_model(
        "[model]\n[[model.table]]\nname = \"msg\"\nrole = \"edge\"\n         [[model.table.access]]\nkind = \"traverse\"\ncolumns = [\"id\", \"author\"]\n         [[model.derived]]\nname = \"r\"\nkind = \"reverse-edge\"\nsource = \"msg\"\n",
    )
    .unwrap();
    let e = db.sync_model_derived().unwrap_err();
    assert!(e.to_string().contains("UNIQUE"), "{e}");

    // Name collision with a real (non-derived) table refuses.
    db.set_model(
        "[model]\n[[model.derived]]\nname = \"msg\"\nkind = \"counter\"\nsource = \"edge\"\ngroup_by = [\"src\"]\n",
    )
    .unwrap();
    let e = db.sync_model_derived().unwrap_err();
    assert!(e.to_string().contains("not"), "{e}");

    // Counter key may not be called `n`.
    let (db2, _g2) = open("refuse-n");
    db2.query("ALTER TABLE msg RENAME COLUMN author TO n", &[]).unwrap();
    db2.set_model(
        "[model]\n[[model.derived]]\nname = \"c\"\nkind = \"counter\"\nsource = \"msg\"\ngroup_by = [\"n\"]\n",
    )
    .unwrap();
    let e = db2.sync_model_derived().unwrap_err();
    assert!(e.to_string().contains("cannot be named `n`"), "{e}");

    // Language-level: counter without exactly one group_by refuses at parse.
    let e = db
        .set_model("[model]\n[[model.derived]]\nname = \"c\"\nkind = \"counter\"\nsource = \"msg\"\n")
        .unwrap_err();
    assert!(e.to_string().contains("group_by"), "{e}");
}
