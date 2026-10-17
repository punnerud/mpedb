#![cfg(feature = "oracle-source")]
use mpedb_mirror::{oracle, oracle_source::OracleSource, sourcecfg, SourceSpec};
use mpedb_types::Value;

#[test]
#[ignore = "requires MPEDB_ORACLE_SOURCE_CONFIG and Oracle client libraries"]
fn native_oracle_snapshot_preserves_metadata_values_and_one_scn() {
    let path = std::env::var("MPEDB_ORACLE_SOURCE_CONFIG")
        .expect("explicit private source config required");
    let spec = sourcecfg::load(std::path::Path::new(&path)).unwrap();
    let SourceSpec::Oracle {
        connect_string,
        username,
        password,
    } = &spec
    else {
        panic!("Oracle config required")
    };
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_SNAPSHOT_CASE PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute("CREATE TABLE MPEDB_SNAPSHOT_CASE (id NUMBER(10), amount NUMBER, v VARCHAR2(10 CHAR) DEFAULT 'ø', raw_value RAW(4), date_value DATE, stamp TIMESTAMP(9), flag BOOLEAN, text_lob CLOB, binary_lob BLOB, generated_value NUMBER GENERATED ALWAYS AS (id+1) VIRTUAL, PRIMARY KEY (id,v))",&[]).unwrap();
    c.execute("INSERT INTO MPEDB_SNAPSHOT_CASE (id,amount,v,raw_value,date_value,stamp,flag,text_lob,binary_lob) VALUES (1,12345678901234567890.1234567890123456789,'ø',HEXTORAW('00ff'),TO_DATE('2026-10-17 03:04:05','YYYY-MM-DD HH24:MI:SS'),TIMESTAMP '2026-10-17 03:04:05.123456789',TRUE,TO_CLOB('text'),TO_BLOB(HEXTORAW('001122')))",&[]).unwrap();
    c.commit().unwrap();
    c.execute("CREATE OR REPLACE FUNCTION MPEDB_SOURCE_FLIP(x BOOLEAN) RETURN BOOLEAN IS BEGIN RETURN NOT x; END;",&[]).unwrap();
    assert_eq!(
        c.query_row_as::<i64>(
            "SELECT CASE WHEN MPEDB_SOURCE_FLIP(TRUE) THEN 1 ELSE 0 END FROM dual",
            &[]
        )
        .unwrap(),
        0
    );
    let source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(Some(&["MPEDB_SNAPSHOT_CASE".into()]), &[])
        .unwrap();
    assert_eq!(tables.len(), 1);
    let table = &tables[0];
    assert_eq!(table.pk, vec![0, 2]);
    assert!(table.ddl_source.contains("PRIMARY KEY"));
    let mut invalid = table.clone();
    invalid.pk = vec![usize::MAX];
    assert!(source
        .read_snapshot(&invalid, source.current_scn().unwrap())
        .is_err());
    assert_eq!(table.columns[0].declared_type(), "NUMBER(10,0)");
    assert_eq!(table.columns[2].declared_type(), "VARCHAR2(10 CHAR)");
    assert!(table.columns[2]
        .default_source
        .as_ref()
        .unwrap()
        .contains('ø'));
    assert!(table.columns[9].generated);
    let schema = oracle::build_schema(&tables).unwrap();
    let scn = source.current_scn().unwrap();
    let before = source.read_snapshot(table, scn).unwrap();
    assert_eq!(before.len(), 1);
    let row = &before[0];
    assert_eq!(row[0], Value::Numeric("1".into()));
    assert_eq!(
        row[1],
        Value::Numeric("12345678901234567890.1234567890123456789".into())
    );
    assert_eq!(row[2], Value::Text("ø".into()));
    assert_eq!(row[3], Value::Blob(vec![0, 255]));
    assert!(matches!(&row[4],Value::Text(v) if v.ends_with("2026-10-17T03:04:05")));
    assert!(matches!(&row[5],Value::Text(v) if v.ends_with("2026-10-17T03:04:05.123456789")));
    assert_eq!(row[6], Value::Bool(true));
    assert_eq!(row[7], Value::Text("text".into()));
    assert_eq!(row[8], Value::Blob(vec![0, 17, 34]));
    assert_eq!(row[9], Value::Numeric("2".into()));
    c.execute("UPDATE MPEDB_SNAPSHOT_CASE SET v='after' WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    assert_eq!(source.read_snapshot(table, scn).unwrap(), before);
    let current = source
        .read_snapshot(table, source.current_scn().unwrap())
        .unwrap();
    assert_eq!(current[0][2], Value::Text("after".into()));
    // Exercise the real carrier schema and typed row API, not just a mock.
    let db_path =
        mpedb_testkit::scratch_base().join(format!("oracle-snapshot-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&db_path);
    let mut config = mpedb_types::Config::from_toml_str(&format!(
        "[database]\npath = \"{}\"\nsize_mb = 32\ndurability = \"none\"\n",
        mpedb_testkit::toml_path(&db_path)
    ))
    .unwrap();
    config.schema = schema;
    let db = mpedb::Database::open_with_config(config).unwrap();
    let mut tx = db.begin().unwrap();
    tx.insert_row(0, row).unwrap();
    tx.commit().unwrap();
    let rows = match db
        .query("SELECT \"AMOUNT\",\"V\" FROM \"MPEDB_SNAPSHOT_CASE\"", &[])
        .unwrap()
    {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        _ => panic!("rows required"),
    };
    assert_eq!(rows, vec![vec![row[1].clone(), row[2].clone()]]);
    // Real Oracle values enter the existing rRETL engine and retain lineage.
    let routines = source.routines().unwrap();
    let routine = routines
        .iter()
        .find(|r| r.name == "MPEDB_SOURCE_FLIP" && r.kind == "FUNCTION")
        .unwrap();
    assert_eq!(routine.owner, source.owner());
    db.create_function(mpedb::spellfn::SpellLang::PlSql, &routine.create_source())
        .unwrap();
    db.create_lens(
        "oracle_flag",
        "MPEDB_SOURCE_FLIP",
        "MPEDB_SOURCE_FLIP",
        mpedb::lens::LensClass::Bijective,
    )
    .unwrap();
    let run = db
        .rretl_apply("oracle_flag", "MPEDB_SNAPSHOT_CASE", "FLAG")
        .unwrap();
    assert_eq!(run.rows, 1);
    assert!(!db.rretl_log().unwrap().is_empty());
    assert!(
        matches!(db.query("SELECT \"FLAG\" FROM \"MPEDB_SNAPSHOT_CASE\"", &[]).unwrap(), mpedb::ExecResult::Rows { rows, .. } if rows == vec![vec![Value::Bool(false)]])
    );
    db.rretl_revert(run.run_id).unwrap();
    assert!(
        matches!(db.query("SELECT \"FLAG\" FROM \"MPEDB_SNAPSHOT_CASE\"", &[]).unwrap(), mpedb::ExecResult::Rows { rows, .. } if rows == vec![vec![Value::Bool(true)]])
    );
    c.execute("DROP FUNCTION MPEDB_SOURCE_FLIP", &[]).unwrap();
    drop(db);
    let _ = std::fs::remove_file(db_path);
    c.execute("DROP TABLE MPEDB_SNAPSHOT_CASE PURGE", &[])
        .unwrap();
}
