//! Local echo knowledge. Never infer ownership from equal row values: the
//! source must attest provenance AND every request must account for this key.
use super::*;
use crate::adapter::WriteOrigin;
use mpedb::WriteSession;
use std::collections::{BTreeMap, HashMap};
type Touch = (u32, Vec<u8>);
struct Knowledge {
    file: [u8; 16],
    touches: BTreeMap<Touch, [u8; 32]>,
}
fn key(id: &[u8; 32]) -> Vec<u8> {
    let mut key = b"push/ack/".to_vec();
    for byte in id {
        key.extend_from_slice(format!("{byte:02x}").as_bytes());
    }
    key
}
fn corrupt(message: &str) -> Error {
    Error::Corrupt(format!("push ACK knowledge: {message}"))
}
pub(super) fn record(
    s: &mut WriteSession<'_>,
    file: [u8; 16],
    id: [u8; 32],
    schema: &mpedb_types::Schema,
    ops: &[NetOp],
    applied: &[bool],
) -> Result<()> {
    if ops.len() != applied.len() || file == [0; 16] || id == [0; 32] {
        return Err(corrupt("invalid ACK identity/count"));
    }
    let mut body = vec![1];
    body.extend_from_slice(&file);
    body.extend_from_slice(&(applied.iter().filter(|&&yes| yes).count() as u32).to_le_bytes());
    for (op, &yes) in ops.iter().zip(applied) {
        if !yes {
            continue;
        }
        let table = schema
            .tables
            .get(op.table_id as usize)
            .ok_or_else(|| corrupt("unknown table"))?;
        let pk = keycode::encode_key(&op.pk);
        if body.len().saturating_add(44).saturating_add(pk.len()) > record::LIMIT {
            return Err(Error::Unsupported(
                "push ACK footprint exceeds bounds".into(),
            ));
        }
        body.extend_from_slice(&op.table_id.to_le_bytes());
        body.extend_from_slice(&table_hash(table));
        body.extend_from_slice(&(pk.len() as u64).to_le_bytes());
        body.extend_from_slice(&pk);
    }
    record::store(s, &key(&id), &body)?;
    Ok(())
}
fn decode(bytes: &[u8]) -> Result<Knowledge> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8]> {
        let end = at.checked_add(n).ok_or_else(|| corrupt("frame overflow"))?;
        let part = bytes
            .get(at..end)
            .ok_or_else(|| corrupt("truncated footprint"))?;
        at = end;
        Ok(part)
    };
    if take(1)? != [1] {
        return Err(corrupt("invalid version"));
    }
    let file: [u8; 16] = take(16)?.try_into().unwrap();
    let count = u32::from_le_bytes(take(4)?.try_into().unwrap());
    if file == [0; 16] || count > 100_000 {
        return Err(corrupt("invalid FILE/count"));
    }
    let mut touches = BTreeMap::new();
    for _ in 0..count {
        let table = u32::from_le_bytes(take(4)?.try_into().unwrap());
        let hash: [u8; 32] = take(32)?.try_into().unwrap();
        let size = u64::from_le_bytes(take(8)?.try_into().unwrap());
        if size > record::LIMIT as u64 {
            return Err(corrupt("row identity exceeds bounds"));
        }
        let pk = take(size as usize)?.to_vec();
        if touches.insert((table, pk), hash).is_some() {
            return Err(corrupt("duplicate row identity"));
        }
    }
    if at != bytes.len() {
        return Err(corrupt("trailing footprint bytes"));
    }
    Ok(Knowledge { file, touches })
}
struct PendingKnowledge {
    file: [u8; 16],
    id: [u8; 32],
    touches: std::collections::BTreeSet<Touch>,
}
#[derive(Default)]
pub(crate) struct KnownRequests {
    acked: HashMap<[u8; 32], Option<Knowledge>>,
    pending: Option<Option<PendingKnowledge>>,
    table_hashes: HashMap<u32, [u8; 32]>,
}
impl KnownRequests {
    pub(crate) fn accounts_for(
        &mut self,
        s: &mut WriteSession<'_>,
        origin: &WriteOrigin,
        op: &NetOp,
    ) -> Result<bool> {
        let touch = (op.table_id, keycode::encode_key(&op.pk));
        for id in &origin.request_ids {
            if !self.acked.contains_key(id) {
                let knowledge = record::read(s, &key(id))?
                    .map(|(_, body)| decode(&body))
                    .transpose()?;
                self.acked.insert(*id, knowledge);
            }
            if let Some(known) = self.acked.get(id).and_then(Option::as_ref) {
                if known.file != origin.file_incarnation {
                    return Ok(false);
                }
                let Some(hash) = known.touches.get(&touch) else {
                    return Ok(false);
                };
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.table_hashes.entry(op.table_id)
                {
                    let bundle = s.schema();
                    let table = bundle
                        .tables
                        .get(op.table_id as usize)
                        .ok_or_else(|| corrupt("echo table disappeared"))?;
                    if table.dead {
                        return Err(Error::Unsupported("echo ACK table was dropped".into()));
                    }
                    entry.insert(table_hash(table));
                }
                if self.table_hashes[&op.table_id] != *hash {
                    return Err(Error::Unsupported(
                        "echo ACK footprint carrier schema changed".into(),
                    ));
                }
                continue;
            }
            if self.pending.is_none() {
                self.pending = Some(if let Some(prepared) = super::outbox::pending(s)? {
                    let (file, id) = super::outbox::pending_identity(s)?;
                    let touches = prepared
                        .pending
                        .into_iter()
                        .map(|p| (p.table_id, p.pk_keycode))
                        .collect();
                    Some(PendingKnowledge { file, id, touches })
                } else {
                    None
                });
            }
            let Some(pending) = self.pending.as_ref().and_then(Option::as_ref) else {
                return Ok(false);
            };
            if pending.id != *id
                || pending.file != origin.file_incarnation
                || !pending.touches.contains(&touch)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
