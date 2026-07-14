//! sqlite tracked-mode change capture (DESIGN-MIRROR §5.1): per-table changelog
//! tables maintained by AFTER INSERT/UPDATE/DELETE triggers, and the per-table
//! seq cursor. This is the primary sqlite CDC path — triggers are schema
//! objects, so they fire for writes from ALL connections/processes, and the
//! delete trigger disables sqlite's truncate optimisation so `DELETE FROM t`
//! (no WHERE) is captured per row.
//!
//! Trigger payload is PK-only; the pull round (M3.3) re-reads the current row
//! image from a snapshot. `seq` is `INTEGER PRIMARY KEY AUTOINCREMENT` so it is
//! strictly monotone and never reused after GC (a plain rowid would restart at
//! max+1 and corrupt the cursor — §5.1).

use std::collections::BTreeMap;

use mpedb_types::{Error, Result};
use rusqlite::Connection;

use crate::sqlite::SourceTable;

/// Op tags in the changelog `op` column.
pub const OP_UPSERT: i64 = 1;
pub const OP_TOMBSTONE: i64 = 2;

/// The per-table changelog table name for a mirrored source table.
pub fn log_table(src_table: &str) -> String {
    format!("_mpedb_log_{src_table}")
}

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Install the changelog table + AFTER row triggers for one mirrored table.
/// Idempotent (`IF NOT EXISTS` / `DROP TRIGGER IF EXISTS`).
pub fn install_triggers(conn: &Connection, src: &SourceTable) -> Result<()> {
    let name = &src.name;
    let log = log_table(name);
    let npk = src.pk.len();
    // log PK columns are positional (pk0, pk1, …); dynamic typing holds any PK.
    let log_pk_cols: Vec<String> = (0..npk).map(|i| format!("pk{i}")).collect();
    let pk_src_cols: Vec<&str> = src.pk.iter().map(|&i| src.columns[i].name.as_str()).collect();

    let create_log = format!(
        "CREATE TABLE IF NOT EXISTS {} (\
             seq INTEGER PRIMARY KEY AUTOINCREMENT, \
             op INTEGER NOT NULL, \
             origin TEXT, {})",
        q(&log),
        log_pk_cols.join(", ")
    );

    let cols_list = log_pk_cols.join(", ");
    let new_refs = pk_src_cols
        .iter()
        .map(|c| format!("NEW.{}", q(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let old_refs = pk_src_cols
        .iter()
        .map(|c| format!("OLD.{}", q(c)))
        .collect::<Vec<_>>()
        .join(", ");
    // NULL-safe PK-change predicate (IS NOT), OR'd across PK columns.
    let pk_changed = pk_src_cols
        .iter()
        .map(|c| format!("OLD.{0} IS NOT NEW.{0}", q(c)))
        .collect::<Vec<_>>()
        .join(" OR ");

    let ai = format!("_mpedb_ai_{name}");
    let au = format!("_mpedb_au_{name}");
    let ad = format!("_mpedb_ad_{name}");

    let trig_ai = format!(
        "CREATE TRIGGER {} AFTER INSERT ON {} BEGIN \
             INSERT INTO {}(op, origin, {cols_list}) VALUES ({OP_UPSERT}, NULL, {new_refs}); \
         END",
        q(&ai),
        q(name),
        q(&log)
    );
    // UPDATE: if the PK changed, tombstone OLD then upsert NEW; else just upsert.
    let trig_au = format!(
        "CREATE TRIGGER {} AFTER UPDATE ON {} BEGIN \
             INSERT INTO {}(op, origin, {cols_list}) \
                 SELECT {OP_TOMBSTONE}, NULL, {old_refs} WHERE {pk_changed}; \
             INSERT INTO {}(op, origin, {cols_list}) VALUES ({OP_UPSERT}, NULL, {new_refs}); \
         END",
        q(&au),
        q(name),
        q(&log),
        q(&log)
    );
    let trig_ad = format!(
        "CREATE TRIGGER {} AFTER DELETE ON {} BEGIN \
             INSERT INTO {}(op, origin, {cols_list}) VALUES ({OP_TOMBSTONE}, NULL, {old_refs}); \
         END",
        q(&ad),
        q(name),
        q(&log)
    );

    let batch = format!(
        "{create_log};\n\
         DROP TRIGGER IF EXISTS {0};\n{trig_ai};\n\
         DROP TRIGGER IF EXISTS {1};\n{trig_au};\n\
         DROP TRIGGER IF EXISTS {2};\n{trig_ad};",
        q(&ai),
        q(&au),
        q(&ad)
    );
    conn.execute_batch(&batch)
        .map_err(|e| Error::Config(format!("install triggers for `{name}`: {e}")))?;
    Ok(())
}

/// Remove the changelog + triggers for one table (used by `mirror detach`).
pub fn uninstall_triggers(conn: &Connection, src_table: &str) -> Result<()> {
    let batch = format!(
        "DROP TRIGGER IF EXISTS {};\nDROP TRIGGER IF EXISTS {};\n\
         DROP TRIGGER IF EXISTS {};\nDROP TABLE IF EXISTS {};",
        q(&format!("_mpedb_ai_{src_table}")),
        q(&format!("_mpedb_au_{src_table}")),
        q(&format!("_mpedb_ad_{src_table}")),
        q(&log_table(src_table))
    );
    conn.execute_batch(&batch)
        .map_err(|e| Error::Config(format!("uninstall triggers for `{src_table}`: {e}")))?;
    Ok(())
}

/// The current max `seq` in a table's changelog (0 if empty) — an import
/// watermark or lag probe.
pub fn log_head(conn: &Connection, src_table: &str) -> Result<u64> {
    let log = log_table(src_table);
    conn.query_row(&format!("SELECT COALESCE(MAX(seq), 0) FROM {}", q(&log)), [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|v| v as u64)
    .map_err(|e| Error::Config(format!("log head for `{src_table}`: {e}")))
}

/// sqlite's per-table cursor: mpedb `table_id` → last consumed changelog `seq`.
/// AUTOINCREMENT counters are per-table, so a single scalar cursor would skip
/// entries in tables whose counter lags (§5.1 / review CONF#6) — hence a vector.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqliteCursor(pub BTreeMap<u32, u64>);

impl SqliteCursor {
    pub fn seq(&self, table_id: u32) -> u64 {
        self.0.get(&table_id).copied().unwrap_or(0)
    }

    pub fn set(&mut self, table_id: u32, seq: u64) {
        self.0.insert(table_id, seq);
    }

    /// Layout: n u16 BE, then n × (table_id u32 BE, seq u64 BE), table_id-sorted.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(2 + self.0.len() * 12);
        v.extend_from_slice(&(self.0.len() as u16).to_be_bytes());
        for (&tid, &seq) in &self.0 {
            v.extend_from_slice(&tid.to_be_bytes());
            v.extend_from_slice(&seq.to_be_bytes());
        }
        v
    }

    pub fn decode(bytes: &[u8]) -> Result<SqliteCursor> {
        if bytes.len() < 2 {
            return Err(Error::Corrupt(format!(
                "sqlite cursor is {} bytes (need >= 2)",
                bytes.len()
            )));
        }
        let n = u16::from_be_bytes(bytes[0..2].try_into().unwrap()) as usize;
        let want = 2 + n * 12;
        if bytes.len() != want {
            return Err(Error::Corrupt(format!(
                "sqlite cursor: expected {want} bytes for {n} entries, got {}",
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
        Ok(SqliteCursor(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::introspect;

    fn one_table(conn: &Connection) -> SourceTable {
        introspect(conn, None, &[]).unwrap().into_iter().next().unwrap()
    }

    #[test]
    fn triggers_capture_all_ops_and_pk_change() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);")
            .unwrap();
        let src = one_table(&conn);
        install_triggers(&conn, &src).unwrap();

        conn.execute_batch(
            "INSERT INTO t VALUES (1,'a');
             UPDATE t SET v='b' WHERE id=1;
             INSERT INTO t VALUES (2,'x');
             UPDATE t SET id=3 WHERE id=2;
             DELETE FROM t WHERE id=1;",
        )
        .unwrap();

        let log = log_table("t");
        let mut stmt = conn
            .prepare(&format!("SELECT seq, op, pk0 FROM {} ORDER BY seq", q(&log)))
            .unwrap();
        let got: Vec<(i64, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();

        // insert 1, update 1, insert 2, (pk-change 2->3 = tombstone 2 + upsert 3),
        // delete 1
        assert_eq!(
            got,
            vec![
                (1, OP_UPSERT, 1),
                (2, OP_UPSERT, 1),
                (3, OP_UPSERT, 2),
                (4, OP_TOMBSTONE, 2),
                (5, OP_UPSERT, 3),
                (6, OP_TOMBSTONE, 1),
            ]
        );
        assert_eq!(log_head(&conn, "t").unwrap(), 6);
    }

    #[test]
    fn delete_without_where_is_captured_per_row() {
        // the delete trigger disables sqlite's truncate optimisation
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);").unwrap();
        let src = one_table(&conn);
        install_triggers(&conn, &src).unwrap();
        conn.execute_batch("INSERT INTO t VALUES (1),(2),(3); DELETE FROM t;")
            .unwrap();
        // 3 inserts + 3 tombstones
        assert_eq!(log_head(&conn, "t").unwrap(), 6);
        let tombs: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {} WHERE op={OP_TOMBSTONE}", q(&log_table("t"))),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tombs, 3);
    }

    #[test]
    fn composite_pk_triggers() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(a INTEGER, b INTEGER, v TEXT, PRIMARY KEY(a,b));")
            .unwrap();
        let src = one_table(&conn);
        install_triggers(&conn, &src).unwrap();
        conn.execute_batch(
            "INSERT INTO t VALUES (1,2,'x');
             UPDATE t SET b=5 WHERE a=1 AND b=2;",
        )
        .unwrap();
        let log = log_table("t");
        let rows: Vec<(i64, i64, i64)> = conn
            .prepare(&format!("SELECT op, pk0, pk1 FROM {} ORDER BY seq", q(&log)))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        // insert (1,2); pk-change (1,2)->(1,5): tombstone (1,2) + upsert (1,5)
        assert_eq!(
            rows,
            vec![
                (OP_UPSERT, 1, 2),
                (OP_TOMBSTONE, 1, 2),
                (OP_UPSERT, 1, 5),
            ]
        );
    }

    #[test]
    fn cursor_roundtrip_and_truncation() {
        let mut c = SqliteCursor::default();
        c.set(0, 50_000);
        c.set(5, 12);
        assert_eq!(c.seq(0), 50_000);
        assert_eq!(c.seq(9), 0);
        let bytes = c.encode();
        assert_eq!(SqliteCursor::decode(&bytes).unwrap(), c);
        for n in 0..bytes.len() {
            assert!(SqliteCursor::decode(&bytes[..n]).is_err(), "len {n}");
        }
        assert_eq!(SqliteCursor::decode(&SqliteCursor::default().encode()).unwrap().0.len(), 0);
    }
}
