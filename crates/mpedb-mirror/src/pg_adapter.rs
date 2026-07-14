//! The PostgreSQL [`SourceAdapter`] (DESIGN-MIRROR §5.2/§5.4). A pull opens a
//! REPEATABLE READ read-only snapshot, reads the shared changelog for each
//! mirrored table over the **consecutive-snapshot window** (visible in the new
//! snapshot but not the stored one — avoids the torn-read of a naive
//! `txid < xmin` window, review CONF#30), coalesces per PK, re-reads upserted
//! rows at the same snapshot, and emits a [`PullBatch`]. Applied via the shared
//! [`crate::apply::apply_batch`].
//!
//! The cursor is the opaque `pg_snapshot` text; `zero_cursor` is empty and pull
//! treats it as "everything committed so far". The whole visible window is
//! drained each round (so advancing the snapshot never skips an entry); bounded
//! batching within a window is a later refinement.

use std::collections::BTreeMap;

use mpedb_types::{keycode, ColumnType, Error, Result, Value};
use postgres::{Client, IsolationLevel};

use postgres::types::ToSql;

use crate::adapter::{Cursor, NetOp, NetOpKind, PullBatch, SourceAdapter};
use crate::pg::{self, PgColumn, PgTable};
use crate::pg_import::{read_expr, read_value};
use crate::pg_track::{OP_TOMBSTONE, OP_TRUNCATE, OP_UPSERT};

struct PgTableMeta {
    table_id: u32,
    src: PgTable,
}

/// A PostgreSQL source adapter over an owned client.
pub struct PgAdapter {
    client: Client,
    tables: Vec<PgTableMeta>,
    /// This mirror's origin tag, filtered out of the changelog (echo
    /// suppression). Genuine source writes carry NULL and are always included.
    origin: String,
}

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn pgerr(e: postgres::Error) -> Error {
    Error::Config(format!("postgres: {e}"))
}

impl PgAdapter {
    pub fn new(
        mut client: Client,
        include: Option<&[String]>,
        exclude: &[String],
    ) -> Result<PgAdapter> {
        let src = pg::introspect(&mut client, include, exclude)?;
        let tables = src
            .into_iter()
            .enumerate()
            .map(|(i, src)| PgTableMeta {
                table_id: i as u32,
                src,
            })
            .collect();
        Ok(PgAdapter {
            client,
            tables,
            origin: "mpedb-self".to_string(),
        })
    }

    pub fn client(&mut self) -> &mut Client {
        &mut self.client
    }

    /// Install the shared changelog + capture triggers for every mirrored table.
    pub fn install_triggers(&mut self) -> Result<()> {
        crate::pg_track::install_changelog(&mut self.client)?;
        // clone table metas to avoid borrow conflict with &mut client
        let tables: Vec<PgTable> = self.tables.iter().map(|t| t.src.clone()).collect();
        for t in &tables {
            crate::pg_track::install_triggers(&mut self.client, t)?;
        }
        Ok(())
    }
}

fn parse_pk(s: &str, ct: ColumnType) -> Result<Value> {
    Ok(match ct {
        ColumnType::Int64 => Value::Int(
            s.parse()
                .map_err(|_| Error::Corrupt(format!("bad int PK `{s}`")))?,
        ),
        ColumnType::Float64 => Value::Float(
            s.parse()
                .map_err(|_| Error::Corrupt(format!("bad float PK `{s}`")))?,
        ),
        ColumnType::Bool => match s {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(Error::Corrupt(format!("bad bool PK `{s}`"))),
        },
        ColumnType::Text => Value::Text(s.to_string()),
        ColumnType::Blob | ColumnType::Timestamp => {
            return Err(Error::Unsupported(
                "blob/timestamp primary keys are not yet supported for PostgreSQL pull".into(),
            ))
        }
    })
}

