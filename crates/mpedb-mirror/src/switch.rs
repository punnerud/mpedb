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

use crate::state;

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
}
