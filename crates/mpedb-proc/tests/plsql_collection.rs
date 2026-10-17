//! Nested-table procedures against a real shared-memory database file.
//! `proc.rs` is near the 2000-line ceiling, so new collection tests live here.

use mpedb::{Config, Database, Value};
use mpedb_proc::{Lang, ProcEngine, ProcValue};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn test_config(name: &str) -> (Config, FileGuard) {
    let dir = mpedb_testkit::scratch_base();
    let path = dir.join(format!(
        "mpedb-proc-{name}-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        r#"
[database]
path = "{}"
size_mb = 16
max_readers = 64

[[table]]
name = "accounts"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "balance"
  type = "int64"
  nullable = false
"#,
        path.display()
    );
    (Config::from_toml_str(&toml).unwrap(), FileGuard(path))
}

struct FileGuard(PathBuf);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn plsql_nested_table_extend_of_null() {
    let (cfg, _guard) = test_config("plsql-nested-extend");
    let db = Database::open_with_config(cfg).unwrap();
    db.query(
        "CREATE TABLE marks (id INTEGER PRIMARY KEY, score INTEGER)",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO marks (id, score) VALUES (1, 4)", &[])
        .unwrap();
    db.query("INSERT INTO marks (id, score) VALUES (2, NULL)", &[])
        .unwrap();
    let engine = ProcEngine::new(&db);
    engine
        .define(
            "CREATE FUNCTION see_null RETURN PLS_INTEGER IS \
             TYPE nums IS TABLE OF NUMBER; \
             n nums; \
             BEGIN \
               n.EXTEND; \
               RETURN 1; \
             EXCEPTION \
               WHEN COLLECTION_IS_NULL THEN \
                 RETURN NULL; \
             END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("SEE_NULL", &[]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
    engine
        .define(
            "CREATE FUNCTION see_extend RETURN PLS_INTEGER IS \
             TYPE nums IS TABLE OF NUMBER; \
             n nums; \
             BEGIN \
               SELECT score BULK COLLECT INTO n FROM marks; \
               n.EXTEND; \
               IF n.COUNT = 3 AND n(2) IS NULL AND n(3) IS NULL THEN \
                 RETURN NULL; \
               END IF; \
               RETURN 1; \
             END;",
            Lang::PlSql,
        )
        .unwrap();
    assert_eq!(
        engine.call("SEE_EXTEND", &[]).unwrap(),
        ProcValue::Scalar(Value::Null)
    );
}
