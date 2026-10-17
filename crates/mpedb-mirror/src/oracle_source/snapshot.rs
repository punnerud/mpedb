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
        self.snapshot_inner(tables, None, None)
    }
    /// Bootstrap a mirror after its authority row was initialized. Its epoch
    /// is read at the same SCN as the application rows, never from current data.
    pub fn snapshot_for_mirror(
        &mut self,
        tables: &[OracleTable],
        mirror_id: &str,
    ) -> Result<OracleSnapshot> {
        self.snapshot_inner(tables, Some(mirror_id), None)
    }
    pub(crate) fn snapshot_for_mirror_with_ledger(
        &mut self,
        tables: &[OracleTable],
        mirror_id: &str,
        ledger: Option<&OracleLedgerIdentity>,
    ) -> Result<OracleSnapshot> {
        self.snapshot_inner(tables, Some(mirror_id), ledger)
    }
    fn snapshot_inner(
        &mut self,
        tables: &[OracleTable],
        mirror_id: Option<&str>,
        ledger: Option<&OracleLedgerIdentity>,
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
            let rows = tables
                .iter()
                .map(|table| self.read_snapshot(table, scn))
                .collect::<Result<Vec<_>>>()?;
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
