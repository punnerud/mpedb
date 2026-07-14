//! Export a mirrored `.mpedb` back to a fresh sqlite database — the reverse of
//! [`crate::import`]. Used for the round-trip differential test
//! (sqlite → mpedb → sqlite, then diff) and, later, as a building block for the
//! switch-back verification and regenerate flows.
//!
//! Reopens the `.mpedb` config-free (the file is schema-authoritative), so it
//! works on any mirror file without a TOML config.

use std::path::Path;

use mpedb_core::Engine;
use mpedb_types::{ColumnType, Error, Result, Schema, Value};
use rusqlite::types::Value as SqlVal;
use rusqlite::Connection;

/// Per-table row count of an export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportStat {
    pub name: String,
    pub rows: u64,
}

/// Export summary.
#[derive(Clone, Debug, Default)]
pub struct ExportReport {
    pub tables: Vec<ExportStat>,
}

impl ExportReport {
    pub fn total_rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
}

/// The sqlite declared type an mpedb column maps back to — chosen so a
/// re-import (declared-type sniff + affinity) reconstructs the same mpedb type.
fn sqlite_type(ct: ColumnType) -> &'static str {
    match ct {
        ColumnType::Int64 => "INTEGER",
        ColumnType::Float64 => "REAL",
        ColumnType::Bool => "BOOLEAN",
        ColumnType::Text => "TEXT",
        ColumnType::Blob => "BLOB",
        ColumnType::Timestamp => "DATETIME",
    }
}

/// Reverse value mapping (mpedb → sqlite), inverse of import's `convert_value`.
/// Shared with the push adapter.
pub(crate) fn to_sql(v: &Value) -> SqlVal {
    match v {
        Value::Null => SqlVal::Null,
        Value::Int(i) => SqlVal::Integer(*i),
        Value::Float(f) => SqlVal::Real(*f),
        Value::Bool(b) => SqlVal::Integer(*b as i64),
        Value::Text(s) => SqlVal::Text(s.clone()),
        Value::Blob(b) => SqlVal::Blob(b.clone()),
        // Unreachable: a context list (§2.6) is param-only and cannot be stored,
        // so nothing read out of a mirrored column can be one. NULL is the least
        // harmful thing to emit if that ever changes.
        Value::List(_) => SqlVal::Null,
        // import mapped INTEGER seconds → micros; go back to seconds
        Value::Timestamp(us) => SqlVal::Integer(us.div_euclid(1_000_000)),
    }
}

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Export the mirror at `mpedb_path` into a new sqlite file `dest`.
pub fn export_sqlite(mpedb_path: &Path, dest: &Path) -> Result<ExportReport> {
    if dest.exists() {
        return Err(Error::Config(format!(
            "export destination {} already exists",
            dest.display()
        )));
    }
    let eng = Engine::open_from_file(mpedb_path)?;
    let r = eng.begin_read()?;
    let schema: Schema = r.stored_schema()?;

    let mut conn = Connection::open(dest)
        .map_err(|e| Error::Config(format!("create sqlite `{}`: {e}", dest.display())))?;

    let mut report = ExportReport::default();
    let tx = conn
        .transaction()
        .map_err(|e| Error::Config(format!("sqlite txn: {e}")))?;
    for (table_id, t) in schema.tables.iter().enumerate() {
        // CREATE TABLE with reverse-mapped types + NOT NULL + PK + UNIQUE
        let mut col_defs = Vec::new();
        for c in &t.columns {
            let mut def = format!("{} {}", q(&c.name), sqlite_type(c.ty));
            if !c.nullable {
                def.push_str(" NOT NULL");
            }
            if c.unique {
                def.push_str(" UNIQUE");
            }
            col_defs.push(def);
        }
        let pk_cols = t
            .primary_key
            .iter()
            .map(|&i| q(&t.columns[i as usize].name))
            .collect::<Vec<_>>()
            .join(", ");
        let create = format!(
            "CREATE TABLE {} ({}, PRIMARY KEY ({pk_cols}))",
            q(&t.name),
            col_defs.join(", ")
        );
        tx.execute(&create, [])
            .map_err(|e| Error::Config(format!("create `{}`: {e}", t.name)))?;

        // stream rows out of mpedb in PK order and insert
        let placeholders = vec!["?"; t.columns.len()].join(", ");
        let insert = format!("INSERT INTO {} VALUES ({placeholders})", q(&t.name));
        let mut stmt = tx
            .prepare(&insert)
            .map_err(|e| Error::Config(format!("prepare insert `{}`: {e}", t.name)))?;

        let mut cur = r.scan(table_id as u32, None, None)?;
        let mut n = 0u64;
        while let Some(row) = cur.next()? {
            let params: Vec<SqlVal> = row.iter().map(to_sql).collect();
            stmt.execute(rusqlite::params_from_iter(params.iter()))
                .map_err(|e| Error::Config(format!("insert into `{}`: {e}", t.name)))?;
            n += 1;
        }
        drop(stmt);
        report.tables.push(ExportStat {
            name: t.name.clone(),
            rows: n,
        });
    }
    tx.commit()
        .map_err(|e| Error::Config(format!("sqlite commit: {e}")))?;
    r.finish()?;
    Ok(report)
}

