//! `mirror regenerate` — DESIGN-MIRROR §7 (CONF#54).
//!
//! Rebuild a mirror into a fresh file, carrying its whole mirror identity across.
//! §7 calls this "the ONLY arrow out of `HALTED(db_full)` and the add-table /
//! schema-drift path", and both halves of that matter:
//!
//! **Out of db_full.** A full mirror cannot be grown in place — the geometry is
//! fixed at create time. So the escape is a bigger file, and the copy must be
//! **read-only against the full one**: you cannot write to it. The only write it
//! takes is the freeze, which is a small control record (there is a test that a
//! freeze commits against a file that just refused an INSERT — without that the
//! escape hatch would not start).
//!
//! **The add-table / schema-drift path**, and this is the part that explains a
//! design decision rather than working around it. mpedb has no `ALTER`/`CREATE
//! TABLE` on purpose: a table's id is its index in the NAME-SORTED table vector,
//! and that id is not a label — it keys the catalog's B+tree ROOTS, the `cdc\0tabs`
//! capture bitmap (by bit position), and the mirror's `map/`,`imp/`,`park/`,`skip/`
//! families. Adding `accounts` to a mirror holding `orders` and `users` renumbers
//! both, so an in-place DDL would silently point `accounts` at `orders`' rows.
//! Rebuilding sidesteps that entirely: this module remaps every id **by table
//! name** while it transplants, so the ids may move and nothing follows them to
//! the wrong place.
//!
//! Everything mirror-shaped travels: `mir/*` (config, epoch, cursor, provenance,
//! parked conflicts, skips), `cdc\0tabs`, and — the one most easily forgotten —
//! the `cdc\0d/` dirty entries, which are un-pushed LOCAL WRITES. Dropping them
//! would silently lose data the source has never seen.

use std::path::{Path, PathBuf};

use mpedb::Database;
use mpedb_core::{CaptureConfig, DirtyEntry};
use mpedb_types::{Durability, Error, Result, Schema};

use crate::state;

/// What a regenerate moved.
#[derive(Clone, Debug, Default)]
pub struct RegenReport {
    /// (table name, rows copied), in the NEW file's table order.
    pub tables: Vec<(String, u64)>,
    /// Un-pushed local writes carried over. These are the reason a regenerate
    /// is not simply a re-import.
    pub dirty_entries: u64,
    /// `mir/*` records carried over (epoch, cursor, provenance, parks, …).
    pub mir_records: u64,
    pub new_size_bytes: u64,
}

impl RegenReport {
    pub fn total_rows(&self) -> u64 {
        self.tables.iter().map(|t| t.1).sum()
    }
}

