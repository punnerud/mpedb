//! Initial full import of a PostgreSQL source into a fresh `.mpedb`
//! (DESIGN-MIRROR §4.3). Introspect → build schema → create the mpedb file →
//! read every table from ONE REPEATABLE READ read-only snapshot, casting each
//! column to a form the sync `postgres` client decodes directly (int8/float8/
//! bool/text/bytea, timestamps to int8 micros, uuid to 16-byte bytea, numeric/
//! json to canonical text) → bulk-insert in bounded batches with resume
//! watermarks → publish the mirror config/epoch and enable capture.

use std::path::Path;

use mpedb::Database;
use mpedb_types::{ColumnType, Error, Result, Value};
use postgres::{Client, IsolationLevel};

use crate::import::{
    create_mirror_db, flush_batch, publish_mirror_state, ImportOptions, ImportReport,
    TableImportStat,
};
use crate::pg::{self, PgColumn, PgTable};
use crate::state;

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// The SQL expression that reads a source column in a form the `postgres` client
/// decodes to the primitive matching the column's mpedb type.
fn read_expr(c: &PgColumn) -> String {
    let col = q(&c.name);
    match c.mapped {
        Some(ColumnType::Int64) => format!("{col}::int8"),
        Some(ColumnType::Float64) => format!("{col}::float8"),
        Some(ColumnType::Bool) => format!("{col}::bool"),
        Some(ColumnType::Text) => match c.pg_type.as_str() {
            // jsonb::text is canonical (sorted keys, normalised whitespace)
            "json" | "jsonb" => format!("{col}::jsonb::text"),
            _ => format!("{col}::text"),
        },
        Some(ColumnType::Blob) => match c.pg_type.as_str() {
            "uuid" => format!("decode(replace({col}::text, '-', ''), 'hex')"),
            _ => format!("{col}::bytea"),
        },
        // micros since the Unix epoch (timestamptz/timestamp/date)
        Some(ColumnType::Timestamp) => format!("(extract(epoch from {col}) * 1000000)::int8"),
        None => "NULL".to_string(), // unreachable: rejected at build_schema
    }
}

fn read_value(row: &postgres::Row, i: usize, ty: ColumnType) -> Value {
    match ty {
        ColumnType::Int64 => row
            .get::<usize, Option<i64>>(i)
            .map_or(Value::Null, Value::Int),
        ColumnType::Float64 => row
            .get::<usize, Option<f64>>(i)
            .map_or(Value::Null, Value::Float),
        ColumnType::Bool => row
            .get::<usize, Option<bool>>(i)
            .map_or(Value::Null, Value::Bool),
        ColumnType::Text => row
            .get::<usize, Option<String>>(i)
            .map_or(Value::Null, Value::Text),
        ColumnType::Blob => row
            .get::<usize, Option<Vec<u8>>>(i)
            .map_or(Value::Null, Value::Blob),
        ColumnType::Timestamp => row
            .get::<usize, Option<i64>>(i)
            .map_or(Value::Null, Value::Timestamp),
    }
}

/// Import PostgreSQL `client`'s `public` schema into a NEW `.mpedb` at `dest`.
pub fn import_pg(
    client: &mut Client,
    dest: &Path,
    opts: &ImportOptions,
) -> Result<(Database, ImportReport)> {
    if dest.exists() {
        return Err(Error::Config(format!(
            "import destination {} already exists",
            dest.display()
        )));
    }
    let src_tables = pg::introspect(client, opts.include.as_deref(), &opts.exclude)?;
    if src_tables.is_empty() {
        return Err(Error::Config("no mirrorable tables in source".into()));
    }
    let schema = pg::build_schema(&src_tables)?;
    let db = create_mirror_db(dest, schema.clone(), opts.size_bytes, opts.durability)?;

    // one consistent snapshot for the whole import
    let mut tx = client
        .build_transaction()
        .read_only(true)
        .isolation_level(IsolationLevel::RepeatableRead)
        .start()
        .map_err(|e| Error::Config(format!("pg snapshot: {e}")))?;

    let mut report = ImportReport::default();
    for src_t in &src_tables {
        let table_id = schema
            .tables
            .iter()
            .position(|t| t.name == src_t.name)
            .expect("introspected table is in the built schema") as u32;
        let rows = import_table(&db, &mut tx, src_t, table_id, opts.batch_rows)?;
        report.tables.push(TableImportStat {
            name: src_t.name.clone(),
            table_id,
            rows,
        });
    }
    tx.rollback()
        .map_err(|e| Error::Config(format!("pg snapshot release: {e}")))?;

    publish_mirror_state(&db, &schema, state::SourceKind::Postgres)?;
    Ok((db, report))
}

