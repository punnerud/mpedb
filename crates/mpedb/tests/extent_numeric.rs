//! Exact numeric bodies must count toward extent lengths, including numeric
//! values in ANY columns. Found by importing Oracle NUMBER with large LOBs.
use mpedb::{Config, Database, ExecResult, Value};
#[test]
fn numeric_and_any_numeric_survive_extent_and_overflow_reopen() {
    for (external, threshold) in [(false, 1), (true, 1), (false, 0)] {
        let dir = mpedb_testkit::scratch_base().join(format!(
            "numeric-extent-{}-{external}-{threshold}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.mpedb");
        let config = Config::from_toml_str(&format!(
            r#"
[database]
path = "{}"
size_mb = 64
max_readers = 8
extent_threshold_kb = {threshold}
external_extents = {external}
[[table]]
name = "t"
primary_key = ["id"]
[[table.column]]
name = "id"
type = "int64"
[[table.column]]
name = "amount"
type = "numeric"
[[table.column]]
name = "dynamic"
type = "any"
[[table.column]]
name = "text_value"
type = "text"
[[table.column]]
name = "binary_value"
type = "blob"
"#,
            path.display()
        ))
        .unwrap();
        let db = Database::open_with_config(config.clone()).unwrap();
        let values = vec![
            Value::Int(1),
            Value::Numeric("12345678901234567890.1234567890123456789".into()),
            Value::Numeric("12.5".into()),
            Value::Text("ø🙂".repeat(60000)),
            Value::Blob((0..200001).map(|i| (i % 256) as u8).collect()),
        ];
        let mut s = db.begin().unwrap();
        s.insert_row(0, &values).unwrap();
        s.commit().unwrap();
        assert!(
            matches!(db.query("SELECT * FROM t",&[]).unwrap(),ExecResult::Rows{rows,..} if rows==vec![values.clone()])
        );
        db.verify().unwrap();
        drop(db);
        let db = Database::open_with_config(config).unwrap();
        assert!(
            matches!(db.query("SELECT * FROM t",&[]).unwrap(),ExecResult::Rows{rows,..} if rows==vec![values.clone()])
        );
        db.verify().unwrap();
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