impl SourceAdapter for PgAdapter {
    fn pull(&mut self, from: &Cursor, _max_ops: usize) -> Result<Option<PullBatch>> {
        let snap_prev = if from.is_empty() {
            "1:1:".to_string()
        } else {
            String::from_utf8(from.clone())
                .map_err(|_| Error::Corrupt("pg cursor is not valid utf-8".into()))?
        };
        let origin = self.origin.clone();
        let metas: Vec<(u32, PgTable)> =
            self.tables.iter().map(|t| (t.table_id, t.src.clone())).collect();

        let mut tx = self
            .client
            .build_transaction()
            .read_only(true)
            .isolation_level(IsolationLevel::RepeatableRead)
            .start()
            .map_err(pgerr)?;
        let snap_now: String = tx
            .query_one("SELECT pg_current_snapshot()::text", &[])
            .map_err(pgerr)?
            .get(0);

        let mut ops: Vec<NetOp> = Vec::new();
        for (table_id, src) in &metas {
            let npk = src.pk.len();
            let pk_types: Vec<ColumnType> =
                src.pk.iter().map(|&i| src.columns[i].mapped.unwrap()).collect();
            let pk_extract = (0..npk)
                .map(|j| format!("pk->>{j}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT seq, op, {pk_extract} FROM mpedb_mirror.changelog \
                 WHERE tbl = $1 \
                   AND pg_visible_in_snapshot(xid, $2::text::pg_snapshot) \
                   AND NOT pg_visible_in_snapshot(xid, $3::text::pg_snapshot) \
                   AND (origin IS NULL OR origin <> $4) \
                 ORDER BY seq"
            );
            let rows = tx
                .query(&sql, &[&src.name, &snap_now, &snap_prev, &origin])
                .map_err(pgerr)?;

            // coalesce per PK, latest op wins
            let mut order: Vec<Vec<u8>> = Vec::new();
            let mut by_key: BTreeMap<Vec<u8>, (Vec<Value>, Vec<String>, i16)> = BTreeMap::new();
            for row in &rows {
                let op: i16 = row.get(1);
                if op == OP_TRUNCATE {
                    tx.rollback().map_err(pgerr)?;
                    return Err(Error::Unsupported(format!(
                        "TRUNCATE detected on `{}`; run an anti-entropy reconcile",
                        src.name
                    )));
                }
                let mut mpedb_pk = Vec::with_capacity(npk);
                let mut text_pk = Vec::with_capacity(npk);
                for (j, &ct) in pk_types.iter().enumerate() {
                    let s: String = row.get(2 + j);
                    text_pk.push(s.clone());
                    mpedb_pk.push(parse_pk(&s, ct)?);
                }
                let key = keycode::encode_key(&mpedb_pk);
                match by_key.get_mut(&key) {
                    Some(entry) => entry.2 = op,
                    None => {
                        order.push(key.clone());
                        by_key.insert(key, (mpedb_pk, text_pk, op));
                    }
                }
            }

            // materialise ops (re-read upsert images at snap_now)
            for key in order {
                let (mpedb_pk, text_pk, op) = by_key.remove(&key).unwrap();
                let kind = if op == OP_TOMBSTONE {
                    NetOpKind::Delete
                } else if op == OP_UPSERT {
                    match reread_row(&mut tx, src, &text_pk)? {
                        Some(row) => NetOpKind::Upsert(row),
                        None => NetOpKind::Delete,
                    }
                } else {
                    return Err(Error::Corrupt(format!("bad changelog op {op}")));
                };
                ops.push(NetOp {
                    table_id: *table_id,
                    pk: mpedb_pk,
                    kind,
                });
            }
        }
        tx.rollback().map_err(pgerr)?;

        if ops.is_empty() {
            return Ok(None);
        }
        Ok(Some(PullBatch {
            ops,
            end_cursor: snap_now.into_bytes(),
            source_epoch: None,
        }))
    }

    fn head(&mut self) -> Result<Cursor> {
        let snap: String = self
            .client
            .query_one("SELECT pg_current_snapshot()::text", &[])
            .map_err(pgerr)?
            .get(0);
        Ok(snap.into_bytes())
    }

    fn zero_cursor(&self) -> Cursor {
        Vec::new()
    }

    fn push(&mut self, ops: &[NetOp]) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        // clone touched-table metadata (avoids borrowing self across the txn)
        let mut metas: BTreeMap<u32, PgTable> = BTreeMap::new();
        for op in ops {
            if let std::collections::btree_map::Entry::Vacant(e) = metas.entry(op.table_id) {
                let src = self
                    .tables
                    .iter()
                    .find(|t| t.table_id == op.table_id)
                    .ok_or_else(|| Error::Config(format!("push to unmirrored table {}", op.table_id)))?
                    .src
                    .clone();
                e.insert(src);
            }
        }
        let origin = self.origin.clone();

        let mut tx = self.client.transaction().map_err(pgerr)?;
        // echo suppression: the capture trigger stamps origin from this GUC, so
        // our own writes are filtered out of the next pull (§6).
        tx.batch_execute(&format!(
            "SET LOCAL mpedb.mirror_origin = '{}'",
            origin.replace('\'', "''")
        ))
        .map_err(pgerr)?;

        for op in ops {
            let src = &metas[&op.table_id];
            match &op.kind {
                NetOpKind::Upsert(row) => pg_upsert(&mut tx, src, row)?,
                NetOpKind::Delete => pg_delete(&mut tx, src, &op.pk)?,
            }
        }
        tx.commit().map_err(pgerr)?;
        Ok(())
    }

