//! Operator-driven conflict resolution (DESIGN-MIRROR §8). By default the mirror
//! auto-resolves via authority-wins (source-wins in S2/S4, local-wins in S6/S7)
//! and parks each decision for audit. `resolve` lets the operator OVERRIDE the
//! parked set in either direction and clear it:
//!
//!   - [`Take::Source`] — converge mpedb to the CURRENT source row (discarding
//!     the local change) and drop the dirty entry so it never pushes back.
//!   - [`Take::Local`] — force mpedb's current row onto the source, bypassing the
//!     source-wins push check, and drop the dirty entry. This sticks because the
//!     next pull re-reads the (now-overridden) current source image, not the
//!     stale logged value.
//!
//! Either way the `park/` record and any manual `skip/` marker for the PK are
//! cleared. All mpedb writes are capture-suppressed (replication plane, §3.8).

use std::collections::BTreeMap;

use mpedb::Database;
use mpedb_core::cdc;
use mpedb_types::{keycode, Result, Value};

use crate::adapter::{NetOp, NetOpKind, SourceAdapter};
use crate::conflicts::{self, Parked};
use crate::state;

/// Which side wins when the operator resolves a parked conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    /// Accept the current source row (source-wins).
    Source,
    /// Force mpedb's current row onto the source (local-wins).
    Local,
}

/// What a resolve did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResolveStats {
    /// Conflicts resolved by taking the current source row into mpedb.
    pub took_source: u64,
    /// Conflicts resolved by forcing mpedb's row onto the source.
    pub took_local: u64,
    /// Conflicts left parked (the source row still fails a stricter mpedb rule).
    pub still_parked: u64,
}

/// The `cdc` sys subkey (`d/…`) of a PK's dirty entry (strip the `cdc\0` prefix).
fn dirty_subkey(table_id: u32, pk_keycode: &[u8]) -> Vec<u8> {
    cdc::dirty_key(table_id, pk_keycode)[4..].to_vec()
}

/// Clear the dirty entry, park record, and any skip marker for one PK.
fn clear_marks(
    s: &mut mpedb::WriteSession,
    table_id: u32,
    kc: &[u8],
) -> Result<()> {
    s.sys_record_delete("cdc", &dirty_subkey(table_id, kc))?;
    s.sys_record_delete(state::MIR_NS, &state::park_key(table_id, kc))?;
    s.sys_record_delete(state::MIR_NS, &state::skip_key(table_id, kc))?;
    Ok(())
}

/// Resolve every parked conflict with the chosen policy.
pub fn resolve<A: SourceAdapter + ?Sized>(
    db: &Database,
    adapter: &mut A,
    take: Take,
) -> Result<ResolveStats> {
    let parked = conflicts::list(db)?;
    if parked.is_empty() {
        return Ok(ResolveStats::default());
    }
    match take {
        Take::Source => take_source(db, adapter, &parked),
        Take::Local => take_local(db, adapter, &parked),
    }
}

/// Take::Source — converge mpedb to the current source image for each parked PK.
fn take_source<A: SourceAdapter + ?Sized>(
    db: &Database,
    adapter: &mut A,
    parked: &[Parked],
) -> Result<ResolveStats> {
    let schema = db.schema().clone();
    let mut stats = ResolveStats::default();

    // group parked PKs by table so each source table is read once
    let mut by_table: BTreeMap<u32, Vec<&Parked>> = BTreeMap::new();
    for p in parked {
        by_table.entry(p.record.table_id).or_default().push(p);
    }

    let mut s = db.begin()?;
    s.set_capture(false);
    for (&table_id, group) in &by_table {
        let Some(tdef) = schema.tables.get(table_id as usize) else {
            stats.still_parked += group.len() as u64;
            continue;
        };
        let pk_idx: Vec<usize> = tdef.primary_key.iter().map(|&i| i as usize).collect();
        // current source rows keyed by PK keycode
        let mut src: BTreeMap<Vec<u8>, Vec<Value>> = BTreeMap::new();
        for row in adapter.read_table_rows(table_id)? {
            let pk: Vec<Value> = pk_idx.iter().map(|&i| row[i].clone()).collect();
            src.insert(keycode::encode_key(&pk), row);
        }

        for p in group {
            let kc = keycode::encode_key(&p.pk);
            match src.get(&kc) {
                Some(srow) => {
                    // replace local with the source image; restore + keep parked
                    // if the source row fails a stricter mpedb rule.
                    let old = s.get_by_pk(table_id, &p.pk)?;
                    s.delete_by_pk(table_id, &p.pk)?;
                    let sp = s.savepoint();
                    match s.insert_row(table_id, srow) {
                        Ok(()) => {
                            clear_marks(&mut s, table_id, &kc)?;
                            stats.took_source += 1;
                        }
                        Err(_) => {
                            s.rollback_to(sp);
                            if let Some(o) = old {
                                let sp2 = s.savepoint();
                                if s.insert_row(table_id, &o).is_err() {
                                    s.rollback_to(sp2);
                                }
                            }
                            stats.still_parked += 1;
                        }
                    }
                }
                None => {
                    // the source no longer has this PK → delete it locally
                    s.delete_by_pk(table_id, &p.pk)?;
                    clear_marks(&mut s, table_id, &kc)?;
                    stats.took_source += 1;
                }
            }
        }
    }
    s.commit()?;
    Ok(stats)
}

