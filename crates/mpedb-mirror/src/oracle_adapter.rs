//! Oracle pull adapter over durable bootstrap metadata. Captured transactions
//! enter MPEdb through the shared atomic apply protocol. Write-back is refused
//! until its durable echo ledger and conflict protocol are connected.
use crate::{
    adapter::{Cursor, NetOp, PullBatch, SourceAdapter},
    oracle::{carrier_tables, OracleTable},
    oracle_cursor::{OracleIdentity, OraclePosition},
    oracle_import::load_bootstrap,
    oracle_source::{OracleSource, OracleStateIdentity},
};
use mpedb::Database;
use mpedb_types::{Error, Result, Value};

pub struct OracleAdapter {
    source: OracleSource,
    identity: OracleIdentity,
    state_identity: OracleStateIdentity,
    tables: Vec<OracleTable>,
    initial: Cursor,
    mirror_id: String,
    max_events: usize,
}
impl OracleAdapter {
    /// Reconnect using metadata stored in the imported MPEdb file. The source
    /// must be the same database/container incarnation and application owner.
    pub fn from_database(source: OracleSource, db: &Database) -> Result<Self> {
        let metadata = load_bootstrap(db)?;
        let tables = carrier_tables(&metadata.tables)?
            .into_iter()
            .cloned()
            .collect();
        let mut adapter = Self {
            source,
            identity: metadata.identity,
            state_identity: metadata.state_identity,
            tables,
            initial: metadata.cursor,
            mirror_id: metadata.mirror_id,
            max_events: 100_000,
        };
        adapter.check_identity()?;
        if adapter
            .source
            .read_source_state(&adapter.mirror_id)?
            .is_none()
        {
            return Err(Error::Corrupt(
                "Oracle mirror authority row is missing; reimport is required".into(),
            ));
        }
        Ok(adapter)
    }
    /// Limit raw versions per capture query. Oversized windows are bisected;
    /// an oversized single commit SCN fails rather than skipping its rows.
    pub fn with_event_budget(mut self, max_events: usize) -> Result<Self> {
        if max_events == 0 {
            return Err(Error::Config(
                "Oracle capture event budget must be positive".into(),
            ));
        }
        self.max_events = max_events;
        Ok(self)
    }
    fn check_identity(&self) -> Result<()> {
        if self.source.source_state_identity()? != self.state_identity {
            return Err(Error::Unsupported(
                "Oracle control table was recreated or truncated; reimport is required".into(),
            ));
        }
        if self.source.identity()? != self.identity {
            return Err(Error::Unsupported(
                "Oracle source incarnation or application identity changed; reimport is required"
                    .into(),
            ));
        }
        Ok(())
    }
    fn position(&self, cursor: &Cursor) -> Result<u64> {
        let scn = OraclePosition::decode(cursor, &self.identity, &self.tables)?.scn();
        let initial = OraclePosition::decode(&self.initial, &self.identity, &self.tables)?.scn();
        if scn < initial {
            return Err(Error::Config(
                "Oracle cursor precedes its imported snapshot".into(),
            ));
        }
        Ok(scn)
    }
    fn cursor(&self, scn: u64) -> Result<Cursor> {
        Ok(OraclePosition::new(&self.identity, &self.tables, scn)?.encode())
    }
    fn same_mirror(&self, id: &str) -> Result<()> {
        if id != self.mirror_id {
            return Err(Error::Config(
                "Oracle adapter authority request names another mirror".into(),
            ));
        }
        Ok(())
    }
}
impl SourceAdapter for OracleAdapter {
    fn pull(&mut self, from: &Cursor, max_ops: usize) -> Result<Option<PullBatch>> {
        if max_ops == 0 {
            return Err(Error::Config("Oracle pull max_ops must be positive".into()));
        }
        self.check_identity()?;
        let from_scn = self.position(from)?;
        let head = self.source.current_scn()?;
        if from_scn > head {
            return Err(Error::Config(
                "Oracle resume cursor is ahead of the source".into(),
            ));
        }
        if from_scn == head {
            return Ok(None);
        }
        let mut end = head;
        let batches = loop {
            match self
                .source
                .read_changes(&self.tables, from_scn, end, self.max_events)
            {
                Ok(batches) => break batches,
                Err(Error::Unsupported(ref message))
                    if message.starts_with("Oracle capture event budget exceeded;")
                        && end - from_scn > 1 =>
                {
                    end = from_scn + (end - from_scn) / 2;
                }
                Err(e) => return Err(e),
            }
        };
        let (ops, scn) = match batches.into_iter().next() {
            Some(batch) => {
                if batch.ops.len() > max_ops {
                    return Err(Error::Unsupported("Oracle commit exceeds max_ops; increase the limit without advancing the cursor".into()));
                }
                (batch.ops, batch.scn)
            }
            None => (Vec::new(), end),
        };
        let epoch = self
            .source
            .read_source_state_at(&self.mirror_id, scn)?
            .ok_or_else(|| {
                Error::Corrupt("Oracle authority row is missing at the captured SCN".into())
            })?
            .0;
        self.check_identity()?;
        Ok(Some(PullBatch {
            ops,
            end_cursor: self.cursor(scn)?,
            source_epoch: Some(epoch),
        }))
    }
    fn head(&mut self) -> Result<Cursor> {
        self.check_identity()?;
        let scn = self.source.current_scn()?;
        self.check_identity()?;
        self.cursor(scn)
    }
    fn zero_cursor(&self) -> Cursor {
        self.initial.clone()
    }
    fn read_table_rows(&mut self, table_id: u32) -> Result<Vec<Vec<Value>>> {
        self.check_identity()?;
        let table = self
            .tables
            .get(table_id as usize)
            .ok_or_else(|| Error::Config("unknown Oracle carrier table".into()))?;
        let snapshot = self.source.snapshot(std::slice::from_ref(table))?;
        if snapshot.identity != self.identity {
            return Err(Error::Unsupported(
                "Oracle identity changed during reconcile".into(),
            ));
        }
        self.check_identity()?;
        Ok(snapshot.rows.into_iter().next().unwrap_or_default())
    }
    fn push(&mut self, _ops: &[NetOp]) -> Result<Vec<bool>> {
        Err(Error::Unsupported("Oracle mirror write-back requires the durable echo ledger and conflict protocol; use verified low-level write_batch only for explicit transport".into()))
    }
    fn push_checked(&mut self, _from: &Cursor, _ops: &[NetOp]) -> Result<Vec<bool>> {
        Err(Error::Unsupported(
            "Oracle conflict-checked write-back is not connected".into(),
        ))
    }
    fn ensure_source_state(&mut self, id: &str, epoch: u64, authority: &str) -> Result<()> {
        self.same_mirror(id)?;
        self.check_identity()?;
        let _ = epoch; // An established adapter must never reseed lost history.
        if !matches!(authority, "source" | "mpedb") {
            return Err(Error::Config("invalid Oracle authority".into()));
        }
        if self.source.read_source_state(id)?.is_none() {
            return Err(Error::Corrupt(
                "Oracle authority row was lost; reimport is required".into(),
            ));
        }
        self.check_identity()
    }
    fn read_source_state(&mut self, id: &str) -> Result<Option<(u64, String)>> {
        self.same_mirror(id)?;
        self.check_identity()?;
        let state = self.source.read_source_state(id)?;
        self.check_identity()?;
        Ok(state)
    }
    fn cas_source_state(
        &mut self,
        id: &str,
        expected: u64,
        next: u64,
        authority: &str,
    ) -> Result<bool> {
        self.same_mirror(id)?;
        self.check_identity()?;
        if authority == "mpedb" {
            return Err(Error::Unsupported(
                "Oracle authority cutover requires mirror write-back support".into(),
            ));
        }
        self.source
            .cas_source_state_fenced(id, expected, next, authority, &self.state_identity)
    }
}
