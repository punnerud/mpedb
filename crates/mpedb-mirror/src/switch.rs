//! Authority-switch machinery (DESIGN-MIRROR §7): freeze, verify, and the
//! epoch-fenced flip between source-authoritative and mpedb-authoritative.
//!
//! Freeze is enforced at the ENGINE, not the facade (§3.9): the mirror sets the
//! write-block bitmap in `cdc\0tabs` (the M1.4 mutator check refuses writes to
//! those tables from every path — typed API, ring leader, raw engine), atomically
//! with `mir\0epoch.frozen`. That makes S7's "mpedb frozen → no writes leak"
//! true by construction.

use mpedb::Database;
use mpedb_core::CaptureConfig;
use mpedb_types::{Error, Result};

use crate::adapter::{Cursor, SourceAdapter};
use crate::apply::apply_batch;
use crate::push::push_batch;
use crate::reconcile::verify;
use crate::state::{self, Authority, MirrorState};

const PULL_BATCH: usize = 5000;

fn read_epoch_via(s: &mut mpedb::WriteSession) -> Result<state::Epoch> {
    let bytes = s
        .sys_record_get(state::MIR_NS, state::KEY_EPOCH)?
        .ok_or_else(|| Error::Unsupported("not a mirror (no mir/epoch)".into()))?;
    state::Epoch::decode(&bytes)
}

/// Read the mirror's current epoch record.
pub fn read_epoch(db: &Database) -> Result<state::Epoch> {
    let bytes = db
        .sys_record_get(state::MIR_NS, state::KEY_EPOCH)?
        .ok_or_else(|| Error::Unsupported("not a mirror (no mir/epoch)".into()))?;
    state::Epoch::decode(&bytes)
}

/// Freeze (or unfreeze) all mirrored tables: block every capture-enabled table
/// at the engine and set `mir\0epoch.frozen` — in one capture-suppressed commit.
/// A bumped generation forces every process to re-read the block.
pub fn set_frozen(db: &Database, frozen: bool) -> Result<()> {
    let mut s = db.begin()?;
    s.set_capture(false);

    let mut cap = match s.sys_record_get("cdc", b"tabs")? {
        Some(b) => CaptureConfig::decode(&b)?,
        None => return Err(Error::Unsupported("mirror has no CDC capture record".into())),
    };
    // block exactly the captured (mirrored) tables while frozen
    cap.blocked = if frozen { cap.captured } else { 0 };
    cap.generation = cap.generation.wrapping_add(1);
    s.sys_record_put("cdc", b"tabs", &cap.encode())?;

    let mut epoch = read_epoch_via(&mut s)?;
    epoch.frozen = frozen;
    s.sys_record_put(state::MIR_NS, state::KEY_EPOCH, &epoch.encode())?;

    s.commit()
}