/// Take::Local — force mpedb's current row onto the source for each parked PK.
fn take_local<A: SourceAdapter + ?Sized>(
    db: &Database,
    adapter: &mut A,
    parked: &[Parked],
) -> Result<ResolveStats> {
    // read the current mpedb image for every parked PK (read-only session)
    let mut ops: Vec<NetOp> = Vec::new();
    let mut marks: Vec<(u32, Vec<u8>)> = Vec::new();
    {
        let mut s = db.begin()?;
        s.set_capture(false);
        for p in parked {
            let table_id = p.record.table_id;
            let kind = match s.get_by_pk(table_id, &p.pk)? {
                Some(row) => NetOpKind::Upsert(row),
                None => NetOpKind::Delete,
            };
            ops.push(NetOp {
                table_id,
                pk: p.pk.clone(),
                kind,
            });
            marks.push((table_id, keycode::encode_key(&p.pk)));
        }
        s.rollback(); // read-only
    }

    // push UNCONDITIONALLY — the operator has chosen local-wins, so bypass the
    // source-wins conflict check. The next pull re-reads this (now-current)
    // source image, so mpedb keeps the forced value.
    adapter.push(&ops)?;

    let mut s = db.begin()?;
    s.set_capture(false);
    for (table_id, kc) in &marks {
        clear_marks(&mut s, *table_id, kc)?;
    }
    s.commit()?;

    Ok(ResolveStats {
        took_local: parked.len() as u64,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{import_sqlite, ImportOptions};
    use crate::push::push_batch;
    use crate::SqliteAdapter;
    use mpedb::ExecResult;
    use rusqlite::Connection;

    fn tmp(name: &str, ext: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("{name}-{}.{ext}", std::process::id()));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        p
    }

    /// Import users, make a concurrent source write + a local write on the same
    /// PK, push (→ id parks push_rejected), and return the mirror + adapter.
    fn parked_conflict(tag: &str) -> (Database, SqliteAdapter, std::path::PathBuf, std::path::PathBuf) {
        let src = tmp(tag, "db");
        let mid = tmp(tag, "mpedb");
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch(
                "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL, age INTEGER);
                 INSERT INTO users VALUES (1,'a@x',30),(2,'b@x',40);",
            )
            .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
        };
        let mut adapter = SqliteAdapter::new(Connection::open(&src).unwrap(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();
        // concurrent source write on id=1, then a local write on id=1
        adapter.conn().execute_batch("UPDATE users SET age=100 WHERE id=1;").unwrap();
        db.query("UPDATE users SET age=$1 WHERE id=$2", &[Value::Int(99), Value::Int(1)])
            .unwrap();
        let stats = push_batch(&db, &mut adapter).unwrap();
        assert_eq!(stats.conflicts, 1);
        assert_eq!(conflicts::list(&db).unwrap().len(), 1);
        (db, adapter, src, mid)
    }

    fn age(db: &Database, id: i64) -> i64 {
        match db.query("SELECT age FROM users WHERE id=$1", &[Value::Int(id)]).unwrap() {
            ExecResult::Rows { rows, .. } => match rows[0][0] {
                Value::Int(i) => i,
                _ => panic!(),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn resolve_take_source_converges_mpedb_and_clears_park() {
        let (db, mut adapter, src, mid) = parked_conflict("resolve-src");
        let st = resolve(&db, &mut adapter, Take::Source).unwrap();
        assert_eq!(st.took_source, 1);
        assert_eq!(age(&db, 1), 100, "mpedb took the source's value");
        assert!(conflicts::list(&db).unwrap().is_empty(), "park cleared");
        // no dirty entry survives → a push is a no-op
        assert_eq!(push_batch(&db, &mut adapter).unwrap().conflicts, 0);
        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn resolve_take_local_forces_source_and_survives_pull() {
        let (db, mut adapter, src, mid) = parked_conflict("resolve-loc");
        let st = resolve(&db, &mut adapter, Take::Local).unwrap();
        assert_eq!(st.took_local, 1);
        assert!(conflicts::list(&db).unwrap().is_empty(), "park cleared");

        // the source now holds mpedb's 99, not its own 100
        let src_age: i64 = adapter
            .conn()
            .query_row("SELECT age FROM users WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(src_age, 99);

        // and it sticks: the follow-up pull re-reads the current (forced) image
        let from = db
            .sys_record_get(state::MIR_NS, state::KEY_CUR)
            .unwrap()
            .unwrap_or_else(|| adapter.zero_cursor());
        if let Some(batch) = adapter.pull(&from, 1000).unwrap() {
            crate::apply::apply_batch(&db, &from, &batch).unwrap();
        }
        assert_eq!(age(&db, 1), 99, "local value survived the pull");
        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }
}
