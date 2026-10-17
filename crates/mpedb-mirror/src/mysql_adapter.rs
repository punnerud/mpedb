//! MySQL-protocol [`SourceAdapter`].
//!
//! Pull reads the trigger log, coalesces to one net op per primary key, and
//! re-reads that key's current image from the same transaction. Push applies
//! each op in its own savepoint and stamps `origin = 'mpedb'` on the log rows
//! that op just produced, so the next pull does not echo them. Sync stays on
//! whatever authority the mirror file already has; this adapter does not switch.

use std::collections::BTreeMap;

use mpedb_types::{keycode, Error, Result, Value};
use mysql::prelude::Queryable;

use crate::adapter::{Cursor, NetOp, NetOpKind, PullBatch, SourceAdapter};
use crate::mysql_src::{self, from_mysql, is_row_rejection, missing_table, q, row_value, to_mysql};
use crate::mysql_track::{self, log_head, log_table, MysqlCursor, OP_TOMBSTONE, OP_UPSERT, SELF_ORIGIN};
use crate::sourcecfg::SourceSpec;
use crate::sqlite::SourceTable;

struct TableMeta {
    table_id: u32,
    src: SourceTable,
}

pub struct MysqlAdapter {
    conn: mysql::Conn,
    tables: Vec<TableMeta>,
}

struct Coalesced {
    mpedb_pk: Vec<Value>,
    my_pk: Vec<mysql::Value>,
    last_op: i64,
}

impl MysqlAdapter {
    /// Open the source and introspect the same table set import used.
    /// `table_id` is the name-sorted position, which is the id [`crate::sqlite::build_schema`] assigns.
    pub fn connect(
        spec: &SourceSpec,
        include: Option<&[String]>,
        exclude: &[String],
    ) -> Result<MysqlAdapter> {
        let mut conn = mysql_src::connect(spec)?;
        let src_tables = mysql_src::introspect(&mut conn, include, exclude)?;
        let tables = src_tables
            .into_iter()
            .enumerate()
            .map(|(i, src)| TableMeta { table_id: i as u32, src })
            .collect();
        Ok(MysqlAdapter { conn, tables })
    }

    pub fn install_triggers(&mut self) -> Result<()> {
        for meta in &self.tables {
            let src = meta.src.clone();
            mysql_track::install_triggers(&mut self.conn, &src)?;
        }
        Ok(())
    }

    /// The live connection, for a test that commits a source change.
    pub fn conn(&mut self) -> &mut mysql::Conn {
        &mut self.conn
    }

    fn meta(&self, table_id: u32) -> Result<&TableMeta> {
        self.tables
            .iter()
            .find(|t| t.table_id == table_id)
            .ok_or_else(|| Error::Config(format!("table id {table_id} is not mirrored")))
    }

    fn decode_cursor(from: &Cursor) -> Result<MysqlCursor> {
        if from.is_empty() {
            Ok(MysqlCursor::default())
        } else {
            MysqlCursor::decode(from)
        }
    }
}

impl SourceAdapter for MysqlAdapter {
    fn pull(&mut self, from: &Cursor, max_ops: usize) -> Result<Option<PullBatch>> {
        let cursor = Self::decode_cursor(from)?;
        self.conn
            .query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
            .map_err(|e| Error::Config(format!("mysql snapshot: {e}")))?;
        let pulled = pull_in_snapshot(&mut self.conn, &self.tables, &cursor, max_ops.max(1));
        let released = self.conn.query_drop("ROLLBACK");
        let batch = pulled?;
        released.map_err(|e| Error::Config(format!("mysql snapshot release: {e}")))?;
        Ok(batch)
    }

    fn head(&mut self) -> Result<Cursor> {
        let mut c = MysqlCursor::default();
        for meta in &self.tables {
            let name = meta.src.name.clone();
            c.set(meta.table_id, log_head(&mut self.conn, &name)?);
        }
        Ok(c.encode())
    }

