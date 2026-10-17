//! Checksummed, chunked immutable request snapshots. The request is committed
//! before source I/O, and removed in the SAME transaction as its local ACK.
use super::*;
use mpedb::WriteSession;
use mpedb_core::{cdc, row};
use mpedb_types::{Schema, Value};
const KEY: &[u8] = b"push/outbox";
const LIMIT: usize = 64 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
fn chunk_key(n: usize) -> Vec<u8> {
    format!("push/outbox/{n:08x}").into_bytes()
}
fn corrupt(message: &str) -> Error {
    Error::Corrupt(format!("push outbox: {message}"))
}
fn frame(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    if out
        .len()
        .checked_add(8)
        .and_then(|n| n.checked_add(bytes.len()))
        .is_none_or(|n| n > LIMIT)
    {
        return Err(Error::Unsupported(
            "durable push request exceeds 64 MiB; reduce the batch".into(),
        ));
    }
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}
fn optional(out: &mut Vec<u8>, bytes: Option<&[u8]>) -> Result<()> {
    out.push(u8::from(bytes.is_some()));
    if let Some(bytes) = bytes {
        frame(out, bytes)?;
    }
    Ok(())
}
fn types(table: &mpedb_types::TableDef) -> Vec<mpedb_types::ColumnType> {
    table.columns.iter().map(|c| c.ty).collect()
}
fn encode(identity: [u8; 16], prepared: &PreparedPush) -> Result<Vec<u8>> {
    if prepared.ops.len() != prepared.pending.len() || prepared.ops.is_empty() {
        return Err(corrupt("invalid operation count"));
    }
    if prepared.ops.len() > 100_000 {
        return Err(Error::Unsupported(
            "durable push request exceeds 100000 operations; no source writes were sent".into(),
        ));
    }
    let mut bytes = vec![1];
    bytes.extend_from_slice(&identity);
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| {
        Error::Unsupported("secure entropy unavailable for durable push request".into())
    })?;
    if nonce == [0; 16] {
        return Err(corrupt("invalid fresh request nonce"));
    }
    bytes.extend_from_slice(&nonce);
    optional(&mut bytes, prepared.epoch_bytes.as_deref())?;
    optional(&mut bytes, prepared.cursor.as_deref())?;
    optional(&mut bytes, prepared.cfg_bytes.as_deref())?;
    bytes.extend_from_slice(&(prepared.ops.len() as u32).to_le_bytes());
    for (op, pending) in prepared.ops.iter().zip(&prepared.pending) {
        let table = prepared
            .schema
            .tables
            .get(op.table_id as usize)
            .ok_or_else(|| corrupt("unknown table"))?;
        bytes.extend_from_slice(&op.table_id.to_le_bytes());
        bytes.extend_from_slice(&pending.last_txn.to_le_bytes());
        bytes.extend_from_slice(&table_hash(table));
        frame(&mut bytes, &pending.pk_keycode)?;
        match &op.kind {
            NetOpKind::Delete => bytes.push(0),
            NetOpKind::Upsert(values) => {
                bytes.push(1);
                frame(&mut bytes, &row::encode_row(values, &types(table))?)?;
            }
        }
    }
    if bytes.len() > LIMIT {
        return Err(corrupt("request exceeds bounds"));
    }
    Ok(bytes)
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| corrupt("frame overflow"))?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| corrupt("truncated request"))?;
        self.at = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn frame(&mut self) -> Result<&'a [u8]> {
        let size = u64::from_le_bytes(self.take(8)?.try_into().unwrap());
        if size > LIMIT as u64 {
            return Err(corrupt("frame exceeds bounds"));
        }
        self.take(size as usize)
    }
    fn optional(&mut self) -> Result<Option<Vec<u8>>> {
        match self.byte()? {
            0 => Ok(None),
            1 => Ok(Some(self.frame()?.to_vec())),
            _ => Err(corrupt("invalid optional field")),
        }
    }
}
fn decode(bytes: &[u8], identity: [u8; 16], schema: Schema, tag: [u8; 32]) -> Result<PreparedPush> {
    let mut r = Reader { bytes, at: 0 };
    if r.byte()? != 1 || r.take(16)? != identity {
        return Err(corrupt("version or FILE incarnation mismatch"));
    }
    if r.take(16)? == [0; 16] {
        return Err(corrupt("zero request nonce"));
    }
    let epoch_bytes = r.optional()?;
    if let Some(epoch) = &epoch_bytes {
        Epoch::decode(epoch)?;
    }
    let cursor = r.optional()?;
    let cfg_bytes = r.optional()?;
    if let Some(cfg) = &cfg_bytes {
        state::MirrorConfig::decode(cfg)?;
    }
    let n = u32::from_le_bytes(r.take(4)?.try_into().unwrap()) as usize;
    if n == 0 || n > 100_000 {
        return Err(corrupt("invalid operation count"));
    }
    let mut prepared = PreparedPush {
        schema,
        epoch_bytes,
        cursor,
        cfg_bytes,
        ops: Vec::new(),
        pending: Vec::new(),
        outbox_tag: Some(tag),
    };
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..n {
        let table_id = u32::from_le_bytes(r.take(4)?.try_into().unwrap());
        let last_txn = u64::from_le_bytes(r.take(8)?.try_into().unwrap());
        let table = prepared
            .schema
            .tables
            .get(table_id as usize)
            .ok_or_else(|| corrupt("unknown table"))?;
        if table.id != table_id || table.dead || last_txn == 0 || r.take(32)? != table_hash(table) {
            return Err(Error::Unsupported(
                "durable push carrier schema/generation changed; retain request and recover".into(),
            ));
        }
        let pk_keycode = r.frame()?.to_vec();
        let pk = keycode::decode_key(&pk_keycode, &table.pk_types())?;
        if pk.contains(&Value::Null) || keycode::encode_key(&pk) != pk_keycode {
            return Err(corrupt("noncanonical row identity"));
        }
        let kind = match r.byte()? {
            0 => NetOpKind::Delete,
            1 => {
                let bytes = r.frame()?;
                let types = types(table);
                let values = row::decode_row(bytes, &types)?;
                let key = table
                    .primary_key
                    .iter()
                    .map(|&k| values[k as usize].clone())
                    .collect::<Vec<_>>();
                if row::encode_row(&values, &types)? != bytes
                    || keycode::encode_key(&key) != pk_keycode
                {
                    return Err(corrupt("noncanonical row image or changed primary key"));
                }
                NetOpKind::Upsert(values)
            }
            _ => return Err(corrupt("unknown operation kind")),
        };
        let subkey = cdc::dirty_key(table_id, &pk_keycode)[4..].to_vec();
        if !seen.insert(subkey.clone()) {
            return Err(corrupt("duplicate row identity"));
        }
        prepared.ops.push(NetOp { table_id, pk, kind });
        prepared.pending.push(Pending {
            table_id,
            last_txn,
            pk_keycode,
            subkey,
        });
    }
    if r.at != bytes.len() {
        return Err(corrupt("trailing request bytes"));
    }
    Ok(prepared)
}
fn read_bytes(s: &mut WriteSession<'_>) -> Result<Option<([u8; 32], Vec<u8>)>> {
    let Some(header) = s.sys_record_get(state::MIR_NS, KEY)? else {
        return Ok(None);
    };
    if header.len() != 41 || header[0] != 1 {
        return Err(corrupt("invalid header"));
    }
    let length = u64::from_le_bytes(header[1..9].try_into().unwrap());
    if length == 0 || length > LIMIT as u64 {
        return Err(corrupt("invalid request length"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    for n in 0..(length as usize).div_ceil(CHUNK) {
        let part = s
            .sys_record_get(state::MIR_NS, &chunk_key(n))?
            .ok_or_else(|| corrupt("missing chunk"))?;
        if part.len() != CHUNK.min(length as usize - bytes.len()) {
            return Err(corrupt("invalid chunk length"));
        }
        bytes.extend_from_slice(&part);
    }
    let tag: [u8; 32] = header[9..].try_into().unwrap();
    if blake3::hash(&bytes).as_bytes() != &tag {
        return Err(corrupt("request checksum mismatch"));
    }
    Ok(Some((tag, bytes)))
}
pub(super) fn load(
    s: &mut WriteSession<'_>,
    identity: Option<[u8; 16]>,
) -> Result<Option<PreparedPush>> {
    let Some((tag, bytes)) = read_bytes(s)? else {
        return Ok(None);
    };
    let identity = identity.ok_or_else(|| {
        Error::Unsupported(
            "pending durable push requires its original receipt-capable adapter".into(),
        )
    })?;
    let prepared = decode(
        &bytes,
        identity,
        Schema {
            tables: s.schema().tables.clone(),
        },
        tag,
    )?;
    if s.sys_record_get(state::MIR_NS, state::KEY_EPOCH)? != prepared.epoch_bytes
        || s.sys_record_get(state::MIR_NS, state::KEY_CFG)? != prepared.cfg_bytes
    {
        return Err(Error::Unsupported(
            "durable push authority/config changed; retain request and recover before sending"
                .into(),
        ));
    }
    Ok(Some(prepared))
}
pub(super) fn store(
    s: &mut WriteSession<'_>,
    identity: [u8; 16],
    prepared: &PreparedPush,
) -> Result<[u8; 32]> {
    if s.sys_record_get(state::MIR_NS, KEY)?.is_some() {
        return Err(corrupt("request already exists"));
    }
    let bytes = encode(identity, prepared)?;
    let tag = *blake3::hash(&bytes).as_bytes();
    for (n, part) in bytes.chunks(CHUNK).enumerate() {
        s.sys_record_put(state::MIR_NS, &chunk_key(n), part)?;
    }
    let mut header = vec![1];
    header.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    header.extend_from_slice(&tag);
    s.sys_record_put(state::MIR_NS, KEY, &header)?;
    Ok(tag)
}
pub(super) fn validate_tag(s: &mut WriteSession<'_>, tag: [u8; 32]) -> Result<()> {
    let header = s.sys_record_get(state::MIR_NS, KEY)?.ok_or_else(|| {
        Error::Unsupported("durable push was already acknowledged or replaced".into())
    })?;
    if header.len() != 41 || header[0] != 1 || header[9..] != tag {
        return Err(Error::Unsupported(
            "durable push request changed during source I/O".into(),
        ));
    }
    Ok(())
}
pub(super) fn clear(s: &mut WriteSession<'_>, expected: [u8; 32]) -> Result<()> {
    let (tag, bytes) = read_bytes(s)?.ok_or_else(|| corrupt("request disappeared before ACK"))?;
    if tag != expected {
        return Err(corrupt("request replaced before ACK"));
    }
    for n in 0..bytes.len().div_ceil(CHUNK) {
        s.sys_record_delete(state::MIR_NS, &chunk_key(n))?;
    }
    s.sys_record_delete(state::MIR_NS, KEY)?;
    Ok(())
}
