//! Attest key origins from complete per-key transaction evidence and explicit
//! transactional request footprints. Local ACK knowledge is checked by apply.
use super::*;
use crate::adapter::WriteOrigin;
use std::collections::{BTreeMap, BTreeSet};
#[derive(Default)]
struct Transaction<'a> {
    requests: usize,
    foreign: bool,
    keys: BTreeMap<(u64, &'a [u8]), BTreeSet<[u8; 32]>>,
}
pub fn attest_write_origins(
    tables: &[OracleTable],
    batch: &OracleChangeBatch,
    events: &[OracleOriginEvent],
    file_incarnation: [u8; 16],
) -> Result<Vec<Option<WriteOrigin>>> {
    if file_incarnation == [0; 16]
        || batch.ops.len() != batch.key_transactions.len()
        || events.len() > 100_000
    {
        return Err(Error::Corrupt(
            "invalid Oracle provenance FILE/evidence count".into(),
        ));
    }
    let canonical = crate::oracle::carrier_tables(tables)?;
    let mut transactions = BTreeMap::<&[u8], Transaction<'_>>::new();
    let mut seen = BTreeSet::new();
    for event in events.iter().filter(|event| event.scn == batch.scn) {
        if event.xid.is_empty() || event.request_id == [0; 32] || !seen.insert(event.request_id) {
            return Err(Error::Corrupt(
                "invalid/duplicate Oracle origin request event".into(),
            ));
        }
        let tx = transactions.entry(&event.xid).or_default();
        tx.requests += 1;
        tx.foreign |= event.file_incarnation != file_incarnation;
        for touch in &event.touches {
            tx.keys
                .entry((touch.object_id, &touch.key))
                .or_default()
                .insert(event.request_id);
        }
    }
    let mut proofs = Vec::with_capacity(batch.ops.len());
    for (op, evidence) in batch.ops.iter().zip(&batch.key_transactions) {
        if !evidence.complete || evidence.xids.is_empty() {
            proofs.push(None);
            continue;
        }
        let table = canonical
            .get(op.table_id as usize)
            .ok_or_else(|| Error::Corrupt("unknown Oracle proof table".into()))?;
        let key = mpedb_types::keycode::encode_key(&op.pk);
        let mut requests = BTreeSet::new();
        let mut known = true;
        for xid in &evidence.xids {
            let Some(tx) = transactions.get(xid.as_slice()) else {
                known = false;
                break;
            };
            let Some(ids) = tx.keys.get(&(table.object_id, key.as_slice())) else {
                known = false;
                break;
            };
            if tx.foreign || ids.len() != tx.requests {
                known = false;
                break;
            }
            requests.extend(ids.iter().copied());
        }
        proofs.push(if known {
            Some(WriteOrigin {
                file_incarnation,
                request_ids: requests.into_iter().collect(),
            })
        } else {
            None
        });
    }
    Ok(proofs)
}
