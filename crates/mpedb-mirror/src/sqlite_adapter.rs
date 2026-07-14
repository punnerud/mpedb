//! The sqlite [`SourceAdapter`] (DESIGN-MIRROR §5.1/§5.4): a pull round reads
//! the per-table changelog over `(cursor, head]`, coalesces per PK to the latest
//! op, re-reads each upserted row's current image from the SAME snapshot, and
//! emits a [`PullBatch`] of net ops for the applier (M3.4).

use std::collections::BTreeMap;

use mpedb_types::{keycode, ColumnType, Error, Result, Value};
use rusqlite::types::Value as SqlVal;
use rusqlite::Connection;

use crate::adapter::{Cursor, NetOp, NetOpKind, PullBatch, SourceAdapter};
use crate::import::convert_value;
use crate::sqlite::{introspect, SourceColumn, SourceTable};
use crate::sqlite_track::{log_table, log_head, SqliteCursor, OP_TOMBSTONE, OP_UPSERT};

/// Per-mirrored-table metadata the adapter needs.
struct TableMeta {
    table_id: u32,
    src: SourceTable,
}

/// A sqlite source adapter over an owned connection.
pub struct SqliteAdapter {
    conn: Connection,
    tables: Vec<TableMeta>,
}

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

impl SqliteAdapter {
    /// Build an adapter over the mirrored tables (matching the import scope).
    /// `table_id` is the position of the table's name in the name-sorted set,
    /// which equals its mpedb table id (both `introspect` and `Schema::new`
    /// sort by name).
    pub fn new(
        conn: Connection,
        include: Option<&[String]>,
        exclude: &[String],
    ) -> Result<SqliteAdapter> {
        let src_tables = introspect(&conn, include, exclude)?;
        let tables = src_tables
            .into_iter()
            .enumerate()
            .map(|(i, src)| TableMeta {
                table_id: i as u32,
                src,
            })
            .collect();
        Ok(SqliteAdapter { conn, tables })
    }

    /// Borrow the underlying connection (e.g. to install triggers).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn table_ids(&self) -> Vec<(u32, String)> {
        self.tables
            .iter()
            .map(|t| (t.table_id, t.src.name.clone()))
            .collect()
    }
}

/// One coalesced changelog entry for a PK, pre-image resolution.
struct Coalesced {
    /// PK as mpedb values (for the NetOp) and as sqlite values (to re-read).
    mpedb_pk: Vec<Value>,
    sqlite_pk: Vec<SqlVal>,
    last_op: i64,
}

impl SqliteAdapter {
    /// Read + coalesce one table's changelog over `(from_seq, …]`, consuming at
    /// most `limit` raw entries. Returns (per-PK coalesced ops in seq order,
    /// new end seq for the table). Reads within the caller's snapshot txn.
    fn scan_table(
        tx: &rusqlite::Transaction,
        meta: &TableMeta,
        from_seq: u64,
        limit: usize,
    ) -> Result<(Vec<Coalesced>, u64)> {
        let src = &meta.src;
        let npk = src.pk.len();
        let log = log_table(&src.name);
        let pk_log_cols: Vec<String> = (0..npk).map(|i| format!("pk{i}")).collect();
        let sql = format!(
            "SELECT seq, op, {} FROM {} WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            pk_log_cols.join(", "),
            q(&log)
        );
        let pk_types: Vec<ColumnType> = src.pk.iter().map(|&i| src.columns[i].mapped).collect();
        let pk_names: Vec<&str> = src.pk.iter().map(|&i| src.columns[i].name.as_str()).collect();

        let mut stmt = tx
            .prepare(&sql)
            .map_err(|e| Error::Config(format!("read changelog `{}`: {e}", src.name)))?;
        let mut rows = stmt
            .query(rusqlite::params![from_seq as i64, limit as i64])
            .map_err(|e| Error::Config(format!("read changelog `{}`: {e}", src.name)))?;

        // preserve first-seen order per PK, but keep the LATEST op (max seq)
        let mut order: Vec<Vec<u8>> = Vec::new();
        let mut by_key: BTreeMap<Vec<u8>, Coalesced> = BTreeMap::new();
        let mut end_seq = from_seq;
        loop {
            let row = rows
                .next()
                .map_err(|e| Error::Config(format!("read changelog `{}`: {e}", src.name)))?;
            let Some(row) = row else { break };
            let seq = row.get::<_, i64>(0).map_err(sqlerr)? as u64;
            let op = row.get::<_, i64>(1).map_err(sqlerr)?;
            end_seq = end_seq.max(seq);

            let mut mpedb_pk = Vec::with_capacity(npk);
            let mut sqlite_pk = Vec::with_capacity(npk);
            for (j, &ct) in pk_types.iter().enumerate() {
                let vr = row.get_ref(2 + j).map_err(sqlerr)?;
                sqlite_pk.push(vr.into());
                mpedb_pk.push(convert_value(vr, ct, &src.name, pk_names[j])?);
            }
            let key = keycode::encode_key(&mpedb_pk);
            match by_key.get_mut(&key) {
                Some(c) => c.last_op = op, // seq-ordered scan → latest op wins
                None => {
                    order.push(key.clone());
                    by_key.insert(
                        key,
                        Coalesced {
                            mpedb_pk,
                            sqlite_pk,
                            last_op: op,
                        },
                    );
                }
            }
        }
        let coalesced = order
            .into_iter()
            .map(|k| by_key.remove(&k).unwrap())
            .collect();
        Ok((coalesced, end_seq))
    }

