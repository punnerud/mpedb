#![cfg(feature = "oracle-source")]
//! Real OracleAdapter -> durable outbox -> source receipts -> shared local ACK.
use mpedb_mirror::{
    import_oracle, oracle_import::load_bootstrap, oracle_source::OracleSource, sourcecfg, state,
    Cursor, ImportOptions, NetOp, OracleAdapter, ProvenancedPullBatch, PullBatch, PushContext,
    PushReply, SourceAdapter, SourceSpec,
};
use mpedb_types::{Error, Result, Value};
struct LoseReply {
    inner: OracleAdapter,
    once: bool,
}
impl SourceAdapter for LoseReply {
    fn pull(&mut self, from: &Cursor, max: usize) -> Result<Option<PullBatch>> {
        self.inner.pull(from, max)
    }
    fn pull_provenanced(
        &mut self,
        from: &Cursor,
        max: usize,
    ) -> Result<Option<ProvenancedPullBatch>> {
        self.inner.pull_provenanced(from, max)
    }
    fn head(&mut self) -> Result<Cursor> {
        self.inner.head()
    }
    fn zero_cursor(&self) -> Cursor {
        self.inner.zero_cursor()
    }
    fn read_table_rows(&mut self, id: u32) -> Result<Vec<Vec<Value>>> {
        self.inner.read_table_rows(id)
    }
    fn push(&mut self, ops: &[NetOp]) -> Result<Vec<bool>> {
        self.inner.push(ops)
    }
    fn ensure_source_state(&mut self, id: &str, epoch: u64, authority: &str) -> Result<()> {
        self.inner.ensure_source_state(id, epoch, authority)
    }
    fn read_source_state(&mut self, id: &str) -> Result<Option<(u64, String)>> {
        self.inner.read_source_state(id)
    }
    fn cas_source_state(&mut self, id: &str, old: u64, next: u64, authority: &str) -> Result<bool> {
        self.inner.cas_source_state(id, old, next, authority)
    }
    fn validate_push_target(&self, db: &mpedb::Database) -> Result<()> {
        self.inner.validate_push_target(db)
    }
    fn push_outbox_identity(&self) -> Option<[u8; 16]> {
        self.inner.push_outbox_identity()
    }
    fn push_versioned(
        &mut self,
        from: Option<&Cursor>,
        ops: &[NetOp],
        context: &PushContext,
    ) -> Result<PushReply> {
        let reply = self.inner.push_versioned(from, ops, context)?;
        if self.once {
            self.once = false;
            return Err(Error::Unsupported(
                "injected lost Oracle adapter response".into(),
            ));
        }
        Ok(reply)
    }
}
fn number(n: i64) -> Value {
    Value::Numeric(n.to_string())
}
fn update(db: &mpedb::Database, n: i64) {
    db.query(
        "UPDATE MPEDB_ADAPTER_PUSH SET AMOUNT=$1 WHERE ID=$2",
        &[number(n), number(1)],
    )
    .unwrap();
}
fn row(db: &mpedb::Database, id: i64) -> Option<Vec<Value>> {
    let mpedb::ExecResult::Rows { mut rows, .. } = db
        .query(
            "SELECT ID,AMOUNT,G FROM MPEDB_ADAPTER_PUSH WHERE ID=$1",
            &[number(id)],
        )
        .unwrap()
    else {
        panic!("rows")
    };
    assert!(rows.len() <= 1);
    rows.pop()
}
fn adapter(spec: &SourceSpec, db: &mpedb::Database) -> OracleAdapter {
    OracleAdapter::from_database(OracleSource::connect(spec).unwrap(), db).unwrap()
}
fn pull(db: &mpedb::Database, a: &mut OracleAdapter) -> u64 {
    let through = a.head().unwrap();
    let mut echoes = 0;
    for _ in 0..100 {
        let from = db
            .sys_record_get(state::MIR_NS, state::KEY_CUR)
            .unwrap()
            .unwrap();
        let Some(batch) = a.pull_through_provenanced(&from, &through, 10000).unwrap() else {
            return echoes;
        };
        let stats = mpedb_mirror::apply_provenanced_batch(db, &from, &batch).unwrap();
        echoes += stats.echoes;
    }
    panic!("bounded Oracle drain did not finish")
}
fn imported(
    source: &mut OracleSource,
    path: &std::path::Path,
    opts: &ImportOptions,
) -> mpedb::Database {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match import_oracle(source, path, opts) {
            Ok((db, _)) => return db,
            Err(e)
                if e.to_string().contains("ORA-01466")
                    && !path.exists()
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => panic!("import: {e}"),
        }
    }
}
#[test]
#[ignore = "requires isolated native MPEDB_ORACLE_FENCE_SOURCE_CONFIG"]
fn native_oracle_adapter_recovers_outbox_preserves_generations_and_resolves_conflicts() {
    let config = std::env::var("MPEDB_ORACLE_FENCE_SOURCE_CONFIG").unwrap();
    let spec = sourcecfg::load(std::path::Path::new(&config)).unwrap();
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
    let names = [
        "MPEDB_ADAPTER_PUSH",
        "MPEDB_WRITE_ORIGINS",
        "MPEDB_WRITE_RECEIPTS",
        "MPEDB_MIRROR_STATE",
    ];
    for name in names {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {name} PURGE';EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE;END IF;END;"),&[]).unwrap();
    }
    c.execute("CREATE TABLE MPEDB_ADAPTER_PUSH(id NUMBER PRIMARY KEY,amount NUMBER(4,1),g NUMBER GENERATED ALWAYS AS(id+100) VIRTUAL)",&[]).unwrap();
    c.execute("INSERT INTO MPEDB_ADAPTER_PUSH(id,amount) VALUES(1,1)", &[])
        .unwrap();
    c.commit().unwrap();
    let path = mpedb_testkit::scratch_base()
        .join(format!("oracle-adapter-push-{}.mpedb", std::process::id()));
    let readonly = path.with_extension("readonly.mpedb");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&readonly);
    let opts = ImportOptions {
        include: Some(vec!["MPEDB_ADAPTER_PUSH".into()]),
        size_bytes: 32 * 1024 * 1024,
        durability: mpedb_types::Durability::Commit,
        ..Default::default()
    };
    let mut source = OracleSource::connect(&spec).unwrap();
    // A manifest without tracking identities remains readable, never writable.
    let db = imported(&mut source, &readonly, &opts);
    let metadata = load_bootstrap(&db).unwrap();
    assert!(metadata.receipt_ledger.is_none());
    assert!(metadata.origin_ledger.is_none());
    let mut a = adapter(&spec, &db);
    assert!(a.push_outbox_identity().is_none());
    update(&db, 2);
    assert!(mpedb_mirror::push_batch(&db, &mut a)
        .unwrap_err()
        .to_string()
        .contains("ledger pinned at import"));
    assert!(db
        .sys_record_get(state::MIR_NS, b"push/outbox")
        .unwrap()
        .is_none());
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_ADAPTER_PUSH WHERE id=1", &[])
            .unwrap(),
        1
    );
    drop(a);
    drop(db);
    let ledger = source.prepare_write_ledger().unwrap();
    let origin = source.prepare_origin_ledger().unwrap();
    let db = imported(&mut source, &path, &opts);
    let metadata = load_bootstrap(&db).unwrap();
    assert_eq!(metadata.receipt_ledger, Some(ledger));
    assert_eq!(metadata.origin_ledger, Some(origin));
    let other = mpedb::Database::open_from_file(&readonly).unwrap();
    let mut wrong = adapter(&spec, &db);
    assert!(mpedb_mirror::push_batch(&other, &mut wrong)
        .unwrap_err()
        .to_string()
        .contains("another FILE or bootstrap"));
    assert!(other
        .sys_record_get(state::MIR_NS, b"push/outbox")
        .unwrap()
        .is_none());
    drop(wrong);
    drop(other);
    std::fs::remove_file(&readonly).unwrap();
    let file = mpedb_mirror::oracle_import::load_file_incarnation(&db).unwrap();
    let mut a = LoseReply {
        inner: adapter(&spec, &db),
        once: true,
    };
    assert_eq!(a.push_outbox_identity(), Some(file));
    update(&db, 2);
    assert!(mpedb_mirror::push_batch(&db, &mut a)
        .unwrap_err()
        .to_string()
        .contains("injected lost"));
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_ADAPTER_PUSH WHERE id=1", &[])
            .unwrap(),
        2
    );
    assert!(db
        .sys_record_get(state::MIR_NS, b"push/outbox")
        .unwrap()
        .is_some());
    update(&db, 3);
    drop(a);
    drop(db);
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let mut a = adapter(&spec, &db);
    let stats = mpedb_mirror::push_batch(&db, &mut a).unwrap();
    assert_eq!(stats.upserts, 1);
    assert_eq!(
        row(&db, 1).unwrap()[1],
        number(3),
        "old ACK never overwrites a newer local generation"
    );
    assert!(db
        .sys_record_get(state::MIR_NS, b"push/outbox")
        .unwrap()
        .is_none());
    assert!(pull(&db, &mut a) > 0);
    assert_eq!(
        row(&db, 1).unwrap()[1],
        number(3),
        "own echo preserves the newer generation"
    );
    assert_eq!(mpedb_mirror::push_batch(&db, &mut a).unwrap().upserts, 1);
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_ADAPTER_PUSH WHERE id=1", &[])
            .unwrap(),
        3
    );
    update(&db, 9);
    c.execute("UPDATE MPEDB_ADAPTER_PUSH SET amount=3 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    pull(&db, &mut a);
    assert_eq!(
        row(&db, 1).unwrap()[1],
        number(3),
        "a foreign equal-image write is never suppressed"
    );
    assert_eq!(
        mpedb_mirror::push_batch(&db, &mut a).unwrap(),
        mpedb_mirror::PushStats::default()
    );
    c.execute("UPDATE MPEDB_ADAPTER_PUSH SET amount=4 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    update(&db, 5);
    db.query(
        "INSERT INTO MPEDB_ADAPTER_PUSH(ID,AMOUNT) VALUES($1,$2)",
        &[number(2), number(6)],
    )
    .unwrap();
    let mut fault = LoseReply {
        inner: a,
        once: true,
    };
    assert!(mpedb_mirror::push_batch(&db, &mut fault)
        .unwrap_err()
        .to_string()
        .contains("injected lost"));
    assert_eq!(
        c.query_row_as::<u64>("SELECT COUNT(*) FROM MPEDB_ADAPTER_PUSH WHERE id=2", &[])
            .unwrap(),
        0,
        "conflict rejects the whole batch"
    );
    update(&db, 7);
    drop(fault);
    drop(db);
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let mut a = adapter(&spec, &db);
    let stats = mpedb_mirror::push_batch(&db, &mut a).unwrap();
    assert_eq!(stats.conflicts, 2);
    assert_eq!(row(&db, 1).unwrap()[1], number(7));
    pull(&db, &mut a);
    assert_eq!(row(&db, 1).unwrap()[1], number(4));
    assert_eq!(mpedb_mirror::push_batch(&db, &mut a).unwrap().upserts, 1);
    assert_eq!(
        row(&db, 2).unwrap(),
        vec![number(2), number(6), number(102)],
        "actual generated postimage is acknowledged locally"
    );
    pull(&db, &mut a);
    db.query("DELETE FROM MPEDB_ADAPTER_PUSH WHERE ID=$1", &[number(2)])
        .unwrap();
    assert_eq!(mpedb_mirror::push_batch(&db, &mut a).unwrap().deletes, 1);
    assert_eq!(
        c.query_row_as::<u64>("SELECT COUNT(*) FROM MPEDB_ADAPTER_PUSH WHERE id=2", &[])
            .unwrap(),
        0
    );
    pull(&db, &mut a);
    update(&db, 8);
    let mut fault = LoseReply {
        inner: a,
        once: true,
    };
    assert!(mpedb_mirror::push_batch(&db, &mut fault)
        .unwrap_err()
        .to_string()
        .contains("injected lost"));
    drop(fault);
    drop(db);
    // Losing the source receipt table cannot silently turn a pending retry
    // into a second execution. Its original object identity lives in the FILE.
    c.execute("DROP TABLE MPEDB_WRITE_RECEIPTS PURGE", &[])
        .unwrap();
    source.prepare_write_ledger().unwrap();
    let db = mpedb::Database::open_from_file(&path).unwrap();
    let mut a = adapter(&spec, &db);
    assert!(mpedb_mirror::push_batch(&db, &mut a)
        .unwrap_err()
        .to_string()
        .contains("ledger changed"));
    assert!(db
        .sys_record_get(state::MIR_NS, b"push/outbox")
        .unwrap()
        .is_some());
    assert_eq!(row(&db, 1).unwrap()[1], number(8));
    drop(a);
    drop(db);
    std::fs::remove_file(&path).unwrap();
    for name in names {
        c.execute(&format!("DROP TABLE {name} PURGE"), &[]).unwrap();
    }
}
