#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    oracle_source::{OracleSource, OracleWriteRequest},
    sourcecfg, NetOp, NetOpKind, SourceSpec,
};
use mpedb_types::Value;
// Isolated no-foreign-writer fixture for the common ACK bridge. This does not
// implement production conflict/echo policy and is never exposed as an adapter.
struct PostimageTransport<'a> {
    source: &'a mut OracleSource,
    tables: &'a [mpedb_mirror::oracle::OracleTable],
    metadata: &'a mpedb_mirror::OracleBootstrap,
    ledger: &'a mpedb_mirror::oracle_source::OracleLedgerIdentity,
    file_incarnation: [u8; 16],
    lose_reply_once: bool,
}
impl mpedb_mirror::SourceAdapter for PostimageTransport<'_> {
    fn pull(
        &mut self,
        _: &Vec<u8>,
        _: usize,
    ) -> mpedb_types::Result<Option<mpedb_mirror::PullBatch>> {
        Ok(None)
    }
    fn head(&mut self) -> mpedb_types::Result<Vec<u8>> {
        Ok(self.metadata.cursor.clone())
    }
    fn zero_cursor(&self) -> Vec<u8> {
        self.metadata.cursor.clone()
    }
    fn read_table_rows(&mut self, _: u32) -> mpedb_types::Result<Vec<Vec<Value>>> {
        unreachable!()
    }
    fn push(&mut self, _: &[NetOp]) -> mpedb_types::Result<Vec<bool>> {
        unreachable!()
    }
    fn ensure_source_state(&mut self, _: &str, _: u64, _: &str) -> mpedb_types::Result<()> {
        unreachable!()
    }
    fn read_source_state(&mut self, _: &str) -> mpedb_types::Result<Option<(u64, String)>> {
        unreachable!()
    }
    fn cas_source_state(&mut self, _: &str, _: u64, _: u64, _: &str) -> mpedb_types::Result<bool> {
        unreachable!()
    }
    fn push_outbox_identity(&self) -> Option<[u8; 16]> {
        Some(self.file_incarnation)
    }
    fn push_versioned(
        &mut self,
        from: Option<&Vec<u8>>,
        ops: &[NetOp],
        context: &mpedb_mirror::PushContext,
    ) -> mpedb_types::Result<mpedb_mirror::PushReply> {
        assert_eq!(from, Some(&self.metadata.cursor));
        assert_eq!(context.generations.len(), ops.len());
        let epoch = context.epoch.unwrap();
        assert_eq!(epoch.authority, mpedb_mirror::Authority::Source);
        let request = OracleWriteRequest {
            mirror_id: &self.metadata.mirror_id,
            token: context
                .request_id
                .expect("durable request must precede Oracle I/O"),
            epoch: epoch.epoch,
            authority: "source",
            state_identity: &self.metadata.state_identity,
            ledger_identity: self.ledger,
        };
        let images = self
            .source
            .write_batch_receipted(self.tables, ops, &request)?;
        if self.lose_reply_once {
            self.lose_reply_once = false;
            return Err(mpedb_types::Error::Unsupported(
                "injected lost Oracle commit response".into(),
            ));
        }
        Ok(mpedb_mirror::PushReply {
            applied: vec![true; ops.len()],
            postimages: Some(images),
        })
    }
}

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
        .write_batch_receipted(
            &tables,
            &[op(changed.clone()), op(second.clone())],
            &request2,
        )
        .unwrap();
    assert_eq!(result[1].as_ref().unwrap()[4], Value::Numeric("3".into()));
    assert!(source
        .cas_source_state_fenced(mirror_id, 1, 2, "source", &state)
        .unwrap());
    // A lost commit response remains recoverable after the epoch changes.
    // Recovery never rewrites data or weakens the normal stale-write guard.
    assert_eq!(
        source
            .recover_write_receipt(
                &tables,
                &[op(changed.clone()), op(second.clone())],
                &request2
            )
            .unwrap(),
        Some(result.clone())
    );
    assert!(source
        .write_batch_receipted(
            &tables,
            &[op(changed.clone()), op(second.clone())],
            &request2
        )
        .unwrap_err()
        .to_string()
        .contains("fenced by authority"));
    assert!(
        source
            .recover_write_receipt(&tables, &[op(image.clone())], &request)
            .is_err(),
        "corrupt historical receipts remain errors during recovery"
    );
    let request3 = OracleWriteRequest {
        token: [9; 32],
        ..request2
    };
    assert!(source
        .recover_write_receipt(&tables, &[op(changed.clone())], &request3)
        .unwrap()
        .is_none());
    let mut wrong_payload = changed.clone();
    wrong_payload[1] = Value::Numeric("6.6".into());
    assert!(source
        .recover_write_receipt(&tables, &[op(wrong_payload), op(second)], &request2)
        .unwrap_err()
        .to_string()
        .contains("another payload"));
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
    let metadata = mpedb_mirror::load_oracle_bootstrap(&db).unwrap();
    let nonce = mpedb_mirror::oracle_import::load_file_incarnation(&db).unwrap();
    db.query(
        "UPDATE \"MPEDB_RECEIPT_CASE\" SET \"GENERATED_VALUE\"=$1, \"TEXT_VALUE\"=$3 WHERE \"ID\"=$2",
        &[Value::Numeric("0".into()), Value::Numeric("3".into()), Value::Text("ø🙂漢".repeat(12000))],
    )
    .unwrap();
    {
        let mut transport = PostimageTransport {
            source: &mut source,
            tables: &tables,
            metadata: &metadata,
            ledger: &ledger,
            file_incarnation: nonce,
            lose_reply_once: true,
        };
        assert!(mpedb_mirror::push_batch(&db, &mut transport)
            .unwrap_err()
            .to_string()
            .contains("lost Oracle commit response"));
    }
    assert!(db
        .sys_record_get(mpedb_mirror::state::MIR_NS, b"push/outbox")
        .unwrap()
        .is_some());
    assert!(db
        .sys_record_get(mpedb_mirror::state::MIR_NS, b"push/outbox/00000001")
        .unwrap()
        .is_some());
    drop(db);
    drop(source);
    let db = mpedb::Database::open_from_file(&dest).unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    // A newer local generation must survive recovery of the OLD committed
    // request, and receive its own request id on the following push.
    db.query(
        "UPDATE MPEDB_RECEIPT_CASE SET AMOUNT=$1 WHERE ID=$2",
        &[Value::Numeric("6.6".into()), Value::Numeric("3".into())],
    )
    .unwrap();
    {
        let mut transport = PostimageTransport {
            source: &mut source,
            tables: &tables,
            metadata: &metadata,
            ledger: &ledger,
            file_incarnation: nonce,
            lose_reply_once: false,
        };
        assert_eq!(
            mpedb_mirror::push_batch(&db, &mut transport)
                .unwrap()
                .upserts,
            1
        );
        assert!(
            matches!(db.query("SELECT AMOUNT,GENERATED_VALUE FROM MPEDB_RECEIPT_CASE WHERE ID=$1", &[Value::Numeric("3".into())]).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![Value::Numeric("6.6".into()),Value::Numeric("0".into())]])
        );
        assert_eq!(
            mpedb_mirror::push_batch(&db, &mut transport)
                .unwrap()
                .upserts,
            1
        );
    }
    assert!(
        matches!(db.query("SELECT \"GENERATED_VALUE\" FROM \"MPEDB_RECEIPT_CASE\" WHERE \"ID\"=$1",&[Value::Numeric("3".into())]).unwrap(),mpedb::ExecResult::Rows {rows,..} if rows==vec![vec![Value::Numeric("4".into())]])
    );
    assert!(!db
        .sys_record_scan("cdc")
        .unwrap()
        .iter()
        .any(|(k, _)| k.starts_with(b"d/")));
    drop(db);
    std::fs::remove_file(&dest).unwrap();
    assert!(source
        .cas_source_state_fenced(mirror_id, 2, 3, "mpedb", &state)
        .unwrap());
    assert!(source
        .recover_write_receipt(&tables, &[op(changed.clone())], &request3)
        .unwrap()
        .is_some());
    assert!(source
        .write_batch_receipted(&tables, &[op(changed.clone())], &request3)
        .is_err());
    assert!(source
        .cas_source_state_fenced(mirror_id, 3, 4, "source", &state)
        .unwrap());
    let request3 = OracleWriteRequest {
        epoch: 4,
        ..request3
    };
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
    assert!(source
        .recover_write_receipt(&tables, &[op(changed.clone())], &request3)
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
    assert!(source
        .recover_write_receipt(&tables, &[], &request3)
        .unwrap_err()
        .to_string()
        .contains("ledger changed"));
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&mirror_id.as_bytes()],
    )
    .unwrap();
    c.commit().unwrap();
    source.ensure_source_state(mirror_id, 4, "source").unwrap();
    assert!(source
        .recover_write_receipt(&tables, &[], &request3)
        .unwrap_err()
        .to_string()
        .contains("fenced by authority incarnation"));
    for table in [
        "MPEDB_RECEIPT_CASE",
        "MPEDB_WRITE_RECEIPTS",
        "MPEDB_MIRROR_STATE",
    ] {
        c.execute(&format!("DROP TABLE {table} PURGE"), &[])
            .unwrap();
    }
}
