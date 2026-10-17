//! Complete bootstrap snapshots with schema fences and incarnation checks.
use super::*;
use crate::oracle_cursor::{OracleIdentity, OraclePosition};

#[derive(Debug)]
pub struct OracleSnapshot {
    pub identity: OracleIdentity,
    pub scn: u64,
    /// Canonical carrier-schema table id order, including original source DDL.
    pub tables: Vec<OracleTable>,
    pub rows: Vec<Vec<Vec<Value>>>,
    pub source_state: Option<(u64, String)>,
    pub source_state_identity: Option<OracleStateIdentity>,
}
impl OracleSnapshot {
    pub fn position(&self) -> Result<OraclePosition> {
        OraclePosition::new(&self.identity, &self.tables, self.scn)
    }
}
impl OracleSource {
    /// Read all selected tables at ONE SCN while fencing source DDL. A failed
    /// bootstrap publishes no cursor; callers may retry the entire bootstrap.
    pub fn snapshot(&mut self, tables: &[OracleTable]) -> Result<OracleSnapshot> {
        self.snapshot_inner(tables, None, None, None, None)
    }
    /// Bootstrap a mirror after its authority row was initialized. Its epoch
    /// is read at the same SCN as the application rows, never from current data.
    pub fn snapshot_for_mirror(
        &mut self,
        tables: &[OracleTable],
        mirror_id: &str,
    ) -> Result<OracleSnapshot> {
        self.snapshot_inner(tables, Some(mirror_id), None, None, None)
    }
    /// Returns the snapshot header with empty `rows`; values go to the sink.
    pub(crate) fn stream_mirror_snapshot(
        &mut self,
        tables: &[OracleTable],
        mirror_id: &str,
        ledger: Option<&OracleLedgerIdentity>,
        origins: Option<&OracleOriginLedgerIdentity>,
        sink: &mut dyn FnMut(u32, Vec<Value>) -> Result<()>,
    ) -> Result<OracleSnapshot> {
        self.snapshot_inner(tables, Some(mirror_id), ledger, origins, Some(sink))
    }
    fn snapshot_inner(
        &mut self,
        tables: &[OracleTable],
        mirror_id: Option<&str>,
        ledger: Option<&OracleLedgerIdentity>,
        origins: Option<&OracleOriginLedgerIdentity>,
        mut sink: Option<&mut dyn FnMut(u32, Vec<Value>) -> Result<()>>,
    ) -> Result<OracleSnapshot> {
        let result = (|| {
            let tables = crate::oracle::carrier_tables(tables)?;
            let identity = self.identity()?;
            if mirror_id.is_some() {
                self.lock_state()?;
            }
            if let Some(ledger) = ledger {
                self.lock_write_ledger(ledger)?;
            }
            if let Some(origins) = origins {
                self.lock_origin_ledger(origins)?;
            }
            for table in &tables {
                self.lock_schema(table)?;
            }
            let scn = self.current_scn()?;
            let source_state = if let Some(id) = mirror_id {
                Some(self.state_row(id, Some(scn))?.ok_or_else(|| {
                    Error::Corrupt(
                        "Oracle mirror bootstrap requires its initialized authority row".into(),
                    )
                })?)
            } else {
                None
            };
            let rows = if let Some(sink) = sink.as_mut() {
                for (id, table) in tables.iter().enumerate() {
                    self.stream_snapshot(table, scn, |row| sink(id as u32, row))?;
                }
                Vec::new()
            } else {
                tables
                    .iter()
                    .map(|table| self.read_snapshot(table, scn))
                    .collect::<Result<Vec<_>>>()?
            };
            if self.identity()? != identity {
                return Err(Error::Unsupported(
                    "Oracle database incarnation changed during bootstrap; discard the snapshot"
                        .into(),
                ));
            }
            let snapshot = OracleSnapshot {
                identity,
                scn,
                tables: tables.into_iter().cloned().collect(),
                rows,
                source_state,
                source_state_identity: mirror_id
                    .map(|id| self.state_identity_at(id, Some(scn)))
                    .transpose()?,
            };
            snapshot.position()?; // validate the complete resume domain before publication
            Ok(snapshot)
        })();
        self.connection
            .rollback()
            .map_err(|e| err("bootstrap fence release", e))?;
        result
    }
}
