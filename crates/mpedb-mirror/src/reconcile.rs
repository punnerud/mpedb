//! Full-table merge-diff reconcile (DESIGN-MIRROR §5.5) — converges mpedb to
//! the sqlite source (source-wins). This one primitive is:
//!   - **no-touch mode**: mirror a source without installing triggers;
//!   - the **anti-entropy** pass that catches trigger holes (sqlite's
//!     REPLACE-secondary-unique delete, out-of-band file writes);
//!   - the raw material for the migration diff (what does/does not survive).
//!
//! It compares mpedb and the source table-by-table by PK keycode and applies
//! the corrective ops in a capture-suppressed session. It does NOT advance the
//! pull cursor: reconcile and pull are both idempotent and independently
//! convergent. The bandwidth-optimised chunk-checksum variant (for network
//! sources) lands with PostgreSQL in M4; over a local sqlite file the full
//! ordered compare is simpler and exact.

use std::collections::BTreeMap;

use mpedb::{Database, ExecResult};
use mpedb_types::{keycode, Error, Result, Value};

use crate::adapter::SourceAdapter;
use crate::sqlite_adapter::SqliteAdapter;
use crate::sqlite_track::SqliteCursor;

/// What a reconcile changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReconcileStats {
    pub upserts: u64,
    pub deletes: u64,
    /// Tables that needed at least one correction.
    pub tables_changed: u64,
}

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn pk_keycode(row: &[Value], pk_idx: &[usize]) -> Vec<u8> {
    let pk: Vec<Value> = pk_idx.iter().map(|&i| row[i].clone()).collect();
    keycode::encode_key(&pk)
}

fn pk_values(row: &[Value], pk_idx: &[usize]) -> Vec<Value> {
    pk_idx.iter().map(|&i| row[i].clone()).collect()
}

fn query_all(db: &Database, table: &str, cols: &[String]) -> Result<Vec<Vec<Value>>> {
    let list = cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT {list} FROM {}", q(table));
    match db.query(&sql, &[])? {
        ExecResult::Rows { rows, .. } => Ok(rows),
        other => Err(Error::Internal(format!("reconcile read got {other:?}"))),
    }
}

/// Reconcile mpedb to the source (source-wins), over any adapter — the merge-
/// diff / anti-entropy pass and no-touch mode for both sqlite and PostgreSQL.
/// Also the natural handler for a PostgreSQL TRUNCATE (the source table is now
/// empty → the mpedb rows are deleted).
pub fn reconcile<A: SourceAdapter>(db: &Database, adapter: &mut A) -> Result<ReconcileStats> {
    let schema = db.schema().clone();
    let mut stats = ReconcileStats::default();

    for (tid, tdef) in schema.tables.iter().enumerate() {
        let table_id = tid as u32;
        let pk_idx: Vec<usize> = tdef.primary_key.iter().map(|&i| i as usize).collect();
        let col_names: Vec<String> = tdef.columns.iter().map(|c| c.name.clone()).collect();

        // both sides keyed by PK keycode
        let mut mp: BTreeMap<Vec<u8>, Vec<Value>> = BTreeMap::new();
        for row in query_all(db, &tdef.name, &col_names)? {
            mp.insert(pk_keycode(&row, &pk_idx), row);
        }
        let mut sq: BTreeMap<Vec<u8>, Vec<Value>> = BTreeMap::new();
        for row in adapter.read_table_rows(table_id)? {
            sq.insert(pk_keycode(&row, &pk_idx), row);
        }

        // corrections: source rows missing/differing → upsert; mpedb-only → delete
        let mut upserts: Vec<Vec<Value>> = Vec::new();
        let mut deletes: Vec<Vec<Value>> = Vec::new();
        for (key, srow) in &sq {
            if mp.get(key) != Some(srow) {
                upserts.push(srow.clone());
            }
        }
        for (key, mrow) in &mp {
            if !sq.contains_key(key) {
                deletes.push(pk_values(mrow, &pk_idx));
            }
        }
        if upserts.is_empty() && deletes.is_empty() {
            continue;
        }
        stats.tables_changed += 1;

        let mut s = db.begin()?;
        s.set_capture(false);
        // delete-phase then insert-phase (dissolves unique swaps / PK changes)
        for pk in &deletes {
            if s.delete_by_pk(table_id, pk)? {
                stats.deletes += 1;
            }
        }
        for row in &upserts {
            s.delete_by_pk(table_id, &pk_values(row, &pk_idx))?;
        }
        for row in &upserts {
            s.insert_row(table_id, row)?;
            stats.upserts += 1;
        }
        s.commit()?;
    }
    Ok(stats)
}

