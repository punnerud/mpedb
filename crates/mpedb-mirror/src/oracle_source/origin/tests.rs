use super::*;
use crate::adapter::{NetOpKind, WriteOrigin};
fn table() -> OracleTable {
    OracleTable {
        owner: "APP".into(),
        name: "T".into(),
        object_id: 41,
        ddl_source: "ddl".into(),
        schema_ddl: "ddl".into(),
        columns: vec![OracleColumn {
            name: "ID".into(),
            data_type: "NUMBER".into(),
            type_owner: None,
            precision: None,
            scale: None,
            data_length: 22,
            char_length: 0,
            char_used: None,
            not_null: true,
            generated: false,
            identity: false,
            default_source: None,
            collation: None,
        }],
        pk: vec![0],
        pk_validated: true,
        temporary: false,
        nested: false,
        external: false,
    }
}
fn op(n: i64) -> NetOp {
    NetOp {
        table_id: 0,
        pk: vec![Value::Numeric(n.to_string())],
        kind: NetOpKind::Upsert(vec![Value::Numeric(n.to_string())]),
    }
}
fn batch() -> OracleChangeBatch {
    OracleChangeBatch {
        scn: 10,
        xids: vec![vec![1], vec![2]],
        ops: vec![op(1), op(2)],
        key_transactions: vec![
            OracleKeyTransactions {
                xids: vec![vec![1]],
                complete: true,
            },
            OracleKeyTransactions {
                xids: vec![vec![2]],
                complete: true,
            },
        ],
    }
}
fn event() -> OracleOriginEvent {
    OracleOriginEvent {
        scn: 10,
        xid: vec![1],
        file_incarnation: [3; 16],
        request_id: [4; 32],
        touches: codec::decode(&codec::encode(&[table()], &[op(1)]).unwrap()).unwrap(),
    }
}
#[test]
fn footprint_codec_is_bounded_exact_and_rejects_truncations_and_duplicates() {
    let bytes = codec::encode(&[table()], &[op(1), op(2)]).unwrap();
    let touches = codec::decode(&bytes).unwrap();
    assert_eq!(touches.len(), 2);
    assert_eq!(touches[0].object_id, 41);
    assert!(touches.iter().all(|touch| touch.upsert));
    for end in 0..bytes.len() {
        assert!(codec::decode(&bytes[..end]).is_err(), "prefix {end}");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(codec::decode(&trailing).is_err());
    let mut oversized = vec![1];
    oversized.extend_from_slice(&100_001u32.to_be_bytes());
    assert!(codec::decode(&oversized).is_err());
    assert!(codec::encode(&[table()], &[op(1), op(1)]).is_err());
    let one = codec::encode(&[table()], &[op(1)]).unwrap();
    let mut duplicate = one.clone();
    duplicate[1..5].copy_from_slice(&2u32.to_be_bytes());
    duplicate.extend_from_slice(&one[5..]);
    assert!(codec::decode(&duplicate).is_err());
}
#[test]
fn same_scn_and_same_xid_cannot_attest_other_keys_or_tables() {
    let mut b = batch();
    let e = event();
    let proof = attest_write_origins(&[table()], &b, std::slice::from_ref(&e), [3; 16]).unwrap();
    assert_eq!(
        proof[0],
        Some(WriteOrigin {
            file_incarnation: [3; 16],
            request_ids: vec![[4; 32]]
        })
    );
    assert!(proof[1].is_none());
    b.key_transactions[1].xids = vec![vec![1]];
    assert!(
        attest_write_origins(&[table()], &b, std::slice::from_ref(&e), [3; 16]).unwrap()[1]
            .is_none()
    );
    let mut other = e.clone();
    other.touches[0].object_id = 42;
    assert!(attest_write_origins(&[table()], &b, &[other], [3; 16])
        .unwrap()
        .iter()
        .all(Option::is_none));
    let mut other = e;
    other.scn = 9;
    assert!(attest_write_origins(&[table()], &b, &[other], [3; 16])
        .unwrap()
        .iter()
        .all(Option::is_none));
}
#[test]
fn every_transaction_and_request_must_attest_the_key_for_the_same_file() {
    let mut b = batch();
    b.ops.truncate(1);
    b.key_transactions.truncate(1);
    b.key_transactions[0].xids = vec![vec![1], vec![2]];
    let first = event();
    let mut second = event();
    second.xid = vec![2];
    second.request_id = [5; 32];
    let events = [first.clone(), second.clone()];
    assert_eq!(
        attest_write_origins(&[table()], &b, &events, [3; 16]).unwrap()[0]
            .as_ref()
            .unwrap()
            .request_ids,
        vec![[4; 32], [5; 32]]
    );
    assert!(
        attest_write_origins(&[table()], &b, std::slice::from_ref(&first), [3; 16]).unwrap()[0]
            .is_none()
    );
    second.file_incarnation = [9; 16];
    assert!(
        attest_write_origins(&[table()], &b, &[first.clone(), second], [3; 16]).unwrap()[0]
            .is_none()
    );
    b.key_transactions[0].xids = vec![vec![1]];
    let mut another = first.clone();
    another.request_id = [6; 32];
    another.touches.clear();
    assert!(
        attest_write_origins(&[table()], &b, &[first.clone(), another], [3; 16]).unwrap()[0]
            .is_none()
    );
    b.key_transactions[0].complete = false;
    assert!(
        attest_write_origins(&[table()], &b, std::slice::from_ref(&first), [3; 16]).unwrap()[0]
            .is_none()
    );
    assert!(attest_write_origins(&[table()], &b, &[first.clone(), first], [3; 16]).is_err());
}
