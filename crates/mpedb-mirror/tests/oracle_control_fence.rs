#![cfg(feature = "oracle-source")]
use mpedb_mirror::{
    import_oracle, load_oracle_bootstrap, oracle_source::OracleSource, sourcecfg, state,
    ImportOptions, OracleAdapter, SourceAdapter, SourceSpec,
};
#[test]
#[ignore = "requires a dedicated MPEDB_ORACLE_FENCE test owner and private config"]
fn native_oracle_control_and_authority_row_recreation_invalidate_old_mirrors() {
    let config = std::env::var("MPEDB_ORACLE_FENCE_SOURCE_CONFIG")
        .expect("dedicated destructive-test config required");
    let spec = sourcecfg::load(std::path::Path::new(&config)).unwrap();
    let SourceSpec::Oracle {
        connect_string,
        username,
        password,
    } = &spec
    else {
        panic!("Oracle required")
    };
    assert_eq!(
        username, "MPEDB_ORACLE_FENCE",
        "never truncate a shared application control table"
    );
    let c = oracle_client::Connection::connect(username, password, connect_string).unwrap();
    for table in ["MPEDB_FENCE_DATA", "MPEDB_MIRROR_STATE"] {
        c.execute(&format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table} PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;"),&[]).unwrap();
    }
    c.execute("CREATE TABLE MPEDB_FENCE_DATA(id NUMBER PRIMARY KEY)", &[])
        .unwrap();
    c.execute("INSERT INTO MPEDB_FENCE_DATA VALUES(1)", &[])
        .unwrap();
    c.commit().unwrap();
    let path = mpedb_testkit::scratch_base()
        .join(format!("oracle-control-fence-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let opts = ImportOptions {
        include: Some(vec!["MPEDB_FENCE_DATA".into()]),
        size_bytes: 64 * 1024 * 1024,
        ..Default::default()
    };
    for mutation in ["truncate", "recreate", "row"] {
        let mut source = OracleSource::connect(&spec).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let (db, _) = loop {
            match import_oracle(&mut source, &path, &opts) {
                Ok(v) => break v,
                Err(e)
                    if e.to_string().contains("ORA-01466")
                        && !path.exists()
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(250))
                }
                Err(e) => panic!("{e}"),
            }
        };
        let metadata = load_oracle_bootstrap(&db).unwrap();
        let mut adapter = OracleAdapter::from_database(source, &db).unwrap();
        let from = adapter.zero_cursor();
        assert!(adapter
            .cas_source_state(&metadata.mirror_id, 1, 2, "source")
            .unwrap());
        assert!(!adapter
            .cas_source_state(&metadata.mirror_id, 1, 2, "source")
            .unwrap());

        match mutation {
            "recreate" => {
                c.execute("DROP TABLE MPEDB_MIRROR_STATE PURGE", &[])
                    .unwrap();
            }
            "truncate" => {
                c.execute("TRUNCATE TABLE MPEDB_MIRROR_STATE", &[]).unwrap();
            }
            _ => {
                c.execute(
                    "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
                    &[&metadata.mirror_id.as_bytes()],
                )
                .unwrap();
                c.commit().unwrap();
                assert!(adapter
                    .ensure_source_state(&metadata.mirror_id, 1, "source")
                    .is_err());
                assert_eq!(
                    c.query_row_as::<u64>(
                        "SELECT COUNT(*) FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
                        &[&metadata.mirror_id.as_bytes()]
                    )
                    .unwrap(),
                    0
                );
            }
        }
        let mut fresh = OracleSource::connect(&spec).unwrap();
        fresh
            .ensure_source_state(&metadata.mirror_id, 1, "source")
            .unwrap();
        assert_eq!(
            fresh.read_source_state(&metadata.mirror_id).unwrap(),
            Some((1, "source".into()))
        );
        assert!(fresh
            .cas_source_state_fenced(
                &metadata.mirror_id,
                1,
                2,
                "source",
                &metadata.state_identity
            )
            .unwrap_err()
            .to_string()
            .contains("epoch CAS refused"));
        assert_eq!(
            fresh.read_source_state(&metadata.mirror_id).unwrap(),
            Some((1, "source".into()))
        );
        let identity = fresh.source_state_identity(&metadata.mirror_id).unwrap();
        if mutation == "recreate" {
            assert_ne!(identity.object_id, metadata.state_identity.object_id);
        } else {
            assert_eq!(identity.object_id, metadata.state_identity.object_id);
        }
        if mutation == "row" {
            assert_eq!(
                identity.data_object_id,
                metadata.state_identity.data_object_id
            );
        } else {
            assert_ne!(
                identity.data_object_id,
                metadata.state_identity.data_object_id
            );
        }
        assert_ne!(identity.incarnation, metadata.state_identity.incarnation);
        assert!(adapter
            .pull(&from, 16)
            .unwrap_err()
            .to_string()
            .contains("incarnation changed"));
        assert!(adapter.head().is_err());
        assert!(adapter
            .cas_source_state(&metadata.mirror_id, 1, 2, "source")
            .is_err());
        assert!(OracleAdapter::from_database(fresh, &db).is_err());
        assert_eq!(
            db.sys_record_get(state::MIR_NS, state::KEY_CUR)
                .unwrap()
                .unwrap(),
            from
        );
        drop(adapter);
        drop(db);
        std::fs::remove_file(&path).unwrap();
    }
    for table in ["MPEDB_FENCE_DATA", "MPEDB_MIRROR_STATE"] {
        c.execute(&format!("DROP TABLE {table} PURGE"), &[])
            .unwrap();
    }
}
