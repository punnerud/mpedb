//! Oracle pull and receipted push over durable bootstrap identities.
//! Source transactions and local acknowledgements use the shared mirror protocol.
mod push;
use crate::{
    adapter::{
        Cursor, NetOp, ProvenancedPullBatch, PullBatch, PushContext, PushReply, SourceAdapter,
    },
    oracle::{carrier_tables, OracleTable},
    oracle_cursor::{OracleIdentity, OraclePosition},
    oracle_import::{load_bootstrap, load_file_incarnation},
    oracle_source::{
        attest_write_origins, OracleLedgerIdentity, OracleOriginLedgerIdentity, OracleSource,
        OracleStateIdentity,
    },
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
    origin_ledger: Option<OracleOriginLedgerIdentity>,
    receipt_ledger: Option<OracleLedgerIdentity>,
    file_incarnation: Option<[u8; 16]>,
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
            file_incarnation: metadata
                .origin_ledger
                .as_ref()
                .map(|_| load_file_incarnation(db))
                .transpose()?,
            origin_ledger: metadata.origin_ledger,
            receipt_ledger: metadata.receipt_ledger,
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
    /// Pull through ONE captured head, so draining finishes even when Oracle
    /// background/unrelated commits keep moving the global SCN. The boundary
    /// has the same validated resume realm as the import and cannot be future.
    pub fn pull_through(
        &mut self,
        from: &Cursor,
        through: &Cursor,
        max_ops: usize,
    ) -> Result<Option<PullBatch>> {
        Self::legacy(self.pull_window(from, Some(through), max_ops)?)
    }
    pub fn pull_through_provenanced(
        &mut self,
        from: &Cursor,
        through: &Cursor,
        max_ops: usize,
    ) -> Result<Option<ProvenancedPullBatch>> {
        self.pull_window(from, Some(through), max_ops)
    }
    fn legacy(batch: Option<ProvenancedPullBatch>) -> Result<Option<PullBatch>> {
        batch.map(|incoming| {
            if incoming.origins.as_ref().is_some_and(|origins|origins.iter().any(Option::is_some)) {
                return Err(Error::Unsupported("Oracle own writes require pull_provenanced and atomic provenance-aware apply".into()));
            }
            Ok(incoming.batch)
        }).transpose()
    }
    fn pull_window(
        &mut self,
        from: &Cursor,
        through: Option<&Cursor>,
        max_ops: usize,
    ) -> Result<Option<ProvenancedPullBatch>> {
        if max_ops == 0 {
            return Err(Error::Config("Oracle pull max_ops must be positive".into()));
        }
        self.check_identity()?;
        let from_scn = self.position(from)?;
        let head = if let Some(cursor) = through {
            let scn = self.position(cursor)?;
            if scn > self.source.current_scn()? {
                return Err(Error::Config(
                    "Oracle pull boundary is ahead of the source".into(),
                ));
            }
            scn
        } else {
            self.source.current_scn()?
        };
        if from_scn > head {
            return Err(Error::Config(
                "Oracle resume cursor is ahead of the source".into(),
            ));
        }
        if from_scn == head {
            return Ok(None);
        }
        let mut end = head;
        let (first, events) = loop {
            let window = (|| {
                let batches =
                    self.source
                        .read_changes(&self.tables, from_scn, end, self.max_events)?;
                let first = batches.into_iter().next();
                let through = first.as_ref().map_or(end, |batch| batch.scn);
                let events = if let Some(ledger) = &self.origin_ledger {
                    // Check EVERY origin change before advancing, including
                    // origin-only commits and empty app-data watermarks. An
                    // earlier DELETE followed by reinsertion of an old request
                    // must never become a fresh ownership assertion.
                    self.source.read_write_origins(
                        &self.mirror_id,
                        ledger,
                        from_scn,
                        through,
                        self.max_events,
                    )?
                } else {
                    Vec::new()
                };
                Ok((first, events))
            })();
            match window {
                Ok(value) => break value,
                Err(Error::Unsupported(ref message))
                    if (message.starts_with("Oracle capture event budget exceeded;")
                        || message.starts_with("Oracle origin capture event budget exceeded;")
                        || message.starts_with("Oracle origin capture byte budget exceeded;"))
                        && end - from_scn > 1 =>
                {
                    end = from_scn + (end - from_scn) / 2;
                }
                Err(e) => return Err(e),
            }
        };
        let (ops, scn, origins) = match first {
            Some(batch) => {
                if batch.ops.len() > max_ops {
                    return Err(Error::Unsupported("Oracle commit exceeds max_ops; increase the limit without advancing the cursor".into()));
                }
                let origins = self
                    .file_incarnation
                    .map(|file| attest_write_origins(&self.tables, &batch, &events, file))
                    .transpose()?;
                (batch.ops, batch.scn, origins)
            }
            None => (Vec::new(), end, None),
        };
        let epoch = self
            .source
            .read_source_state_at(&self.mirror_id, scn)?
            .ok_or_else(|| {
                Error::Corrupt("Oracle authority row is missing at the captured SCN".into())
            })?
            .0;
        self.check_identity()?;
        Ok(Some(ProvenancedPullBatch {
            batch: PullBatch {
                ops,
                end_cursor: self.cursor(scn)?,
                source_epoch: Some(epoch),
            },
            origins,
        }))
    }
    fn check_identity(&self) -> Result<()> {
        if self.source.source_state_identity(&self.mirror_id)? != self.state_identity {
            return Err(Error::Unsupported(
                "Oracle control table or authority row incarnation changed; reimport is required"
                    .into(),
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
        Self::legacy(self.pull_window(from, None, max_ops)?)
    }
    fn pull_provenanced(
        &mut self,
        from: &Cursor,
        max_ops: usize,
    ) -> Result<Option<ProvenancedPullBatch>> {
        self.pull_window(from, None, max_ops)
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
        Err(Error::Unsupported(
            "Oracle mirror writes require push_versioned with a durable FILE request".into(),
        ))
    }
    fn push_checked(&mut self, _from: &Cursor, _ops: &[NetOp]) -> Result<Vec<bool>> {
        Err(Error::Unsupported(
            "Oracle checked writes require push_versioned with a durable FILE request".into(),
        ))
    }
    fn validate_push_target(&self, db: &Database) -> Result<()> {
        self.check_push_target(db)
    }
    fn push_outbox_identity(&self) -> Option<[u8; 16]> {
        if self.origin_ledger.is_some() && self.receipt_ledger.is_some() {
            self.file_incarnation
        } else {
            None
        }
    }
    fn push_versioned(
        &mut self,
        from: Option<&Cursor>,
        ops: &[NetOp],
        context: &PushContext,
    ) -> Result<PushReply> {
        self.write_back(from, ops, context)
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
