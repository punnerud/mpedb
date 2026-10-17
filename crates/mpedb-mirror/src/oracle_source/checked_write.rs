//! Conservative conflict checks under a source-side DML fence. Fine-grained
//! row/gap locks and own-write filtering belong to a future controller.
use super::*;
use crate::adapter::NetOp;
use std::collections::BTreeSet;
impl OracleSource {
    pub(super) fn write_conflicts(
        &mut self,
        tables: &[OracleTable],
        ops: &[NetOp],
        from_scn: u64,
        max_events: usize,
    ) -> Result<bool> {
        let canonical = crate::oracle::carrier_tables(tables)?;
        let used = ops.iter().map(|op| op.table_id).collect::<BTreeSet<_>>();
        for id in used {
            let table = canonical
                .get(id as usize)
                .ok_or_else(|| Error::Config("unknown Oracle checked-write table".into()))?;
            self.connection
                .execute(
                    &format!(
                        "LOCK TABLE {}.{} IN SHARE ROW EXCLUSIVE MODE NOWAIT",
                        quote(&table.owner),
                        quote(&table.name)
                    ),
                    &[],
                )
                .map_err(|e| err("checked write DML fence", e))?;
        }
        // The boundary is obtained AFTER upgrading every touched table's lock;
        // no foreign writer can commit a new target identity in this interval.
        let to_scn = self.current_scn()?;
        let targets = ops
            .iter()
            .map(|op| (op.table_id, mpedb_types::keycode::encode_key_exact(&op.pk)))
            .collect::<BTreeSet<_>>();
        let changes = self.read_changes_uncommitted(tables, from_scn, to_scn, max_events)?;
        Ok(changes.iter().flat_map(|batch| &batch.ops).any(|op| {
            targets.contains(&(op.table_id, mpedb_types::keycode::encode_key_exact(&op.pk)))
        }))
    }
    pub(super) fn check_write_conflicts(
        &mut self,
        tables: &[OracleTable],
        ops: &[NetOp],
        from_scn: u64,
        max_events: usize,
    ) -> Result<()> {
        if self.write_conflicts(tables, ops, from_scn, max_events)? {
            return Err(Error::Unsupported("Oracle checked write conflicts with changes since the cursor; pull and reconcile before retrying".into()));
        }
        Ok(())
    }
}