fn import_table(
    db: &Database,
    tx: &mut postgres::Transaction,
    src: &PgTable,
    table_id: u32,
    batch_rows: usize,
) -> Result<u64> {
    let col_types: Vec<ColumnType> = src
        .columns
        .iter()
        .map(|c| c.mapped.expect("build_schema rejected unmappable columns"))
        .collect();
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
    let pk_cols: Vec<usize> = src.pk.clone();

    let rows = tx
        .query(&sql, &[])
        .map_err(|e| Error::Config(format!("pg read `{}`: {e}", src.name)))?;

    let mut total = 0u64;
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(batch_rows);
    for row in &rows {
        let values: Vec<Value> = col_types
            .iter()
            .enumerate()
            .map(|(i, &ct)| read_value(row, i, ct))
            .collect();
        batch.push(values);
        if batch.len() >= batch_rows {
            total += flush_batch(db, table_id, &pk_cols, &mut batch)?;
        }
    }
    if !batch.is_empty() {
        total += flush_batch(db, table_id, &pk_cols, &mut batch)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpedb::ExecResult;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("{name}-{}.mpedb", std::process::id()));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        p
    }

    fn rows(db: &Database, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
        match db.query(sql, params).unwrap() {
            ExecResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "needs PostgreSQL (run with --ignored)"]
    fn import_pg_roundtrips_types() {
        let pg = crate::pg_harness::ThrowawayPg::start();
        let mut c = pg.client();
        c.batch_execute(
            "CREATE TABLE users(
                 id bigint PRIMARY KEY,
                 email text NOT NULL UNIQUE,
                 age int,
                 active boolean,
                 balance double precision,
                 amount numeric,
                 uid uuid,
                 created_at timestamptz);
             INSERT INTO users VALUES
                 (1,'a@x',30,true,1.5,'12.34','00112233-4455-6677-8899-aabbccddeeff',
                  '2023-11-14T22:13:20Z'),
                 (2,'b@x',NULL,false,-2.0,'0.01',NULL,'2023-11-14T22:15:00Z');",
        )
        .unwrap();

        let dest = tmp("pg-import");
        let (db, report) = import_pg(&mut c, &dest, &ImportOptions::default()).unwrap();
        assert_eq!(report.total_rows(), 2);

        // typed round-trip through mpedb
        let r = rows(&db, "SELECT age, active, balance, amount FROM users WHERE id=$1", &[Value::Int(1)]);
        assert_eq!(r[0][0], Value::Int(30));
        assert_eq!(r[0][1], Value::Bool(true));
        assert_eq!(r[0][2], Value::Float(1.5));
        assert_eq!(r[0][3], Value::Text("12.34".into())); // numeric -> canonical text
        // NULL preserved
        let r = rows(&db, "SELECT age FROM users WHERE id=$1", &[Value::Int(2)]);
        assert_eq!(r[0][0], Value::Null);
        // uuid -> 16 bytes
        let r = rows(&db, "SELECT uid FROM users WHERE id=$1", &[Value::Int(1)]);
        assert_eq!(
            r[0][0],
            Value::Blob(vec![
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ])
        );
        // timestamptz -> micros (2023-11-14T22:13:20Z = 1700000000 s)
        let r = rows(&db, "SELECT created_at FROM users WHERE id=$1", &[Value::Int(1)]);
        assert_eq!(r[0][0], Value::Timestamp(1_700_000_000_000_000));

        // mirror published as Postgres-sourced
        let cfg = db.sys_record_get(state::MIR_NS, state::KEY_CFG).unwrap().unwrap();
        assert_eq!(
            state::MirrorConfig::decode(&cfg).unwrap().source_kind,
            state::SourceKind::Postgres
        );

        let _ = std::fs::remove_file(&dest);
    }
}
