#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    oracle_source::{OracleSource, OracleWriteOrigin, OracleWriteRequest},
    sourcecfg, state, NetOp, NetOpKind, SourceAdapter, SourceSpec,
};
use mpedb_types::Value;
#[test]
#[ignore = "requires dedicated native MPEDB_ORACLE_TEST owner and Oracle client"]
fn native_origin_only_mutations_cannot_be_skipped_by_watermarks_or_reused_as_own_proof() {
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
    assert_eq!(username, "MPEDB_ORACLE_TEST");
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    for table in ["MPEDB_ORIGIN_CURSOR", "MPEDB_ORIGIN_CURSOR_SAVE"] {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"),&[]).unwrap();
    }
    c.execute(
        "CREATE TABLE MPEDB_ORIGIN_CURSOR(id NUMBER PRIMARY KEY,v NUMBER)",
        &[],
    )
    .unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let origins = source.prepare_origin_ledger().unwrap();
    let ledger = source.prepare_write_ledger().unwrap();
    let path = mpedb_testkit::scratch_base()
        .join(format!("oracle-origin-cursor-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let options = mpedb_mirror::ImportOptions {
        include: Some(vec!["MPEDB_ORIGIN_CURSOR".into()]),
        size_bytes: 32 * 1024 * 1024,
        ..Default::default()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (db, _) = loop {
        match mpedb_mirror::import_oracle(&mut source, &path, &options) {
            Ok(value) => break value,
            Err(e)
                if e.to_string().contains("ORA-01466") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => panic!("import failed: {e}"),
        }
    };
    let metadata = mpedb_mirror::load_oracle_bootstrap(&db).unwrap();
    let file = mpedb_mirror::oracle_import::load_file_incarnation(&db).unwrap();
    assert_eq!(metadata.origin_ledger, Some(origins.clone()));
    let request = OracleWriteRequest {
        mirror_id: &metadata.mirror_id,
        token: [61; 32],
        epoch: 1,
        authority: "source",
        state_identity: &metadata.state_identity,
        ledger_identity: &ledger,
        origin: Some(OracleWriteOrigin {
            file_incarnation: file,
            ledger_identity: &origins,
        }),
    };
    let op = NetOp {
        table_id: 0,
        pk: vec![Value::Numeric("1".into())],
        kind: NetOpKind::Upsert(vec![
            Value::Numeric("1".into()),
            Value::Numeric("10".into()),
        ]),
    };
    source
        .write_batch_receipted(&metadata.tables, &[op], &request)
        .unwrap();
    let mut adapter =
        mpedb_mirror::OracleAdapter::from_database(OracleSource::connect(&spec).unwrap(), &db)
            .unwrap();
    let from = db
        .sys_record_get(state::MIR_NS, state::KEY_CUR)
        .unwrap()
        .unwrap();
    let batch = adapter.pull_provenanced(&from, 100).unwrap().unwrap();
    assert!(batch.origins.as_ref().unwrap()[0].is_some());
    // No local ACK knowledge: a restored/unknown source request is applied,
    // despite carrying this FILE identity. Ownership alone is not an echo.
    let applied = mpedb_mirror::apply_provenanced_batch(&db, &from, &batch).unwrap();
    assert_eq!(applied.echoes, 0);
    assert_eq!(applied.upserts, 1);
    let from = db
        .sys_record_get(state::MIR_NS, state::KEY_CUR)
        .unwrap()
        .unwrap();
    c.execute(
        "CREATE TABLE MPEDB_ORIGIN_CURSOR_SAVE AS SELECT * FROM MPEDB_WRITE_ORIGINS WHERE 1=0",
        &[],
    )
    .unwrap();
    let key = metadata.mirror_id.as_bytes().to_vec();
    c.execute(
        "INSERT INTO MPEDB_ORIGIN_CURSOR_SAVE SELECT * FROM MPEDB_WRITE_ORIGINS WHERE mirror_id=:1",
        &[&key],
    )
    .unwrap();
    c.commit().unwrap();
    c.execute(
        "DELETE FROM MPEDB_WRITE_ORIGINS WHERE mirror_id=:1",
        &[&key],
    )
    .unwrap();
    c.commit().unwrap();
    let error = adapter.pull_provenanced(&from, 100).unwrap_err();
    assert!(error.to_string().contains("mutated or expired"), "{error}");
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_CUR).unwrap(),
        Some(from.clone())
    );
    // Reinsert the SAME request id in a later foreign transaction. Looking at
    // only the app commit's SCN would miss the earlier deletion and falsely
    // attest this transaction using an old locally ACKable request identity.
    c.execute(
        "INSERT INTO MPEDB_WRITE_ORIGINS SELECT * FROM MPEDB_ORIGIN_CURSOR_SAVE",
        &[],
    )
    .unwrap();
    c.execute("UPDATE MPEDB_ORIGIN_CURSOR SET v=999 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    let error = adapter.pull_provenanced(&from, 100).unwrap_err();
    assert!(error.to_string().contains("mutated or expired"), "{error}");
    assert_eq!(
        db.sys_record_get(state::MIR_NS, state::KEY_CUR).unwrap(),
        Some(from)
    );
    for table in [
        "MPEDB_WRITE_ORIGINS",
        "MPEDB_WRITE_RECEIPTS",
        "MPEDB_MIRROR_STATE",
    ] {
        c.execute(&format!("DELETE FROM {table} WHERE mirror_id=:1"), &[&key])
            .unwrap();
    }
    c.commit().unwrap();
    drop(adapter);
    drop(db);
    std::fs::remove_file(&path).unwrap();
    for table in ["MPEDB_ORIGIN_CURSOR_SAVE", "MPEDB_ORIGIN_CURSOR"] {
        c.execute(&format!("DROP TABLE {table} PURGE"), &[])
            .unwrap();
    }
}
