//! Shared bounded envelope for immutable push requests and ACK footprints.
use super::*;
use mpedb::WriteSession;
pub(super) const LIMIT: usize = 64 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
fn part(key: &[u8], n: usize) -> Vec<u8> {
    let mut out = key.to_vec();
    out.extend_from_slice(format!("/{n:08x}").as_bytes());
    out
}
fn corrupt(message: &str) -> Error {
    Error::Corrupt(format!("push outbox/receipt: {message}"))
}
pub(super) fn read(s: &mut WriteSession<'_>, key: &[u8]) -> Result<Option<([u8; 32], Vec<u8>)>> {
    let Some(header) = s.sys_record_get(state::MIR_NS, key)? else {
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
        let bytes_part = s
            .sys_record_get(state::MIR_NS, &part(key, n))?
            .ok_or_else(|| corrupt("missing chunk"))?;
        if bytes_part.len() != CHUNK.min(length as usize - bytes.len()) {
            return Err(corrupt("invalid chunk length"));
        }
        bytes.extend_from_slice(&bytes_part);
    }
    let tag: [u8; 32] = header[9..].try_into().unwrap();
    if blake3::hash(&bytes).as_bytes() != &tag {
        return Err(corrupt("request checksum mismatch"));
    }
    Ok(Some((tag, bytes)))
}
pub(super) fn store(s: &mut WriteSession<'_>, key: &[u8], bytes: &[u8]) -> Result<[u8; 32]> {
    if bytes.is_empty() || bytes.len() > LIMIT {
        return Err(Error::Unsupported("push record exceeds bounds".into()));
    }
    let tag = *blake3::hash(bytes).as_bytes();
    if let Some((old, existing)) = read(s, key)? {
        if old == tag && existing == bytes {
            return Ok(tag);
        }
        return Err(corrupt(
            "immutable record already exists with another payload",
        ));
    }
    for (n, bytes_part) in bytes.chunks(CHUNK).enumerate() {
        s.sys_record_put(state::MIR_NS, &part(key, n), bytes_part)?;
    }
    let mut header = vec![1];
    header.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    header.extend_from_slice(&tag);
    s.sys_record_put(state::MIR_NS, key, &header)?;
    Ok(tag)
}
pub(super) fn delete(s: &mut WriteSession<'_>, key: &[u8], expected: [u8; 32]) -> Result<Vec<u8>> {
    let (tag, bytes) = read(s, key)?.ok_or_else(|| corrupt("record disappeared before ACK"))?;
    if tag != expected {
        return Err(corrupt("request replaced before ACK"));
    }
    for n in 0..bytes.len().div_ceil(CHUNK) {
        s.sys_record_delete(state::MIR_NS, &part(key, n))?;
    }
    s.sys_record_delete(state::MIR_NS, key)?;
    Ok(bytes)
}
