//! Import a MySQL-protocol database into a new `.mpedb` mirror.
//!
//! Triggers go in before the consistent snapshot, so a commit that lands during
//! the copy is still in the changelog for the first pull. The snapshot itself
//! is only read. This records [`SourceKind::Mysql`], never sqlite, so a later
//! open does not treat the file as a first-hop sqlite mirror.

use std::path::Path;

use mpedb::Database;
use mpedb_types::{Error, Result, Value};
use mysql::prelude::Queryable;

use crate::import::{self, flush_batch, ImportOptions, ImportReport, TableImportStat};
use crate::mysql_src::{self, data_type_of, from_mysql, map_policy, q, row_value};
use crate::mysql_track;
use crate::sourcecfg::SourceSpec;
use crate::sqlite::{self, SourceTable};
use crate::state::{self, ColumnMap, TableMap};

pub fn import_mysql(
    spec: &SourceSpec,
    dest: &Path,
    opts: &ImportOptions,
) -> Result<(Database, ImportReport)> {
    if !matches!(spec, SourceSpec::Mysql { .. }) {
        return Err(Error::Config("import_mysql requires a mysql source spec".into()));
    }
    if dest.exists() {
        return Err(Error::Config(format!(
            "import destination {} already exists",
            dest.display()
        )));
    }
    let mut conn = mysql_src::connect(spec)?;
    let src_tables = mysql_src::introspect(&mut conn, opts.include.as_deref(), &opts.exclude)?;
    let schema = sqlite::build_schema(&src_tables)?;
    // Lenient bare-column dialect, the same one a sqlite import records. A
    // MySQL mirror is not a PostgreSQL mirror and must not grow PG's GROUP BY
    // rejection on the way through.
    let db = import::create_mirror_db(
        dest,
        schema.clone(),
        opts.size_bytes,
        opts.durability,
        mpedb_types::Dialect::Sqlite,
    )?;
    for src in &src_tables {
        mysql_track::install_triggers(&mut conn, src)?;
    }
    conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT")
        .map_err(|e| Error::Config(format!("mysql snapshot: {e}")))?;
    let copied = read_snapshot(&mut conn, &db, &schema, &src_tables, opts);
    let released = conn.query_drop("ROLLBACK");
    let report = copied?;
    released.map_err(|e| Error::Config(format!("mysql snapshot release: {e}")))?;
    let maps = table_maps(&src_tables, &schema);
    import::publish_mirror_state(&db, &schema, state::SourceKind::Mysql, &maps)?;
    Ok((db, report))
}

fn read_snapshot(
    conn: &mut mysql::Conn,
    db: &Database,
    schema: &mpedb_types::Schema,
    src_tables: &[SourceTable],
    opts: &ImportOptions,
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    for src in src_tables {
        let table_id = schema
            .tables
            .iter()
            .position(|t| t.name == src.name)
            .ok_or_else(|| Error::Internal(format!("mysql table `{}` missing from schema", src.name)))?
            as u32;
        let rows = copy_table(conn, db, src, table_id, opts.batch_rows)?;
        report.tables.push(TableImportStat {
            name: src.name.clone(),
            table_id,
            rows,
        });
    }
    Ok(report)
}

fn copy_table(
    conn: &mut mysql::Conn,
    db: &Database,
    src: &SourceTable,
    table_id: u32,
    batch_rows: usize,
) -> Result<u64> {
    let cols = src.columns.iter().map(|c| q(&c.name)).collect::<Vec<_>>().join(", ");
    let order = src
        .pk
        .iter()
        .map(|&i| q(&src.columns[i].name))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {cols} FROM {} ORDER BY {order}", q(&src.name));
    let queried: Vec<mysql::Row> = conn
        .exec(sql, ())
        .map_err(|e| Error::Config(format!("mysql read `{}`: {e}", src.name)))?;
    let pk_cols = src.pk.clone();
    let mut batch: Vec<Vec<Value>> = Vec::new();
    let mut total = 0u64;
    for row in queried {
        let mut values = Vec::with_capacity(src.columns.len());
        for (i, col) in src.columns.iter().enumerate() {
            values.push(from_mysql(row_value(&row, i)?, col.mapped, &src.name, &col.name)?);
        }
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

fn table_maps(src_tables: &[SourceTable], schema: &mpedb_types::Schema) -> Vec<(u32, TableMap)> {
    let mut out = Vec::with_capacity(src_tables.len());
    for src in src_tables {
        let Some(table_id) = schema.tables.iter().position(|t| t.name == src.name) else {
            continue;
        };
        let columns = src
            .columns
            .iter()
            .map(|c| ColumnMap {
                source_name: c.name.clone(),
                source_type: c.declared_type.clone(),
                not_null: c.not_null,
                generated: false,
                identity: false,
                unique: c.unique,
                mapped: c.mapped,
                policy: map_policy(data_type_of(&c.declared_type)),
            })
            .collect();
        out.push((
            table_id as u32,
            TableMap {
                source_name: src.name.clone(),
                columns,
            },
        ));
    }
    out
}