/// Restore guard (DESIGN-MIRROR §5.1 / review CONF#24): a source whose changelog
/// AUTOINCREMENT counter has regressed below the stored cursor was restored from
/// an older backup of the same file — pulls would then silently miss writes.
/// Returns an error so the caller can HALT and demand an explicit resync.
pub fn check_source_not_restored(adapter: &SqliteAdapter, cursor: &[u8]) -> Result<()> {
    let cur = if cursor.is_empty() {
        SqliteCursor::default()
    } else {
        SqliteCursor::decode(cursor)?
    };
    for (table_id, name) in adapter.table_ids() {
        let live = adapter.log_autoincrement(table_id)?;
        let stored = cur.seq(table_id);
        if live < stored {
            return Err(Error::Unsupported(format!(
                "source appears restored from an older backup: table `{name}` changelog \
                 counter {live} < cursor {stored}; a full resync is required"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn mpedb_ids(db: &Database) -> Vec<i64> {
        let ExecResult::Rows { rows, .. } =
            db.query("SELECT id FROM t ORDER BY id", &[]).unwrap()
        else {
            panic!()
        };
        rows.iter()
            .map(|r| match r[0] {
                Value::Int(i) => i,
                _ => panic!(),
            })
            .collect()
    }

    #[test]
    fn reconcile_converges_mpedb_to_the_source() {
        let src_path = tmp("rec-src", "db");
        let mpedb_path = tmp("rec-mid", "mpedb");
        {
            let c = Connection::open(&src_path).unwrap();
            c.execute_batch(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);
                 INSERT INTO t VALUES (1,10),(2,20),(3,30);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src_path).unwrap();
            import_sqlite(&mut c, &mpedb_path, &ImportOptions::default())
                .unwrap()
                .0
        };
        assert_eq!(mpedb_ids(&db), vec![1, 2, 3]);

        // diverge out-of-band on BOTH sides (no triggers → pull can't see it):
        // mpedb loses id=3 and has a stale id=1; source changes id=1, adds id=4,
        // drops id=2.
        {
            let mut s = db.begin().unwrap();
            s.set_capture(false);
            s.delete_by_pk(0, &[Value::Int(3)]).unwrap();
            s.commit().unwrap();
        }
        let mut adapter = SqliteAdapter::new(Connection::open(&src_path).unwrap(), None, &[]).unwrap();
        adapter
            .conn()
            .execute_batch(
                "UPDATE t SET v=11 WHERE id=1;
                 DELETE FROM t WHERE id=2;
                 INSERT INTO t VALUES (4,40);",
            )
            .unwrap();

        let stats = reconcile(&db, &mut adapter).unwrap();
        assert_eq!(stats.tables_changed, 1);

        // mpedb now equals the source: ids {1,3?,4}? source has {1,3,4} (id=3
        // still in source, was only deleted in mpedb → re-added; id=2 deleted)
        assert_eq!(mpedb_ids(&db), vec![1, 3, 4]);
        let ExecResult::Rows { rows, .. } =
            db.query("SELECT v FROM t WHERE id=$1", &[Value::Int(1)]).unwrap()
        else {
            panic!()
        };
        assert_eq!(rows[0][0], Value::Int(11)); // source value won

        // reconcile again is a no-op (converged)
        let again = reconcile(&db, &mut adapter).unwrap();
        assert_eq!(again.tables_changed, 0);

        for p in [src_path, mpedb_path] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn restore_guard_flags_a_regressed_counter() {
        let src_path = tmp("restore-src", "db");
        {
            let c = Connection::open(&src_path).unwrap();
            c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);").unwrap();
        }
        let adapter = SqliteAdapter::new(Connection::open(&src_path).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();
        adapter.conn().execute_batch("INSERT INTO t VALUES (1),(2);").unwrap();
        // live autoincrement is now 2; a cursor claiming seq 5 means the source
        // regressed (older backup restored)
        let mut ahead = SqliteCursor::default();
        ahead.set(0, 5);
        assert!(check_source_not_restored(&adapter, &ahead.encode()).is_err());
        // a cursor at or below the live counter is fine
        let mut ok = SqliteCursor::default();
        ok.set(0, 2);
        assert!(check_source_not_restored(&adapter, &ok.encode()).is_ok());

        let _ = std::fs::remove_file(src_path);
    }
}
