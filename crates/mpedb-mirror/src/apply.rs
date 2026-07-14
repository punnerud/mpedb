//! The pull apply transaction (DESIGN-MIRROR §5.4) — the core correctness
//! piece. A [`PullBatch`] is applied to mpedb in ONE capture-suppressed
//! WriteSession so the row writes and the cursor advance commit atomically in a
//! single meta flip.
//!
//! Structure (§5.4):
//!   1. cursor guard  — `mir\0cur` must equal the cursor the batch started from
//!      (defends against a second daemon or a stale batch);
//!   2. epoch guard   — abort if the mirror is frozen (the full epoch CAS lands
//!      with the switch machinery in M6);
//!   3. DELETE phase  — delete every tombstone PK and every upserted PK that
//!      currently exists (so the insert phase is clean and unique-value swaps /
//!      PK changes dissolve order-free);
//!   4. INSERT phase  — insert each upserted row;
//!   5. advance `mir\0cur` in the same txn, then commit.
//!
//! Capture is suppressed for the whole apply, so the applier's own writes are
//! not self-captured (which would echo back on the next push — §3.8).
//!
//! DECIDE-before-mutate parking (per-op CHECK/unique-blocked handling that keeps
//! the batch going) needs the conflict engine and lands in M7; here a failing
//! op rolls the WHOLE batch back (safe — no torn state — and idempotently
//! retryable), never a half-applied commit.

use mpedb::Database;
use mpedb_types::{Error, Result};

use crate::adapter::{Cursor, NetOpKind, PullBatch};
use crate::state;

/// What an apply did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub upserts: u64,
    pub deletes: u64,
}

/// Apply `batch` (pulled starting from cursor `from`) to `db`. Advances
/// `mir\0cur` to `batch.end_cursor` atomically with the row writes.
pub fn apply_batch(db: &Database, from: &Cursor, batch: &PullBatch) -> Result<ApplyStats> {
    let mut s = db.begin()?;
    s.set_capture(false); // replication plane: do not self-capture (§3.8)

    // 1. cursor guard — bootstrap-accept when no cursor is stored yet
    match s.sys_record_get(state::MIR_NS, state::KEY_CUR)? {
        Some(stored) if stored != *from => {
            s.rollback();
            return Err(Error::Unsupported(
                "mirror cursor moved since this batch was pulled; retry".into(),
            ));
        }
        _ => {}
    }

    // 2. epoch guard — refuse while frozen (full CAS in M6)
    if let Some(bytes) = s.sys_record_get(state::MIR_NS, state::KEY_EPOCH)? {
        let epoch = state::Epoch::decode(&bytes)?;
        if epoch.frozen {
            s.rollback();
            return Err(Error::Frozen { table_id: 0 });
        }
    }

    let mut stats = ApplyStats::default();

    // 3. DELETE phase: tombstones + existing upsert PKs
    for op in &batch.ops {
        match &op.kind {
            NetOpKind::Delete => {
                if s.delete_by_pk(op.table_id, &op.pk)? {
                    stats.deletes += 1;
                }
            }
            NetOpKind::Upsert(_) => {
                // remove any current row so the insert below is a clean insert
                s.delete_by_pk(op.table_id, &op.pk)?;
            }
        }
    }

    // 4. INSERT phase: upserted rows
    for op in &batch.ops {
        if let NetOpKind::Upsert(row) = &op.kind {
            s.insert_row(op.table_id, row)?;
            stats.upserts += 1;
        }
    }

    // 5. advance the cursor in the same txn, then commit
    s.sys_record_put(state::MIR_NS, state::KEY_CUR, &batch.end_cursor)?;
    s.commit()?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::SourceAdapter;
    use crate::import::{import_sqlite, ImportOptions};
    use crate::SqliteAdapter;
    use mpedb::ExecResult;
    use mpedb_types::Value;
    use rusqlite::Connection;

    fn tmp(name: &str, ext: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("{name}-{}.{ext}", std::process::id()));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        p
    }

    fn rows(db: &Database, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
        match db.query(sql, params).unwrap() {
            ExecResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    fn pull_apply_propagates_source_changes_to_mpedb() {
        let src_path = tmp("apply-src", "db");
        let mpedb_path = tmp("apply-mid", "mpedb");
        {
            let c = Connection::open(&src_path).unwrap();
            c.execute_batch(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL, age INTEGER);
                 INSERT INTO users VALUES (1,'a@x',30),(2,'b@x',40);",
            )
            .unwrap();
        }

        // import the initial state
        let db = {
            let mut c = Connection::open(&src_path).unwrap();
            import_sqlite(&mut c, &mpedb_path, &ImportOptions::default())
                .unwrap()
                .0
        };
        assert_eq!(rows(&db, "SELECT id FROM users", &[]).len(), 2);

        // now start tracking and make concurrent source changes
        let mut adapter = SqliteAdapter::new(Connection::open(&src_path).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();
        adapter
            .conn()
            .execute_batch(
                "UPDATE users SET age=31 WHERE id=1;   -- change
                 INSERT INTO users VALUES (3,'c@x',50); -- new row
                 DELETE FROM users WHERE id=2;",        // gone
            )
            .unwrap();

        // pull + apply
        let from = adapter.zero_cursor();
        let batch = adapter.pull(&from, 1000).unwrap().unwrap();
        let stats = apply_batch(&db, &from, &batch).unwrap();
        assert_eq!(stats.upserts, 2); // id=1 (updated) + id=3 (new)
        assert_eq!(stats.deletes, 1); // id=2

        // mpedb now mirrors the source
        let ids: Vec<i64> = rows(&db, "SELECT id FROM users", &[])
            .iter()
            .map(|r| match r[0] {
                Value::Int(i) => i,
                _ => panic!(),
            })
            .collect();
        assert_eq!({ let mut v = ids.clone(); v.sort(); v }, vec![1, 3]);
        let age1 = rows(&db, "SELECT age FROM users WHERE id=$1", &[Value::Int(1)]);
        assert_eq!(age1[0][0], Value::Int(31));

        // a second pull from the advanced cursor is empty (idempotent follow)
        assert!(adapter.pull(&batch.end_cursor, 1000).unwrap().is_none());

        for p in [src_path, mpedb_path] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn stale_cursor_is_rejected() {
        let src_path = tmp("apply-stale-src", "db");
        let mpedb_path = tmp("apply-stale-mid", "mpedb");
        {
            let c = Connection::open(&src_path).unwrap();
            c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);")
                .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src_path).unwrap();
            import_sqlite(&mut c, &mpedb_path, &ImportOptions::default())
                .unwrap()
                .0
        };
        let mut adapter = SqliteAdapter::new(Connection::open(&src_path).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();
        adapter.conn().execute_batch("INSERT INTO t VALUES (1,1);").unwrap();

        let from = adapter.zero_cursor();
        let batch = adapter.pull(&from, 1000).unwrap().unwrap();
        // first apply advances the cursor
        apply_batch(&db, &from, &batch).unwrap();
        // re-applying from the OLD cursor must be rejected (cursor moved)
        assert!(apply_batch(&db, &from, &batch).is_err());

        for p in [src_path, mpedb_path] {
            let _ = std::fs::remove_file(p);
        }
    }
}