    /// Re-read the current row for an upserted PK from the snapshot, mapped to
    /// mpedb values. `None` if the row is absent (a later delete in the same
    /// snapshot → becomes a tombstone).
    fn read_row(
        tx: &rusqlite::Transaction,
        src: &SourceTable,
        sqlite_pk: &[SqlVal],
    ) -> Result<Option<Vec<Value>>> {
        let cols = src
            .columns
            .iter()
            .map(|c| q(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let where_sql = src
            .pk
            .iter()
            .enumerate()
            .map(|(j, &i)| format!("{} IS ?{}", q(&src.columns[i].name), j + 1))
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = format!("SELECT {cols} FROM {} WHERE {where_sql}", q(&src.name));
        let mut stmt = tx
            .prepare(&sql)
            .map_err(|e| Error::Config(format!("re-read `{}`: {e}", src.name)))?;
        let types: Vec<ColumnType> = src.columns.iter().map(|c: &SourceColumn| c.mapped).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(sqlite_pk.iter()))
            .map_err(|e| Error::Config(format!("re-read `{}`: {e}", src.name)))?;
        let Some(row) = rows.next().map_err(sqlerr)? else {
            return Ok(None);
        };
        let mut vals = Vec::with_capacity(types.len());
        for (i, &ct) in types.iter().enumerate() {
            let vr = row.get_ref(i).map_err(sqlerr)?;
            vals.push(convert_value(vr, ct, &src.name, &src.columns[i].name)?);
        }
        Ok(Some(vals))
    }
}

fn sqlerr(e: rusqlite::Error) -> Error {
    Error::Config(format!("sqlite: {e}"))
}

impl SourceAdapter for SqliteAdapter {
    fn pull(&mut self, from: &Cursor, max_ops: usize) -> Result<Option<PullBatch>> {
        let cursor = if from.is_empty() {
            SqliteCursor::default()
        } else {
            SqliteCursor::decode(from)?
        };
        let mut end = cursor.clone();
        let mut ops: Vec<NetOp> = Vec::new();
        let mut budget = max_ops.max(1);

        let tx = self
            .conn
            .transaction()
            .map_err(|e| Error::Config(format!("sqlite snapshot: {e}")))?;

        for meta in &self.tables {
            if budget == 0 {
                break;
            }
            let from_seq = cursor.seq(meta.table_id);
            let (coalesced, end_seq) = Self::scan_table(&tx, meta, from_seq, budget)?;
            if end_seq == from_seq {
                continue; // nothing new for this table
            }
            end.set(meta.table_id, end_seq);
            for c in coalesced {
                let kind = if c.last_op == OP_TOMBSTONE {
                    NetOpKind::Delete
                } else if c.last_op == OP_UPSERT {
                    match Self::read_row(&tx, &meta.src, &c.sqlite_pk)? {
                        Some(row) => NetOpKind::Upsert(row),
                        None => NetOpKind::Delete, // deleted after the log upsert
                    }
                } else {
                    return Err(Error::Corrupt(format!("bad changelog op {}", c.last_op)));
                };
                ops.push(NetOp {
                    table_id: meta.table_id,
                    pk: c.mpedb_pk,
                    kind,
                });
                budget = budget.saturating_sub(1);
            }
        }
        tx.rollback().map_err(sqlerr)?; // read-only, drop the snapshot

        if end == cursor {
            return Ok(None); // caught up
        }
        Ok(Some(PullBatch {
            ops,
            end_cursor: end.encode(),
            source_epoch: None, // source-side state row lands in M6
        }))
    }

