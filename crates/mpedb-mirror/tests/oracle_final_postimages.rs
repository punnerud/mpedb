#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    oracle_source::{OracleSource, OracleWriteRequest},
    sourcecfg, NetOp, NetOpKind, SourceSpec,
};
use mpedb_types::Value;
fn op(table_id: u32, v: &str) -> NetOp {
    NetOp {
        table_id,
        pk: vec![Value::Numeric("1".into())],
        kind: NetOpKind::Upsert(vec![Value::Numeric("1".into()), Value::Numeric(v.into())]),
    }
}
#[test]
#[ignore = "requires native Oracle application test owner with CREATE TRIGGER"]
fn native_later_triggers_cannot_invalidate_earlier_postimages_or_create_false_receipts() {
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
    for table in ["MPEDB_BATCH_FINAL_B", "MPEDB_BATCH_FINAL_A"] {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"),&[]).unwrap();
    }
    c.execute(
        "CREATE TABLE MPEDB_BATCH_FINAL_A(id NUMBER PRIMARY KEY,v NUMBER)",
        &[],
    )
    .unwrap();
    c.execute(
        "CREATE TABLE MPEDB_BATCH_FINAL_B(id NUMBER PRIMARY KEY,v NUMBER)",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_BATCH_FINAL_A VALUES(1,1)", &[])
        .unwrap();
    c.execute("INSERT INTO MPEDB_BATCH_FINAL_B VALUES(1,2)", &[])
        .unwrap();
    c.commit().unwrap();
    c.execute("CREATE OR REPLACE TRIGGER MPEDB_BATCH_FINAL_TRIGGER AFTER INSERT OR UPDATE ON MPEDB_BATCH_FINAL_B FOR EACH ROW BEGIN UPDATE MPEDB_BATCH_FINAL_A SET v=999 WHERE id=:NEW.id; END;",&[]).unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(
            Some(&["MPEDB_BATCH_FINAL_B".into(), "MPEDB_BATCH_FINAL_A".into()]),
            &[],
        )
        .unwrap();
    let error = source
        .write_batch(&tables, &[op(0, "10"), op(1, "20")])
        .unwrap_err();
    assert!(error.to_string().contains("final batch"), "{error}");
    assert_eq!(
        c.query_row_as::<u64>("SELECT v FROM MPEDB_BATCH_FINAL_A WHERE id=1", &[])
            .unwrap(),
        1
    );
    assert_eq!(
        c.query_row_as::<u64>("SELECT v FROM MPEDB_BATCH_FINAL_B WHERE id=1", &[])
            .unwrap(),
        2
    );
    let id = "native-final-postimages";
    source.ensure_source_state(id, 1, "source").unwrap();
    let ledger = source.prepare_write_ledger().unwrap();
    c.execute(
        "DELETE FROM MPEDB_WRITE_RECEIPTS WHERE mirror_id=:1",
        &[&id.as_bytes()],
    )
    .unwrap();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&id.as_bytes()],
    )
    .unwrap();
    c.commit().unwrap();
    source.ensure_source_state(id, 1, "source").unwrap();
    let state = source.source_state_identity(id).unwrap();
    let request = OracleWriteRequest {
        mirror_id: id,
        token: [21; 32],
        epoch: 1,
        authority: "source",
        state_identity: &state,
        ledger_identity: &ledger,
    };
    assert!(source
        .write_batch_receipted(&tables, &[op(0, "10"), op(1, "20")], &request)
        .unwrap_err()
        .to_string()
        .contains("final batch"));
    assert_eq!(
        c.query_row_as::<u64>(
            "SELECT COUNT(*) FROM MPEDB_WRITE_RECEIPTS WHERE mirror_id=:1",
            &[&id.as_bytes()]
        )
        .unwrap(),
        0
    );
    assert_eq!(
        c.query_row_as::<u64>("SELECT v FROM MPEDB_BATCH_FINAL_A WHERE id=1", &[])
            .unwrap(),
        1
    );
    c.execute("CREATE OR REPLACE TRIGGER MPEDB_BATCH_FINAL_TRIGGER AFTER INSERT OR UPDATE ON MPEDB_BATCH_FINAL_B FOR EACH ROW BEGIN INSERT INTO MPEDB_BATCH_FINAL_A(id,v) VALUES(:NEW.id,777); END;",&[]).unwrap();
    let deleting = NetOp {
        table_id: 0,
        pk: vec![Value::Numeric("1".into())],
        kind: NetOpKind::Delete,
    };
    assert!(source
        .write_batch(&tables, &[deleting, op(1, "20")])
        .unwrap_err()
        .to_string()
        .contains("final batch"));
    assert_eq!(
        c.query_row_as::<u64>("SELECT v FROM MPEDB_BATCH_FINAL_A WHERE id=1", &[])
            .unwrap(),
        1
    );
    assert_eq!(
        c.query_row_as::<u64>("SELECT v FROM MPEDB_BATCH_FINAL_B WHERE id=1", &[])
            .unwrap(),
        2
    );
    c.execute("DROP TRIGGER MPEDB_BATCH_FINAL_TRIGGER", &[])
        .unwrap();
    let images = source
        .write_batch_receipted(&tables, &[op(0, "10"), op(1, "20")], &request)
        .unwrap();
    assert_eq!(
        images,
        vec![
            Some(vec![
                Value::Numeric("1".into()),
                Value::Numeric("10".into())
            ]),
            Some(vec![
                Value::Numeric("1".into()),
                Value::Numeric("20".into())
            ])
        ]
    );
    c.execute(
        "DELETE FROM MPEDB_WRITE_RECEIPTS WHERE mirror_id=:1",
        &[&id.as_bytes()],
    )
    .unwrap();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&id.as_bytes()],
    )
    .unwrap();
    c.commit().unwrap();
    for table in ["MPEDB_BATCH_FINAL_B", "MPEDB_BATCH_FINAL_A"] {
        c.execute(&format!("DROP TABLE {table} PURGE"), &[])
            .unwrap();
    }
}
