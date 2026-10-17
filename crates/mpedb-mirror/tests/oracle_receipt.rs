#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    oracle_source::{OracleSource, OracleWriteRequest},
    sourcecfg, NetOp, NetOpKind, SourceSpec,
};
use mpedb_types::Value;
#[test]
#[ignore = "requires dedicated MPEDB_ORACLE_FENCE test owner and private config"]
fn native_oracle_receipts_retry_without_reapplying_and_rollback_failed_batches() {
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
    for table in [
        "MPEDB_RECEIPT_CASE",
        "MPEDB_WRITE_RECEIPTS",
        "MPEDB_MIRROR_STATE",
    ] {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"),&[]).unwrap();
    }
    c.execute("CREATE TABLE MPEDB_RECEIPT_CASE(id NUMBER PRIMARY KEY,amount NUMBER(4,1),text_value CLOB,binary_value BLOB,generated_value NUMBER GENERATED ALWAYS AS(id+1) VIRTUAL)",&[]).unwrap();
    c.execute("INSERT INTO MPEDB_RECEIPT_CASE(id,amount,text_value,binary_value) VALUES(1,1,EMPTY_CLOB(),EMPTY_BLOB())",&[]).unwrap();
    c.commit().unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let mirror_id = "native-receipt-owner";
    source.ensure_source_state(mirror_id, 1, "source").unwrap();
    let state = source.source_state_identity(mirror_id).unwrap();
    c.execute(
        "CREATE TABLE MPEDB_WRITE_RECEIPTS(business_value NUMBER)",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_WRITE_RECEIPTS VALUES(42)", &[])
        .unwrap();
    c.commit().unwrap();
    assert!(source
        .prepare_write_ledger()
        .unwrap_err()
        .to_string()
        .contains("incompatible table"));
    assert_eq!(
        c.query_row_as::<u64>("SELECT business_value FROM MPEDB_WRITE_RECEIPTS", &[])
            .unwrap(),
        42
    );
    c.execute("DROP TABLE MPEDB_WRITE_RECEIPTS PURGE", &[])
        .unwrap();
    let ledger = source.prepare_write_ledger().unwrap();
    assert_eq!(source.prepare_write_ledger().unwrap(), ledger);
    let tables = source
        .introspect(Some(&["MPEDB_RECEIPT_CASE".into()]), &[])
        .unwrap();
    let image = vec![
        Value::Numeric("1".into()),
        Value::Numeric("9.9".into()),
        Value::Text("ø🙂漢".repeat(10000)),
        Value::Blob((0..131073).map(|i| (i % 256) as u8).collect()),
        Value::Null,
    ];
    let op = |row: Vec<Value>| NetOp {
        table_id: 0,
        pk: vec![row[0].clone()],
        kind: NetOpKind::Upsert(row),
    };
    let request = OracleWriteRequest {
        mirror_id,
        token: [7; 32],
        epoch: 1,
        authority: "source",
        state_identity: &state,
        ledger_identity: &ledger,
    };
    let expected = source
        .write_batch_receipted(&tables, &[op(image.clone())], &request)
        .unwrap();
    assert_eq!(expected[0].as_ref().unwrap()[4], Value::Numeric("2".into()));
    c.execute("UPDATE MPEDB_RECEIPT_CASE SET amount=77.7 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    drop(source);
    let mut source = OracleSource::connect(&spec).unwrap();
    assert_eq!(
        source
            .write_batch_receipted(&tables, &[op(image.clone())], &request)
            .unwrap(),
        expected
    );
    assert_eq!(
        c.query_row_as::<String>(
            "SELECT TO_CHAR(amount,'TM9') FROM MPEDB_RECEIPT_CASE WHERE id=1",
            &[]
        )
        .unwrap(),
        "77.7"
    );
    let mut changed = image.clone();
    changed[1] = Value::Numeric("8.8".into());
    assert!(source
        .write_batch_receipted(&tables, &[op(changed.clone())], &request)
        .unwrap_err()
        .to_string()
        .contains("another payload"));
    c.execute("UPDATE MPEDB_WRITE_RECEIPTS SET result_bytes=TO_BLOB(HEXTORAW('00')) WHERE mirror_id=:1 AND request_token=:2",&[&mirror_id.as_bytes(),&vec![7u8;32]]).unwrap();
    c.commit().unwrap();
    assert!(source
        .write_batch_receipted(&tables, &[op(image.clone())], &request)
        .unwrap_err()
        .to_string()
        .contains("receipt hash"));
    let mut forged = vec![1, 0, 0, 0, 1, 0, 0, 0, 0, 1];
    forged.extend_from_slice(&u64::MAX.to_be_bytes());
    let forged_hash = blake3::hash(&forged).as_bytes().to_vec();
    c.execute("UPDATE MPEDB_WRITE_RECEIPTS SET result_bytes=TO_BLOB(:1),result_hash=:2 WHERE mirror_id=:3 AND request_token=:4", &[&forged,&forged_hash,&mirror_id.as_bytes(),&vec![7u8;32]]).unwrap();
    c.commit().unwrap();
    assert!(source
        .write_batch_receipted(&tables, &[op(image.clone())], &request)
        .unwrap_err()
        .to_string()
        .contains("frame exceeds bounds"));
    let mut second = changed.clone();
    second[0] = Value::Numeric("2".into());
    second[1] = Value::Numeric("99999.9".into());
    let request2 = OracleWriteRequest {
        token: [8; 32],
        ..request
    };
    assert!(source
        .write_batch_receipted(
            &tables,
            &[op(changed.clone()), op(second.clone())],
            &request2
        )
        .is_err());
    assert_eq!(
        c.query_row_as::<String>(
            "SELECT TO_CHAR(amount,'TM9') FROM MPEDB_RECEIPT_CASE WHERE id=1",
            &[]
        )
        .unwrap(),
        "77.7"
    );
    assert_eq!(
        c.query_row_as::<u64>(
            "SELECT COUNT(*) FROM MPEDB_WRITE_RECEIPTS WHERE request_token=:1",
            &[&vec![8u8; 32]]
        )
        .unwrap(),
        0
    );
    second[1] = Value::Numeric("4.4".into());
    let result = source
        .write_batch_receipted(&tables, &[op(changed.clone()), op(second)], &request2)
        .unwrap();
    assert_eq!(result[1].as_ref().unwrap()[4], Value::Numeric("3".into()));
    assert!(source
        .cas_source_state_fenced(mirror_id, 1, 2, "source", &state)
        .unwrap());
    let request3 = OracleWriteRequest {
        token: [9; 32],
        ..request2
    };
    assert!(source
        .write_batch_receipted(&tables, &[op(changed.clone())], &request3)
        .unwrap_err()
        .to_string()
        .contains("fenced by authority"));
    let request3 = OracleWriteRequest {
        epoch: 2,
        ..request3
    };
    source
        .write_batch_receipted(&tables, &[op(changed.clone())], &request3)
        .unwrap();
    let mut third = image.clone();
    third[0] = Value::Numeric("3".into());
    third[1] = Value::Numeric("5.5".into());
    third[2] = Value::Text("concurrent".into());
    third[3] = Value::Blob(vec![1, 2]);
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let jobs = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut connection = OracleSource::connect(&spec).unwrap();
                    let request = OracleWriteRequest {
                        mirror_id,
                        token: [10; 32],
                        epoch: 2,
                        authority: "source",
                        state_identity: &state,
                        ledger_identity: &ledger,
                    };
                    barrier.wait();
                    connection.write_batch_receipted(&tables, &[op(third.clone())], &request)
                })
            })
            .collect::<Vec<_>>();
        jobs.into_iter()
            .map(|j| j.join().unwrap().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results[0], results[1]);
    assert_eq!(
        results[0][0].as_ref().unwrap()[4],
        Value::Numeric("4".into())
    );
    assert_eq!(
        c.query_row_as::<u64>(
            "SELECT COUNT(*) FROM MPEDB_WRITE_RECEIPTS WHERE request_token=:1",
            &[&vec![10u8; 32]]
        )
        .unwrap(),
        1
    );
    let dest = mpedb_testkit::scratch_base().join(format!(
        "oracle-receipt-import-{}.mpedb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&dest);
    let options = mpedb_mirror::ImportOptions {
        size_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };
    let (db, report) = mpedb_mirror::import_oracle(&mut source, &dest, &options).unwrap();
    assert_eq!(report.tables.len(), 1);
    assert_eq!(report.tables[0].name, "MPEDB_RECEIPT_CASE");
    assert_eq!(report.total_rows(), 3);
    assert_eq!(
        mpedb_mirror::load_oracle_bootstrap(&db)
            .unwrap()
            .tables
            .len(),
        1
    );
    drop(db);
    std::fs::remove_file(&dest).unwrap();
    c.execute(
        "ALTER TABLE MPEDB_RECEIPT_CASE MODIFY amount NUMBER(5,1)",
        &[],
    )
    .unwrap();
    assert!(source
        .write_batch_receipted(&tables, &[op(changed.clone())], &request3)
        .unwrap_err()
        .to_string()
        .contains("source schema changed"));
    c.execute("DROP TABLE MPEDB_WRITE_RECEIPTS PURGE", &[])
        .unwrap();
    let replacement = source.prepare_write_ledger().unwrap();
    assert_ne!(replacement.object_id, ledger.object_id);
    assert!(source
        .write_batch_receipted(&tables, &[op(changed)], &request3)
        .unwrap_err()
        .to_string()
        .contains("ledger changed"));
    for table in [
        "MPEDB_RECEIPT_CASE",
        "MPEDB_WRITE_RECEIPTS",
        "MPEDB_MIRROR_STATE",
    ] {
        c.execute(&format!("DROP TABLE {table} PURGE"), &[])
            .unwrap();
    }
}
