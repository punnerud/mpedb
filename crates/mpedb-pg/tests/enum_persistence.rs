use mpedb::{Config, Database, ExecResult, Value};
use mpedb_types::Dialect;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn enum_definitions_and_column_validation_survive_reopen() {
    let path = std::path::PathBuf::from(mpedb_testkit::scratch_base_str()).join(format!(
        "pg-enum-{}-{}.mpedb",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let cfg = Config::from_toml_str(&format!(
        "[database]\npath={:?}\nsize_mb=16\n[compat]\ndialect=\"postgres\"\n",
        path.to_str().unwrap()
    ))
    .unwrap();
    let db = Database::open_with_config(cfg).unwrap();
    db.query("CREATE TYPE \"Mood\" AS ENUM ('z','a','réveillé')", &[])
        .unwrap();
    db.query(
        "CREATE TABLE moods (id int PRIMARY KEY, mood \"Mood\")",
        &[],
    )
    .unwrap();
    db.query("INSERT INTO moods VALUES (1,'a'),(2,'z'),(3,NULL)", &[])
        .unwrap();
    assert_eq!(db.schema().schema.canonical_bytes()[0], 22);
    let definitions = db.list_pg_enums().unwrap();
    drop(db);
    let mut reopened = Database::open_from_file(&path).unwrap();
    reopened.set_dialect(Dialect::Postgres);
    assert_eq!(reopened.list_pg_enums().unwrap(), definitions);
    let ExecResult::Rows { rows, .. } = reopened
        .query("SELECT mood FROM moods ORDER BY mood", &[])
        .unwrap()
    else {
        panic!("rows expected")
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("z".into())],
            vec![Value::Text("a".into())],
            vec![Value::Null]
        ]
    );
    assert!(reopened
        .query("UPDATE moods SET mood='unknown' WHERE id=1", &[])
        .is_err());
    assert!(reopened
        .query(
            "SELECT mood FROM moods WHERE mood=$1 AND id=999",
            &[Value::Text("unknown".into())]
        )
        .is_err());
    assert!(reopened
        .query(
            "UPDATE moods SET mood=$1 WHERE id=999",
            &[Value::Text("unknown".into())]
        )
        .is_err());
    let ExecResult::Rows { rows, .. } = reopened
        .query("SELECT mood FROM moods ORDER BY mood DESC", &[])
        .unwrap()
    else {
        panic!("rows expected")
    };
    assert_eq!(
        rows,
        vec![
            vec![Value::Null],
            vec![Value::Text("a".into())],
            vec![Value::Text("z".into())]
        ]
    );

    assert!(reopened.query("DROP TYPE \"Mood\"", &[]).is_err());
    assert!(reopened.query("SELECT mood FROM moods WHERE FALSE AND mood=$1", &[Value::Text("unknown".into())]).is_err());
    let peer = Database::open_from_file(&path).unwrap();
    assert_eq!(peer.list_pg_enums().unwrap(), definitions);
    drop(peer);
    reopened.query("DROP TABLE moods", &[]).unwrap();
    reopened.query("DROP TYPE \"Mood\"", &[]).unwrap();
    assert!(reopened.list_pg_enums().unwrap().is_empty());
    reopened
        .query("CREATE TYPE \"Mood\" AS ENUM ('new')", &[])
        .unwrap();
    assert_ne!(reopened.list_pg_enums().unwrap()[0].oid, definitions[0].oid);
    reopened.query("DROP TYPE \"Mood\"", &[]).unwrap();
    reopened.query("CREATE TYPE digit_label AS ENUM ('1')", &[]).unwrap();
    reopened.query("CREATE TABLE strict_enum (id int PRIMARY KEY, state digit_label)", &[]).unwrap();
    assert!(reopened.query("INSERT INTO strict_enum VALUES (1,1)", &[]).is_err());
    assert!(reopened.query("INSERT INTO strict_enum VALUES (1,$1)", &[Value::Int(1)]).is_err());
    reopened.query("INSERT INTO strict_enum VALUES (1,'1')", &[]).unwrap();
    drop(reopened);
    std::fs::remove_file(path).unwrap();
}
