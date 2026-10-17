//! Bounded explicit request footprints; no source-to-source transformations.
use super::*;
use crate::adapter::NetOpKind;
use std::collections::BTreeSet;
pub(super) const LIMIT: usize = 64 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OracleOriginTouch {
    pub object_id: u64,
    pub key: Vec<u8>,
    pub upsert: bool,
}
pub(super) fn encode(tables: &[OracleTable], ops: &[NetOp]) -> Result<Vec<u8>> {
    if ops.len() > 100_000 {
        return Err(Error::Unsupported(
            "Oracle origin footprint exceeds operation budget".into(),
        ));
    }
    let canonical = crate::oracle::carrier_tables(tables)?;
    let mut out = vec![1];
    out.extend_from_slice(&(ops.len() as u32).to_be_bytes());
    let mut seen = BTreeSet::new();
    for op in ops {
        let table = canonical
            .get(op.table_id as usize)
            .ok_or_else(|| Error::Config("unknown Oracle origin table".into()))?;
        if op.pk.len() != table.pk.len() || op.pk.contains(&Value::Null) {
            return Err(Error::Config("invalid Oracle origin key".into()));
        }
        let key = mpedb_types::keycode::encode_key(&op.pk);
        if !seen.insert((table.object_id, key.clone())) {
            return Err(Error::Config("duplicate Oracle origin target".into()));
        }
        if out.len().saturating_add(17).saturating_add(key.len()) > LIMIT {
            return Err(Error::Unsupported(
                "Oracle origin footprint exceeds size budget".into(),
            ));
        }
        out.extend_from_slice(&table.object_id.to_be_bytes());
        out.extend_from_slice(&(key.len() as u64).to_be_bytes());
        out.extend_from_slice(&key);
        out.push(u8::from(matches!(op.kind, NetOpKind::Upsert(_))));
    }
    Ok(out)
}
pub(super) fn decode(bytes: &[u8]) -> Result<Vec<OracleOriginTouch>> {
    if bytes.len() > LIMIT {
        return Err(Error::Corrupt("oversized Oracle origin footprint".into()));
    }
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8]> {
        let end = at
            .checked_add(n)
            .ok_or_else(|| Error::Corrupt("Oracle origin frame overflow".into()))?;
        let part = bytes
            .get(at..end)
            .ok_or_else(|| Error::Corrupt("truncated Oracle origin footprint".into()))?;
        at = end;
        Ok(part)
    };
    if take(1)? != [1] {
        return Err(Error::Corrupt(
            "unknown Oracle origin footprint version".into(),
        ));
    }
    let count = u32::from_be_bytes(take(4)?.try_into().unwrap()) as usize;
    if count > 100_000 || count > bytes.len().saturating_sub(5) / 17 {
        return Err(Error::Corrupt(
            "invalid Oracle origin footprint count".into(),
        ));
    }
    let mut touches = Vec::with_capacity(count);
    let mut seen = BTreeSet::new();
    for _ in 0..count {
        let object_id = u64::from_be_bytes(take(8)?.try_into().unwrap());
        let length = u64::from_be_bytes(take(8)?.try_into().unwrap());
        if length == 0 || length > LIMIT as u64 || object_id == 0 {
            return Err(Error::Corrupt("invalid Oracle origin key frame".into()));
        }
        let key = take(length as usize)?.to_vec();
        let upsert = match take(1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(Error::Corrupt("invalid Oracle origin operation".into())),
        };
        if !seen.insert((object_id, key.clone())) {
            return Err(Error::Corrupt(
                "duplicate Oracle origin footprint key".into(),
            ));
        }
        touches.push(OracleOriginTouch {
            object_id,
            key,
            upsert,
        });
    }
    if at != bytes.len() {
        return Err(Error::Corrupt(
            "trailing Oracle origin footprint bytes".into(),
        ));
    }
    Ok(touches)
}