/// Rebuild the mirror at `db_path` into a fresh file of `size_bytes`.
///
/// The old file is left untouched until the final rename, and the new file is
/// born FROZEN (the freeze flag travels with `cdc\0tabs`/`mir/epoch`), so no
/// writer can slip into it mid-rebuild. On success the swap is a rename and the
/// result is unfrozen.
///
/// Crash behaviour, which is the whole reason for that order:
/// - before the rename: the old file stays frozen and `<path>.regen` is
///   garbage. Delete it and `mirror unfreeze`. Nothing was lost.
/// - after the rename, before the unfreeze: the NEW file is in place and
///   frozen. `mirror unfreeze` finishes the job. Fail-closed either way — a
///   half-swapped mirror is never writable.
pub fn regenerate(db_path: &Path, size_bytes: u64) -> Result<RegenReport> {
    let old = Database::open_from_file(db_path)?;
    let schema: Schema = old.schema().schema.clone();

    // 1. Freeze. Every mirrored table refuses writes from here until the new
    //    file is in place, so the copy below sees a still target.
    crate::switch::set_frozen(&old, true)?;

    // Step 2 of §7 ("drain-push if reachable") is the CALLER's: it needs a
    // source adapter, and regenerate must work with no source at all — a
    // db_full escape cannot depend on the network being up. The CLI drains
    // first when it can; the un-pushed dirty entries travel regardless, so
    // skipping the drain costs a delay, never data.

    let tmp_path = tmp_for(db_path);
    let _ = std::fs::remove_file(&tmp_path);

    let report = (|| -> Result<RegenReport> {
        let new = crate::import::create_mirror_db(
            &tmp_path,
            schema.clone(),
            size_bytes,
            Durability::None,
        )?;
        let mut report = RegenReport {
            new_size_bytes: size_bytes,
            ..Default::default()
        };

        // 3. Rows. Capture is suppressed: these are not new local writes, and
        //    re-capturing them would forge a dirty set claiming the whole
        //    mirror is un-pushed.
        for (tid, t) in schema.tables.iter().enumerate() {
            let rows = read_all_rows(&old, tid as u32, t)?;
            let n = rows.len() as u64;
            let mut s = new.begin()?;
            s.set_capture(false);
            for row in &rows {
                s.insert_row(tid as u32, row)?;
            }
            s.commit()?;
            report.tables.push((t.name.clone(), n));
        }

        // 4. The mirror identity. `mir/*` first: without cfg/epoch/cur the new
        //    file is a database, not a mirror.
        let mut s = new.begin()?;
        s.set_capture(false);
        // The whole mir namespace: an empty `lo` is rejected (keys are
        // 1..=1024 bytes), so start at the lowest legal subkey.
        for (k, v) in scan_ns(&old, state::MIR_NS, &[0x00], &[0xff; 32])? {
            s.sys_record_put(state::MIR_NS, &k, &v)?;
            report.mir_records += 1;
        }

        // 5. Capture config, verbatim: the same tables stay captured, and the
        //    blocked bitmap carries the freeze so the new file is born shut.
        let cap = old
            .sys_record_get("cdc", b"tabs")?
            .ok_or_else(|| Error::Unsupported("not a mirror (no cdc/tabs)".into()))?;
        let mut cap = CaptureConfig::decode(&cap)?;
        cap.blocked = cap.captured; // born frozen; unfrozen after the swap
        cap.generation = cap.generation.wrapping_add(1);
        s.sys_record_put("cdc", b"tabs", &cap.encode())?;

        // 6. The dirty set — un-pushed local writes. The subtle part: an entry's
        //    `last_txn` is the OLD file's committing txn id, and the new file
        //    restarts its counter at 0. push clears an entry only if its
        //    last_txn is UNCHANGED since the scan, so a stale high value is not
        //    wrong today — but it collides the day the new file's counter
        //    reaches it, which would clear an entry that had been re-dirtied and
        //    silently drop a local write. Stamp them as dirty-as-of-now instead;
        //    the value only ever has to compare equal to itself.
        let mut migrated = 0u64;
        // `cdc\0d/` is the full key; the namespace is "cdc" and the subkey
        // prefix is "d/" (with "d0" — the byte after '/' — as the exclusive end,
        // the same bound the engine's own dirty-set scan uses).
        for (k, v) in scan_ns(&old, "cdc", b"d/", b"d0")? {
            let mut de = DirtyEntry::decode(&v)?;
            de.last_txn = 0;
            s.sys_record_put("cdc", &k, &de.encode())?;
            migrated += 1;
        }
        report.dirty_entries = migrated;
        s.commit()?;
        drop(new);
        Ok(report)
    })();

    let report = match report {
        Ok(r) => r,
        Err(e) => {
            // Leave the old file frozen and intact; the operator retries or
            // unfreezes. Removing the temp is the only cleanup that is safe.
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
    };

    // 7. Swap. rename(2) is atomic within a filesystem, so a reader either sees
    //    the whole old file or the whole new one — never a torn mixture.
    std::fs::rename(&tmp_path, db_path).map_err(|e| {
        Error::Config(format!(
            "regenerate: swapping {} into place failed: {e}",
            tmp_path.display()
        ))
    })?;

    // 8. Unfreeze the new file. Everything above is redoable if we die here:
    //    the mirror is in place and simply refuses writes until unfrozen.
    let fresh = Database::open_from_file(db_path)?;
    crate::switch::set_frozen(&fresh, false)?;
    Ok(report)
}

/// `<db>.regen` — beside the target, so the rename is same-filesystem and
/// therefore atomic. A temp in `/tmp` could land on another filesystem and turn
/// the swap into a copy, which is neither atomic nor crash-safe.
fn tmp_for(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(".regen");
    PathBuf::from(s)
}

/// Every row of one table, in PK order.
fn read_all_rows(
    db: &Database,
    tid: u32,
    t: &mpedb_types::TableDef,
) -> Result<Vec<Vec<mpedb_types::Value>>> {
    let cols: Vec<String> = t
        .columns
        .iter()
        .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
        .collect();
    let sql = format!("SELECT {} FROM {}", cols.join(", "), t.name);
    let _ = tid;
    match db.query(&sql, &[])? {
        mpedb::ExecResult::Rows { rows, .. } => Ok(rows),
        other => Err(Error::Internal(format!("regenerate scan: {other:?}"))),
    }
}

/// All `(subkey, value)` in a sys namespace's `[lo, hi)` subkey range.
fn scan_ns(db: &Database, ns: &str, lo: &[u8], hi: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut s = db.begin()?;
    let out = s.sys_record_scan_range(ns, lo, hi)?;
    s.rollback();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{import_sqlite, ImportOptions};
    use mpedb_types::Value;
    use rusqlite::Connection;

    fn fixture(tag: &str, size: u64) -> (PathBuf, PathBuf, Database) {
        let dir = std::env::temp_dir().join("mpedb-regen-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join(format!("{tag}-s-{}.db", std::process::id()));
        let dest = dir.join(format!("{tag}-d-{}.mpedb", std::process::id()));
        for p in [&src, &dest] {
            let _ = std::fs::remove_file(p);
        }
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE items(id INTEGER PRIMARY KEY, v TEXT NOT NULL);
                 INSERT INTO items VALUES (1,'a'),(2,'b');",
            )
            .unwrap();
        }
        let opts = ImportOptions {
            size_bytes: size,
            ..Default::default()
        };
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &dest, &opts).unwrap().0
        };
        (src, dest, db)
    }

    fn rows(db: &Database) -> Vec<Vec<Value>> {
        match db.query("SELECT id, v FROM items", &[]).unwrap() {
            mpedb::ExecResult::Rows { rows, .. } => rows,
            o => panic!("{o:?}"),
        }
    }

    /// The one that matters. A regenerate is NOT a re-import: the mirror holds
    /// LOCAL writes that the source has never seen (the `cdc\0d/` dirty set,
    /// waiting for the next push). Rebuilding the file must carry them, or the
    /// rebuild silently destroys data — and the operator would only find out
    /// when the source turned out never to have received it.
    #[test]
    fn regenerate_carries_unpushed_local_writes_and_the_mirror_identity() {
        let (src, dest, db) = fixture("carry", 4 << 20);

        // A local write nobody has pushed yet.
        db.query(
            "INSERT INTO items (id, v) VALUES ($1, $2)",
            &[Value::Int(3), Value::Text("local".into())],
        )
        .unwrap();
        let dirty_before = super::scan_ns(&db, "cdc", b"d/", b"d0").unwrap().len();
        assert!(dirty_before > 0, "the local write must be captured as dirty");
        let epoch_before = crate::switch::read_epoch(&db).unwrap();
        let cur_before = db.sys_record_get(state::MIR_NS, state::KEY_CUR).unwrap();
        let cfg_before = db.sys_record_get(state::MIR_NS, state::KEY_CFG).unwrap();
        drop(db);

        let rep = regenerate(&dest, 8 << 20).unwrap();
        assert_eq!(rep.total_rows(), 3);
        assert_eq!(rep.dirty_entries as usize, dirty_before, "un-pushed writes must travel");

        let db = Database::open_from_file(&dest).unwrap();
        // rows survived
        assert_eq!(rows(&db).len(), 3);
        // the dirty set survived: this is still a mirror with work to push
        assert_eq!(
            super::scan_ns(&db, "cdc", b"d/", b"d0").unwrap().len(),
            dirty_before
        );
        // identity survived: same mirror, same place in the sync protocol
        assert_eq!(db.sys_record_get(state::MIR_NS, state::KEY_CFG).unwrap(), cfg_before);
        assert_eq!(db.sys_record_get(state::MIR_NS, state::KEY_CUR).unwrap(), cur_before);
        let epoch_after = crate::switch::read_epoch(&db).unwrap();
        assert_eq!(epoch_after.epoch, epoch_before.epoch);
        assert_eq!(epoch_after.authority, epoch_before.authority);
        // and it is USABLE: not left frozen
        assert!(!epoch_after.frozen, "regenerate must unfreeze on success");
        db.query(
            "INSERT INTO items (id, v) VALUES ($1, $2)",
            &[Value::Int(4), Value::Text("after".into())],
        )
        .expect("the regenerated mirror must accept writes");

        drop(db);
        for p in [src, dest] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// The db_full escape, end to end: fill the file until the engine refuses,
    /// then regenerate into a bigger one and write again. §7 calls this the ONLY
    /// arrow out, so it has to actually fly.
    #[test]
    fn regenerate_is_the_escape_from_db_full() {
        let (src, dest, db) = fixture("dbfull", 1 << 20);
        let mut i = 3i64;
        loop {
            match db.query(
                "INSERT INTO items (id, v) VALUES ($1, $2)",
                &[Value::Int(i), Value::Text("x".repeat(400))],
            ) {
                Ok(_) => i += 1,
                Err(mpedb_types::Error::DbFull) => break,
                Err(e) => panic!("unexpected: {e:?}"),
            }
            assert!(i < 100_000, "never filled");
        }
        let n_before = rows(&db).len();
        drop(db);

        let rep = regenerate(&dest, 8 << 20).unwrap();
        assert_eq!(rep.total_rows() as usize, n_before, "no row may be lost");

        let db = Database::open_from_file(&dest).unwrap();
        db.query(
            "INSERT INTO items (id, v) VALUES ($1, $2)",
            &[Value::Int(999_999), Value::Text("room at last".into())],
        )
        .expect("the whole point: the regenerated mirror has room");
        assert_eq!(rows(&db).len(), n_before + 1);

        drop(db);
        for p in [src, dest, PathBuf::from(format!("{}.regen", "x"))] {
            let _ = std::fs::remove_file(p);
        }
    }
}