    fn push_checked(&mut self, from: &Cursor, ops: &[NetOp]) -> Result<Vec<bool>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        // the pull cursor's snapshot = the xid window boundary. Empty cursor →
        // "everything committed so far" is un-consumed, so any source write is a
        // conflict (matches sqlite from_seq=0).
        let snap_prev = if from.is_empty() {
            "1:1:".to_string()
        } else {
            String::from_utf8(from.clone())
                .map_err(|_| Error::Corrupt("pg cursor is not valid utf-8".into()))?
        };
        let mut metas: BTreeMap<u32, PgTable> = BTreeMap::new();
        for op in ops {
            if let std::collections::btree_map::Entry::Vacant(e) = metas.entry(op.table_id) {
                let src = self
                    .tables
                    .iter()
                    .find(|t| t.table_id == op.table_id)
                    .ok_or_else(|| Error::Config(format!("push to unmirrored table {}", op.table_id)))?
                    .src
                    .clone();
                e.insert(src);
            }
        }
        let origin = self.origin.clone();

        let mut tx = self.client.transaction().map_err(pgerr)?;
        tx.batch_execute(&format!(
            "SET LOCAL mpedb.mirror_origin = '{}'",
            origin.replace('\'', "''")
        ))
        .map_err(pgerr)?;

        let mut results = Vec::with_capacity(ops.len());
        for op in ops {
            let src = &metas[&op.table_id];
            if pg_source_changed(&mut tx, src, &op.pk, &snap_prev, &origin)? {
                results.push(false); // source concurrently won — leave un-applied
                continue;
            }
            match &op.kind {
                NetOpKind::Upsert(row) => pg_upsert(&mut tx, src, row)?,
                NetOpKind::Delete => pg_delete(&mut tx, src, &op.pk)?,
            }
            results.push(true);
        }
        tx.commit().map_err(pgerr)?;
        Ok(results)
    }

    fn ensure_source_state(&mut self, mirror_id: &str, epoch: u64, authority: &str) -> Result<()> {
        self.client
            .batch_execute(
                "CREATE SCHEMA IF NOT EXISTS mpedb_mirror;
                 CREATE TABLE IF NOT EXISTS mpedb_mirror.state (\
                     mirror_id text PRIMARY KEY, epoch bigint NOT NULL, authority text NOT NULL)",
            )
            .map_err(pgerr)?;
        self.client
            .execute(
                "INSERT INTO mpedb_mirror.state(mirror_id, epoch, authority) \
                 VALUES ($1, $2, $3) ON CONFLICT (mirror_id) DO NOTHING",
                &[&mirror_id, &(epoch as i64), &authority],
            )
            .map_err(pgerr)?;
        Ok(())
    }

    fn read_source_state(&mut self, mirror_id: &str) -> Result<Option<(u64, String)>> {
        let rows = self
            .client
            .query(
                "SELECT epoch, authority FROM mpedb_mirror.state WHERE mirror_id=$1",
                &[&mirror_id],
            )
            .map_err(pgerr)?;
        Ok(rows
            .first()
            .map(|r| (r.get::<_, i64>(0) as u64, r.get::<_, String>(1))))
    }

    fn cas_source_state(
        &mut self,
        mirror_id: &str,
        expected_epoch: u64,
        new_epoch: u64,
        new_authority: &str,
    ) -> Result<Option<Cursor>> {
        let mut tx = self.client.transaction().map_err(pgerr)?;
        let n = tx
            .execute(
                "UPDATE mpedb_mirror.state SET epoch=$1, authority=$2 \
                 WHERE mirror_id=$3 AND epoch=$4",
                &[&(new_epoch as i64), &new_authority, &mirror_id, &(expected_epoch as i64)],
            )
            .map_err(pgerr)?;
        if n == 0 {
            let _ = tx.rollback();
            return Ok(None); // fenced
        }
        let head: String = tx
            .query_one("SELECT pg_current_snapshot()::text", &[])
            .map_err(pgerr)?
            .get(0);
        tx.commit().map_err(pgerr)?;
        Ok(Some(head.into_bytes()))
    }

    fn read_table_rows(&mut self, table_id: u32) -> Result<Vec<Vec<Value>>> {
        let src = self
            .tables
            .iter()
            .find(|t| t.table_id == table_id)
            .ok_or_else(|| Error::Config(format!("table id {table_id} is not mirrored")))?
            .src
            .clone();
        let exprs = src.columns.iter().map(read_expr).collect::<Vec<_>>().join(", ");
        let order = src
            .pk
            .iter()
            .map(|&i| q(&src.columns[i].name))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {exprs} FROM \"public\".{} ORDER BY {order}",
            q(&src.name)
        );
        let types: Vec<ColumnType> = src.columns.iter().map(|c| c.mapped.unwrap()).collect();
        let rows = self.client.query(&sql, &[]).map_err(pgerr)?;
        Ok(rows
            .iter()
            .map(|row| {
                types
                    .iter()
                    .enumerate()
                    .map(|(i, &ct)| read_value(row, i, ct))
                    .collect()
            })
            .collect())
    }
}

