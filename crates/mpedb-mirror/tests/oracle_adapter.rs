#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    apply_batch, import_oracle, load_oracle_bootstrap, oracle_source::OracleSource, sourcecfg,
    state, ImportOptions, OracleAdapter, SourceAdapter, SourceSpec,
};
use mpedb_types::Value;
fn rows(db: &mpedb::Database, sql: &str) -> Vec<Vec<Value>> {
    match db.query(sql, &[]).unwrap() {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        _ => panic!("rows required"),
    }
}
#[test]
#[ignore = "requires MPEDB_ORACLE_SOURCE_CONFIG and Oracle client libraries"]
fn native_oracle_adapter_applies_commits_resumes_and_fences_stale_state() {
    let path = std::env::var("MPEDB_ORACLE_SOURCE_CONFIG").unwrap();
    let spec = sourcecfg::load(std::path::Path::new(&path)).unwrap();
    let SourceSpec::Oracle {
        connect_string,
        username,
        password,
    } = &spec
    else {
        panic!("Oracle required")
    };
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    for name in ["MPEDB_ADAPTER_A", "MPEDB_ADAPTER_B", "MPEDB_ADAPTER_C"] {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {name} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"),&[]).unwrap();
    }
    c.execute("CREATE TABLE MPEDB_ADAPTER_A(id NUMBER, part VARCHAR2(10 CHAR), text_value CLOB, PRIMARY KEY(id,part))", &[]).unwrap();
    c.execute(
        "CREATE TABLE MPEDB_ADAPTER_B(id NUMBER PRIMARY KEY, amount NUMBER)",
        &[],
    )
    .unwrap();
    c.execute("CREATE TABLE MPEDB_ADAPTER_C(id NUMBER PRIMARY KEY)", &[])
        .unwrap();
    c.execute(
        "INSERT INTO MPEDB_ADAPTER_A VALUES(1,'ø',TO_CLOB('initial'))",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_ADAPTER_B VALUES(1,0)", &[])
        .unwrap();
    c.commit().unwrap();
    let dest =
        mpedb_testkit::scratch_base().join(format!("oracle-adapter-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&dest);
    let mut source = OracleSource::connect(&spec).unwrap();
    let opts = ImportOptions {
        include: Some(vec![
            "MPEDB_ADAPTER_B".into(),
            "MPEDB_ADAPTER_A".into(),
            "MPEDB_ADAPTER_C".into(),
        ]),
        size_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (db, _) = loop {
        match import_oracle(&mut source, &dest, &opts) {
            Ok(v) => break v,
            Err(e)
                if e.to_string().contains("ORA-01466")
                    && !dest.exists()
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => panic!("{e}"),
        }
    };
    let metadata = load_oracle_bootstrap(&db).unwrap();
    let mut adapter = OracleAdapter::from_database(source, &db).unwrap();
    let initial = adapter.zero_cursor();
    assert_eq!(initial, metadata.cursor);
    assert!(adapter.pull(&initial, 0).is_err());
    c.execute(
        "UPDATE MPEDB_ADAPTER_A SET part='🙂',text_value=TO_CLOB('new ø🙂') WHERE id=1",
        &[],
    )
    .unwrap();
    c.execute(
        "UPDATE MPEDB_ADAPTER_B SET amount=12345678901234567890.123456789 WHERE id=1",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_ADAPTER_C VALUES(1)", &[])
        .unwrap();
    c.commit().unwrap();
    assert!(adapter
        .pull(&initial, 1)
        .unwrap_err()
        .to_string()
        .contains("commit exceeds max_ops"));
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_CUR)
            .unwrap()
            .unwrap(),
        initial
    );
    let mut limited = OracleAdapter::from_database(OracleSource::connect(&spec).unwrap(), &db)
        .unwrap()
        .with_event_budget(2)
        .unwrap();
    match limited.pull(&initial, 16) {
        Ok(Some(batch)) => assert!(
            batch.ops.is_empty(),
            "a single commit cannot be partially captured"
        ),
        Err(e) => assert!(
            e.to_string().contains("capture event budget exceeded"),
            "{e}"
        ),
        Ok(None) => panic!("committed source changes exist"),
    }
    drop(limited);
    let first = adapter.pull(&initial, 16).unwrap().unwrap();
    assert_eq!(first.ops.len(), 4);
    assert_eq!(first.source_epoch, Some(1));
    assert_eq!(
        rows(&db, "SELECT \"PART\" FROM \"MPEDB_ADAPTER_A\""),
        vec![vec![Value::Text("ø".into())]]
    );
    assert!(apply_batch(&db, &first.end_cursor, &first).is_err());
    let stats = apply_batch(&db, &initial, &first).unwrap();
    assert_eq!((stats.upserts, stats.deletes), (3, 1));
    assert_eq!(
        rows(
            &db,
            "SELECT \"PART\",\"TEXT_VALUE\" FROM \"MPEDB_ADAPTER_A\""
        ),
        vec![vec![
            Value::Text("🙂".into()),
            Value::Text("new ø🙂".into())
        ]]
    );
    assert_eq!(
        rows(&db, "SELECT \"AMOUNT\" FROM \"MPEDB_ADAPTER_B\""),
        vec![vec![Value::Numeric(
            "12345678901234567890.123456789".into()
        )]]
    );
    assert_eq!(
        rows(&db, "SELECT * FROM \"MPEDB_ADAPTER_C\""),
        vec![vec![Value::Numeric("1".into())]]
    );
    assert!(adapter.push(&[]).is_err());
    assert!(adapter
        .cas_source_state(&metadata.mirror_id, 1, 2, "mpedb")
        .is_err());
    c.execute("DELETE FROM MPEDB_ADAPTER_A", &[]).unwrap();
    c.execute("INSERT INTO MPEDB_ADAPTER_B VALUES(2,7)", &[])
        .unwrap();
    c.commit().unwrap();
    drop(adapter);
    drop(db);
    let db = mpedb::Database::open_from_file(&dest).unwrap();
    let mut adapter =
        OracleAdapter::from_database(OracleSource::connect(&spec).unwrap(), &db).unwrap();
    let from = db
        .sys_record_get(state::MIR_NS, state::KEY_CUR)
        .unwrap()
        .unwrap();
    assert_eq!(from, first.end_cursor);
    let second = adapter.pull(&from, 16).unwrap().unwrap();
    assert_eq!(second.ops.len(), 2);
    apply_batch(&db, &from, &second).unwrap();
    assert!(rows(&db, "SELECT * FROM \"MPEDB_ADAPTER_A\"").is_empty());
    assert_eq!(adapter.read_table_rows(1).unwrap().len(), 2);
    assert!(adapter
        .cas_source_state(&metadata.mirror_id, 1, 2, "source")
        .unwrap());
    let fenced = adapter.pull(&second.end_cursor, 16).unwrap().unwrap();
    assert_eq!(fenced.source_epoch, Some(2));
    assert!(apply_batch(&db, &second.end_cursor, &fenced).is_err());
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_CUR)
            .unwrap()
            .unwrap(),
        second.end_cursor
    );
    c.execute("ALTER TABLE MPEDB_ADAPTER_B ADD more_value NUMBER", &[])
        .unwrap();
    assert!(adapter.pull(&second.end_cursor, 16).is_err());
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_CUR)
            .unwrap()
            .unwrap(),
        second.end_cursor
    );
    drop(adapter);
    drop(db);
    std::fs::remove_file(&dest).unwrap();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&metadata.mirror_id.as_bytes()],
    )
    .unwrap();
    c.commit().unwrap();
    for name in ["MPEDB_ADAPTER_A", "MPEDB_ADAPTER_B", "MPEDB_ADAPTER_C"] {
        c.execute(&format!("DROP TABLE {name} PURGE"), &[]).unwrap();
    }
}
