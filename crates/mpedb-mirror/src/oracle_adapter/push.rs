//! The shared durable outbox supplies one immutable request across reconnects.
use super::*;
use crate::oracle_source::{OracleWriteOrigin, OracleWriteRequest};
impl OracleAdapter {
    pub(super) fn check_push_target(&self, db: &Database) -> Result<()> {
        let metadata = load_bootstrap(db)?;
        let same_file = match self.file_incarnation {
            Some(file) => load_file_incarnation(db)? == file,
            None => true,
        };
        if metadata.identity != self.identity
            || metadata.state_identity != self.state_identity
            || metadata.mirror_id != self.mirror_id
            || metadata.cursor != self.initial
            || metadata.tables != self.tables
            || metadata.origin_ledger != self.origin_ledger
            || metadata.receipt_ledger != self.receipt_ledger
            || !same_file
        {
            return Err(Error::Config("Oracle push adapter belongs to another FILE or bootstrap; reconnect from this mirror".into()));
        }
        Ok(())
    }
    pub(super) fn write_back(
        &mut self,
        from: Option<&Cursor>,
        ops: &[NetOp],
        context: &PushContext,
    ) -> Result<PushReply> {
        let ledger = self.receipt_ledger.as_ref().ok_or_else(|| Error::Unsupported("Oracle write-back requires a receipt ledger pinned at import; prepare tracking and reimport".into()))?;
        let origin = self.origin_ledger.as_ref().ok_or_else(|| Error::Unsupported("Oracle write-back requires an origin ledger pinned at import; prepare tracking and reimport".into()))?;
        let file = self
            .file_incarnation
            .ok_or_else(|| Error::Corrupt("Oracle push FILE incarnation is missing".into()))?;
        let epoch = context
            .epoch
            .ok_or_else(|| Error::Config("Oracle push requires a persisted epoch".into()))?;
        let token = context
            .request_id
            .filter(|id| *id != [0; 32])
            .ok_or_else(|| Error::Config("Oracle push requires a durable outbox request".into()))?;
        if epoch.epoch == 0
            || ops.is_empty()
            || context.generations.len() != ops.len()
            || context.generations.contains(&0)
        {
            return Err(Error::Config(
                "Oracle push has an invalid generation/operation snapshot".into(),
            ));
        }
        let from = from
            .map(|cursor| {
                let scn = self.position(cursor)?;
                OraclePosition::new(&self.identity, &self.tables, scn)
            })
            .transpose()?;
        self.check_identity()?;
        let request = OracleWriteRequest {
            mirror_id: &self.mirror_id,
            token,
            epoch: epoch.epoch,
            authority: match epoch.authority {
                crate::state::Authority::Source => "source",
                crate::state::Authority::Mpedb => "mpedb",
            },
            state_identity: &self.state_identity,
            ledger_identity: ledger,
            origin: Some(OracleWriteOrigin {
                file_incarnation: file,
                ledger_identity: origin,
            }),
        };
        let reply = self.source.write_mirror_batch(
            &self.tables,
            ops,
            &request,
            from.as_ref(),
            self.max_events,
        )?;
        self.check_identity()?;
        Ok(reply)
    }
}
