//! Trigger changelog for a MySQL-protocol source.
//!
//! The shape matches the sqlite tracker: one `_mpedb_log_<table>` per mirrored
//! table, PK columns only, `op` 1 = upsert and 2 = tombstone, and an `origin`
//! tag so a later pull can drop this mirror's own push. The pull re-reads the
//! row image; the log never stores it.

use std::collections::BTreeMap;

use mpedb_types::{Error, Result};
use mysql::prelude::Queryable;

use crate::mysql_src::q;
use crate::sqlite::SourceTable;

pub(crate) const OP_UPSERT: i64 = 1;
pub(crate) const OP_TOMBSTONE: i64 = 2;
pub(crate) const SELF_ORIGIN: &str = "mpedb";

pub(crate) fn log_table(src_table: &str) -> String {
    format!("_mpedb_log_{src_table}")
}

fn trigger_name(prefix: &str, table: &str) -> Result<String> {
    let name = format!("_mpedb_{prefix}_{table}");
    if name.len() > 64 {
        return Err(Error::Unsupported(format!(
            "mysql table `{table}` is too long for a trigger name (64-character limit)"
        )));
    }
    Ok(name)
}

/// Install the changelog and AFTER row triggers. Idempotent: an existing
/// trigger is dropped and created again, so a second attach sees the same log.
pub(crate) fn install_triggers(conn: &mut mysql::Conn, src: &SourceTable) -> Result<()> {
    let log = log_table(&src.name);
    let pk_defs: Vec<String> = src
        .pk
        .iter()
        .enumerate()
        .map(|(n, &i)| format!("pk{n} {} NOT NULL", src.columns[i].declared_type))
        .collect();
    let pk_cols: Vec<String> = (0..src.pk.len()).map(|n| format!("pk{n}")).collect();
    let cols_list = pk_cols.join(", ");
    let new_refs = src
        .pk
        .iter()
        .map(|&i| format!("NEW.{}", q(&src.columns[i].name)))
        .collect::<Vec<_>>()
        .join(", ");
    let old_refs = src
        .pk
        .iter()
        .map(|&i| format!("OLD.{}", q(&src.columns[i].name)))
        .collect::<Vec<_>>()
        .join(", ");
    let unchanged = src
        .pk
        .iter()
        .map(|&i| {
            let c = q(&src.columns[i].name);
            format!("OLD.{c} <=> NEW.{c}")
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let create_log = format!(
        "CREATE TABLE IF NOT EXISTS {} (\
             seq BIGINT NOT NULL AUTO_INCREMENT, \
             op TINYINT NOT NULL, \
             origin VARCHAR(16) NULL, \
             {}, \
             PRIMARY KEY (seq)) ENGINE=InnoDB",
        q(&log),
        pk_defs.join(", ")
    );
    let ai = trigger_name("ai", &src.name)?;
    let au = trigger_name("au", &src.name)?;
    let ad = trigger_name("ad", &src.name)?;
    let trig_ai = format!(
        "CREATE TRIGGER {} AFTER INSERT ON {} FOR EACH ROW \
         BEGIN \
           INSERT INTO {} (op, origin, {cols_list}) VALUES ({OP_UPSERT}, NULL, {new_refs}); \
         END",
        q(&ai),
        q(&src.name),
        q(&log)
    );
    // A primary-key change is a delete of the old key plus an upsert of the new
    // one. `<=>` is NULL-safe; a PK is NOT NULL, and the same predicate still
    // holds if a column is later allowed to be null.
    let trig_au = format!(
        "CREATE TRIGGER {} AFTER UPDATE ON {} FOR EACH ROW \
         BEGIN \
           IF NOT ({unchanged}) THEN \
             INSERT INTO {} (op, origin, {cols_list}) VALUES ({OP_TOMBSTONE}, NULL, {old_refs}); \
           END IF; \
           INSERT INTO {} (op, origin, {cols_list}) VALUES ({OP_UPSERT}, NULL, {new_refs}); \
         END",
        q(&au),
        q(&src.name),
        q(&log),
        q(&log)
    );
    let trig_ad = format!(
        "CREATE TRIGGER {} AFTER DELETE ON {} FOR EACH ROW \
         BEGIN \
           INSERT INTO {} (op, origin, {cols_list}) VALUES ({OP_TOMBSTONE}, NULL, {old_refs}); \
         END",
        q(&ad),
        q(&src.name),
        q(&log)
    );
    for sql in [
        create_log,
        format!("DROP TRIGGER IF EXISTS {}", q(&ai)),
        trig_ai,
        format!("DROP TRIGGER IF EXISTS {}", q(&au)),
        trig_au,
        format!("DROP TRIGGER IF EXISTS {}", q(&ad)),
        trig_ad,
    ] {
        conn.query_drop(sql)
            .map_err(|e| Error::Config(format!("mysql triggers `{}`: {e}", src.name)))?;
    }
    Ok(())
}

pub(crate) fn log_head(conn: &mut mysql::Conn, src_table: &str) -> Result<u64> {
    let sql = format!("SELECT COALESCE(MAX(seq), 0) FROM {}", q(&log_table(src_table)));
    let seq: Option<i64> = conn
        .exec_first(sql, ())
        .map_err(|e| Error::Config(format!("mysql log head `{src_table}`: {e}")))?;
    Ok(seq.unwrap_or(0) as u64)
}

/// Per-table cursor. Same layout as sqlite: the counters are per table, so one
/// scalar would skip a table whose counter lags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MysqlCursor(pub BTreeMap<u32, u64>);

impl MysqlCursor {
    pub(crate) fn seq(&self, table_id: u32) -> u64 {
        self.0.get(&table_id).copied().unwrap_or(0)
    }

    pub(crate) fn set(&mut self, table_id: u32, seq: u64) {
        self.0.insert(table_id, seq);
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(2 + self.0.len() * 12);
        v.extend_from_slice(&(self.0.len() as u16).to_be_bytes());
        for (&tid, &seq) in &self.0 {
            v.extend_from_slice(&tid.to_be_bytes());
            v.extend_from_slice(&seq.to_be_bytes());
        }
        v
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<MysqlCursor> {
        if bytes.len() < 2 {
            return Err(Error::Corrupt(format!(
                "mysql cursor is {} bytes (need >= 2)",
                bytes.len()
            )));
        }
        let n = u16::from_be_bytes(bytes[0..2].try_into().unwrap()) as usize;
        let want = 2 + n * 12;
        if bytes.len() != want {
            return Err(Error::Corrupt(format!(
                "mysql cursor: expected {want} bytes for {n} entries, got {}",
                bytes.len()
            )));
        }
        let mut map = BTreeMap::new();
        for i in 0..n {
            let off = 2 + i * 12;
            let tid = u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap());
            let seq = u64::from_be_bytes(bytes[off + 4..off + 12].try_into().unwrap());
            map.insert(tid, seq);
        }
        Ok(MysqlCursor(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips_sorted_by_table_id() {
        let mut c = MysqlCursor::default();
        c.set(2, 9);
        c.set(1, 4);
        let back = MysqlCursor::decode(&c.encode()).unwrap();
        assert_eq!(back.seq(1), 4);
        assert_eq!(back.seq(2), 9);
        assert_eq!(back.seq(3), 0);
        assert!(MysqlCursor::decode(&[0]).is_err());
    }

    #[test]
    fn quoted_identifier_doubles_backticks() {
        assert_eq!(crate::mysql_src::q("a`b"), "`a``b`");
    }
}
