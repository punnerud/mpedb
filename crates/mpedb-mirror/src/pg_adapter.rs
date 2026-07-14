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

use crate::adapter::{Cursor, NetOp, NetOpKind, PullBatch, SourceAdapter};
use crate::pg::{self, PgTable};
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
}
