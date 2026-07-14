//! Parked-conflict inspection and cleanup (DESIGN-MIRROR §8). Rows that could
//! not be applied (a stricter mpedb rule, a unique block, a divergence resolved
//! source-wins) are parked under `mir\0park/`. This module lists them for the
//! operator and clears them once handled.

use mpedb::Database;
use mpedb_types::{keycode, Result, Value};

use crate::state::{self, ParkRecord};

/// A parked conflict with its PK decoded for display.
#[derive(Clone, Debug)]
pub struct Parked {
    pub record: ParkRecord,
    pub table: String,
    pub pk: Vec<Value>,
}

/// List all parked conflicts, PK decoded against the stored schema.
pub fn list(db: &Database) -> Result<Vec<Parked>> {
    let schema = db.schema();
    let mut out = Vec::new();
    for (k, v) in db.sys_record_scan(state::MIR_NS)? {
        if !k.starts_with(b"park/") {
            continue;
        }
        let record = ParkRecord::decode(&v)?;
        let (table, pk) = match schema.tables.get(record.table_id as usize) {
            Some(t) => (
                t.name.clone(),
                keycode::decode_key(&record.pk_keycode, &t.pk_types()).unwrap_or_default(),
            ),
            None => (format!("<table {}>", record.table_id), Vec::new()),
        };
        out.push(Parked { record, table, pk });
    }
    Ok(out)
}

/// Clear every parked-conflict record (after the operator has reconciled).
/// Returns how many were removed.
pub fn clear(db: &Database) -> Result<u64> {
    let mut s = db.begin()?;
    s.set_capture(false);
    let recs = s.sys_record_scan_range(state::MIR_NS, b"park/", state::KEY_PARK_END)?;
    let n = recs.len() as u64;
    for (k, _) in recs {
        s.sys_record_delete(state::MIR_NS, &k)?;
    }
    s.commit()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{import_sqlite, ImportOptions};
    use crate::state::{ConflictKind, ParkRecord};
    use rusqlite::Connection;

    #[test]
    fn list_decodes_and_clear_removes_parked() {
        let src = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("conf-src-{}.db", std::process::id()));
        let mid = std::env::temp_dir()
            .join("mpedb-mirror-tests")
            .join(format!("conf-mid-{}.mpedb", std::process::id()));
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&mid);
        {
            let c = Connection::open(&src).unwrap();
            c.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER);")
                .unwrap();
        }
        let db = {
            let mut c = Connection::open(&src).unwrap();
            import_sqlite(&mut c, &mid, &ImportOptions::default()).unwrap().0
        };

        // park a record by hand
        let kc = keycode::encode_key(&[Value::Int(7)]);
        let rec = ParkRecord {
            kind: ConflictKind::UniqueBlocked,
            wall_us: 0,
            table_id: 0,
            pk_keycode: kc.clone(),
        };
        {
            let mut s = db.begin().unwrap();
            s.set_capture(false);
            s.sys_record_put(state::MIR_NS, &state::park_key(0, &kc), &rec.encode())
                .unwrap();
            s.commit().unwrap();
        }

        let parked = list(&db).unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].table, "t");
        assert_eq!(parked[0].pk, vec![Value::Int(7)]);
        assert_eq!(parked[0].record.kind, ConflictKind::UniqueBlocked);

        assert_eq!(clear(&db).unwrap(), 1);
        assert!(list(&db).unwrap().is_empty());

        for p in [src, mid] {
            let _ = std::fs::remove_file(p);
        }
    }
}
