#![cfg(feature = "oracle-source")]
//! Durable source outcomes, tested through native public SQL/OCI only.
use mpedb_mirror::{
    oracle_cursor::OraclePosition,
    oracle_source::{OracleSource, OracleWriteOrigin, OracleWriteRequest},
    sourcecfg, NetOp, NetOpKind, SourceSpec,
};
use mpedb_types::Value;
fn number(n: i64) -> Value {
    Value::Numeric(n.to_string())
}
fn op(id: i64, amount: i64) -> NetOp {
    NetOp {
        table_id: 0,
        pk: vec![number(id)],
        kind: NetOpKind::Upsert(vec![number(id), number(amount)]),
    }
}
#[test]
#[ignore = "requires native MPEDB_ORACLE_SOURCE_CONFIG and Oracle client"]
fn native_mirror_decisions_replay_rejection_and_original_images_after_epoch_change() {
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
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_MIRROR_DECISION PURGE';EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE;END IF;END;",&[]).unwrap();
    c.execute(
        "CREATE TABLE MPEDB_MIRROR_DECISION(id NUMBER PRIMARY KEY,amount NUMBER)",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_MIRROR_DECISION VALUES(1,1)", &[])
        .unwrap();
    c.commit().unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let ledger = source.prepare_write_ledger().unwrap();
    let origins = source.prepare_origin_ledger().unwrap();
    let mirror = format!("mirror-decision-{}", std::process::id());
    source.ensure_source_state(&mirror, 1, "source").unwrap();
    let state = source.source_state_identity(&mirror).unwrap();
    let file: [u8; 16] = c
        .query_row_as::<Vec<u8>>("SELECT SYS_GUID() FROM dual", &[])
        .unwrap()
        .try_into()
        .unwrap();
    let tables = source
        .introspect(Some(&["MPEDB_MIRROR_DECISION".into()]), &[])
        .unwrap();
    let identity = source.identity().unwrap();
    let from = OraclePosition::new(&identity, &tables, source.current_scn().unwrap()).unwrap();
    c.execute("UPDATE MPEDB_MIRROR_DECISION SET amount=2 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    let ops = [op(1, 3), op(2, 5)];
    let rejected = OracleWriteRequest {
        mirror_id: &mirror,
        token: *blake3::hash(&[file.as_slice(), b"reject"].concat()).as_bytes(),
        epoch: 1,
        authority: "source",
        state_identity: &state,
        origin: Some(OracleWriteOrigin {
            file_incarnation: file,
            ledger_identity: &origins,
        }),
        ledger_identity: &ledger,
    };
    let reply = source
        .write_mirror_batch(&tables, &ops, &rejected, Some(&from), 10000)
        .unwrap();
    assert_eq!(reply.applied, vec![false, false]);
    assert_eq!(reply.postimages, Some(vec![None, None]));
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_MIRROR_DECISION WHERE id=1", &[])
            .unwrap(),
        2
    );
    assert_eq!(
        c.query_row_as::<u64>("SELECT COUNT(*) FROM MPEDB_MIRROR_DECISION WHERE id=2", &[])
            .unwrap(),
        0,
        "one conflict rejects the entire local batch"
    );
    let end = source.current_scn().unwrap();
    let events = source
        .read_write_origins(&mirror, &origins, from.scn(), end, 10000)
        .unwrap();
    let event = events
        .iter()
        .find(|event| event.request_id == rejected.token)
        .unwrap();
    assert!(
        event.touches.is_empty(),
        "a rejection may never attest ownership of a target"
    );
    assert!(source
        .cas_source_state_fenced(&mirror, 1, 2, "source", &state)
        .unwrap());
    c.execute("UPDATE MPEDB_MIRROR_DECISION SET amount=7 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    drop(source);
    let mut source = OracleSource::connect(&spec).unwrap();
    let replay = source
        .write_mirror_batch(&tables, &ops, &rejected, Some(&from), 10000)
        .unwrap();
    assert_eq!(replay.applied, reply.applied);
    assert_eq!(replay.postimages, reply.postimages);
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_MIRROR_DECISION WHERE id=1", &[])
            .unwrap(),
        7
    );
    let fresh = OraclePosition::new(&identity, &tables, source.current_scn().unwrap()).unwrap();
    assert!(
        source
            .write_mirror_batch(&tables, &ops, &rejected, Some(&fresh), 10000)
            .unwrap_err()
            .to_string()
            .contains("another payload"),
        "the decision binds its exact conflict cursor"
    );
    let accepted = OracleWriteRequest {
        token: *blake3::hash(&[file.as_slice(), b"accept"].concat()).as_bytes(),
        epoch: 2,
        ..rejected
    };
    let reply = source
        .write_mirror_batch(&tables, &ops, &accepted, Some(&fresh), 10000)
        .unwrap();
    assert_eq!(reply.applied, vec![true, true]);
    assert_eq!(
        reply.postimages,
        Some(vec![
            Some(vec![number(1), number(3)]),
            Some(vec![number(2), number(5)])
        ])
    );
    c.execute("UPDATE MPEDB_MIRROR_DECISION SET amount=9 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    assert!(source
        .cas_source_state_fenced(&mirror, 2, 3, "mpedb", &state)
        .unwrap());
    drop(source);
    let mut source = OracleSource::connect(&spec).unwrap();
    let replay = source
        .write_mirror_batch(&tables, &ops, &accepted, Some(&fresh), 10000)
        .unwrap();
    assert_eq!(replay.applied, reply.applied);
    assert_eq!(
        replay.postimages, reply.postimages,
        "recover original images after a lost response and authority change"
    );
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_MIRROR_DECISION WHERE id=1", &[])
            .unwrap(),
        9,
        "recovery never reruns DML"
    );
    let stale = OracleWriteRequest {
        token: *blake3::hash(&[file.as_slice(), b"stale"].concat()).as_bytes(),
        epoch: 2,
        ..accepted
    };
    assert!(source
        .write_mirror_batch(&tables, &ops, &stale, Some(&fresh), 10000)
        .unwrap_err()
        .to_string()
        .contains("fenced"));
    // The MPEdb-authoritative path is explicitly unconditional, but it keeps
    // exactly the same receipt/epoch recovery guarantees.
    let local = OracleWriteRequest {
        token: *blake3::hash(&[file.as_slice(), b"local"].concat()).as_bytes(),
        epoch: 3,
        authority: "mpedb",
        ..accepted
    };
    let local_ops = [op(1, 11), op(2, 12)];
    let reply = source
        .write_mirror_batch(&tables, &local_ops, &local, None, 10000)
        .unwrap();
    assert_eq!(reply.applied, vec![true, true]);
    assert_eq!(
        c.query_row_as::<i64>("SELECT amount FROM MPEDB_MIRROR_DECISION WHERE id=1", &[])
            .unwrap(),
        11
    );
    assert!(source
        .cas_source_state_fenced(&mirror, 3, 4, "source", &state)
        .unwrap());
    let replay = source
        .write_mirror_batch(&tables, &local_ops, &local, None, 10000)
        .unwrap();
    assert_eq!(replay.postimages, reply.postimages);
    // A missing origin invalidates even a durable rejection; no positive ACK.
    let mirror_key = mirror.as_bytes().to_vec();
    let token = rejected.token.to_vec();
    c.execute(
        "DELETE FROM MPEDB_WRITE_ORIGINS WHERE mirror_id=:1 AND request_token=:2",
        &[&mirror_key, &token],
    )
    .unwrap();
    c.commit().unwrap();
    assert!(source
        .write_mirror_batch(&tables, &ops, &rejected, Some(&from), 10000)
        .unwrap_err()
        .to_string()
        .contains("missing its committed origin"));
    c.execute("DROP TABLE MPEDB_MIRROR_DECISION PURGE", &[])
        .unwrap();
}
