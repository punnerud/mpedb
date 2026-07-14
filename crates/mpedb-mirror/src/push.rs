//! The write-back push (DESIGN-MIRROR §6): drain mpedb's local CDC dirty-set to
//! the source. Scan `cdc\0d/` at a brief snapshot, read each upserted row's
//! current image, RELEASE the writer lock, apply to the source (echo-tagged),
//! then clear the dirty entries that were not re-dirtied since (so a concurrent
//! local write is never lost — §6 step 4).
//!
//! v1 push is last-writer-wins from mpedb; the high-water CAS + epoch fencing
//! come with the switch machinery (M6), and per-op source-conflict resolution
//! with M7. Draining the whole dirty-set each round avoids the bounded-batch
//! high-water hazard the review flagged (CONF#0/15).

use mpedb::Database;
use mpedb_core::{DirtyEntry, DirtyOp};
use mpedb_types::{keycode, Error, Result};

use crate::adapter::{NetOp, NetOpKind, SourceAdapter};
use crate::state::{self, Authority, ConflictKind, Epoch, ParkRecord};

/// What a push did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PushStats {
    pub upserts: u64,
    pub deletes: u64,
    /// Ops the source concurrently won (write-write conflict, §6): parked
    /// `push_rejected`, dirty entry kept for the next pull to resolve source-wins.
    pub conflicts: u64,
}

