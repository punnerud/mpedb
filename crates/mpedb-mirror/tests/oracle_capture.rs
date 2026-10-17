#![cfg(feature = "oracle-source")]
use mpedb_mirror::{adapter::NetOpKind, oracle_source::OracleSource, sourcecfg, SourceSpec};
use mpedb_types::Value;

#[test]
#[ignore = "requires MPEDB_ORACLE_SOURCE_CONFIG and Oracle client libraries"]
fn native_oracle_capture_keeps_commits_historical_images_and_compound_key_changes() {
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
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    let delayed = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    for name in ["MPEDB_CAPTURE_A", "MPEDB_CAPTURE_B"] {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {name} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"),&[]).unwrap();
    }
    c.execute("CREATE TABLE MPEDB_CAPTURE_A (id NUMBER(10), part VARCHAR2(8 CHAR), payload VARCHAR2(20 CHAR), PRIMARY KEY(id,part))",&[]).unwrap();
    c.execute(
        "CREATE INDEX MPEDB_CAPTURE_PAYLOAD ON MPEDB_CAPTURE_A(payload)",
        &[],
    )
    .unwrap();
    c.execute(
        "CREATE TABLE MPEDB_CAPTURE_B (id NUMBER(10) PRIMARY KEY, amount NUMBER)",
        &[],
    )
    .unwrap();
    c.execute("INSERT INTO MPEDB_CAPTURE_A VALUES(1,'ø','initial')", &[])
        .unwrap();
    c.execute("INSERT INTO MPEDB_CAPTURE_A VALUES(2,'delete','old')", &[])
        .unwrap();
    c.execute("INSERT INTO MPEDB_CAPTURE_B VALUES(1,0)", &[])
        .unwrap();
    c.commit().unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(
            Some(&["MPEDB_CAPTURE_A".into(), "MPEDB_CAPTURE_B".into()]),
            &[],
        )
        .unwrap();
    assert_eq!(tables[0].name, "MPEDB_CAPTURE_A");
    assert_eq!(tables[1].name, "MPEDB_CAPTURE_B");
    let mirror_id = format!("snapshot-{}", std::process::id());
    source.ensure_source_state(&mirror_id, 1, "source").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let bootstrap = loop {
        match source.snapshot_for_mirror(&tables, &mirror_id) {
            Ok(snapshot) => break snapshot,
            Err(e)
                if e.to_string().contains("ORA-01466") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            Err(e) => panic!("mirror bootstrap failed: {e}"),
        }
    };
    let from = bootstrap.scn;
    assert_eq!(bootstrap.source_state, Some((1, "source".into())));
    assert_eq!(
        bootstrap.rows.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(bootstrap.position().unwrap().scn(), from);
    let identity = source.identity().unwrap();
    assert_eq!(identity.owner, source.owner());
    assert_eq!(identity.session_user, source.owner());
    assert!(identity.database_id > 0);
    let position =
        mpedb_mirror::oracle_cursor::OraclePosition::new(&identity, &tables, from).unwrap();
    let reconnect = OracleSource::connect(&spec).unwrap();
    assert_eq!(reconnect.identity().unwrap(), identity);
    c.execute("UPDATE MPEDB_CAPTURE_A SET payload='first' WHERE id=1", &[])
        .unwrap();
    c.execute("INSERT INTO MPEDB_CAPTURE_A VALUES(3,'new','created')", &[])
        .unwrap();
    c.execute("UPDATE MPEDB_CAPTURE_B SET amount=100 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    delayed
        .execute(
            "UPDATE MPEDB_CAPTURE_A SET payload='pending' WHERE id=1",
            &[],
        )
        .unwrap();
    c.execute("UPDATE MPEDB_CAPTURE_B SET amount=200 WHERE id=1", &[])
        .unwrap();
    c.commit().unwrap();
    let mid = source.current_scn().unwrap();
    let prefix = source.read_changes(&tables, from, mid, 100).unwrap();
    assert_eq!(prefix.len(), 2, "uncommitted work must not appear");
    assert_eq!(
        prefix[0].ops.len(),
        3,
        "one commit across two tables stays together"
    );
    assert_eq!(prefix[0].xids.len(), 1);
    assert_eq!(prefix[1].ops.len(), 1);
    assert!(prefix[0].ops.iter().any(|op| op.table_id == 0
        && matches!(&op.kind,NetOpKind::Upsert(row) if row[2]==Value::Text("first".into()))));
    assert!(prefix[0].ops.iter().any(|op| op.table_id == 1
        && matches!(&op.kind,NetOpKind::Upsert(row) if row[1]==Value::Numeric("100".into()))));
    c.execute("UPDATE MPEDB_CAPTURE_A SET part='later' WHERE id=3", &[])
        .unwrap();
    c.execute("DELETE FROM MPEDB_CAPTURE_A WHERE id=2", &[])
        .unwrap();
    c.execute("INSERT INTO MPEDB_CAPTURE_B VALUES(4,400)", &[])
        .unwrap();
    c.commit().unwrap();
    delayed.commit().unwrap();
    c.execute(
        "UPDATE MPEDB_CAPTURE_A SET payload='rolled back' WHERE id=1",
        &[],
    )
    .unwrap();
    c.rollback().unwrap();
    let to = source.current_scn().unwrap();
    let all = source.read_changes(&tables, from, to, 100).unwrap();
    assert_eq!(all.len(), 4);
    assert!(all.windows(2).all(|w| w[0].scn < w[1].scn));
    assert!(all
        .iter()
        .all(|b| b.scn > from && b.scn <= to && b.xids.len() == 1));
    // Re-reading the first commit must not substitute the later current image.
    assert!(all[0].ops.iter().any(|op| op.table_id == 0
        && matches!(&op.kind,NetOpKind::Upsert(row) if row[2]==Value::Text("first".into()))));
    assert!(all[2].ops.iter().any(|op| op.table_id == 0
        && op.pk == vec![Value::Numeric("3".into()), Value::Text("new".into())]
        && matches!(op.kind, NetOpKind::Delete)));
    assert!(all[2].ops.iter().any(|op| op.table_id == 0
        && op.pk == vec![Value::Numeric("3".into()), Value::Text("later".into())]
        && matches!(&op.kind,NetOpKind::Upsert(row) if row[2]==Value::Text("created".into()))));
    assert!(all[2].ops.iter().any(|op| op.table_id == 0
        && op.pk[0] == Value::Numeric("2".into())
        && matches!(op.kind, NetOpKind::Delete)));
    assert!(all[3].ops.iter().any(|op| op.table_id == 0
        && matches!(&op.kind,NetOpKind::Upsert(row) if row[2]==Value::Text("pending".into()))));
    let mut reversed = tables.clone();
    reversed.reverse();
    assert_eq!(
        mpedb_mirror::oracle_cursor::OraclePosition::decode(
            &position.encode(),
            &identity,
            &reversed
        )
        .unwrap()
        .scn(),
        from
    );
    let mut changed_scope = tables.clone();
    changed_scope[0].ddl_source.push_str(" -- schema change");
    assert!(mpedb_mirror::oracle_cursor::OraclePosition::decode(
        &position.encode(),
        &identity,
        &changed_scope
    )
    .is_err());
    let reordered = source.read_changes(&reversed, from, to, 100).unwrap();
    assert_eq!(
        reordered.iter().map(|b| &b.ops).collect::<Vec<_>>(),
        all.iter().map(|b| &b.ops).collect::<Vec<_>>(),
        "table ids must follow the carrier schema, not metadata slice order"
    );
    let resumed = source.read_changes(&tables, all[0].scn, to, 100).unwrap();
    assert_eq!(resumed.len(), 3);
    assert_eq!(resumed[0].scn, all[1].scn);
    assert!(source
        .read_changes(&tables, from, to, 1)
        .unwrap_err()
        .to_string()
        .contains("event budget exceeded"));
    assert_eq!(
        source.read_changes(&tables, from, to, 100).unwrap().len(),
        4,
        "budget rejection must not consume history"
    );
    assert!(source
        .read_changes(&tables, to, to, 100)
        .unwrap()
        .is_empty());
    assert!(source.read_changes(&tables, to, from, 100).is_err());
    source
        .write_batch(
            &reversed,
            &[mpedb_mirror::adapter::NetOp {
                table_id: 1,
                pk: vec![Value::Numeric("1".into())],
                kind: NetOpKind::Upsert(vec![
                    Value::Numeric("1".into()),
                    Value::Numeric("999".into()),
                ]),
            }],
        )
        .unwrap();
    assert_eq!(
        c.query_row_as::<String>(
            "SELECT TO_CHAR(amount,'TM9') FROM MPEDB_CAPTURE_B WHERE id=1",
            &[]
        )
        .unwrap(),
        "999"
    );
    let mirror_key = mirror_id.as_bytes().to_vec();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&mirror_key],
    )
    .unwrap();
    c.commit().unwrap();
    for name in ["MPEDB_CAPTURE_A", "MPEDB_CAPTURE_B"] {
        c.execute(&format!("DROP TABLE {name} PURGE"), &[]).unwrap();
    }
}