/// Compare the DATA of two sqlite databases table-by-table, row-by-row in PK
/// order. Returns a list of human-readable differences (empty ⇒ identical) —
/// the round-trip differential check (DESIGN-MIRROR §10.3).
pub fn diff_sqlite_data(a: &Connection, b: &Connection) -> Result<Vec<String>> {
    let mut diffs = Vec::new();
    let ta = user_tables(a)?;
    let tb = user_tables(b)?;
    if ta != tb {
        diffs.push(format!("table sets differ: {ta:?} vs {tb:?}"));
        return Ok(diffs);
    }
    for t in &ta {
        let pk = pk_columns(a, t)?;
        let ra = read_all(a, t, &pk)?;
        let rb = read_all(b, t, &pk)?;
        if ra.len() != rb.len() {
            diffs.push(format!("`{t}`: {} rows vs {} rows", ra.len(), rb.len()));
            continue;
        }
        for (i, (rowa, rowb)) in ra.iter().zip(rb.iter()).enumerate() {
            if rowa != rowb {
                diffs.push(format!("`{t}` row {i}: {rowa:?} != {rowb:?}"));
            }
        }
    }
    Ok(diffs)
}

fn user_tables(c: &Connection) -> Result<Vec<String>> {
    let mut stmt = c
        .prepare(
            "SELECT name FROM pragma_table_list WHERE type='table' \
             AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             AND name NOT LIKE '\\_mpedb\\_%' ESCAPE '\\' ORDER BY name",
        )
        .map_err(|e| Error::Config(format!("table_list: {e}")))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| Error::Config(format!("table_list: {e}")))?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Config(format!("table_list: {e}")))
}

fn pk_columns(c: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = c
        .prepare("SELECT name, pk FROM pragma_table_xinfo(?1) WHERE pk>0 ORDER BY pk")
        .map_err(|e| Error::Config(format!("table_xinfo: {e}")))?;
    let rows = stmt
        .query_map([table], |r| r.get::<_, String>(0))
        .map_err(|e| Error::Config(format!("table_xinfo: {e}")))?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Config(format!("table_xinfo: {e}")))
}

fn read_all(c: &Connection, table: &str, pk: &[String]) -> Result<Vec<Vec<SqlVal>>> {
    let order = if pk.is_empty() {
        String::new()
    } else {
        format!(
            " ORDER BY {}",
            pk.iter().map(|p| q(p)).collect::<Vec<_>>().join(", ")
        )
    };
    let sql = format!("SELECT * FROM {}{order}", q(table));
    let mut stmt = c
        .prepare(&sql)
        .map_err(|e| Error::Config(format!("read `{table}`: {e}")))?;
    let ncol = stmt.column_count();
    let rows = stmt
        .query_map([], |row| {
            let mut vals = Vec::with_capacity(ncol);
            for i in 0..ncol {
                vals.push(row.get::<_, SqlVal>(i)?);
            }
            Ok(vals)
        })
        .map_err(|e| Error::Config(format!("read `{table}`: {e}")))?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Config(format!("read `{table}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{import_sqlite, ImportOptions};

    fn tmp(name: &str, ext: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("{name}-{}.{ext}", std::process::id()));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn roundtrip_sqlite_to_mpedb_to_sqlite_is_identical() {
        // 1. original sqlite source with a spread of types
        let orig_path = tmp("rt-orig", "db");
        let orig = Connection::open(&orig_path).unwrap();
        orig.execute_batch(
            "CREATE TABLE users(
                 id INTEGER PRIMARY KEY,
                 email TEXT NOT NULL UNIQUE,
                 age INTEGER,
                 active BOOLEAN,
                 balance REAL,
                 created_at DATETIME);
             INSERT INTO users VALUES
                 (1,'a@x.no',30,1,1.5,1700000000),
                 (2,'b@x.no',NULL,0,-2.25,1700000100),
                 (3,'c@x.no',41,1,0.0,1700000200);
             CREATE TABLE parts(sku INTEGER PRIMARY KEY, blob BLOB, label TEXT);
             INSERT INTO parts VALUES (10, x'0102ff', 'bolt'), (11, NULL, 'nut');",
        )
        .unwrap();
        drop(orig);

        // 2. import into mpedb, then drop the handle so we can reopen the file
        let mpedb_path = tmp("rt-mid", "mpedb");
        {
            let mut src = Connection::open(&orig_path).unwrap();
            let (_db, report) =
                import_sqlite(&mut src, &mpedb_path, &ImportOptions::default()).unwrap();
            assert_eq!(report.total_rows(), 5);
        }

        // 3. export mpedb back out to a fresh sqlite
        let rt_path = tmp("rt-out", "db");
        let exp = export_sqlite(&mpedb_path, &rt_path).unwrap();
        assert_eq!(exp.total_rows(), 5);

        // 4. diff original vs round-tripped — must be byte-identical data
        let a = Connection::open(&orig_path).unwrap();
        let b = Connection::open(&rt_path).unwrap();
        let diffs = diff_sqlite_data(&a, &b).unwrap();
        assert!(diffs.is_empty(), "round-trip differences: {diffs:#?}");

        // and the round-tripped file re-imports (schema is preserved enough)
        let tables = crate::sqlite::introspect(&b, None, &[]).unwrap();
        assert_eq!(tables.len(), 2);

        for p in [orig_path, mpedb_path, rt_path] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn diff_detects_a_changed_row() {
        let pa = tmp("diff-a", "db");
        let pb = tmp("diff-b", "db");
        let a = Connection::open(&pa).unwrap();
        let b = Connection::open(&pb).unwrap();
        for c in [&a, &b] {
            c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);")
                .unwrap();
        }
        a.execute_batch("INSERT INTO t VALUES (1,10),(2,20);").unwrap();
        b.execute_batch("INSERT INTO t VALUES (1,10),(2,99);").unwrap();
        let diffs = diff_sqlite_data(&a, &b).unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("row 1"));
        for p in [pa, pb] {
            let _ = std::fs::remove_file(p);
        }
    }
}
