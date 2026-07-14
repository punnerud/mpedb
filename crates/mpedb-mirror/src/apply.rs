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

use mpedb::{Database, WriteSession};
use mpedb_core::cdc;
use mpedb_types::{keycode, Error, Result, Value};

use crate::adapter::{Cursor, NetOp, NetOpKind, PullBatch};
use crate::state::{self, ConflictKind, ParkRecord};

/// What an apply did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub upserts: u64,
    pub deletes: u64,
    /// Rows parked (a stricter mpedb rule / unique block rejected the source
    /// row) — kept out of the batch, local value preserved.
    pub parked: u64,
    /// Divergences resolved (a pulled PK also had a local unpushed change).
    pub conflicts: u64,
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// The `cdc` sys subkey (`d/…`) of a PK's dirty entry — strip the `cdc\0` prefix
/// off the full engine key so it can be read/deleted via the facade ns API.
fn dirty_subkey(table_id: u32, pk_keycode: &[u8]) -> Vec<u8> {
    cdc::dirty_key(table_id, pk_keycode)[4..].to_vec()
}

fn park_kind_for(e: &Error) -> ConflictKind {
    match e {
        Error::UniqueViolation { .. } => ConflictKind::UniqueBlocked,
        _ => ConflictKind::Validation,
    }
}

fn park(s: &mut WriteSession, kind: ConflictKind, op: &NetOp, now: i64) -> Result<()> {
    let kc = keycode::encode_key(&op.pk);
    let rec = ParkRecord {
        kind,
        wall_us: now,
        table_id: op.table_id,
        pk_keycode: kc.clone(),
    };
    s.sys_record_put(state::MIR_NS, &state::park_key(op.table_id, &kc), &rec.encode())
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
    let now = now_micros();

    // 3. DECIDE + DELETE phase. For each op: honour a manual-policy skip marker;
    //    on a divergence (a local unpushed dirty entry for this PK) resolve
    //    source-wins — audit it and clear the dirty entry so the local change is
    //    not pushed back. Delete tombstones and existing upsert PKs (capturing
    //    the old row so a parked insert can restore the local value, §5.4 /
    //    CONF#18), which also dissolves unique swaps order-free.
    let mut upserts: Vec<(usize, Option<Vec<Value>>)> = Vec::new(); // (op idx, old row)
    for (i, op) in batch.ops.iter().enumerate() {
        let kc = keycode::encode_key(&op.pk);

        // manual-policy skip: leave this PK at its local value, don't touch it
        if s.sys_record_get(state::MIR_NS, &state::skip_key(op.table_id, &kc))?.is_some() {
            continue;
        }

        // divergence: source-wins → keep an audit record and drop the local
        // dirty entry so it does not push back over the (source-won) row
        let dsub = dirty_subkey(op.table_id, &kc);
        if s.sys_record_get("cdc", &dsub)?.is_some() {
            park(&mut s, ConflictKind::Divergence, op, now)?;
            s.sys_record_delete("cdc", &dsub)?;
            stats.conflicts += 1;
        }

        match &op.kind {
            NetOpKind::Delete => {
                if s.delete_by_pk(op.table_id, &op.pk)? {
                    stats.deletes += 1;
                }
            }
            NetOpKind::Upsert(_) => {
                let old = s.get_by_pk(op.table_id, &op.pk)?;
                s.delete_by_pk(op.table_id, &op.pk)?;
                upserts.push((i, old));
            }
        }
    }

    // 4. INSERT phase. Each insert under its own savepoint: on failure (a
    //    stricter CHECK/NOT NULL/type rule or a unique block), roll it back,
    //    restore the pre-delete local row so it never vanishes, and PARK the
    //    offender — the batch keeps going instead of wedging on one bad row.
    for (i, old) in upserts {
        let NetOpKind::Upsert(row) = &batch.ops[i].kind else {
            unreachable!("upserts holds only upsert ops")
        };
        let sp = s.savepoint();
        match s.insert_row(batch.ops[i].table_id, row) {
            Ok(()) => stats.upserts += 1,
            Err(e) => {
                let kind = park_kind_for(&e);
                s.rollback_to(sp);
                if let Some(oldrow) = old {
                    let sp2 = s.savepoint();
                    if s.insert_row(batch.ops[i].table_id, &oldrow).is_err() {
                        s.rollback_to(sp2);
                    }
                }
                park(&mut s, kind, &batch.ops[i], now)?;
                stats.parked += 1;
            }
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
    fn unique_conflict_parks_and_the_batch_keeps_going() {
        let src_path = tmp("park-src", "db");
        let mpedb_path = tmp("park-mid", "mpedb");
        {
            let c = Connection::open(&src_path).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE);
                 INSERT INTO t VALUES (1,'a'),(2,'b');",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src_path).unwrap();
            import_sqlite(&mut c, &mpedb_path, &ImportOptions::default())
                .unwrap()
                .0
        };

        // a LOCAL mpedb row takes the unique email 'x'
        db.query("INSERT INTO t (id,email) VALUES ($1,$2)", &[Value::Int(10), Value::Text("x".into())])
            .unwrap();

        // the source adds a DIFFERENT PK with the same email (conflicts) plus a
        // clean row — both arrive in one pull batch
        let mut adapter =
            SqliteAdapter::new(Connection::open(&src_path).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();
        adapter
            .conn()
            .execute_batch("INSERT INTO t VALUES (5,'x'), (6,'y');")
            .unwrap();

        let from = adapter.zero_cursor();
        let batch = adapter.pull(&from, 1000).unwrap().unwrap();
        let stats = apply_batch(&db, &from, &batch).unwrap();
        assert_eq!(stats.parked, 1); // id=5 (email 'x' is taken by local id=10)
        assert!(stats.upserts >= 1); // id=6 still applied — batch did not wedge

        // id=6 landed, id=5 did NOT, local id=10 is intact
        let ids: Vec<i64> = rows(&db, "SELECT id FROM t", &[])
            .iter()
            .map(|r| match r[0] {
                Value::Int(i) => i,
                _ => panic!(),
            })
            .collect();
        assert!(ids.contains(&6), "clean row applied");
        assert!(!ids.contains(&5), "conflicting row parked");
        assert!(ids.contains(&10), "local row preserved");

        // a park record exists for the conflict
        let parks: Vec<_> = db
            .sys_record_scan("mir")
            .unwrap()
            .into_iter()
            .filter(|(k, _)| k.starts_with(b"park/"))
            .collect();
        assert_eq!(parks.len(), 1);
        assert_eq!(
            state::ParkRecord::decode(&parks[0].1).unwrap().kind,
            state::ConflictKind::UniqueBlocked
        );

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
