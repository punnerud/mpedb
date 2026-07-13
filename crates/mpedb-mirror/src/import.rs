//! Initial full import of a sqlite source into a fresh `.mpedb` file
//! (DESIGN-MIRROR §4). Introspect → build schema → create the mpedb file →
//! stream rows from ONE sqlite read snapshot → bulk-insert in bounded batches,
//! writing per-table resume watermarks. The final commit publishes the mirror
//! config/epoch and turns on CDC capture, so later local writes are tracked.
//!
//! Type-fidelity policy for M2.3 is strict-reject (the §4.5 import default):
//! a per-row conversion violation aborts the import with a report. Per-column
//! overrides, quarantine, and the timestamp-convention config are refinements
//! for later stages.

use std::path::Path;

use mpedb::{Database, WriteSession};
use mpedb_core::CaptureConfig;
use mpedb_types::{
    keycode, ColumnType, Config, DbOptions, Durability, FilePerms, Value,
};
use mpedb_types::{Concurrency, Error, Result};
use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::sqlite;
use crate::state;

/// Knobs for an import run.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Size of the created `.mpedb` file. Size for growth + churn + headroom.
    pub size_bytes: u64,
    pub durability: Durability,
    /// Allow-list of source table names (None = all).
    pub include: Option<Vec<String>>,
    /// Removed source table names.
    pub exclude: Vec<String>,
    /// Rows per apply transaction.
    pub batch_rows: usize,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            size_bytes: 256 * 1024 * 1024,
            durability: Durability::None,
            include: None,
            exclude: Vec::new(),
            batch_rows: 4096,
        }
    }
}

/// Per-table outcome of an import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableImportStat {
    pub name: String,
    pub table_id: u32,
    pub rows: u64,
}

/// Summary of an import run.
#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub tables: Vec<TableImportStat>,
}

impl ImportReport {
    pub fn total_rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
}

/// Import `src` (a sqlite connection) into a NEW `.mpedb` at `dest_path`.
/// Returns the opened mirror database and a report. `dest_path` must not exist.
pub fn import_sqlite(
    src: &mut Connection,
    dest_path: &Path,
    opts: &ImportOptions,
) -> Result<(Database, ImportReport)> {
    if dest_path.exists() {
        return Err(Error::Config(format!(
            "import destination {} already exists (use regenerate to re-import)",
            dest_path.display()
        )));
    }

    // 1. introspect + build the mpedb schema
    let src_tables = sqlite::introspect(src, opts.include.as_deref(), &opts.exclude)?;
    if src_tables.is_empty() {
        return Err(Error::Config("no mirrorable tables in source".into()));
    }
    let schema = sqlite::build_schema(&src_tables)?;

    // 2. create the mpedb file (secure-by-default perms)
    let config = Config {
        options: DbOptions {
            path: dest_path.to_path_buf(),
            size_bytes: opts.size_bytes,
            max_readers: 64,
            durability: opts.durability,
            concurrency: Concurrency::Serial,
            perms: FilePerms {
                mode: None,
                owner: None,
                group: None,
            },
        },
        schema: schema.clone(),
    };
    let db = Database::open_with_config(config)?;

    // 3. read rows from ONE sqlite snapshot (deferred txn pins at first read)
    let tx = src
        .transaction()
        .map_err(|e| Error::Config(format!("sqlite snapshot: {e}")))?;

    let mut report = ImportReport::default();
    for src_t in &src_tables {
        // table_id = position of this table's name in the (name-sorted) schema
        let table_id = schema
            .tables
            .iter()
            .position(|t| t.name == src_t.name)
            .expect("introspected table is in the built schema") as u32;
        let rows = import_table(&db, &tx, src_t, table_id, opts.batch_rows)?;
        report.tables.push(TableImportStat {
            name: src_t.name.clone(),
            table_id,
            rows,
        });
    }
    // read-only; explicit rollback drops the snapshot
    tx.rollback()
        .map_err(|e| Error::Config(format!("sqlite snapshot release: {e}")))?;

    // 4. publish mirror config + epoch, and enable CDC capture (final commit)
    publish_mirror_state(&db, &schema)?;

    Ok((db, report))
}

