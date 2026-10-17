#![cfg(feature = "oracle-source")]
use mpedb_mirror::{oracle_source::OracleSource, sourcecfg, NetOp, NetOpKind, SourceSpec};
use mpedb_types::Value;
mod oracle_support;
fn same_rows(a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            a.len() == b.len()
                && a.iter().zip(b).all(|(a, b)| match (a, b) {
                    (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
                    _ => a == b,
                })
        })
}
#[test]
#[ignore = "requires Oracle application test owner and native client libraries"]
fn native_oracle_binary_floats_roundtrip_specials_and_reject_signed_zero_loss() {
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
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_BINARY_FLOAT_CASE PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute("CREATE TABLE MPEDB_BINARY_FLOAT_CASE(id NUMBER PRIMARY KEY,single_value BINARY_FLOAT,double_value BINARY_DOUBLE)",&[]).unwrap();
    for (id, single, double) in [
        (1i64, "BINARY_FLOAT_NAN", "BINARY_DOUBLE_NAN"),
        (2, "BINARY_FLOAT_INFINITY", "BINARY_DOUBLE_INFINITY"),
        (3, "-BINARY_FLOAT_INFINITY", "-BINARY_DOUBLE_INFINITY"),
        (4, "TO_BINARY_FLOAT('-0')", "TO_BINARY_DOUBLE('-0')"),
        (
            5,
            "TO_BINARY_FLOAT('1.401298464324817e-45')",
            "TO_BINARY_DOUBLE('4.9406564584124654e-324')",
        ),
        (
            6,
            "TO_BINARY_FLOAT('3.4028234663852886e38')",
            "TO_BINARY_DOUBLE('1.7976931348623157e308')",
        ),
        (
            7,
            "TO_BINARY_FLOAT('1.1754943508222875e-38')",
            "TO_BINARY_DOUBLE('2.2250738585072014e-308')",
        ),
    ] {
        c.execute(
            &format!("INSERT INTO MPEDB_BINARY_FLOAT_CASE VALUES(:1,{single},{double})"),
            &[&id],
        )
        .unwrap();
    }
    let negative_zero = -0.0f64;
    c.execute(
        "UPDATE MPEDB_BINARY_FLOAT_CASE SET single_value=:1,double_value=:2 WHERE id=4",
        &[
            &(
                &negative_zero,
                &oracle_client::sql_type::OracleType::BinaryFloat,
            ),
            &(
                &negative_zero,
                &oracle_client::sql_type::OracleType::BinaryDouble,
            ),
        ],
    )
    .unwrap();
    c.commit().unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(Some(&["MPEDB_BINARY_FLOAT_CASE".into()]), &[])
        .unwrap();
    let (_, mut snapshots) = oracle_support::initial_snapshot(&source, &tables);
    let original = snapshots.remove(0);
    assert!(matches!(original[0][1],Value::Float(n) if n.is_nan()));
    assert!(matches!(original[0][2],Value::Float(n) if n.is_nan()));
    assert!(matches!(original[3][2],Value::Float(n) if n == 0.0));
    assert!(matches!(original[4][1],Value::Float(n) if (n as f32).is_subnormal()));
    assert!(matches!(original[4][2],Value::Float(n) if n.is_subnormal()));
    assert!(matches!(original[5][2],Value::Float(n) if n == f64::MAX));
    c.execute("DELETE FROM MPEDB_BINARY_FLOAT_CASE", &[])
        .unwrap();
    c.commit().unwrap();
    let ops: Vec<_> = original
        .iter()
        .map(|row| NetOp {
            table_id: 0,
            pk: vec![row[0].clone()],
            kind: NetOpKind::Upsert(row.clone()),
        })
        .collect();
    for _ in 0..2 {
        let images = source
            .write_batch(&tables, &ops)
            .unwrap()
            .into_iter()
            .map(Option::unwrap)
            .collect::<Vec<_>>();
        assert!(
            same_rows(&images, &original),
            "every IEEE carrier bit must survive write verification"
        );
    }
    let actual = source
        .read_snapshot(&tables[0], source.current_scn().unwrap())
        .unwrap();
    assert!(same_rows(&actual, &original));
    let mut rounded = original[4].clone();
    rounded[1] = Value::Float(0.1);
    let op = NetOp {
        table_id: 0,
        pk: vec![rounded[0].clone()],
        kind: NetOpKind::Upsert(rounded),
    };
    assert!(
        source.write_batch(&tables, &[op]).is_err(),
        "binary64 values rounded to binary32 must remain rejected"
    );
    assert!(same_rows(
        &source
            .read_snapshot(&tables[0], source.current_scn().unwrap())
            .unwrap(),
        &original
    ));
    let mut negative = original[3].clone();
    negative[2] = Value::Float(-0.0);
    let op = NetOp {
        table_id: 0,
        pk: vec![negative[0].clone()],
        kind: NetOpKind::Upsert(negative),
    };
    assert!(source.write_batch(&tables, &[op]).is_err(), "Oracle's negative-zero normalization must be detected rather than silently losing the submitted sign");
    // The durable receipt codec must retain IEEE images too, including after
    // reconnect and an intervening source update.
    let receipt_id = "native-binary-float-receipt";
    source.ensure_source_state(receipt_id, 1, "source").unwrap();
    let state = source.source_state_identity(receipt_id).unwrap();
    let ledger = source.prepare_write_ledger().unwrap();
    let request = mpedb_mirror::oracle_source::OracleWriteRequest {
        mirror_id: receipt_id,
        token: [42; 32],
        epoch: 1,
        authority: "source",
        state_identity: &state,
        ledger_identity: &ledger,
    };
    source
        .write_batch_receipted(&tables, &ops[..1], &request)
        .unwrap();
    c.execute(
        "UPDATE MPEDB_BINARY_FLOAT_CASE SET single_value=1,double_value=2 WHERE id=1",
        &[],
    )
    .unwrap();
    c.commit().unwrap();
    drop(source);
    let mut source = OracleSource::connect(&spec).unwrap();
    let recovered = source
        .recover_write_receipt(&tables, &ops[..1], &request)
        .unwrap()
        .unwrap()
        .into_iter()
        .map(Option::unwrap)
        .collect::<Vec<_>>();
    assert!(same_rows(&recovered, &original[..1]));
    let replay = source
        .write_batch_receipted(&tables, &ops[..1], &request)
        .unwrap()
        .into_iter()
        .map(Option::unwrap)
        .collect::<Vec<_>>();
    assert!(same_rows(&replay, &original[..1]));
    assert_eq!(
        c.query_row_as::<u64>(
            "SELECT double_value FROM MPEDB_BINARY_FLOAT_CASE WHERE id=1",
            &[]
        )
        .unwrap(),
        2
    );
    c.execute("UPDATE MPEDB_BINARY_FLOAT_CASE SET single_value=BINARY_FLOAT_NAN,double_value=BINARY_DOUBLE_NAN WHERE id=1", &[]).unwrap();
    c.execute(
        "DELETE FROM MPEDB_WRITE_RECEIPTS WHERE mirror_id=:1",
        &[&receipt_id.as_bytes()],
    )
    .unwrap();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&receipt_id.as_bytes()],
    )
    .unwrap();
    c.commit().unwrap();
    let dest = mpedb_testkit::scratch_base()
        .join(format!("oracle-binary-float-{}.mpedb", std::process::id()));
    let (db, _) = mpedb_mirror::import_oracle(
        &mut source,
        &dest,
        &mpedb_mirror::ImportOptions {
            include: Some(vec!["MPEDB_BINARY_FLOAT_CASE".into()]),
            size_bytes: 64 * 1024 * 1024,
            ..Default::default()
        },
    )
    .unwrap();
    let metadata = mpedb_mirror::load_oracle_bootstrap(&db).unwrap();
    let mut adapter =
        mpedb_mirror::OracleAdapter::from_database(OracleSource::connect(&spec).unwrap(), &db)
            .unwrap();
    assert!(
        mpedb_mirror::verify(&db, &mut adapter).unwrap(),
        "NaNs must not create false divergence"
    );
    let stats = mpedb_mirror::reconcile(&db, &mut adapter).unwrap();
    assert_eq!(stats.upserts, 0);
    assert_eq!(stats.deletes, 0);
    db.verify().unwrap();
    drop((adapter, db));
    std::fs::remove_file(dest).unwrap();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&metadata.mirror_id.as_bytes()],
    )
    .unwrap();
    c.commit().unwrap();
    c.execute("DROP TABLE MPEDB_BINARY_FLOAT_CASE PURGE", &[])
        .unwrap();
}