/// Re-read the current row for a PK at the transaction's snapshot, as mpedb
/// values. `None` if absent (deleted after the upsert in this snapshot).
fn reread_row(
    tx: &mut postgres::Transaction,
    src: &PgTable,
    text_pk: &[String],
) -> Result<Option<Vec<Value>>> {
    let exprs = src.columns.iter().map(read_expr).collect::<Vec<_>>().join(", ");
    // Compare the column cast to text against the (text) jsonb-extracted PK, so
    // the bound param is unambiguously text (binding a String against `$n::int8`
    // fails to serialize). `col::text` equals `jsonb_build_array(col)->>0` for
    // the supported PK types (int/text/bool/float).
    let where_sql = src
        .pk
        .iter()
        .enumerate()
        .map(|(j, &i)| format!("{}::text = ${}", q(&src.columns[i].name), j + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("SELECT {exprs} FROM \"public\".{} WHERE {where_sql}", q(&src.name));
    let params: Vec<&(dyn postgres::types::ToSql + Sync)> =
        text_pk.iter().map(|s| s as &(dyn postgres::types::ToSql + Sync)).collect();
    let rows = tx.query(&sql, &params).map_err(pgerr)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let types: Vec<ColumnType> = src.columns.iter().map(|c| c.mapped.unwrap()).collect();
    Ok(Some(
        types
            .iter()
            .enumerate()
            .map(|(i, &ct)| read_value(row, i, ct))
            .collect(),
    ))
}

/// An mpedb value as text, in the form `pg_value_expr` casts back to the source
/// type. `None` binds SQL NULL.
fn to_pg_text(v: &Value, c: &PgColumn) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(if *b { "true".into() } else { "false".into() }),
        Value::Text(s) => Some(s.clone()),
        Value::Timestamp(us) => Some(us.to_string()),
        Value::Blob(b) => Some(match c.pg_type.as_str() {
            "uuid" => uuid_string(b),
            _ => hex(b),
        }),
    }
}

