#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    oracle_cursor::OraclePosition,
    oracle_source::{OracleSource, OracleWriteRequest},
    sourcecfg, NetOp, NetOpKind, SourceSpec,
};
use mpedb_types::Value;
mod oracle_support;
fn upsert(id: &str, part: &str, amount: &str) -> NetOp {
    let row = vec![
        Value::Numeric(id.into()),
        Value::Text(part.into()),
        Value::Numeric(amount.into()),
    ];
    NetOp {
        table_id: 0,
        pk: row[..2].to_vec(),
        kind: NetOpKind::Upsert(row),
    }
}
#[test]
#[ignore = "requires dedicated MPEDB_ORACLE_FENCE test owner and private config"]
fn native_oracle_checked_writes_preserve_foreign_commits_and_fence_concurrent_dml() {
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
    let owned = [
        "MPEDB_CHECKED_CASE",
        "MPEDB_WRITE_RECEIPTS",
        "MPEDB_MIRROR_STATE",
    ];
    for table in owned {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"), &[]).unwrap();
    }
    c.execute("CREATE TABLE MPEDB_CHECKED_CASE(id NUMBER,part VARCHAR2(10 CHAR),amount NUMBER,PRIMARY KEY(id,part))", &[]).unwrap();
    c.execute(
        "INSERT INTO MPEDB_CHECKED_CASE VALUES(1,'a',1),(2,'b',2),(3,'c',3)",
        &[],
    )
    .unwrap();
    c.commit().unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let id = "native-checked-write";
    source.ensure_source_state(id, 1, "source").unwrap();
    let state = source.source_state_identity(id).unwrap();
    let ledger = source.prepare_write_ledger().unwrap();
    let tables = source.introspect(Some(&[owned[0].into()]), &[]).unwrap();
    let (scn, _) = oracle_support::initial_snapshot(&source, &tables);
    let identity = source.identity().unwrap();
    let from = OraclePosition::new(&identity, &tables, scn).unwrap();
    let request = OracleWriteRequest {
        mirror_id: id,
        token: [1; 32],
        epoch: 1,
        authority: "source",
        state_identity: &state,
        ledger_identity: &ledger,
    };
    c.execute("UPDATE MPEDB_CHECKED_CASE SET amount=99 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    let error = source
        .write_batch_receipted_checked(&tables, &[upsert("1", "a", "9")], &request, &from, 100)
        .unwrap_err();
    assert!(
        error.to_string().contains("conflicts with changes"),
        "{error}"
    );
    assert_eq!(
        c.query_row_as::<u64>("SELECT COUNT(*) FROM MPEDB_WRITE_RECEIPTS", &[])
            .unwrap(),
        0
    );
    assert_eq!(
        c.query_row_as::<u64>("SELECT amount FROM MPEDB_CHECKED_CASE WHERE id=1", &[])
            .unwrap(),
        99
    );
    // An unrelated changed key does not prevent a checked write.
    let request2 = OracleWriteRequest {
        token: [2; 32],
        ..request
    };
    let ops2 = [upsert("2", "b", "9")];
    let images = source
        .write_batch_receipted_checked(&tables, &ops2, &request2, &from, 100)
        .unwrap();
    c.execute("UPDATE MPEDB_CHECKED_CASE SET amount=88 WHERE id=2", &[])
        .unwrap();
    c.commit().unwrap();
    assert_eq!(
        source
            .write_batch_receipted_checked(&tables, &ops2, &request2, &from, 100)
            .unwrap(),
        images
    );
    assert_eq!(
        source
            .recover_write_receipt(&tables, &ops2, &request2)
            .unwrap(),
        Some(images)
    );
    assert_eq!(
        c.query_row_as::<u64>("SELECT amount FROM MPEDB_CHECKED_CASE WHERE id=2", &[])
            .unwrap(),
        88
    );
    // Any conflict rejects the whole new batch; clean targets stay untouched.
    let request3 = OracleWriteRequest {
        token: [3; 32],
        ..request
    };
    assert!(source
        .write_batch_receipted_checked(
            &tables,
            &[upsert("3", "c", "7"), upsert("1", "a", "7")],
            &request3,
            &from,
            100
        )
        .unwrap_err()
        .to_string()
        .contains("conflicts with changes"));
    assert_eq!(
        c.query_row_as::<u64>("SELECT amount FROM MPEDB_CHECKED_CASE WHERE id=3", &[])
            .unwrap(),
        3
    );
    assert!(source
        .write_batch_receipted_checked(&tables, &[upsert("3", "c", "7")], &request3, &from, 1)
        .unwrap_err()
        .to_string()
        .contains("event budget exceeded"));
    let future = OraclePosition::new(&identity, &tables, u64::MAX).unwrap();
    assert!(source
        .write_batch_receipted_checked(&tables, &ops2, &request2, &future, 100)
        .is_err());
    let mut other_tables = tables.clone();
    other_tables[0].object_id += 1;
    let wrong = OraclePosition::new(&identity, &other_tables, scn).unwrap();
    assert!(source
        .write_batch_receipted_checked(&tables, &ops2, &request2, &wrong, 100)
        .is_err());
    // An uncommitted external writer holds RX: our SRX NOWAIT must fail.
    let fresh = OraclePosition::new(&identity, &tables, source.current_scn().unwrap()).unwrap();
    c.execute("UPDATE MPEDB_CHECKED_CASE SET amount=33 WHERE id=3", &[])
        .unwrap();
    let request4 = OracleWriteRequest {
        token: [4; 32],
        ..request
    };
    let error = source
        .write_batch_receipted_checked(&tables, &[upsert("3", "c", "6")], &request4, &fresh, 100)
        .unwrap_err();
    assert!(
        error.to_string().contains("checked write DML fence"),
        "{error}"
    );
    c.rollback().unwrap();
    assert!(source
        .recover_write_receipt(&tables, &[upsert("3", "c", "6")], &request4)
        .unwrap()
        .is_none());
    source
        .write_batch_receipted_checked(&tables, &[upsert("3", "c", "6")], &request4, &fresh, 100)
        .unwrap();
    assert_eq!(
        c.query_row_as::<u64>("SELECT amount FROM MPEDB_CHECKED_CASE WHERE id=3", &[])
            .unwrap(),
        6
    );
    // A moved compound key conflicts for BOTH its vanished and new identities.
    let fresh = OraclePosition::new(&identity, &tables, source.current_scn().unwrap()).unwrap();
    c.execute("UPDATE MPEDB_CHECKED_CASE SET part='z' WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    let request5 = OracleWriteRequest {
        token: [5; 32],
        ..request
    };
    for part in ["a", "z"] {
        let op = NetOp {
            table_id: 0,
            pk: vec![Value::Numeric("1".into()), Value::Text(part.into())],
            kind: NetOpKind::Delete,
        };
        assert!(source
            .write_batch_receipted_checked(&tables, &[op], &request5, &fresh, 100)
            .unwrap_err()
            .to_string()
            .contains("conflicts with changes"));
    }
    assert_eq!(
        c.query_row_as::<u64>("SELECT COUNT(*) FROM MPEDB_WRITE_RECEIPTS", &[])
            .unwrap(),
        2
    );
    assert!(source
        .cas_source_state_fenced(id, 1, 2, "source", &state)
        .unwrap());
    assert!(source
        .write_batch_receipted_checked(&tables, &ops2, &request2, &from, 100)
        .unwrap_err()
        .to_string()
        .contains("fenced by authority"));
    assert!(source
        .recover_write_receipt(&tables, &ops2, &request2)
        .unwrap()
        .is_some());
    for table in owned {
        c.execute(&format!("DROP TABLE {table} PURGE"), &[])
            .unwrap();
    }
}