/// A scanned dirty entry carried from the snapshot phase to the clear/park phase.
struct Pending {
    subkey: Vec<u8>,
    last_txn: u64,
    table_id: u32,
    pk_keycode: Vec<u8>,
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Push all pending local changes to the source. No-op when the dirty-set is
/// empty.
pub fn push_batch<A: SourceAdapter>(db: &Database, adapter: &mut A) -> Result<PushStats> {
    let schema = db.schema().clone();

    // 1. scan the dirty-set + read upsert images at a brief snapshot, then drop
    //    the writer lock before any (slow) source I/O.
    let mut ops: Vec<NetOp> = Vec::new();
    let mut pending: Vec<Pending> = Vec::new();
    {
        let mut s = db.begin()?;
        s.set_capture(false);
        for (subkey, val) in s.sys_record_scan_range("cdc", b"d/", b"d0")? {
            if subkey.len() < 6 {
                return Err(Error::Corrupt("short cdc dirty key".into()));
            }
            let de = DirtyEntry::decode(&val)?;
            let table_id = u32::from_be_bytes(subkey[2..6].try_into().unwrap());
            let tdef = schema
                .tables
                .get(table_id as usize)
                .ok_or_else(|| Error::Corrupt(format!("dirty entry for unknown table {table_id}")))?;
            let pk = keycode::decode_key(&de.pk_keycode, &tdef.pk_types())?;
            let kind = match de.op {
                DirtyOp::Upsert => match s.get_by_pk(table_id, &pk)? {
                    Some(row) => NetOpKind::Upsert(row),
                    None => NetOpKind::Delete, // dirtied upsert but row is gone now
                },
                DirtyOp::Delete => NetOpKind::Delete,
            };
            ops.push(NetOp { table_id, pk, kind });
            pending.push(Pending {
                subkey,
                last_txn: de.last_txn,
                table_id,
                pk_keycode: de.pk_keycode,
            });
        }
        s.rollback(); // read-only; releases the lock
    }
    if ops.is_empty() {
        return Ok(PushStats::default());
    }

    // 2. apply to the source. When the source holds authority (S2/S4, the
    //    default after import), write-back is source-wins: detect a concurrent
    //    source write on the same PK and leave that op un-applied (§6, §8). When
    //    mpedb holds authority (S6/S7 drain), local-wins → unconditional push.
    let source_authoritative = match db.sys_record_get(state::MIR_NS, state::KEY_EPOCH)? {
        Some(b) => Epoch::decode(&b)?.authority == Authority::Source,
        None => true,
    };
    let applied: Vec<bool> = if source_authoritative {
        let from = db
            .sys_record_get(state::MIR_NS, state::KEY_CUR)?
            .unwrap_or_else(|| adapter.zero_cursor());
        adapter.push_checked(&from, &ops)?
    } else {
        adapter.push(&ops)?;
        vec![true; ops.len()]
    };
    if applied.len() != ops.len() {
        return Err(Error::Corrupt("push_checked returned a mis-sized result".into()));
    }

    // 3. tally, clear the applied dirty entries, and park the rejected ones —
    //    all in one capture-suppressed txn. An applied entry is cleared ONLY if
    //    it was not re-dirtied since the scan (a concurrent local write bumps
    //    last_txn and must survive to push next round — §6 step 4). A rejected
    //    entry is KEPT: the next pull carries the winning source row and resolves
    //    it source-wins (which also deletes the dirty entry).
    let mut stats = PushStats::default();
    let now = now_micros();
    {
        let mut s = db.begin()?;
        s.set_capture(false);
        for (i, op) in ops.iter().enumerate() {
            let p = &pending[i];
            if applied[i] {
                match op.kind {
                    NetOpKind::Upsert(_) => stats.upserts += 1,
                    NetOpKind::Delete => stats.deletes += 1,
                }
                if let Some(cur) = s.sys_record_get("cdc", &p.subkey)? {
                    if DirtyEntry::decode(&cur)?.last_txn == p.last_txn {
                        s.sys_record_delete("cdc", &p.subkey)?;
                    }
                }
            } else {
                stats.conflicts += 1;
                let rec = ParkRecord {
                    kind: ConflictKind::PushRejected,
                    wall_us: now,
                    table_id: p.table_id,
                    pk_keycode: p.pk_keycode.clone(),
                };
                s.sys_record_put(
                    state::MIR_NS,
                    &state::park_key(p.table_id, &p.pk_keycode),
                    &rec.encode(),
                )?;
            }
        }
        s.commit()?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::SourceAdapter;
    use crate::import::{import_sqlite, ImportOptions};
    use crate::SqliteAdapter;
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

    #[test]
    fn sqlite_push_writes_local_changes_back_and_suppresses_echo() {
        let src_path = tmp("push-src", "db");
        let mpedb_path = tmp("push-mid", "mpedb");
        {
            let c = Connection::open(&src_path).unwrap();
            c.execute_batch(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, age INTEGER);
                 INSERT INTO users VALUES (1,'a@x',30),(2,'b@x',40),(3,'c@x',50);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src_path).unwrap();
            import_sqlite(&mut c, &mpedb_path, &ImportOptions::default())
                .unwrap()
                .0
        };

        // LOCAL mpedb changes (captured — cdc\0tabs was enabled at import)
        db.query("UPDATE users SET age=$1 WHERE id=$2", &[Value::Int(99), Value::Int(1)])
            .unwrap();
        db.query(
            "INSERT INTO users (id,email,age) VALUES ($1,$2,$3)",
            &[Value::Int(5), Value::Text("e@x".into()), Value::Int(60)],
        )
        .unwrap();
        db.query("DELETE FROM users WHERE id=$1", &[Value::Int(2)]).unwrap();

        // adapter over the source, tracking installed so echo tagging works
        let mut adapter =
            SqliteAdapter::new(Connection::open(&src_path).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();

        let stats = push_batch(&db, &mut adapter).unwrap();
        assert_eq!(stats.upserts, 2); // id=1 updated, id=5 new
        assert_eq!(stats.deletes, 1); // id=2 gone

        // the source now reflects the local mpedb state
        let age1: i64 = adapter
            .conn()
            .query_row("SELECT age FROM users WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(age1, 99);
        let n5: i64 = adapter
            .conn()
            .query_row("SELECT COUNT(*) FROM users WHERE id=5", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n5, 1);
        let n2: i64 = adapter
            .conn()
            .query_row("SELECT COUNT(*) FROM users WHERE id=2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n2, 0);

        // dirty-set cleared → a second push is a no-op
        assert_eq!(push_batch(&db, &mut adapter).unwrap(), PushStats::default());

        // echo suppression: our pushed writes are origin-tagged, so a pull sees
        // nothing (no infinite loop back into mpedb)
        let from = adapter.zero_cursor();
        assert!(adapter.pull(&from, 1000).unwrap().is_none());

        for p in [src_path, mpedb_path] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn sqlite_push_conflict_source_wins_parks_and_next_pull_converges() {
        let src_path = tmp("pushconf-src", "db");
        let mpedb_path = tmp("pushconf-mid", "mpedb");
        {
            let c = Connection::open(&src_path).unwrap();
            c.execute_batch(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL, age INTEGER);
                 INSERT INTO users VALUES (1,'a@x',30),(2,'b@x',40);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src_path).unwrap();
            import_sqlite(&mut c, &mpedb_path, &ImportOptions::default())
                .unwrap()
                .0
        };
        let mut adapter =
            SqliteAdapter::new(Connection::open(&src_path).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();

        // CONCURRENT source write on id=1 (after our cursor) — origin NULL.
        adapter
            .conn()
            .execute_batch("UPDATE users SET age=100 WHERE id=1;")
            .unwrap();

        // LOCAL mpedb writes: id=1 collides with the source write; id=3 does not.
        db.query("UPDATE users SET age=$1 WHERE id=$2", &[Value::Int(99), Value::Int(1)])
            .unwrap();
        db.query(
            "INSERT INTO users (id,email,age) VALUES ($1,$2,$3)",
            &[Value::Int(3), Value::Text("c@x".into()), Value::Int(50)],
        )
        .unwrap();

        let stats = push_batch(&db, &mut adapter).unwrap();
        assert_eq!(stats.upserts, 1, "only the non-conflicting id=3 lands");
        assert_eq!(stats.conflicts, 1, "id=1 is a write-write conflict");

        // source-wins: id=1 keeps the source's 100, NOT mpedb's 99.
        let age1: i64 = adapter
            .conn()
            .query_row("SELECT age FROM users WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(age1, 100);
        // the non-conflicting insert did reach the source.
        let n3: i64 = adapter
            .conn()
            .query_row("SELECT COUNT(*) FROM users WHERE id=3", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n3, 1);

        // id=1 is parked push_rejected; its dirty entry survives for the pull.
        let parked = crate::conflicts::list(&db).unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].pk, vec![Value::Int(1)]);
        assert_eq!(parked[0].record.kind, ConflictKind::PushRejected);

        // the follow-up pull carries the source's winning row into mpedb; apply
        // resolves the divergence source-wins → mpedb id=1 converges to 100.
        let from = db
            .sys_record_get(state::MIR_NS, state::KEY_CUR)
            .unwrap()
            .unwrap_or_else(|| adapter.zero_cursor());
        let batch = adapter.pull(&from, 1000).unwrap().unwrap();
        crate::apply::apply_batch(&db, &from, &batch).unwrap();
        let got = match db.query("SELECT age FROM users WHERE id=$1", &[Value::Int(1)]).unwrap() {
            mpedb::ExecResult::Rows { rows, .. } => rows,
            other => panic!("{other:?}"),
        };
        assert_eq!(got[0][0], Value::Int(100), "mpedb converged to the source value");

        for p in [src_path, mpedb_path] {
            let _ = std::fs::remove_file(p);
        }
    }
}
