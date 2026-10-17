#![cfg(feature = "oracle-source")]
use mpedb_mirror::{oracle_source::OracleSource, sourcecfg, SourceSpec};

#[test]
#[ignore = "requires MPEDB_ORACLE_SOURCE_CONFIG and Oracle client libraries"]
fn native_oracle_authority_state_initializes_once_cas_fences_and_reads_history() {
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
    let mut first = OracleSource::connect(&spec).unwrap();
    let mut second = OracleSource::connect(&spec).unwrap();
    let id = format!("state-{}-ø", std::process::id());
    let key = id.as_bytes().to_vec();
    assert_eq!(first.read_source_state(&id).unwrap(), None);
    first.ensure_source_state(&id, 1, "source").unwrap();
    first.ensure_source_state(&id, 0, "mpedb").unwrap();
    assert_eq!(
        first.read_source_state(&id).unwrap(),
        Some((1, "source".into()))
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let start = loop {
        let scn = first.current_scn().unwrap();
        match first.read_source_state_at(&id, scn) {
            Ok(Some((1, authority))) if authority == "source" => break scn,
            Err(e)
                if e.to_string().contains("ORA-01466") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250))
            }
            other => panic!("initial state snapshot failed: {other:?}"),
        }
    };
    assert!(!first.cas_source_state(&id, 0, 1, "mpedb").unwrap());
    assert!(first.cas_source_state(&id, 1, 2, "mpedb").unwrap());
    assert_eq!(
        second.read_source_state(&id).unwrap(),
        Some((2, "mpedb".into()))
    );
    assert!(first.cas_source_state(&id, 2, 3, "source").unwrap());
    assert!(!second.cas_source_state(&id, 2, 3, "mpedb").unwrap());
    assert_eq!(
        second.read_source_state(&id).unwrap(),
        Some((3, "source".into()))
    );
    assert_eq!(
        first.read_source_state_at(&id, start).unwrap(),
        Some((1, "source".into()))
    );
    assert!(first.cas_source_state(&id, u64::MAX, 0, "source").is_err());
    assert!(first.ensure_source_state(&id, 9, "invalid").is_err());
    // RAW mirror ids keep binary identity even under linguistic NLS settings.
    let upper = id.to_uppercase();
    let upper_key = upper.as_bytes().to_vec();
    second.ensure_source_state(&upper, 7, "mpedb").unwrap();
    assert_eq!(
        first.read_source_state(&id).unwrap(),
        Some((3, "source".into()))
    );
    assert_eq!(
        first.read_source_state(&upper).unwrap(),
        Some((7, "mpedb".into()))
    );
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            let mut source = OracleSource::connect(&spec).unwrap();
            barrier.wait();
            source.cas_source_state(&id, 3, 4, "source").unwrap()
        });
        let b = scope.spawn(|| {
            let mut source = OracleSource::connect(&spec).unwrap();
            barrier.wait();
            source.cas_source_state(&id, 3, 4, "mpedb").unwrap()
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(
        results.0, results.1,
        "exactly one competing source CAS may succeed"
    );
    let winner = if results.0 { "source" } else { "mpedb" };
    assert_eq!(
        first.read_source_state(&id).unwrap(),
        Some((4, winner.into()))
    );
    c.execute(
        "ALTER TABLE MPEDB_MIRROR_STATE DISABLE CONSTRAINT MPEDB_MIRROR_STATE_V2",
        &[],
    )
    .unwrap();
    assert!(first
        .read_source_state(&id)
        .unwrap_err()
        .to_string()
        .contains("ownership marker"));
    c.execute(
        "ALTER TABLE MPEDB_MIRROR_STATE ENABLE VALIDATE CONSTRAINT MPEDB_MIRROR_STATE_V2",
        &[],
    )
    .unwrap();
    assert_eq!(
        first.read_source_state(&id).unwrap(),
        Some((4, winner.into()))
    );
    c.execute("DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1", &[&key])
        .unwrap();
    c.execute(
        "DELETE FROM MPEDB_MIRROR_STATE WHERE mirror_id=:1",
        &[&upper_key],
    )
    .unwrap();
    c.commit().unwrap();
}