/// The `VALUES` expression that casts a text param `$idx` back to the source
/// column type — the inverse of `pg_import::read_expr`.
fn pg_value_expr(c: &PgColumn, idx: usize) -> String {
    let p = format!("${idx}::text");
    match c.pg_type.as_str() {
        "int2" | "int4" | "int8" | "float4" | "float8" | "bool" | "text" | "varchar" | "bpchar"
        | "name" | "citext" | "numeric" | "json" | "jsonb" | "uuid" => format!("{p}::{}", c.pg_type),
        "bytea" => format!("decode({p}, 'hex')"),
        "timestamptz" => format!("to_timestamp({p}::float8 / 1000000.0)"),
        "timestamp" => format!("to_timestamp({p}::float8 / 1000000.0)::timestamp"),
        "date" => format!("to_timestamp({p}::float8 / 1000000.0)::date"),
        "time" => format!("(time '00:00:00' + ({p}::bigint) * interval '1 microsecond')"),
        other => format!("{p}::{other}"),
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        use std::fmt::Write as _;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

fn uuid_string(b: &[u8]) -> String {
    let h = hex(b);
    if h.len() == 32 {
        format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
    } else {
        h
    }
}

fn pg_upsert(tx: &mut postgres::Transaction, src: &PgTable, row: &[Value]) -> Result<()> {
    let mut cols = Vec::new();
    let mut exprs = Vec::new();
    let mut params: Vec<Option<String>> = Vec::new();
    let mut has_identity = false;
    for (i, c) in src.columns.iter().enumerate() {
        if c.generated {
            continue; // cannot INSERT a generated column
        }
        if c.identity {
            has_identity = true;
        }
        params.push(to_pg_text(&row[i], c));
        cols.push(q(&c.name));
        exprs.push(pg_value_expr(c, params.len()));
    }
    let pk_cols: Vec<String> = src.pk.iter().map(|&i| q(&src.columns[i].name)).collect();
    let non_pk: Vec<String> = src
        .columns
        .iter()
        .enumerate()
        .filter(|(i, c)| !c.generated && !src.pk.contains(i))
        .map(|(_, c)| {
            let cq = q(&c.name);
            format!("{cq}=excluded.{cq}")
        })
        .collect();
    let conflict = if non_pk.is_empty() {
        format!("ON CONFLICT ({}) DO NOTHING", pk_cols.join(", "))
    } else {
        format!("ON CONFLICT ({}) DO UPDATE SET {}", pk_cols.join(", "), non_pk.join(", "))
    };
    let overriding = if has_identity { " OVERRIDING SYSTEM VALUE" } else { "" };
    let sql = format!(
        "INSERT INTO \"public\".{} ({}){overriding} VALUES ({}) {conflict}",
        q(&src.name),
        cols.join(", "),
        exprs.join(", ")
    );
    let boxed: Vec<&(dyn ToSql + Sync)> =
        params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    tx.execute(&sql, &boxed).map_err(pgerr)?;
    Ok(())
}

/// Lock-then-check push-conflict detection (§6, CONF#11/27). Locks the target
/// row (`FOR UPDATE`; a no-op if it doesn't exist yet), then tests the changelog
/// xid-window consistent with the pull cursor: a non-echo entry for this PK that
/// is NOT visible in the stored snapshot = a source write we haven't consumed →
/// a write-write conflict. The lock + the caller's subsequent write are atomic.
fn pg_source_changed(
    tx: &mut postgres::Transaction,
    src: &PgTable,
    pk: &[Value],
    snap_prev: &str,
    origin: &str,
) -> Result<bool> {
    let pk_text: Vec<Option<String>> = src
        .pk
        .iter()
        .enumerate()
        .map(|(j, &i)| to_pg_text(&pk[j], &src.columns[i]))
        .collect();

    // 1. take the row lock (lock-then-check).
    let lock_where = src
        .pk
        .iter()
        .enumerate()
        .map(|(j, &i)| format!("{}::text = ${}", q(&src.columns[i].name), j + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    let lock_sql = format!(
        "SELECT 1 FROM \"public\".{} WHERE {lock_where} FOR UPDATE",
        q(&src.name)
    );
    let lock_params: Vec<&(dyn ToSql + Sync)> =
        pk_text.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    tx.query(&lock_sql, &lock_params).map_err(pgerr)?;

    // 2. xid-window changelog check (in a fresh statement, after the lock).
    let npk = src.pk.len();
    let pk_match = (0..npk)
        .map(|j| format!("pk->>{j} = ${}", j + 2))
        .collect::<Vec<_>>()
        .join(" AND ");
    let snap_idx = npk + 2;
    let origin_idx = npk + 3;
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM mpedb_mirror.changelog \
         WHERE tbl = $1 AND {pk_match} \
           AND NOT pg_visible_in_snapshot(xid, ${snap_idx}::text::pg_snapshot) \
           AND (origin IS NULL OR origin <> ${origin_idx}))"
    );
    let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(npk + 3);
    params.push(&src.name as &(dyn ToSql + Sync));
    for p in &pk_text {
        params.push(p as &(dyn ToSql + Sync));
    }
    params.push(&snap_prev as &(dyn ToSql + Sync));
    params.push(&origin as &(dyn ToSql + Sync));
    let exists: bool = tx.query_one(&sql, &params).map_err(pgerr)?.get(0);
    Ok(exists)
}

fn pg_delete(tx: &mut postgres::Transaction, src: &PgTable, pk: &[Value]) -> Result<()> {
    let where_sql = src
        .pk
        .iter()
        .enumerate()
        .map(|(j, &i)| format!("{}::text = ${}", q(&src.columns[i].name), j + 1))
        .collect::<Vec<_>>()
        .join(" AND ");
    let sql = format!("DELETE FROM \"public\".{} WHERE {where_sql}", q(&src.name));
    let params: Vec<Option<String>> = src
        .pk
        .iter()
        .enumerate()
        .map(|(j, &i)| to_pg_text(&pk[j], &src.columns[i]))
        .collect();
    let boxed: Vec<&(dyn ToSql + Sync)> =
        params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    tx.execute(&sql, &boxed).map_err(pgerr)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::apply_batch;
    use crate::import::ImportOptions;
    use crate::pg_import::import_pg;
    use mpedb::{Database, ExecResult};

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("{name}-{}.mpedb", std::process::id()));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        p
    }

    fn ids(db: &Database) -> Vec<i64> {
        let ExecResult::Rows { rows, .. } = db.query("SELECT id FROM t ORDER BY id", &[]).unwrap()
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
    #[ignore = "needs PostgreSQL (run with --ignored)"]
    fn pg_pull_apply_propagates_changes() {
        let pg = crate::pg_harness::ThrowawayPg::start();
        {
            let mut c = pg.client();
            c.batch_execute(
                "CREATE TABLE t(id bigint PRIMARY KEY, v int);
                 INSERT INTO t VALUES (1,10),(2,20);",
            )
            .unwrap();
        }
        let dest = tmp("pg-pull");
        let db = {
            let mut c = pg.client();
            import_pg(&mut c, &dest, &ImportOptions::default()).unwrap().0
        };
        assert_eq!(ids(&db), vec![1, 2]);

        let mut adapter = PgAdapter::new(pg.client(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();
        adapter
            .client()
            .batch_execute(
                "UPDATE t SET v=11 WHERE id=1;
                 INSERT INTO t VALUES (3,30);
                 DELETE FROM t WHERE id=2;",
            )
            .unwrap();

        let from = adapter.zero_cursor();
        let batch = adapter.pull(&from, 10000).unwrap().unwrap();
        let stats = apply_batch(&db, &from, &batch).unwrap();
        assert_eq!(stats.upserts, 2); // id=1 updated, id=3 new
        assert_eq!(stats.deletes, 1); // id=2

        assert_eq!(ids(&db), vec![1, 3]);
        let ExecResult::Rows { rows, .. } =
            db.query("SELECT v FROM t WHERE id=$1", &[Value::Int(1)]).unwrap()
        else {
            panic!()
        };
        assert_eq!(rows[0][0], Value::Int(11));

        // follow from the advanced cursor is empty
        assert!(adapter.pull(&batch.end_cursor, 10000).unwrap().is_none());

        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    #[ignore = "needs PostgreSQL (run with --ignored)"]
    fn pg_truncate_surfaces_and_reconcile_converges() {
        use crate::reconcile::reconcile;

        let pg = crate::pg_harness::ThrowawayPg::start();
        {
            let mut c = pg.client();
            c.batch_execute(
                "CREATE TABLE t(id bigint PRIMARY KEY, v int);
                 INSERT INTO t VALUES (1,10),(2,20),(3,30);",
            )
            .unwrap();
        }
        let dest = tmp("pg-trunc");
        let db = {
            let mut c = pg.client();
            import_pg(&mut c, &dest, &ImportOptions::default()).unwrap().0
        };
        assert_eq!(ids(&db), vec![1, 2, 3]);

        let mut adapter = PgAdapter::new(pg.client(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();

        // out-of-band divergence + a TRUNCATE-then-reload the pull can't follow
        adapter.client().batch_execute("TRUNCATE t; INSERT INTO t VALUES (9,90);").unwrap();

        // pull hits the TRUNCATE and asks for a reconcile
        let from = adapter.zero_cursor();
        assert!(adapter.pull(&from, 10000).is_err());

        // reconcile converges mpedb to the post-truncate source ({9})
        let stats = reconcile(&db, &mut adapter).unwrap();
        assert!(stats.tables_changed >= 1);
        assert_eq!(ids(&db), vec![9]);
        // reconcile again is a no-op
        assert_eq!(reconcile(&db, &mut adapter).unwrap().tables_changed, 0);

        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    #[ignore = "needs PostgreSQL (run with --ignored)"]
    fn pg_push_writes_local_changes_back_and_suppresses_echo() {
        use crate::push::push_batch;

        let pg = crate::pg_harness::ThrowawayPg::start();
        {
            let mut c = pg.client();
            c.batch_execute(
                "CREATE TABLE t(id bigint PRIMARY KEY, v int);
                 INSERT INTO t VALUES (1,10),(2,20),(3,30);",
            )
            .unwrap();
        }
        let dest = tmp("pg-push");
        let db = {
            let mut c = pg.client();
            import_pg(&mut c, &dest, &ImportOptions::default()).unwrap().0
        };

        // LOCAL mpedb changes (captured)
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(99), Value::Int(1)]).unwrap();
        db.query("INSERT INTO t (id,v) VALUES ($1,$2)", &[Value::Int(5), Value::Int(50)]).unwrap();
        db.query("DELETE FROM t WHERE id=$1", &[Value::Int(2)]).unwrap();

        let mut adapter = PgAdapter::new(pg.client(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();

        let stats = push_batch(&db, &mut adapter).unwrap();
        assert_eq!(stats.upserts, 2); // id=1 updated, id=5 new
        assert_eq!(stats.deletes, 1); // id=2

        // the source reflects the local mpedb state
        let v1: i32 = adapter
            .client()
            .query_one("SELECT v FROM t WHERE id=1", &[])
            .unwrap()
            .get(0);
        assert_eq!(v1, 99);
        let n5: i64 = adapter
            .client()
            .query_one("SELECT count(*) FROM t WHERE id=5", &[])
            .unwrap()
            .get(0);
        assert_eq!(n5, 1);
        let n2: i64 = adapter
            .client()
            .query_one("SELECT count(*) FROM t WHERE id=2", &[])
            .unwrap()
            .get(0);
        assert_eq!(n2, 0);

        // second push is a no-op (dirty-set cleared)
        assert_eq!(push_batch(&db, &mut adapter).unwrap().upserts, 0);

        // echo suppression: our GUC-tagged writes are filtered from the pull
        let from = adapter.zero_cursor();
        assert!(adapter.pull(&from, 10000).unwrap().is_none());

        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    #[ignore = "needs PostgreSQL (run with --ignored)"]
    fn pg_push_conflict_source_wins_and_parks() {
        use crate::push::push_batch;

        let pg = crate::pg_harness::ThrowawayPg::start();
        {
            let mut c = pg.client();
            c.batch_execute(
                "CREATE TABLE t(id bigint PRIMARY KEY, v int);
                 INSERT INTO t VALUES (1,10),(2,20);",
            )
            .unwrap();
        }
        let dest = tmp("pg-pushconf");
        let db = {
            let mut c = pg.client();
            import_pg(&mut c, &dest, &ImportOptions::default()).unwrap().0
        };
        let mut adapter = PgAdapter::new(pg.client(), None, &[]).unwrap();
        adapter.install_triggers().unwrap();

        // CONCURRENT source write on id=1 (not our echo, after our cursor).
        adapter.client().batch_execute("UPDATE t SET v=100 WHERE id=1;").unwrap();

        // LOCAL mpedb writes: id=1 collides with the source; id=3 does not.
        db.query("UPDATE t SET v=$1 WHERE id=$2", &[Value::Int(99), Value::Int(1)]).unwrap();
        db.query("INSERT INTO t (id,v) VALUES ($1,$2)", &[Value::Int(3), Value::Int(30)]).unwrap();

        let stats = push_batch(&db, &mut adapter).unwrap();
        assert_eq!(stats.upserts, 1); // only id=3 lands
        assert_eq!(stats.conflicts, 1); // id=1 is a write-write conflict

        // source-wins: id=1 keeps 100, id=3 arrived.
        let v1: i32 = adapter
            .client()
            .query_one("SELECT v FROM t WHERE id=1", &[])
            .unwrap()
            .get(0);
        assert_eq!(v1, 100);
        let n3: i64 = adapter
            .client()
            .query_one("SELECT count(*) FROM t WHERE id=3", &[])
            .unwrap()
            .get(0);
        assert_eq!(n3, 1);

        // id=1 is parked push_rejected.
        let parked = crate::conflicts::list(&db).unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].pk, vec![Value::Int(1)]);
        assert_eq!(parked[0].record.kind, crate::state::ConflictKind::PushRejected);

        let _ = std::fs::remove_file(&dest);
    }
}
