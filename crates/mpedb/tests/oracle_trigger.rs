//! Oracle PL/SQL row triggers. `:OLD` is NULL on INSERT and `:NEW` is NULL
//! on DELETE. A NULL column stays NULL. The body records what it saw; it
//! does not change the row.

use mpedb::{Config, Database, ExecResult, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn toml_for(path: &Path) -> String {
    format!(
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
    )
}

fn fresh_path(name: &str) -> PathBuf {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!(
        "mpedb-otrg-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn open(path: &Path) -> Database {
    Database::open_with_config(Config::from_toml_str(&toml_for(path)).unwrap()).unwrap()
}

fn apply(db: &Database, stmts: &[&str]) {
    for sql in stmts {
        db.query(sql, &[]).unwrap_or_else(|err| panic!("`{sql}` failed: {err}"));
    }
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows from `{sql}`, got {other:?}"),
    }
}

const SEE_INSERT: &str = r#"
CREATE TRIGGER see_ins AFTER INSERT ON ledger FOR EACH ROW
DECLARE
  n VARCHAR2(40);
  o VARCHAR2(40);
  i PLS_INTEGER;
BEGIN
  n := :NEW.note;
  o := :OLD.note;
  i := :NEW.id;
  IF n IS NULL THEN
    NULL;
  END IF;
  INSERT INTO seen (id, kind, new_note, old_note) VALUES (TRG.i, 'ins', TRG.n, TRG.o);
END;
"#;

const SEE_DELETE: &str = r#"
CREATE TRIGGER see_del AFTER DELETE ON ledger FOR EACH ROW
DECLARE
  n VARCHAR2(40);
  o VARCHAR2(40);
  i PLS_INTEGER;
BEGIN
  n := :NEW.note;
  o := :OLD.note;
  i := :OLD.id + 1000;
  IF n IS NULL THEN
    NULL;
  END IF;
  INSERT INTO seen (id, kind, new_note, old_note) VALUES (TRG.i, 'del', TRG.n, TRG.o);
END;
"#;

#[test]
fn oracle_row_trigger_null_images() {
    let path = fresh_path("null-images");
    let db = open(&path);
    apply(
        &db,
        &[
            "CREATE TABLE ledger (id INTEGER PRIMARY KEY, note TEXT)",
            "CREATE TABLE seen (id INTEGER PRIMARY KEY, kind TEXT, new_note TEXT, old_note TEXT)",
            SEE_INSERT,
            SEE_DELETE,
        ],
    );
    apply(
        &db,
        &[
            "INSERT INTO ledger (id, note) VALUES (1, NULL)",
            "INSERT INTO ledger (id, note) VALUES (2, 'kept')",
        ],
    );
    assert_eq!(
        rows(&db, "SELECT id, kind, new_note, old_note FROM seen ORDER BY id"),
        vec![
            vec![
                Value::Int(1),
                Value::Text("ins".into()),
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Int(2),
                Value::Text("ins".into()),
                Value::Text("kept".into()),
                Value::Null,
            ],
        ]
    );

    // Catalog load compiles the body again from the stored source.
    drop(db);
    let db = open(&path);
    apply(
        &db,
        &[
            "DELETE FROM ledger WHERE id = 2",
            "DELETE FROM ledger WHERE id = 1",
        ],
    );
    assert_eq!(
        rows(&db, "SELECT id, kind, new_note, old_note FROM seen ORDER BY id"),
        vec![
            vec![
                Value::Int(1),
                Value::Text("ins".into()),
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Int(2),
                Value::Text("ins".into()),
                Value::Text("kept".into()),
                Value::Null,
            ],
            vec![
                Value::Int(1001),
                Value::Text("del".into()),
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Int(1002),
                Value::Text("del".into()),
                Value::Null,
                Value::Text("kept".into()),
            ],
        ]
    );
    assert_eq!(rows(&db, "SELECT count(*) FROM ledger"), vec![vec![Value::Int(0)]]);

    let err = db
        .query(
            "CREATE TRIGGER bad BEFORE INSERT ON ledger FOR EACH ROW \
             DECLARE n VARCHAR2(40); BEGIN :NEW.note := n; END;",
            &[],
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("a trigger cannot assign :NEW or :OLD"),
        "{err}"
    );
    let _ = std::fs::remove_file(&path);
}