    fn head(&mut self) -> Result<Cursor> {
        let mut c = SqliteCursor::default();
        for meta in &self.tables {
            c.set(meta.table_id, log_head(&self.conn, &meta.src.name)?);
        }
        Ok(c.encode())
    }

    fn zero_cursor(&self) -> Cursor {
        SqliteCursor::default().encode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite_track::install_triggers;

    fn setup() -> SqliteAdapter {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT NOT NULL, age INTEGER);",
        )
        .unwrap();
        let tables = introspect(&conn, None, &[]).unwrap();
        for t in &tables {
            install_triggers(&conn, t).unwrap();
        }
        SqliteAdapter::new(conn, None, &[]).unwrap()
    }

    fn pk(op: &NetOp) -> i64 {
        match &op.pk[0] {
            Value::Int(i) => *i,
            v => panic!("pk not int: {v:?}"),
        }
    }

    #[test]
    fn pull_coalesces_and_reads_current_images() {
        let mut a = setup();
        let c0 = a.zero_cursor();
        // no changes yet
        assert!(a.pull(&c0, 100).unwrap().is_none());

        a.conn
            .execute_batch(
                "INSERT INTO users VALUES (1,'a@x',30);
                 UPDATE users SET age=31 WHERE id=1;   -- coalesces with the insert
                 INSERT INTO users VALUES (2,'b@x',40);
                 DELETE FROM users WHERE id=2;",
            )
            .unwrap();

        let batch = a.pull(&c0, 100).unwrap().unwrap();
        // id=1 → one Upsert (current image, age 31), id=2 → Delete (net)
        assert_eq!(batch.ops.len(), 2);
        let up = batch.ops.iter().find(|o| pk(o) == 1).unwrap();
        match &up.kind {
            NetOpKind::Upsert(row) => {
                assert_eq!(row[0], Value::Int(1));
                assert_eq!(row[2], Value::Int(31)); // coalesced to current image
            }
            k => panic!("expected upsert, got {k:?}"),
        }
        let del = batch.ops.iter().find(|o| pk(o) == 2).unwrap();
        assert_eq!(del.kind, NetOpKind::Delete);

        // cursor advanced → a second pull from the new cursor is empty
        assert!(a.pull(&batch.end_cursor, 100).unwrap().is_none());
    }

    #[test]
    fn pull_is_incremental_across_cursors() {
        let mut a = setup();
        a.conn.execute_batch("INSERT INTO users VALUES (1,'a',1);").unwrap();
        let b1 = a.pull(&a.zero_cursor(), 100).unwrap().unwrap();
        assert_eq!(b1.ops.len(), 1);

        a.conn.execute_batch("INSERT INTO users VALUES (2,'b',2);").unwrap();
        let b2 = a.pull(&b1.end_cursor, 100).unwrap().unwrap();
        assert_eq!(b2.ops.len(), 1);
        assert_eq!(pk(&b2.ops[0]), 2); // only the new row, not id=1 again
    }

    #[test]
    fn pull_respects_the_op_budget() {
        let mut a = setup();
        a.conn
            .execute_batch("INSERT INTO users VALUES (1,'a',1),(2,'b',2),(3,'c',3);")
            .unwrap();
        // budget 2 → at most 2 ops this round
        let b = a.pull(&a.zero_cursor(), 2).unwrap().unwrap();
        assert!(b.ops.len() <= 2);
        // the rest come on the next pull
        let b2 = a.pull(&b.end_cursor, 100).unwrap().unwrap();
        let total = b.ops.len() + b2.ops.len();
        assert_eq!(total, 3);
    }
}
