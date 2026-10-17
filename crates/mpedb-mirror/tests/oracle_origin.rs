#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    oracle_source::{attest_write_origins, OracleSource, OracleWriteOrigin, OracleWriteRequest},
    sourcecfg, NetOp, NetOpKind, SourceSpec,
};
use mpedb_types::Value;
#[test]
#[ignore = "requires dedicated native MPEDB_ORACLE_TEST owner and Oracle client"]
fn native_oracle_origins_are_atomic_and_attest_only_explicit_requested_keys() {
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
    assert_eq!(username, "MPEDB_ORACLE_TEST");
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    for name in ["MPEDB_ORIGIN_A", "MPEDB_ORIGIN_B"] {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {name} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"),&[]).unwrap();
    }
    c.execute(
        "CREATE TABLE MPEDB_ORIGIN_A(id NUMBER PRIMARY KEY,v NUMBER CHECK(v>=0))",
        &[],
    )
    .unwrap();
    c.execute(
        "CREATE TABLE MPEDB_ORIGIN_B(id NUMBER PRIMARY KEY,v NUMBER)",
        &[],
    )
    .unwrap();
    c.execute("CREATE TRIGGER MPEDB_ORIGIN_SIDE AFTER INSERT OR UPDATE ON MPEDB_ORIGIN_A FOR EACH ROW BEGIN MERGE INTO MPEDB_ORIGIN_B b USING (SELECT :new.id id,:new.v v FROM dual) a ON(b.id=a.id) WHEN MATCHED THEN UPDATE SET b.v=a.v WHEN NOT MATCHED THEN INSERT(id,v) VALUES(a.id,a.v); END;",&[]).unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let ledger = source.prepare_write_ledger().unwrap();
    let origins = source.prepare_origin_ledger().unwrap();
    let mirror = format!("origin-{}", std::process::id());
    source.ensure_source_state(&mirror, 1, "source").unwrap();
    let state = source.source_state_identity(&mirror).unwrap();
    let tables = source
        .introspect(
            Some(&["MPEDB_ORIGIN_A".into(), "MPEDB_ORIGIN_B".into()]),
            &[],
        )
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let baseline = loop {
        match source.snapshot(&tables) {
            Ok(snapshot) => break snapshot.scn,
            Err(e)
                if e.to_string().contains("ORA-01466") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => panic!("snapshot failed: {e}"),
        }
    };
    let request = OracleWriteRequest {
        mirror_id: &mirror,
        token: [31; 32],
        epoch: 1,
        authority: "source",
        state_identity: &state,
        ledger_identity: &ledger,
        origin: Some(OracleWriteOrigin {
            file_incarnation: [11; 16],
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
    c.execute("CREATE TRIGGER MPEDB_ORIGIN_BAD_RECEIPT AFTER INSERT ON MPEDB_WRITE_RECEIPTS BEGIN NULL; END;",&[]).unwrap();
    let error = source
        .write_batch_receipted(&tables, std::slice::from_ref(&op), &request)
        .unwrap_err();
    assert!(error.to_string().contains("enabled triggers"), "{error}");
    c.execute("DROP TRIGGER MPEDB_ORIGIN_BAD_RECEIPT", &[])
        .unwrap();
    let images = source
        .write_batch_receipted(&tables, std::slice::from_ref(&op), &request)
        .unwrap();
    assert_eq!(
        images,
        vec![Some(vec![
            Value::Numeric("1".into()),
            Value::Numeric("10".into())
        ])]
    );
    let head = source.current_scn().unwrap();
    let batches = source.read_changes(&tables, baseline, head, 100).unwrap();
    let events = source
        .read_write_origins(&mirror, &origins, baseline, head, 100)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(batches.len(), 1);
    assert_eq!(events[0].scn, batches[0].scn);
    assert_eq!(vec![events[0].xid.clone()], batches[0].xids);
    assert_eq!(events[0].touches.len(), 1);
    let proofs = attest_write_origins(&tables, &batches[0], &events, [11; 16]).unwrap();
    assert_eq!(proofs.len(), 2);
    assert_eq!(proofs[0].as_ref().unwrap().request_ids, vec![[31; 32]]);
    assert!(
        proofs[1].is_none(),
        "same XID cannot hide unrequested trigger effects"
    );
    assert!(
        attest_write_origins(&tables, &batches[0], &events, [12; 16])
            .unwrap()
            .iter()
            .all(Option::is_none)
    );
    assert_eq!(
        source
            .recover_write_receipt(&tables, std::slice::from_ref(&op), &request)
            .unwrap(),
        Some(images.clone())
    );
    assert_eq!(
        source
            .write_batch_receipted(&tables, std::slice::from_ref(&op), &request)
            .unwrap(),
        images
    );
    assert_eq!(
        source
            .read_write_origins(
                &mirror,
                &origins,
                baseline,
                source.current_scn().unwrap(),
                100
            )
            .unwrap()
            .len(),
        1
    );
    // Equal-value foreign writes remain foreign, including their trigger effect.
    let from = source.current_scn().unwrap();
    c.execute("UPDATE MPEDB_ORIGIN_A SET v=10 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    let through = source.current_scn().unwrap();
    let foreign = source.read_changes(&tables, from, through, 100).unwrap();
    let own = source
        .read_write_origins(&mirror, &origins, from, through, 100)
        .unwrap();
    assert!(own.is_empty());
    assert_eq!(foreign.len(), 1);
    assert!(
        attest_write_origins(&tables, &foreign[0], &events, [11; 16])
            .unwrap()
            .iter()
            .all(Option::is_none)
    );
    // A failed later DML publishes neither an earlier data change nor an origin.
    let bad = NetOp {
        table_id: 0,
        pk: vec![Value::Numeric("2".into())],
        kind: NetOpKind::Upsert(vec![
            Value::Numeric("2".into()),
            Value::Numeric("-1".into()),
        ]),
    };
    let failed = OracleWriteRequest {
        token: [32; 32],
        ..request
    };
    assert!(source
        .write_batch_receipted(&tables, &[op.clone(), bad], &failed)
        .is_err());
    let key = mirror.as_bytes().to_vec();
    let count: u64 = c
        .query_row_as(
            "SELECT COUNT(*) FROM MPEDB_WRITE_ORIGINS WHERE mirror_id=:1",
            &[&key],
        )
        .unwrap();
    assert_eq!(count, 1);
    // Mutation is never interpreted as a fresh valid ownership assertion.
    let before = source.current_scn().unwrap();
    c.execute(
        "UPDATE MPEDB_WRITE_ORIGINS SET footprint_hash=footprint_hash WHERE mirror_id=:1",
        &[&key],
    )
    .unwrap();
    c.commit().unwrap();
    let after = source.current_scn().unwrap();
    assert!(source
        .read_write_origins(&mirror, &origins, before, after, 100)
        .unwrap_err()
        .to_string()
        .contains("mutated or expired"));
    // A receipt without its source-origin row cannot be ACKed or replayed.
    c.execute(
        "DELETE FROM MPEDB_WRITE_ORIGINS WHERE mirror_id=:1",
        &[&key],
    )
    .unwrap();
    c.commit().unwrap();
    let error = source
        .recover_write_receipt(&tables, std::slice::from_ref(&op), &request)
        .unwrap_err();
    assert!(error.to_string().contains("atomicity mismatch"), "{error}");
    let error = source
        .write_batch_receipted(&tables, std::slice::from_ref(&op), &request)
        .unwrap_err();
    assert!(
        error.to_string().contains("missing its committed origin"),
        "{error}"
    );
    c.execute(
        "DELETE FROM MPEDB_WRITE_RECEIPTS WHERE mirror_id=:1",
        &[&key],
    )
    .unwrap();
    c.execute("DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1", &[&key])
        .unwrap();
    c.commit().unwrap();
    for name in ["MPEDB_ORIGIN_A", "MPEDB_ORIGIN_B"] {
        c.execute(&format!("DROP TABLE {name} PURGE"), &[]).unwrap();
    }
}
