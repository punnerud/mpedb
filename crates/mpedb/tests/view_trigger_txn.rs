//! VIEW / TRIGGER DDL rides a WriteSession (CPython iterdump / test_table_dump).
use mpedb::{Config, Database, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn open() -> (Database, String) {
    let dir = if std::path::Path::new("/dev/shm").is_dir() {
        "/dev/shm".into()
    } else {
        std::env::temp_dir().display().to_string()
    };
    let path = format!(
        "{dir}/mpedb-vt-{}-{}.mpedb",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = std::fs::remove_file(&path);
    let toml = format!(
        "[database]\npath = \"{path}\"\nsize_mb = 16\nmax_readers = 8\n\n\
         [[table]]\nname = \"t1\"\nprimary_key = [\"id\"]\n\
           [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
           [[table.column]]\n  name = \"s1\"\n  type = \"text\"\n  nullable = true\n\
           [[table.column]]\n  name = \"t1_i1\"\n  type = \"int64\"\n\
           [[table.column]]\n  name = \"i2\"\n  type = \"int64\"\n  nullable = true\n\
         [[table]]\nname = \"t2\"\nprimary_key = [\"id\"]\n\
           [[table.column]]\n  name = \"id\"\n  type = \"int64\"\n\
           [[table.column]]\n  name = \"t2_i1\"\n  type = \"int64\"\n  nullable = true\n\
           [[table.column]]\n  name = \"t2_i2\"\n  type = \"int64\"\n  nullable = true\n"
    );
    let db = Database::open_with_config(Config::from_toml_str(&toml).unwrap()).unwrap();
    (db, path)
}

#[test]
fn view_and_trigger_in_write_session_commit() {
    let (db, path) = open();
    let mut s = db.begin().unwrap();
    s.query(
        "CREATE TRIGGER trigger_1 UPDATE OF t1_i1 ON t1 BEGIN \
         UPDATE t2 SET t2_i1 = NEW.t1_i1 WHERE t2_i1 = OLD.t1_i1; END",
        &[],
    )
    .expect("CREATE TRIGGER in txn");
    // Django/CPython dump shape: JOIN in the view body — stored even when
    // query-time flatten refuses JOIN views (materialise is a separate path).
    s.query(
        "CREATE VIEW v1 AS SELECT * FROM t1 LEFT JOIN t2 USING (id)",
        &[],
    )
    .expect("CREATE VIEW with JOIN in txn");
    s.query("CREATE VIEW v_simple AS SELECT id, s1 FROM t1", &[])
        .expect("CREATE simple VIEW in txn");
    s.commit().unwrap();

    // Visible after commit (catalog + dump surface).
    let views = db.list_views().unwrap();
    assert!(views.iter().any(|(n, src)| n == "v1" && src.contains("LEFT JOIN")), "{views:?}");
    assert!(views.iter().any(|(n, _)| n == "v_simple"), "{views:?}");
    let trgs = db.list_triggers().unwrap();
    assert!(
        trgs.iter().any(|(n, tbl, sql)| n == "trigger_1" && tbl == "t1" && sql.contains("UPDATE")),
        "{trgs:?}"
    );

    // Simple view is queryable (flatten path).
    db.query("INSERT INTO t1 (id, s1, t1_i1, i2) VALUES (1, 'a', 10, 20)", &[])
        .unwrap();
    let rows = match db.query("SELECT id FROM v_simple", &[]).unwrap() {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        o => panic!("{o:?}"),
    };
    assert_eq!(rows, vec![vec![Value::Int(1)]]);

    // ROLLBACK undoes in-txn view/trigger.
    let mut s = db.begin().unwrap();
    s.query("CREATE VIEW v_tmp AS SELECT id FROM t1", &[]).unwrap();
    s.rollback();
    assert!(!db.list_views().unwrap().iter().any(|(n, _)| n == "v_tmp"));

    drop(db);
    let _ = std::fs::remove_file(path);
}