    fn zero_cursor(&self) -> Cursor {
        MysqlCursor::default().encode()
    }

    fn read_table_rows(&mut self, table_id: u32) -> Result<Vec<Vec<Value>>> {
        let src = self.meta(table_id)?.src.clone();
        read_all(&mut self.conn, &src)
    }

    fn push(&mut self, ops: &[NetOp]) -> Result<Vec<bool>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let metas = metas_for(&self.tables, ops)?;
        self.conn
            .query_drop("START TRANSACTION")
            .map_err(|e| Error::Config(format!("mysql push: {e}")))?;
        let applied = push_in_txn(&mut self.conn, &metas, None, ops);
        finish_txn(&mut self.conn, applied)
    }

    fn push_checked(&mut self, from: &Cursor, ops: &[NetOp]) -> Result<Vec<bool>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let cursor = Self::decode_cursor(from)?;
        let metas = metas_for(&self.tables, ops)?;
        self.conn
            .query_drop("START TRANSACTION")
            .map_err(|e| Error::Config(format!("mysql push: {e}")))?;
        let applied = push_in_txn(&mut self.conn, &metas, Some(&cursor), ops);
        finish_txn(&mut self.conn, applied)
    }

    fn ensure_source_state(&mut self, mirror_id: &str, epoch: u64, authority: &str) -> Result<()> {
        self.conn
            .query_drop(
                "CREATE TABLE IF NOT EXISTS `_mpedb_mirror_state` (\
                     mirror_id VARCHAR(64) NOT NULL PRIMARY KEY, \
                     epoch BIGINT NOT NULL, \
                     authority VARCHAR(16) NOT NULL) ENGINE=InnoDB",
            )
            .map_err(|e| Error::Config(format!("mysql mirror state: {e}")))?;
        self.conn
            .exec_drop(
                "INSERT IGNORE INTO `_mpedb_mirror_state` (mirror_id, epoch, authority) VALUES (?, ?, ?)",
                (mirror_id, epoch as i64, authority),
            )
            .map_err(|e| Error::Config(format!("mysql mirror state: {e}")))?;
        Ok(())
    }

    fn read_source_state(&mut self, mirror_id: &str) -> Result<Option<(u64, String)>> {
        let row: std::result::Result<Option<(i64, String)>, mysql::Error> = self.conn.exec_first(
            "SELECT epoch, authority FROM `_mpedb_mirror_state` WHERE mirror_id = ?",
            (mirror_id,),
        );
        match row {
            Ok(Some((epoch, authority))) => Ok(Some((epoch as u64, authority))),
            Ok(None) => Ok(None),
            Err(e) if missing_table(&e) => Ok(None),
            Err(e) => Err(Error::Config(format!("mysql mirror state: {e}"))),
        }
    }

    fn cas_source_state(
        &mut self,
        mirror_id: &str,
        expected_epoch: u64,
        new_epoch: u64,
        new_authority: &str,
    ) -> Result<bool> {
        self.conn
            .query_drop("START TRANSACTION")
            .map_err(|e| Error::Config(format!("mysql mirror state: {e}")))?;
        let updated = self.conn.exec_drop(
            "UPDATE `_mpedb_mirror_state` SET epoch = ?, authority = ? \
             WHERE mirror_id = ? AND epoch = ?",
            (new_epoch as i64, new_authority, mirror_id, expected_epoch as i64),
        );
        let affected = match updated {
            Ok(()) => self.conn.affected_rows(),
            Err(e) => {
                let _ = self.conn.query_drop("ROLLBACK");
                return Err(Error::Config(format!("mysql mirror state: {e}")));
            }
        };
        if affected == 0 {
            let _ = self.conn.query_drop("ROLLBACK");
            return Ok(false);
        }
        self.conn
            .query_drop("COMMIT")
            .map_err(|e| Error::Config(format!("mysql mirror state: {e}")))?;
        Ok(true)
    }
}

