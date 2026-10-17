#![cfg(feature = "oracle-source")]
use mpedb_mirror::{oracle_source::OracleSource, sourcecfg, NetOp, NetOpKind, SourceSpec};
use mpedb_types::Value;
mod oracle_support;
#[test]
#[ignore = "requires Oracle application test owner and native client libraries"]
fn native_oracle_zoned_timestamps_and_intervals_write_back_without_losing_carriers() {
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
    c.execute("ALTER SESSION SET TIME_ZONE='+00:00'", &[])
        .unwrap();
    c.execute("BEGIN EXECUTE IMMEDIATE 'DROP TABLE MPEDB_TEMPORAL_WRITE PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;",&[]).unwrap();
    c.execute("CREATE TABLE MPEDB_TEMPORAL_WRITE(id NUMBER PRIMARY KEY,zoned TIMESTAMP(9) WITH TIME ZONE,local_zoned TIMESTAMP(9) WITH LOCAL TIME ZONE,year_month INTERVAL YEAR(9) TO MONTH,day_second INTERVAL DAY(9) TO SECOND(9),short_year INTERVAL YEAR(2) TO MONTH,short_day INTERVAL DAY(2) TO SECOND(3))", &[]).unwrap();
    for (id, timestamp) in [
        (1i64, "2026-10-25 02:30:00.123456789 Europe/Oslo CEST"),
        (2, "2026-10-25 02:30:00.123456789 Europe/Oslo CET"),
        (3, "2026-10-17 03:04:05.999999999 Asia/Kathmandu +0545"),
    ] {
        let timestamp = if id == 3 {
            "TO_TIMESTAMP_TZ('2026-10-17 03:04:05.999999999 +05:45','YYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM')".to_string()
        } else {
            format!("TO_TIMESTAMP_TZ('{timestamp}','YYYY-MM-DD HH24:MI:SS.FF9 TZR TZD')")
        };
        c.execute(&format!("INSERT INTO MPEDB_TEMPORAL_WRITE VALUES(:1,{timestamp},{timestamp},TO_YMINTERVAL('-123456789-11'),TO_DSINTERVAL('+123456789 23:59:59.123456789'),TO_YMINTERVAL('+12-03'),TO_DSINTERVAL('-12 03:04:05.123'))"), &[&id]).unwrap();
    }
    c.execute("INSERT INTO MPEDB_TEMPORAL_WRITE(id) VALUES(4)", &[])
        .unwrap();
    c.commit().unwrap();
    let mut source = OracleSource::connect(&spec).unwrap();
    let tables = source
        .introspect(Some(&["MPEDB_TEMPORAL_WRITE".into()]), &[])
        .unwrap();
    let (_, mut snapshots) = oracle_support::initial_snapshot(&source, &tables);
    let original = snapshots.remove(0);
    assert_eq!(original.len(), 4);
    assert!(matches!(&original[0][1],Value::Text(s) if s.ends_with("+02:00 EUROPE/OSLO CEST")));
    assert!(matches!(&original[1][1],Value::Text(s) if s.ends_with("+01:00 EUROPE/OSLO CET")));
    assert!(original[3][1..].iter().all(|v| *v == Value::Null));
    c.execute("DELETE FROM MPEDB_TEMPORAL_WRITE", &[]).unwrap();
    c.commit().unwrap();
    let ops: Vec<_> = original
        .iter()
        .map(|row| NetOp {
            table_id: 0,
            pk: vec![row[0].clone()],
            kind: NetOpKind::Upsert(row.clone()),
        })
        .collect();
    for (i, op) in ops.iter().enumerate() {
        source
            .write_batch(&tables, std::slice::from_ref(op))
            .unwrap_or_else(|e| panic!("temporal row {i}: {e}"));
    }
    for _ in 0..2 {
        let actual = source.write_batch(&tables, &ops).unwrap();
        assert_eq!(
            actual,
            original.iter().cloned().map(Some).collect::<Vec<_>>()
        );
    }
    let mut changed = original[0].clone();
    changed[3] = Value::Text("+000000001-02".into());
    let mut rounded = original[1].clone();
    let Value::Text(short) = &mut rounded[6] else {
        panic!("interval required")
    };
    short.push_str("456789");
    let batch = [
        NetOp {
            table_id: 0,
            pk: vec![changed[0].clone()],
            kind: NetOpKind::Upsert(changed),
        },
        NetOp {
            table_id: 0,
            pk: vec![rounded[0].clone()],
            kind: NetOpKind::Upsert(rounded),
        },
    ];
    assert!(
        source.write_batch(&tables, &batch).is_err(),
        "fractional truncation must reject the whole batch"
    );
    assert_eq!(
        source
            .read_snapshot(&tables[0], source.current_scn().unwrap())
            .unwrap(),
        original
    );
    for (find, replace) in [("CEST", "CET"), ("+02:00", "+03:00")] {
        let mut inconsistent = original[0].clone();
        let Value::Text(zone) = &mut inconsistent[1] else {
            panic!("zoned carrier required")
        };
        *zone = zone.replace(find, replace);
        let op = NetOp {
            table_id: 0,
            pk: vec![inconsistent[0].clone()],
            kind: NetOpKind::Upsert(inconsistent),
        };
        assert!(
            source.write_batch(&tables, &[op]).is_err(),
            "inconsistent offset/TZD must not be silently corrected"
        );
        assert_eq!(
            source
                .read_snapshot(&tables[0], source.current_scn().unwrap())
                .unwrap(),
            original
        );
    }
    c.execute("DROP TABLE MPEDB_TEMPORAL_WRITE PURGE", &[])
        .unwrap();
}