fn mirror_id_hex(db: &Database) -> Result<String> {
    let bytes = db
        .sys_record_get(state::MIR_NS, state::KEY_CFG)?
        .ok_or_else(|| Error::Unsupported("not a mirror (no mir/cfg)".into()))?;
    let cfg = state::MirrorConfig::decode(&bytes)?;
    let mut s = String::with_capacity(32);
    for b in cfg.mirror_id {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

fn read_cur<A: SourceAdapter>(db: &Database, adapter: &A) -> Result<Cursor> {
    Ok(db
        .sys_record_get(state::MIR_NS, state::KEY_CUR)?
        .unwrap_or_else(|| adapter.zero_cursor()))
}

/// Pull + apply until the source is caught up. Returns rows applied.
pub fn drain_pull<A: SourceAdapter>(db: &Database, adapter: &mut A) -> Result<u64> {
    let mut applied = 0u64;
    loop {
        let from = read_cur(db, adapter)?;
        match adapter.pull(&from, PULL_BATCH)? {
            Some(batch) => {
                let s = apply_batch(db, &from, &batch)?;
                applied += s.upserts + s.deletes;
            }
            None => break,
        }
    }
    Ok(applied)
}

/// Push until the local dirty-set is empty. Returns rows pushed.
pub fn drain_push<A: SourceAdapter>(db: &Database, adapter: &mut A) -> Result<u64> {
    let mut pushed = 0u64;
    loop {
        let s = push_batch(db, adapter)?;
        let n = s.upserts + s.deletes;
        if n == 0 {
            break;
        }
        pushed += n;
    }
    Ok(pushed)
}

fn set_epoch(db: &Database, epoch: state::Epoch) -> Result<()> {
    let mut s = db.begin()?;
    s.set_capture(false);
    s.sys_record_put(state::MIR_NS, state::KEY_EPOCH, &epoch.encode())?;
    s.commit()
}

/// Switch authority source → mpedb (§7 S4→S5). Drains any pending source
/// changes, then fences the source epoch (losing side first) and flips mpedb to
/// M_AUTH. After this, local mpedb writes accumulate and pulls should stop.
pub fn switch_to_mpedb<A: SourceAdapter>(db: &Database, adapter: &mut A) -> Result<()> {
    let epoch = read_epoch(db)?;
    if epoch.authority != Authority::Source {
        return Err(Error::Unsupported("switch_to_mpedb: not source-authoritative".into()));
    }
    let mid = mirror_id_hex(db)?;
    adapter.ensure_source_state(&mid, epoch.epoch, "source")?;

    drain_pull(db, adapter)?; // catch up first

    let e = epoch.epoch;
    if adapter.cas_source_state(&mid, e, e + 1, "mpedb")?.is_none() {
        return Err(Error::Unsupported(
            "switch_to_mpedb fenced: the source epoch moved".into(),
        ));
    }
    set_epoch(
        db,
        state::Epoch {
            epoch: e + 1,
            authority: Authority::Mpedb,
            state: MirrorState::MAuth,
            frozen: false,
        },
    )
}

/// Switch authority mpedb → source (§7 S7→S8). Freezes mpedb (write-block =
/// fence), pushes all local changes, verifies convergence, then flips the epoch
/// and re-seeds the pull cursor to the head captured at cutover. Leaves the
/// mirror FROZEN on a failed verify so no writes leak before reconciliation.
pub fn switch_to_source<A: SourceAdapter>(db: &Database, adapter: &mut A) -> Result<()> {
    let epoch = read_epoch(db)?;
    if epoch.authority != Authority::Mpedb {
        return Err(Error::Unsupported("switch_to_source: not mpedb-authoritative".into()));
    }
    let mid = mirror_id_hex(db)?;
    let e = epoch.epoch;

    set_frozen(db, true)?; // fence the losing side (mpedb) at the engine
    drain_push(db, adapter)?; // land every local change on the source

    if !verify(db, adapter)? {
        return Err(Error::Unsupported(
            "switch_to_source: verify failed — mpedb and source diverge; run reconcile \
             (the mirror stays frozen)"
                .into(),
        ));
    }

    // fence + capture the re-seed baseline (head at cutover)
    let head = match adapter.cas_source_state(&mid, e, e + 1, "source")? {
        Some(h) => h,
        None => {
            return Err(Error::Unsupported(
                "switch_to_source fenced: the source epoch moved".into(),
            ))
        }
    };

    // finalize atomically: re-seed cur, unblock, set epoch = (E+1, source, steady)
    let mut s = db.begin()?;
    s.set_capture(false);
    s.sys_record_put(state::MIR_NS, state::KEY_CUR, &head)?;
    let mut cap = s
        .sys_record_get("cdc", b"tabs")?
        .map(|b| CaptureConfig::decode(&b))
        .transpose()?
        .ok_or_else(|| Error::Unsupported("mirror has no CDC record".into()))?;
    cap.blocked = 0;
    cap.generation = cap.generation.wrapping_add(1);
    s.sys_record_put("cdc", b"tabs", &cap.encode())?;
    s.sys_record_put(
        state::MIR_NS,
        state::KEY_EPOCH,
        &state::Epoch {
            epoch: e + 1,
            authority: Authority::Source,
            state: MirrorState::SrcAuth,
            frozen: false,
        }
        .encode(),
    )?;
    s.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{import_sqlite, ImportOptions};
    use mpedb_types::{Error as E, Value};
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
    fn freeze_blocks_writes_at_the_engine_and_unfreeze_restores() {
        let src = tmp("freeze-src", "db");
        let mid = tmp("freeze-mid", "mpedb");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);
                 INSERT INTO t VALUES (1,10);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
        };

        // a local write works before freezing
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(11), Value::Int(1)]).unwrap();

        set_frozen(&db, true).unwrap();
        assert!(read_epoch(&db).unwrap().frozen);
        // now a local write to a mirrored table is refused at the engine
        let err = db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(12), Value::Int(1)]);
        assert!(matches!(err, Err(E::Frozen { .. })), "got {err:?}");
        let ins = db.query("INSERT INTO t (id,v) VALUES ($1,$2)", &[Value::Int(2), Value::Int(20)]);
        assert!(matches!(ins, Err(E::Frozen { .. })));

        set_frozen(&db, false).unwrap();
        assert!(!read_epoch(&db).unwrap().frozen);
        // writes work again
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(13), Value::Int(1)]).unwrap();

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn switch_round_trip_moves_authority_and_lands_local_changes() {
        use crate::adapter::SourceAdapter;
        use crate::SqliteAdapter;

        let src = tmp("sw-src", "db");
        let mid = tmp("sw-mid", "mpedb");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);
                 INSERT INTO t VALUES (1,10),(2,20);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
        };
        let mut adapter = SqliteAdapter::new(Connection::open(&src).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();

        // starts source-authoritative (epoch 1)
        assert_eq!(read_epoch(&db).unwrap().authority, Authority::Source);

        // → mpedb-authoritative
        super::switch_to_mpedb(&db, &mut adapter).unwrap();
        let ep = read_epoch(&db).unwrap();
        assert_eq!((ep.authority, ep.epoch), (Authority::Mpedb, 2));

        // local writes while mpedb is authoritative
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(99), Value::Int(1)]).unwrap();
        db.query("INSERT INTO t (id,v) VALUES ($1,$2)", &[Value::Int(5), Value::Int(50)]).unwrap();

        // → back to source-authoritative (pushes + verifies + re-seeds cursor)
        super::switch_to_source(&db, &mut adapter).unwrap();
        let ep = read_epoch(&db).unwrap();
        assert_eq!((ep.authority, ep.epoch, ep.frozen), (Authority::Source, 3, false));

        // the source now holds the local changes
        let v1: i64 = adapter
            .conn()
            .query_row("SELECT v FROM t WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v1, 99);
        let n5: i64 = adapter
            .conn()
            .query_row("SELECT COUNT(*) FROM t WHERE id=5", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n5, 1);

        // the source-side epoch agrees
        let mid_hex = super::mirror_id_hex(&db).unwrap();
        assert_eq!(adapter.read_source_state(&mid_hex).unwrap(), Some((3, "source".into())));

        // and a fresh pull after switch-back sees nothing (cursor re-seeded)
        let from = read_cur(&db, &adapter).unwrap();
        assert!(adapter.pull(&from, 1000).unwrap().is_none());

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }
}