fn finish_txn(conn: &mut mysql::Conn, applied: Result<Vec<bool>>) -> Result<Vec<bool>> {
    match applied {
        Ok(flags) => {
            conn.query_drop("COMMIT")
                .map_err(|e| Error::Config(format!("mysql push commit: {e}")))?;
            Ok(flags)
        }
        Err(e) => {
            let _ = conn.query_drop("ROLLBACK");
            Err(e)
        }
    }
}

fn metas_for(tables: &[TableMeta], ops: &[NetOp]) -> Result<BTreeMap<u32, SourceTable>> {
    let mut metas = BTreeMap::new();
    for op in ops {
        if metas.contains_key(&op.table_id) {
            continue;
        }
        let src = tables
            .iter()
            .find(|t| t.table_id == op.table_id)
            .ok_or_else(|| Error::Config(format!("push to unmirrored table {}", op.table_id)))?
            .src
            .clone();
        metas.insert(op.table_id, src);
    }
    Ok(metas)
}

fn pull_in_snapshot(
    conn: &mut mysql::Conn,
    tables: &[TableMeta],
    cursor: &MysqlCursor,
    mut budget: usize,
) -> Result<Option<PullBatch>> {
    let mut end = cursor.clone();
    let mut ops = Vec::new();
    for meta in tables {
        if budget == 0 {
            break;
        }
        let from_seq = cursor.seq(meta.table_id);
        let (coalesced, end_seq) = scan_table(conn, meta, from_seq, budget)?;
        if end_seq == from_seq {
            continue;
        }
        end.set(meta.table_id, end_seq);
        for c in coalesced {
            let kind = match c.last_op {
                OP_TOMBSTONE => NetOpKind::Delete,
                OP_UPSERT => match read_row(conn, &meta.src, &c.my_pk)? {
                    Some(row) => NetOpKind::Upsert(row),
                    None => NetOpKind::Delete,
                },
                other => return Err(Error::Corrupt(format!("bad mysql changelog op {other}"))),
            };
            ops.push(NetOp { table_id: meta.table_id, pk: c.mpedb_pk, kind });
            budget = budget.saturating_sub(1);
        }
    }
    if end == *cursor {
        return Ok(None);
    }
    Ok(Some(PullBatch {
        ops,
        end_cursor: end.encode(),
        // Import does not write a source-side epoch row. None keeps the local
        // authority check from treating a missing fence as a mismatch.
        source_epoch: None,
    }))
}

fn scan_table(
    conn: &mut mysql::Conn,
    meta: &TableMeta,
    from_seq: u64,
    limit: usize,
) -> Result<(Vec<Coalesced>, u64)> {
    let src = &meta.src;
    let npk = src.pk.len();
    let pk_log_cols = (0..npk).map(|i| format!("pk{i}")).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT seq, op, {pk_log_cols} FROM {} \
         WHERE seq > ? AND (origin IS NULL OR origin <> '{SELF_ORIGIN}') \
         ORDER BY seq LIMIT ?",
        q(&log_table(&src.name))
    );
    let rows: Vec<mysql::Row> = conn
        .exec(sql, (from_seq as i64, limit as i64))
        .map_err(|e| Error::Config(format!("mysql changelog `{}`: {e}", src.name)))?;
    let mut order = Vec::new();
    let mut by_key: BTreeMap<Vec<u8>, Coalesced> = BTreeMap::new();
    let mut end_seq = from_seq;
    for row in rows {
        let seq = match from_mysql(row_value(&row, 0)?, mpedb_types::ColumnType::Int64, &src.name, "seq")? {
            Value::Int(i) => i as u64,
            other => return Err(Error::Corrupt(format!("mysql changelog seq is {other:?}"))),
        };
        let op = match from_mysql(row_value(&row, 1)?, mpedb_types::ColumnType::Int64, &src.name, "op")? {
            Value::Int(i) => i,
            other => return Err(Error::Corrupt(format!("mysql changelog op is {other:?}"))),
        };
        end_seq = end_seq.max(seq);
        let mut mpedb_pk = Vec::with_capacity(npk);
        let mut my_pk = Vec::with_capacity(npk);
        for (j, &col) in src.pk.iter().enumerate() {
            let raw = row_value(&row, 2 + j)?;
            my_pk.push(raw.clone());
            mpedb_pk.push(from_mysql(
                raw,
                src.columns[col].mapped,
                &src.name,
                &src.columns[col].name,
            )?);
        }
        let key = keycode::encode_key(&mpedb_pk);
        match by_key.get_mut(&key) {
            Some(existing) => existing.last_op = op,
            None => {
                order.push(key.clone());
                by_key.insert(key, Coalesced { mpedb_pk, my_pk, last_op: op });
            }
        }
    }
    let coalesced = order.into_iter().filter_map(|k| by_key.remove(&k)).collect();
    Ok((coalesced, end_seq))
}