/// Stream one table's rows from the snapshot into mpedb in bounded batches,
/// writing the `imp/<tid>` resume watermark atomically with each batch.
fn import_table(
    db: &Database,
    tx: &rusqlite::Transaction,
    src_t: &sqlite::SourceTable,
    table_id: u32,
    batch_rows: usize,
) -> Result<u64> {
    let col_types: Vec<ColumnType> = src_t.columns.iter().map(|c| c.mapped).collect();
    let pk_cols = &src_t.pk;

    // SELECT "c1","c2",... FROM "t" ORDER BY "pk1","pk2"
    let cols_sql = src_t
        .columns
        .iter()
        .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let order_sql = pk_cols
        .iter()
        .map(|&i| format!("\"{}\"", src_t.columns[i].name.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {cols_sql} FROM \"{}\" ORDER BY {order_sql}",
        src_t.name.replace('"', "\"\"")
    );

    let mut stmt = tx
        .prepare(&sql)
        .map_err(|e| Error::Config(format!("sqlite read `{}`: {e}", src_t.name)))?;
    let ncol = src_t.columns.len();
    let mut rows_iter = stmt
        .query([])
        .map_err(|e| Error::Config(format!("sqlite read `{}`: {e}", src_t.name)))?;

    let mut total = 0u64;
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(batch_rows);
    loop {
        let next = rows_iter
            .next()
            .map_err(|e| Error::Config(format!("sqlite read `{}`: {e}", src_t.name)))?;
        match next {
            Some(row) => {
                let mut values = Vec::with_capacity(ncol);
                for (i, ct) in col_types.iter().enumerate() {
                    let vr = row
                        .get_ref(i)
                        .map_err(|e| Error::Config(format!("sqlite column read: {e}")))?;
                    values.push(convert_value(vr, *ct, &src_t.name, &src_t.columns[i].name)?);
                }
                batch.push(values);
                if batch.len() >= batch_rows {
                    total += flush_batch(db, table_id, pk_cols, &mut batch)?;
                }
            }
            None => break,
        }
    }
    if !batch.is_empty() {
        total += flush_batch(db, table_id, pk_cols, &mut batch)?;
    }
    Ok(total)
}

/// Insert one batch + its resume watermark in a single (capture-off) commit.
fn flush_batch(
    db: &Database,
    table_id: u32,
    pk_cols: &[usize],
    batch: &mut Vec<Vec<Value>>,
) -> Result<u64> {
    let mut s: WriteSession = db.begin()?;
    // cdc\0tabs is not set yet, so capture is already a no-op; be explicit.
    s.set_capture(false);
    let mut last_pk: Vec<u8> = Vec::new();
    for row in batch.iter() {
        s.insert_row(table_id, row)?;
        let pk_vals: Vec<Value> = pk_cols.iter().map(|&i| row[i].clone()).collect();
        last_pk = keycode::encode_key(&pk_vals);
    }
    s.sys_record_put(state::MIR_NS, &state::imp_key(table_id), &last_pk)?;
    s.commit()?;
    let n = batch.len() as u64;
    batch.clear();
    Ok(n)
}

/// Convert a sqlite value to the mapped mpedb type. Strict-reject on any
/// violation (the §4.5 import default).
fn convert_value(vr: ValueRef, ct: ColumnType, table: &str, col: &str) -> Result<Value> {
    let violation = |what: &str| {
        Err(Error::TypeMismatch(format!(
            "sqlite `{table}.{col}`: {what} (import is strict-reject)"
        )))
    };
    Ok(match vr {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => match ct {
            ColumnType::Int64 => Value::Int(i),
            ColumnType::Float64 => Value::Float(i as f64),
            ColumnType::Bool => match i {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return violation("non-0/1 integer in BOOL column"),
            },
            // default timestamp convention: INTEGER seconds → micros
            ColumnType::Timestamp => Value::Timestamp(i.saturating_mul(1_000_000)),
            ColumnType::Text | ColumnType::Blob => return violation("integer in text/blob column"),
        },
        ValueRef::Real(f) => match ct {
            ColumnType::Float64 => Value::Float(f),
            _ => return violation("real in a non-float column"),
        },
        ValueRef::Text(bytes) => match ct {
            ColumnType::Text => match std::str::from_utf8(bytes) {
                Ok(s) => Value::Text(s.to_string()),
                Err(_) => return violation("invalid UTF-8 in TEXT column"),
            },
            ColumnType::Blob => Value::Blob(bytes.to_vec()),
            _ => return violation("text in a non-text column"),
        },
        ValueRef::Blob(bytes) => match ct {
            ColumnType::Blob => Value::Blob(bytes.to_vec()),
            _ => return violation("blob in a non-blob column"),
        },
    })
}

/// Publish `mir\0cfg`, `mir\0epoch`, and enable capture on the mirrored tables
/// (`cdc\0tabs`) in one final commit — the S1 → SRC_AUTH handoff.
fn publish_mirror_state(db: &Database, schema: &mpedb_types::Schema) -> Result<()> {
    let scope: Vec<u32> = (0..schema.tables.len() as u32).collect();
    let cfg = state::MirrorConfig {
        mirror_id: mirror_id_for(db.path()),
        source_kind: state::SourceKind::Sqlite,
        mode: state::CaptureMode::Tracked,
        canonicalization_id: 1,
        scope: scope.clone(),
    };
    let epoch = state::Epoch {
        epoch: 1,
        authority: state::Authority::Source,
        state: state::MirrorState::SrcAuth,
        frozen: false,
    };
    let mut capture = CaptureConfig {
        generation: 1,
        ..Default::default()
    };
    for &t in &scope {
        capture.set_captured(t, true);
    }

    let mut s = db.begin()?;
    s.set_capture(false);
    s.sys_record_put(state::MIR_NS, state::KEY_CFG, &cfg.encode())?;
    s.sys_record_put(state::MIR_NS, state::KEY_EPOCH, &epoch.encode())?;
    // cdc\0tabs: ns="cdc", key="tabs" → the exact key the engine reads.
    s.sys_record_put("cdc", b"tabs", &capture.encode())?;
    s.commit()
}

/// A stable 128-bit mirror id from the destination path (placeholder for the
/// DSN-plus-nonce id of §12; carries no secret).
fn mirror_id_for(path: &Path) -> [u8; 16] {
    let h = xxhash_rust::xxh3::xxh3_128(path.to_string_lossy().as_bytes());
    h.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(r: mpedb::ExecResult) -> Vec<Vec<Value>> {
        match r {
            mpedb::ExecResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("{name}-{}.mpedb", std::process::id()));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn import_roundtrips_rows_and_marks_mirror() {
        let mut src = Connection::open_in_memory().unwrap();
        src.execute_batch(
            "CREATE TABLE users(
                 id INTEGER PRIMARY KEY,
                 email TEXT NOT NULL UNIQUE,
                 age INTEGER,
                 active BOOLEAN,
                 balance REAL);
             INSERT INTO users VALUES (1,'a@x.no',30,1,1.5),
                                      (2,'b@x.no',NULL,0,-2.0),
                                      (3,'c@x.no',41,1,0.0);
             CREATE TABLE kv(k INTEGER PRIMARY KEY, blob BLOB);
             INSERT INTO kv VALUES (10, x'0102ff'), (11, NULL);",
        )
        .unwrap();

        let dest = tmp("import");
        let (db, report) = import_sqlite(&mut src, &dest, &ImportOptions::default()).unwrap();

        assert_eq!(report.total_rows(), 5);
        let users_stat = report.tables.iter().find(|t| t.name == "users").unwrap();
        assert_eq!(users_stat.rows, 3);

        // rows are queryable in mpedb
        let n = rows(db
            .query("SELECT id FROM users WHERE age > $1", &[Value::Int(35)])
            .unwrap());
        assert_eq!(n.len(), 1); // only id=3 (age 41)

        // NULL age preserved
        let r = rows(db
            .query("SELECT age FROM users WHERE id = $1", &[Value::Int(2)])
            .unwrap());
        assert_eq!(r[0][0], Value::Null);

        // blob preserved
        let r = rows(db
            .query("SELECT blob FROM kv WHERE k = $1", &[Value::Int(10)])
            .unwrap());
        assert_eq!(r[0][0], Value::Blob(vec![1, 2, 255]));

        // mirror state published + capture enabled
        assert_eq!(
            db.sys_record_get(state::MIR_NS, state::KEY_EPOCH).unwrap(),
            Some(state::Epoch {
                epoch: 1,
                authority: state::Authority::Source,
                state: state::MirrorState::SrcAuth,
                frozen: false,
            }
            .encode()
            .to_vec())
        );
        let cap = db.sys_record_get("cdc", b"tabs").unwrap().unwrap();
        let cap = CaptureConfig::decode(&cap).unwrap();
        assert!(cap.is_captured(0) && cap.is_captured(1));

        // resume watermark written for each table
        assert!(db
            .sys_record_get(state::MIR_NS, &state::imp_key(users_stat.table_id))
            .unwrap()
            .is_some());

        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn strict_reject_on_type_violation() {
        // BOOL column carrying a non-0/1 integer must abort the import
        let mut src = Connection::open_in_memory().unwrap();
        src.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, flag BOOLEAN);
             INSERT INTO t VALUES (1, 7);",
        )
        .unwrap();
        let dest = tmp("reject");
        let err = import_sqlite(&mut src, &dest, &ImportOptions::default());
        assert!(err.is_err());
        let _ = std::fs::remove_file(&dest);
    }
}
