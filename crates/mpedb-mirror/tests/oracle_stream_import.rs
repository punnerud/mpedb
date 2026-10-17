#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    import_oracle, oracle_import::load_file_incarnation, oracle_source::OracleSource, sourcecfg,
    ImportOptions, SourceSpec,
};
#[test]
#[ignore = "requires dedicated MPEDB_ORACLE_FENCE test owner and private config"]
fn native_oracle_stream_import_batches_atomically_and_cleans_failed_stages() {
    let path = std::env::var("MPEDB_ORACLE_FENCE_SOURCE_CONFIG").unwrap();
    let spec = sourcecfg::load(std::path::Path::new(&path)).unwrap();
    let SourceSpec::Oracle {
        connect_string,
        username,
        password,
    } = &spec
    else {
        panic!("Oracle required")
    };
    assert_eq!(username, "MPEDB_ORACLE_FENCE");
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    for table in ["MPEDB_STREAM_A", "MPEDB_STREAM_B", "MPEDB_MIRROR_STATE"] {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"),&[]).unwrap();
    }
    c.execute(
        "CREATE TABLE MPEDB_STREAM_A(id NUMBER PRIMARY KEY,payload VARCHAR2(4000 BYTE))",
        &[],
    )
    .unwrap();
    c.execute(
        "CREATE TABLE MPEDB_STREAM_B(id NUMBER PRIMARY KEY,payload VARCHAR2(20 BYTE))",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_STREAM_A SELECT level,RPAD('x',4000,'x') FROM dual CONNECT BY level<=6000",&[]).unwrap();
    c.commit().unwrap();
    let dir =
        mpedb_testkit::scratch_base().join(format!("oracle-stream-import-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let bad = dir.join("small.mpedb");
    let good = dir.join("complete.mpedb");
    let options = ImportOptions {
        include: Some(vec!["MPEDB_STREAM_B".into(), "MPEDB_STREAM_A".into()]),
        batch_rows: 100000,
        size_bytes: 16 * 1024 * 1024,
        ..Default::default()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let error = loop {
        match import_oracle(&mut source, &bad, &options) {
            Ok(_) => panic!("24 MiB of data cannot fit a 16 MiB target"),
            Err(e)
                if e.to_string().contains("ORA-01466") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => break e,
        }
    };
    assert!(
        !bad.exists(),
        "failed row streaming must not publish a destination: {error}"
    );
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "owned stages must be removed"
    );
    let (db, report) = import_oracle(
        &mut source,
        &good,
        &ImportOptions {
            size_bytes: 64 * 1024 * 1024,
            ..options
        },
    )
    .unwrap();
    assert_eq!(report.total_rows(), 6000);
    assert_eq!(report.tables[0].rows, 6000);
    assert_eq!(report.tables[1].rows, 0);
    assert!(
        matches!(db.query("SELECT COUNT(*) FROM \"MPEDB_STREAM_A\"",&[]).unwrap(),mpedb::ExecResult::Rows{rows,..} if rows==vec![vec![mpedb_types::Value::Int(6000)]])
    );
    assert!(
        matches!(db.query("SELECT LENGTH(\"PAYLOAD\") FROM \"MPEDB_STREAM_A\" WHERE \"ID\"=$1",&[mpedb_types::Value::Numeric("6000".into())]).unwrap(),mpedb::ExecResult::Rows{rows,..} if rows==vec![vec![mpedb_types::Value::Int(4000)]])
    );
    let nonce = load_file_incarnation(&db).unwrap();
    assert_ne!(nonce, [0; 16]);
    db.verify().unwrap();
    drop(db);
    let db = mpedb::Database::open_from_file(&good).unwrap();
    assert_eq!(load_file_incarnation(&db).unwrap(), nonce);
    drop(db);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
    for table in ["MPEDB_STREAM_A", "MPEDB_STREAM_B", "MPEDB_MIRROR_STATE"] {
        c.execute(&format!("DROP TABLE {table} PURGE"), &[])
            .unwrap();
    }
}