fn read_row(conn: &mut mysql::Conn, src: &SourceTable, pk: &[mysql::Value]) -> Result<Option<Vec<Value>>> {
    let cols = src.columns.iter().map(|c| q(&c.name)).collect::<Vec<_>>().join(", ");
    let pred = src
        .pk
        .iter()
        .map(|&i| format!("{} = ?", q(&src.columns[i].name)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("SELECT {cols} FROM {} WHERE {pred}", q(&src.name));
    let rows: Vec<mysql::Row> = conn
        .exec(sql, mysql::Params::Positional(pk.to_vec()))
        .map_err(|e| Error::Config(format!("mysql re-read `{}`: {e}", src.name)))?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let mut vals = Vec::with_capacity(src.columns.len());
    for (i, col) in src.columns.iter().enumerate() {
        vals.push(from_mysql(row_value(&row, i)?, col.mapped, &src.name, &col.name)?);
    }
    Ok(Some(vals))
}

fn read_all(conn: &mut mysql::Conn, src: &SourceTable) -> Result<Vec<Vec<Value>>> {
    let cols = src.columns.iter().map(|c| q(&c.name)).collect::<Vec<_>>().join(", ");
    let order = src
        .pk
        .iter()
        .map(|&i| q(&src.columns[i].name))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {cols} FROM {} ORDER BY {order}", q(&src.name));
    let rows: Vec<mysql::Row> = conn
        .exec(sql, ())
        .map_err(|e| Error::Config(format!("mysql read `{}`: {e}", src.name)))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut vals = Vec::with_capacity(src.columns.len());
        for (i, col) in src.columns.iter().enumerate() {
            vals.push(from_mysql(row_value(&row, i)?, col.mapped, &src.name, &col.name)?);
        }
        out.push(vals);
    }
    Ok(out)
}

fn push_in_txn(
    conn: &mut mysql::Conn,
    metas: &BTreeMap<u32, SourceTable>,
    cursor: Option<&MysqlCursor>,
    ops: &[NetOp],
) -> Result<Vec<bool>> {
    let mut premax = BTreeMap::new();
    for (&tid, src) in metas {
        let seq: Option<i64> = conn
            .exec_first(format!("SELECT COALESCE(MAX(seq), 0) FROM {}", q(&log_table(&src.name))), ())
            .map_err(|e| Error::Config(format!("mysql log head `{}`: {e}", src.name)))?;
        premax.insert(tid, seq.unwrap_or(0));
    }
    let mut results = Vec::with_capacity(ops.len());
    for op in ops {
        let src = &metas[&op.table_id];
        if let Some(cursor) = cursor {
            if source_changed(conn, src, &op.pk, cursor.seq(op.table_id))? {
                results.push(false);
                continue;
            }
        }
        results.push(apply_op(conn, src, op)?);
    }
    for (&tid, src) in metas {
        conn.exec_drop(
            format!(
                "UPDATE {} SET origin = '{SELF_ORIGIN}' WHERE seq > ?",
                q(&log_table(&src.name))
            ),
            (premax[&tid],),
        )
        .map_err(|e| Error::Config(format!("mysql echo tag `{}`: {e}", src.name)))?;
    }
    Ok(results)
}

fn source_changed(conn: &mut mysql::Conn, src: &SourceTable, pk: &[Value], from_seq: u64) -> Result<bool> {
    let pred = (0..src.pk.len())
        .map(|j| format!("pk{j} = ?"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM {} WHERE {pred} AND seq > ? \
         AND (origin IS NULL OR origin <> '{SELF_ORIGIN}'))",
        q(&log_table(&src.name))
    );
    let mut params: Vec<mysql::Value> = pk.iter().map(to_mysql).collect::<Result<Vec<_>>>()?;
    params.push(mysql::Value::Int(from_seq as i64));
    let exists: Option<i64> = conn
        .exec_first(sql, params)
        .map_err(|e| Error::Config(format!("mysql conflict check `{}`: {e}", src.name)))?;
    Ok(exists.unwrap_or(0) != 0)
}

fn apply_op(conn: &mut mysql::Conn, src: &SourceTable, op: &NetOp) -> Result<bool> {
    let prepared = match &op.kind {
        NetOpKind::Upsert(row) => upsert_statement(src, row),
        NetOpKind::Delete => delete_statement(src, &op.pk),
    };
    let (sql, params) = match prepared {
        Ok(pair) => pair,
        Err(Error::TypeMismatch(_)) => return Ok(false),
        Err(e) => return Err(e),
    };
    conn.query_drop("SAVEPOINT mpedb_op")
        .map_err(|e| Error::Config(format!("mysql savepoint: {e}")))?;
    let applied = conn.exec_drop(sql, params);
    match applied {
        Ok(()) => {
            conn.query_drop("RELEASE SAVEPOINT mpedb_op")
                .map_err(|e| Error::Config(format!("mysql savepoint: {e}")))?;
            Ok(true)
        }
        Err(e) if is_row_rejection(&e) => {
            conn.query_drop("ROLLBACK TO SAVEPOINT mpedb_op")
                .map_err(|e| Error::Config(format!("mysql savepoint: {e}")))?;
            conn.query_drop("RELEASE SAVEPOINT mpedb_op")
                .map_err(|e| Error::Config(format!("mysql savepoint: {e}")))?;
            Ok(false)
        }
        Err(e) => Err(Error::Config(format!("mysql push `{}`: {e}", src.name))),
    }
}

fn upsert_statement(src: &SourceTable, row: &[Value]) -> Result<(String, Vec<mysql::Value>)> {
    let cols: Vec<String> = src.columns.iter().map(|c| q(&c.name)).collect();
    let places = vec!["?"; row.len()].join(", ");
    let assignments: Vec<String> = src
        .columns
        .iter()
        .enumerate()
        .filter(|(i, _)| !src.pk.contains(i))
        .map(|(_, c)| {
            let name = q(&c.name);
            format!("{name} = VALUES({name})")
        })
        .collect();
    let update = if assignments.is_empty() {
        let pk = q(&src.columns[src.pk[0]].name);
        format!("{pk} = {pk}")
    } else {
        assignments.join(", ")
    };
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({places}) ON DUPLICATE KEY UPDATE {update}",
        q(&src.name),
        cols.join(", ")
    );
    let params = row.iter().map(to_mysql).collect::<Result<Vec<_>>>()?;
    Ok((sql, params))
}

fn delete_statement(src: &SourceTable, pk: &[Value]) -> Result<(String, Vec<mysql::Value>)> {
    let pred = src
        .pk
        .iter()
        .map(|&i| format!("{} = ?", q(&src.columns[i].name)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("DELETE FROM {} WHERE {pred}", q(&src.name));
    let params = pk.iter().map(to_mysql).collect::<Result<Vec<_>>>()?;
    Ok((sql, params))
}
