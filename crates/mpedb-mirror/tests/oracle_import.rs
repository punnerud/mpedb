#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    oracle_import::{import_oracle, load_bootstrap},
    oracle_source::OracleSource,
    sourcecfg, state, ImportOptions, SourceSpec,
};
use mpedb_types::Value;
#[test]
#[ignore = "requires MPEDB_ORACLE_SOURCE_CONFIG and Oracle client libraries"]
fn native_oracle_import_reopens_with_verified_chunked_provenance_and_cursor() {
    let config = std::env::var("MPEDB_ORACLE_SOURCE_CONFIG").unwrap();
    let spec = sourcecfg::load(std::path::Path::new(&config)).unwrap();
    let SourceSpec::Oracle {
        connect_string,
        username,
        password,
    } = &spec
    else {
        panic!("Oracle required")
    };
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_IMPORT_CASE PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;", &[]).unwrap();
    let payload = "x".repeat(3000);
    let extra = (0..20)
        .map(|i| format!("c{i} VARCHAR2(4000) DEFAULT '{payload}'"))
        .collect::<Vec<_>>()
        .join(",");
    c.execute(
        &format!("CREATE TABLE MPEDB_IMPORT_CASE(id NUMBER PRIMARY KEY, flag BOOLEAN, {extra})"),
        &[],
    )
    .unwrap();
    let names = (0..20)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(",");
    c.execute(
        &format!(
            "INSERT INTO MPEDB_IMPORT_CASE(id,flag,{names}) VALUES(1,TRUE,{})",
            vec!["NULL"; 20].join(",")
        ),
        &[],
    )
    .unwrap();
    c.commit().unwrap();
    let path = mpedb_testkit::scratch_base()
        .join(format!("oracle-native-import-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut source = OracleSource::connect(&spec).unwrap();
    let opts = ImportOptions {
        include: Some(vec!["MPEDB_IMPORT_CASE".into()]),
        batch_rows: 1,
        size_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (db, report) = loop {
        match import_oracle(&mut source, &path, &opts) {
            Ok(result) => break result,
            Err(e)
                if e.to_string().contains("ORA-01466")
                    && !path.exists()
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert_eq!(report.total_rows(), 1);
    let original = load_bootstrap(&db).unwrap();
    let file_incarnation = mpedb_mirror::oracle_import::load_file_incarnation(&db).unwrap();
    assert_eq!(
        original.tables[0].columns[2]
            .default_source
            .as_ref()
            .unwrap()
            .trim(),
        format!("'{payload}'")
    );
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_CUR)
            .unwrap()
            .unwrap(),
        original.cursor
    );
    assert!(db
        .sys_record_get(state::MIR_NS, b"ora/bootstrap/00000001")
        .unwrap()
        .is_some());
    assert!(import_oracle(&mut source, &path, &opts).is_err());
    db.create_function(
        mpedb::spellfn::SpellLang::PlSql,
        "CREATE FUNCTION MPEDB_IMPORT_FLIP(x BOOLEAN) RETURN BOOLEAN IS BEGIN RETURN NOT x; END;",
    )
    .unwrap();
    db.create_lens(
        "oracle_import_flip",
        "MPEDB_IMPORT_FLIP",
        "MPEDB_IMPORT_FLIP",
        mpedb::lens::LensClass::Bijective,
    )
    .unwrap();
    let run = db
        .rretl_apply("oracle_import_flip", "MPEDB_IMPORT_CASE", "FLAG")
        .unwrap();
    assert_eq!(run.rows, 1);
    assert!(!db.rretl_log().unwrap().is_empty());
    assert_eq!(load_bootstrap(&db).unwrap(), original);
    db.rretl_revert(run.run_id).unwrap();

    drop(db);
    let db = mpedb::Database::open_from_file(&path).unwrap();
    assert_eq!(
        mpedb_mirror::oracle_import::load_file_incarnation(&db).unwrap(),
        file_incarnation
    );
    assert_eq!(load_bootstrap(&db).unwrap(), original);
    let mut adapter =
        mpedb_mirror::OracleAdapter::from_database(OracleSource::connect(&spec).unwrap(), &db)
            .unwrap();
    assert_eq!(
        mpedb_mirror::SourceAdapter::read_table_rows(&mut adapter, 0)
            .unwrap()
            .len(),
        1
    );
    drop(adapter);
    assert!(
        matches!(db.query("SELECT \"ID\",\"FLAG\" FROM \"MPEDB_IMPORT_CASE\"", &[]).unwrap(), mpedb::ExecResult::Rows{rows,..} if rows == vec![vec![Value::Numeric("1".into()),Value::Bool(true)]])
    );
    db.query(
        "ALTER TABLE \"MPEDB_IMPORT_CASE\" ADD COLUMN \"LOCAL_EXTRA\" TEXT",
        &[],
    )
    .unwrap();
    assert!(load_bootstrap(&db)
        .unwrap_err()
        .to_string()
        .contains("does not match its MPEdb carrier"));
    let mut damaged = db
        .sys_record_get(state::MIR_NS, b"ora/bootstrap/00000001")
        .unwrap()
        .unwrap();
    damaged[0] ^= 1;
    let mut s = db.begin().unwrap();
    s.sys_record_put(state::MIR_NS, b"ora/bootstrap/00000001", &damaged)
        .unwrap();
    s.commit().unwrap();
    assert!(load_bootstrap(&db)
        .unwrap_err()
        .to_string()
        .contains("hash mismatch"));
    let mut s = db.begin().unwrap();
    s.sys_record_put(state::MIR_NS, b"ora/bootstrap/00000001", b"corrupt")
        .unwrap();
    s.commit().unwrap();
    assert!(load_bootstrap(&db).is_err());
    drop(db);
    std::fs::remove_file(&path).unwrap();
    // Reusing a destination path must never reuse outbound request identity.
    let (fresh, _) = import_oracle(&mut source, &path, &opts).unwrap();
    assert_ne!(
        mpedb_mirror::oracle_import::load_file_incarnation(&fresh).unwrap(),
        file_incarnation
    );
    assert_eq!(
        load_bootstrap(&fresh).unwrap().mirror_id,
        original.mirror_id
    );
    drop(fresh);
    std::fs::remove_file(&path).unwrap();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&original.mirror_id.as_bytes()],
    )
    .unwrap();
    c.commit().unwrap();
    c.execute("DROP TABLE MPEDB_IMPORT_CASE PURGE", &[])
        .unwrap();
}
