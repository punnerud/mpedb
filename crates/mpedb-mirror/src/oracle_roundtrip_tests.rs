//! Real component roundtrip through MPEdb/rRETL, with isolated single writers.
//! Deliberately not a production controller: no concurrent-writer/echo policy.
use crate::{
    apply_batch, import_oracle, import_pg, load_oracle_bootstrap,
    oracle_source::{OracleSource, OracleWriteRequest},
    sourcecfg, state, ImportOptions, NetOp, NetOpKind, OracleAdapter, PgAdapter, SourceAdapter,
    SourceSpec,
};
use mpedb::Database;
use mpedb_types::{keycode, Value};
use std::collections::BTreeMap;
const TABLE: &str = "MPEDB_ROUNDTRIP";
fn rows(db: &Database) -> Vec<Vec<Value>> {
    match db
        .query(
            "SELECT * FROM \"MPEDB_ROUNDTRIP\" ORDER BY \"ID\",\"PART\"",
            &[],
        )
        .unwrap()
    {
        mpedb::ExecResult::Rows { rows, .. } => rows,
        _ => panic!("rows required"),
    }
}
fn upserts(db: &Database) -> Vec<NetOp> {
    rows(db)
        .into_iter()
        .map(|row| NetOp {
            table_id: 0,
            pk: row[..2].to_vec(),
            kind: NetOpKind::Upsert(row),
        })
        .collect()
}
fn install_lens(db: &Database) {
    db.create_function(mpedb::spellfn::SpellLang::PlSql,
        "CREATE FUNCTION MPEDB_ROUNDTRIP_FLIP(x BOOLEAN) RETURN BOOLEAN IS BEGIN RETURN NOT x; END;").unwrap();
    db.create_lens(
        "roundtrip_flip",
        "MPEDB_ROUNDTRIP_FLIP",
        "MPEDB_ROUNDTRIP_FLIP",
        mpedb::lens::LensClass::Bijective,
    )
    .unwrap();
}
fn flip(db: &Database) -> i64 {
    db.rretl_apply("roundtrip_flip", TABLE, "FLAG")
        .unwrap()
        .run_id
}
fn drain_oracle(db: &Database, adapter: &mut OracleAdapter) {
    let through = adapter.head().unwrap();
    for _ in 0..32 {
        let from = db
            .sys_record_get(state::MIR_NS, state::KEY_CUR)
            .unwrap()
            .unwrap();
        let Some(batch) = adapter.pull_through(&from, &through, 64).unwrap() else {
            return;
        };
        apply_batch(db, &from, &batch).unwrap();
    }
    panic!("Oracle source did not reach its captured boundary");
}
#[test]
#[ignore = "requires dedicated MPEDB_ORACLE_FENCE owner, Oracle client and PostgreSQL binaries"]
fn oracle_postgres_roundtrip_uses_mpedb_rretl_for_both_directions() {
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
    let oracle = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    let owned = [TABLE, "MPEDB_WRITE_RECEIPTS", "MPEDB_MIRROR_STATE"];
    for table in owned {
        oracle.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"), &[]).unwrap();
    }
    oracle.execute("CREATE TABLE MPEDB_ROUNDTRIP(id NUMBER,part VARCHAR2(20 CHAR),flag BOOLEAN,amount NUMBER,payload CLOB,PRIMARY KEY(id,part))", &[]).unwrap();
    oracle.execute("INSERT INTO MPEDB_ROUNDTRIP VALUES(1,'ø',TRUE,12345678901234567890.123456789,TO_CLOB('original 🙂'))", &[]).unwrap();
    oracle
        .execute(
            "INSERT INTO MPEDB_ROUNDTRIP VALUES(3,'ø',FALSE,0,EMPTY_CLOB())",
            &[],
        )
        .unwrap();
    oracle.commit().unwrap();
    let pg = crate::pg_harness::ThrowawayPg::start();
    let mut pg_client = pg.client();
    pg_client.batch_execute("CREATE TABLE \"MPEDB_ROUNDTRIP\"(\"ID\" NUMERIC,\"PART\" TEXT,\"FLAG\" BOOLEAN,\"AMOUNT\" NUMERIC,\"PAYLOAD\" TEXT,PRIMARY KEY(\"ID\",\"PART\"))").unwrap();
    let dir =
        mpedb_testkit::scratch_base().join(format!("oracle-pg-roundtrip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let raw_path = dir.join("oracle.mpedb");
    let pg_path = dir.join("postgres.mpedb");
    let options = ImportOptions {
        include: Some(vec![TABLE.into()]),
        size_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };
    let mut source = OracleSource::connect(&spec).unwrap();
    let ledger = source.prepare_write_ledger().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (raw, _) = loop {
        match import_oracle(&mut source, &raw_path, &options) {
            Ok(value) => break value,
            Err(e)
                if e.to_string().contains("ORA-01466") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => panic!("{e}"),
        }
    };
    let metadata = load_oracle_bootstrap(&raw).unwrap();
    install_lens(&raw);
    let initial_raw = rows(&raw);
    let run = flip(&raw);
    let transformed = rows(&raw);
    assert_eq!(transformed[0][2], Value::Bool(false));
    let mut pg_adapter = PgAdapter::new(pg.client(), options.include.as_deref(), &[]).unwrap();
    pg_adapter.install_triggers().unwrap();
    assert!(pg_adapter
        .push(&upserts(&raw))
        .unwrap()
        .into_iter()
        .all(|v| v));
    raw.rretl_revert(run).unwrap();
    assert_eq!(rows(&raw), initial_raw);
    let (pg_mirror, _) = import_pg(&mut pg_client, &pg_path, &options).unwrap();
    assert_eq!(rows(&pg_mirror), transformed);
    install_lens(&pg_mirror);
    // Genuine PostgreSQL writes, including a compound-PK move and a delete.
    let from = pg_adapter.head().unwrap();
    pg_client.batch_execute("BEGIN; UPDATE \"MPEDB_ROUNDTRIP\" SET \"PART\"='🙂',\"FLAG\"=TRUE,\"AMOUNT\"=98765432109876543210.987654321,\"PAYLOAD\"='PostgreSQL ø🙂' WHERE \"ID\"=1; INSERT INTO \"MPEDB_ROUNDTRIP\" VALUES(2,'ny',FALSE,0.000000000000000000001,'inserted'); DELETE FROM \"MPEDB_ROUNDTRIP\" WHERE \"ID\"=3; COMMIT;").unwrap();
    let batch = pg_adapter.pull(&from, 64).unwrap().unwrap();
    assert_eq!(batch.ops.len(), 4);
    apply_batch(&pg_mirror, &from, &batch).unwrap();
    let pg_representation = rows(&pg_mirror);
    // The reverse representation is calculated only inside MPEdb/rRETL.
    let run = flip(&pg_mirror);
    let expected_raw = rows(&pg_mirror);
    let by_key: BTreeMap<_, _> = expected_raw
        .iter()
        .map(|row| (keycode::encode_key_exact(&row[..2]), row.clone()))
        .collect();
    let ops: Vec<_> = batch
        .ops
        .iter()
        .map(|op| NetOp {
            table_id: op.table_id,
            pk: op.pk.clone(),
            kind: match &op.kind {
                NetOpKind::Delete => NetOpKind::Delete,
                NetOpKind::Upsert(_) => {
                    NetOpKind::Upsert(by_key[&keycode::encode_key_exact(&op.pk)].clone())
                }
            },
        })
        .collect();
    let token =
        *blake3::hash(&crate::oracle_import::load_file_incarnation(&raw).unwrap()).as_bytes();
    let request = OracleWriteRequest {
        mirror_id: &metadata.mirror_id,
        token,
        epoch: 1,
        authority: "source",
        state_identity: &metadata.state_identity,
        ledger_identity: &ledger,
    };
    let images = source
        .write_batch_receipted(&metadata.tables, &ops, &request)
        .unwrap();
    assert_eq!(images.iter().filter(|row| row.is_some()).count(), 2);
    pg_mirror.rretl_revert(run).unwrap();
    assert_eq!(rows(&pg_mirror), pg_representation);
    let mut oracle_adapter =
        OracleAdapter::from_database(OracleSource::connect(&spec).unwrap(), &raw).unwrap();
    drain_oracle(&raw, &mut oracle_adapter);
    assert_eq!(rows(&raw), expected_raw);
    // A subsequent genuine Oracle update travels forward through the same lens.
    oracle.execute("UPDATE MPEDB_ROUNDTRIP SET flag=FALSE,payload=TO_CLOB('Oracle after roundtrip 🙂') WHERE id=1", &[]).unwrap();
    oracle.commit().unwrap();
    drain_oracle(&raw, &mut oracle_adapter);
    let run = flip(&raw);
    assert!(pg_adapter
        .push(&upserts(&raw))
        .unwrap()
        .into_iter()
        .all(|v| v));
    let check_path = dir.join("check.mpedb");
    let (check, _) = import_pg(&mut pg_client, &check_path, &options).unwrap();
    assert_eq!(rows(&check), rows(&raw));
    raw.rretl_revert(run).unwrap();
    raw.verify().unwrap();
    pg_mirror.verify().unwrap();
    check.verify().unwrap();
    drop((check, raw, pg_mirror, oracle_adapter, source));
    std::fs::remove_dir_all(&dir).unwrap();
    for table in owned {
        oracle
            .execute(&format!("DROP TABLE {table} PURGE"), &[])
            .unwrap();
    }
}
