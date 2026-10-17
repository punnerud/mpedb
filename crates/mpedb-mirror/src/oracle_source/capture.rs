//! Bounded, explicit Oracle Flashback Versions windows. Undo-history failure
//! propagates; there is no fallback that advances past missing changes.
use super::*;
use crate::adapter::{NetOp, NetOpKind};
use std::collections::{BTreeMap, BTreeSet};

/// Committed net images at ONE SCN, across the entire selected table set.
/// Multiple transactions sharing a commit SCN stay together. XIDs preserve
/// source provenance; this is not yet an echo-filtered SourceAdapter batch.
#[derive(Debug)]
pub struct OracleChangeBatch {
    pub scn: u64,
    pub xids: Vec<Vec<u8>>,
    pub ops: Vec<NetOp>,
}
#[derive(Default)]
struct Changes {
    xids: BTreeSet<Vec<u8>>,
    keys: BTreeMap<(u32, Vec<u8>), Vec<Value>>,
}
impl OracleSource {
    /// Capture committed changes in `(from_scn, to_scn]`. `from_scn` must be a
    /// successful import/resume SCN, not an invented zero cursor. Images are
    /// read AS OF each commit SCN, so later updates cannot overwrite history.
    /// `max_events` bounds raw versions retained; exceeding it fails the whole
    /// call and requires a narrower SCN window. No transaction is truncated.
    pub fn read_changes(
        &mut self,
        tables: &[OracleTable],
        from_scn: u64,
        to_scn: u64,
        max_events: usize,
    ) -> Result<Vec<OracleChangeBatch>> {
        if from_scn > to_scn || max_events == 0 {
            return Err(Error::Config(
                "Oracle capture requires an ordered SCN interval and a positive event budget"
                    .into(),
            ));
        }
        if from_scn == to_scn {
            return Ok(Vec::new());
        }
        if to_scn > self.current_scn()? {
            return Err(Error::Config(
                "Oracle capture cannot read a future SCN".into(),
            ));
        }
        let result = (|| {
            let tables = crate::oracle::carrier_tables(tables)?;
            for table in &tables {
                self.lock_schema(table)?;
            }
            let mut changes = BTreeMap::<u64, Changes>::new();
            let mut count = 0usize;
            for (table_id, table) in tables.iter().enumerate() {
                if table.columns.iter().any(|c| {
                    [
                        "VERSIONS_STARTSCN",
                        "VERSIONS_ENDSCN",
                        "VERSIONS_XID",
                        "VERSIONS_OPERATION",
                    ]
                    .iter()
                    .any(|n| c.name.eq_ignore_ascii_case(n))
                }) {
                    return Err(Error::Unsupported(
                        "Oracle capture pseudocolumn name collision".into(),
                    ));
                }
                let keys = table
                    .pk
                    .iter()
                    .map(|&k| table.columns[k].clone())
                    .collect::<Vec<_>>();
                let projection = keys
                    .iter()
                    .map(read_expr)
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(",");
                // Bind by name so repeated SCNs resolve unambiguously and
                // consecutive windows reuse the same SQL statement text.
                let sql=format!("SELECT versions_startscn,versions_endscn,versions_xid,versions_operation,{projection} FROM {}.{} VERSIONS BETWEEN SCN :from_scn AND :to_scn WHERE (versions_startscn>:from_scn AND versions_startscn<=:to_scn) OR (versions_endscn>:from_scn AND versions_endscn<=:to_scn)",quote(&table.owner),quote(&table.name));
                let rows = self
                    .connection
                    .query_named(&sql, &[("from_scn", &from_scn), ("to_scn", &to_scn)])
                    .map_err(|e| err("change versions", e))?;
                for row in rows {
                    count += 1;
                    if count > max_events {
                        return Err(Error::Unsupported("Oracle capture event budget exceeded; retry a narrower SCN window without advancing the cursor".into()));
                    }
                    let row = row.map_err(|e| err("change version row", e))?;
                    let start: Option<u64> = get(&row, 0)?;
                    let end: Option<u64> = get(&row, 1)?;
                    let xid: Option<Vec<u8>> = get(&row, 2)?;
                    let operation: Option<String> = get(&row, 3)?;
                    let pk = decode_row(&row, &keys, 4)?;
                    if pk.contains(&Value::Null) {
                        return Err(Error::Corrupt(
                            "Oracle committed version has NULL identity".into(),
                        ));
                    }
                    let encoded = mpedb_types::keycode::encode_key(&pk);
                    if let Some(scn) = start.filter(|&s| s > from_scn && s <= to_scn) {
                        let xid = xid.filter(|x| !x.is_empty()).ok_or_else(|| {
                            Error::Corrupt(
                                "Oracle committed version is missing its transaction identity"
                                    .into(),
                            )
                        })?;
                        if !matches!(operation.as_deref(), Some("I" | "U" | "D")) {
                            return Err(Error::Corrupt(
                                "Oracle returned invalid committed version operation".into(),
                            ));
                        }
                        let entry = changes.entry(scn).or_default();
                        entry.xids.insert(xid);
                        entry
                            .keys
                            .insert((table_id as u32, encoded.clone()), pk.clone());
                    }
                    if let Some(scn) = end.filter(|&s| s > from_scn && s <= to_scn) {
                        // A PK-changing UPDATE can expose only the NEW key in
                        // its started version. Also retain the expired key at
                        // END SCN, or the old identity would remain forever.
                        // This version's XID belongs to its START, not its end.
                        changes
                            .entry(scn)
                            .or_default()
                            .keys
                            .insert((table_id as u32, encoded), pk);
                    }
                }
            }
            let mut batches = Vec::with_capacity(changes.len());
            for (scn, changes) in changes {
                if changes.xids.is_empty() {
                    return Err(Error::Corrupt("Oracle version history has an expiration without its committing transaction".into()));
                }
                let mut ops = Vec::with_capacity(changes.keys.len());
                for ((table_id, _), pk) in changes.keys {
                    // Index-key updates can appear as DELETE+INSERT, even for
                    // the same PK. The final image at the commit determines
                    // the net op, never the arbitrary order of version rows.
                    let kind = self
                        .read_key(tables[table_id as usize], &pk, Some(scn))?
                        .map_or(NetOpKind::Delete, NetOpKind::Upsert);
                    ops.push(NetOp { table_id, pk, kind });
                }
                batches.push(OracleChangeBatch {
                    scn,
                    xids: changes.xids.into_iter().collect(),
                    ops,
                });
            }
            Ok(batches)
        })();
        // Release all DDL fences on both success and failure. This API never
        // commits a caller's writes; exclusive &mut access protects the session.
        self.connection
            .rollback()
            .map_err(|e| err("capture release", e))?;
        result
    }
}
