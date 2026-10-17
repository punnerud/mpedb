//! Inverse of the import's integer carriers, without float8 epoch conversion.
pub(super) fn bind_expr(kind: &str, param: &str) -> String {
    match kind {
        "date" => format!("(date '1970-01-01' + {param}::int)"),
        "time" => format!("(CASE WHEN {param}::bigint=86400000000 THEN time '24:00:00' ELSE time '00:00:00' + {param}::bigint * interval '1 microsecond' END)"),
        "timestamp" | "timestamptz" => {
            // Interval multiplication uses float8. Split at whole days so both
            // the day count and the sub-day microseconds remain exact integers
            // below 2^53. A naive timestamp epoch avoids session TZ/DST shifts.
            let naive = format!("(timestamp '1970-01-01' + ({param}::bigint / 86400000000)::int * interval '1 day' + ({param}::bigint % 86400000000) * interval '1 microsecond')");
            if kind == "timestamptz" { format!("({naive} AT TIME ZONE 'UTC')") } else { naive }
        }
        _ => unreachable!("temporal binding type checked by caller"),
    }
}
#[cfg(test)]
mod tests {
    use crate::{import_pg, push_batch, verify, ImportOptions, PgAdapter, SourceAdapter};
    use mpedb_types::Value;
    #[test]
    #[ignore = "requires PostgreSQL server binaries; run with --ignored"]
    fn temporal_carriers_preserve_days_microseconds_timezones_and_end_of_day() {
        let pg = crate::pg_harness::ThrowawayPg::start();
        let mut c = pg.client();
        c.batch_execute("SET TIME ZONE 'Asia/Kathmandu';
            CREATE TABLE temporal_case(id bigint PRIMARY KEY,plain_stamp timestamp,zoned_stamp timestamptz,calendar_day date,clock time,revision int DEFAULT 0);
            INSERT INTO temporal_case(id,plain_stamp,zoned_stamp,calendar_day,clock) VALUES
            (1,'2026-10-17 03:04:05.123456','2026-10-17 03:04:05.123456+05:45','2026-10-17','23:59:59.999999'),
            (2,'9999-12-31 23:59:59.999999','9999-12-31 23:59:59.999999+00','9999-12-31','24:00:00'),
            (3,'0001-01-01 00:00:00.000001','0001-01-01 00:00:00.000001+00','0001-01-01','00:00:00.000001'),
            (4,'0044-03-15 12:34:56.654321 BC',NULL,'0044-03-15 BC','12:34:56.654321'),
            (5,NULL,NULL,NULL,NULL),
            (6,'1969-12-31 23:59:59.999999','1969-12-31 23:59:59.999999+00','1969-12-31','00:00:00');").unwrap();
        let path =
            mpedb_testkit::scratch_base().join(format!("pg-temporal-{}.mpedb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (db, _) = import_pg(
            &mut c,
            &path,
            &ImportOptions {
                size_bytes: 64 * 1024 * 1024,
                ..Default::default()
            },
        )
        .unwrap();
        let mut adapter = PgAdapter::new(pg.client(), None, &[]).unwrap();
        adapter
            .client()
            .batch_execute("SET TIME ZONE 'America/New_York'")
            .unwrap();
        assert!(verify(&db, &mut adapter).unwrap());
        for revision in [1, 2] {
            db.query(
                "UPDATE temporal_case SET revision=$1",
                &[Value::Int(revision)],
            )
            .unwrap();
            let stats = push_batch(&db, &mut adapter).unwrap();
            assert_eq!((stats.upserts, stats.conflicts), (6, 0));
            let local = match db
                .query("SELECT * FROM temporal_case ORDER BY id", &[])
                .unwrap()
            {
                mpedb::ExecResult::Rows { rows, .. } => rows,
                _ => panic!("rows"),
            };
            let remote = adapter.read_table_rows(0).unwrap();
            assert_eq!(
                local, remote,
                "temporal carriers must reach a byte-stable fixpoint in every session timezone"
            );
            assert!(verify(&db, &mut adapter).unwrap());
        }
        db.verify().unwrap();
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}
