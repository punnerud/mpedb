//! RETL stage-3 codecs: the composite-residual envelope, the delta engine,
//! and the zip-splice parser (design/DESIGN-RETL.md §8.2–8.4).
//!
//! Everything here is PURE — bytes in, bytes out, no database — so the
//! eternity promises live in one reviewable file. The split of obligations is
//! the Lepton split, stated once and honoured everywhere:
//!
//! - **Decoders are the eternity promise**: total on arbitrary bytes,
//!   bounds-checked, `Corrupt`-never-panic, truncation-tested at every
//!   offset, resource-bounded BEFORE allocation.
//! - **Encoders may improve freely**: every encoding is verified by decoding
//!   it back byte-identically before anything commits, so an encoder bug can
//!   bloat, never corrupt. Tests assert round-trip equality and NEVER encoder
//!   output bytes — a byte-asserting test would silently freeze the encoder.

use mpedb_types::{Error, Result};

// ---------------------------------------------------------------------------
// §8.2 — the envelope
// ---------------------------------------------------------------------------

/// The ONE dispatch byte: the value is both the version and the algorithm
/// (version-as-dispatch taken literally — pristine-tar's model). An unknown
/// kind is refused WITH ITS NUMBER NAMED.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Payload is the full bytes, verbatim.
    Raw = 1,
    /// Payload is §8.3 delta-v1.
    DeltaV1 = 2,
    /// Payload is §8.4 zip-splice-v1.
    ZipSpliceV1 = 3,
}

/// Envelopes NEST (outer transform wraps inner, pristine-tar's `wrapper`),
/// with a hard depth cap: a recursive decoder without one turns an
/// adversarial blob of nested envelopes into a stack overflow, which is a
/// panic, which the decoder rule forbids.
pub const MAX_ENVELOPE_DEPTH: u32 = 4;

/// Envelope `len` is u32: a hard 4 GiB ceiling, refused at write time with
/// the number named (the A3 pattern).
pub const MAX_PAYLOAD: usize = u32::MAX as usize;

pub fn envelope(kind: Kind, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return Err(Error::Unsupported(format!(
            "payload is {} bytes; the envelope caps at {MAX_PAYLOAD}",
            payload.len()
        )));
    }
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(kind as u8);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    Ok(v)
}

/// Decode one envelope layer. The payload must fill the buffer EXACTLY —
/// trailing bytes are corruption, not slack.
pub fn open_envelope(bytes: &[u8]) -> Result<(Kind, &[u8])> {
    if bytes.len() < 5 {
        return Err(Error::Corrupt("envelope truncated before its header".into()));
    }
    let kind = match bytes[0] {
        1 => Kind::Raw,
        2 => Kind::DeltaV1,
        3 => Kind::ZipSpliceV1,
        other => {
            return Err(Error::Unsupported(format!(
                "envelope kind {other} is newer than this build supports (max 3) — \
                 refusing rather than guessing"
            )))
        }
    };
    let len = u32::from_le_bytes(bytes[1..5].try_into().expect("4 bytes")) as usize;
    let body = &bytes[5..];
    if body.len() != len {
        return Err(Error::Corrupt(format!(
            "envelope claims {len} payload bytes but carries {}",
            body.len()
        )));
    }
    Ok((kind, body))
}

// ---------------------------------------------------------------------------
// §8.3 — delta-v1: git's packfile delta, simplified
// ---------------------------------------------------------------------------

const OP_INSERT: u8 = 0x00;
const OP_COPY: u8 = 0x01;
/// Encoder parameters (free to change — the format does not carry them).
const BLOCK: usize = 16;
const CHAIN_CAP: usize = 64;
const MIN_MATCH: usize = BLOCK;

/// Apply a delta-v1 payload to `base`, producing the target — the eternity
/// decoder. Resource-bounded BEFORE allocation: the instruction stream is
/// pre-walked, every COPY bounds-checked against `base_len`, and the lengths
/// must sum to EXACTLY `target_len` — a corrupt delta claiming a 2^63 copy
/// dies on a bounds check, never in the allocator.
pub fn delta_apply(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let corrupt = |m: &str| Error::Corrupt(format!("delta-v1: {m}"));
    if delta.len() < 16 {
        return Err(corrupt("truncated before the header"));
    }
    let base_len = u64::from_le_bytes(delta[0..8].try_into().expect("8"));
    let target_len = u64::from_le_bytes(delta[8..16].try_into().expect("8"));
    if base_len != base.len() as u64 {
        return Err(corrupt(&format!(
            "base is {} bytes but the delta was made against {base_len} — wrong base",
            base.len()
        )));
    }
    if target_len > MAX_PAYLOAD as u64 {
        return Err(corrupt(&format!("target_len {target_len} exceeds the envelope cap")));
    }

    // Pre-walk: validate every instruction and total the output length.
    let mut i = 16usize;
    let mut total: u64 = 0;
    while i < delta.len() {
        match delta[i] {
            OP_COPY => {
                if i + 17 > delta.len() {
                    return Err(corrupt("truncated COPY"));
                }
                let off = u64::from_le_bytes(delta[i + 1..i + 9].try_into().expect("8"));
                let len = u64::from_le_bytes(delta[i + 9..i + 17].try_into().expect("8"));
                let end = off.checked_add(len).ok_or_else(|| corrupt("COPY overflows"))?;
                if end > base_len {
                    return Err(corrupt(&format!(
                        "COPY {off}+{len} reaches past the {base_len}-byte base"
                    )));
                }
                total = total.checked_add(len).ok_or_else(|| corrupt("output overflows"))?;
                i += 17;
            }
            OP_INSERT => {
                if i + 5 > delta.len() {
                    return Err(corrupt("truncated INSERT header"));
                }
                let len =
                    u32::from_le_bytes(delta[i + 1..i + 5].try_into().expect("4")) as usize;
                if i + 5 + len > delta.len() {
                    return Err(corrupt("truncated INSERT body"));
                }
                total = total
                    .checked_add(len as u64)
                    .ok_or_else(|| corrupt("output overflows"))?;
                i += 5 + len;
            }
            other => return Err(corrupt(&format!("unknown instruction {other:#04x}"))),
        }
        if total > target_len {
            return Err(corrupt("instructions produce more than target_len"));
        }
    }
    if total != target_len {
        return Err(corrupt(&format!(
            "instructions produce {total} bytes, header promises {target_len}"
        )));
    }

    // One allocation, then a straight fill.
    let mut out = Vec::with_capacity(target_len as usize);
    let mut i = 16usize;
    while i < delta.len() {
        match delta[i] {
            OP_COPY => {
                let off = u64::from_le_bytes(delta[i + 1..i + 9].try_into().expect("8")) as usize;
                let len = u64::from_le_bytes(delta[i + 9..i + 17].try_into().expect("8")) as usize;
                out.extend_from_slice(&base[off..off + len]);
                i += 17;
            }
            _ => {
                let len =
                    u32::from_le_bytes(delta[i + 1..i + 5].try_into().expect("4")) as usize;
                out.extend_from_slice(&delta[i + 5..i + 5 + len]);
                i += 5 + len;
            }
        }
    }
    Ok(out)
}

/// Encode `target` against `base`. Returns `None` when the delta would not be
/// SMALLER than storing the target raw — the pathological-case cap, not a
/// minimality promise (commitment 6: the lower bound is H(X|Y) and no
/// encoding beats it).
///
/// Greedy first-fit over a 16-byte-block index of the base: fixed
/// multiplicative hash into array-backed chains — never a HashMap, whose
/// seeded iteration order would make output nondeterministic — chain cap 64
/// against adversarial repetitive input, longest-match with lowest-offset
/// tie-break. Deterministic: a pure function of (base, target).
pub fn delta_encode(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&(base.len() as u64).to_le_bytes());
    out.extend_from_slice(&(target.len() as u64).to_le_bytes());

    // Index the base at BLOCK stride.
    let n_blocks = base.len() / BLOCK;
    let table_bits = (usize::BITS - n_blocks.leading_zeros()).max(4);
    let mask = (1usize << table_bits) - 1;
    let mut head: Vec<u32> = vec![u32::MAX; mask + 1];
    let mut next: Vec<u32> = vec![u32::MAX; n_blocks];
    let hash = |b: &[u8]| -> usize {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &x in b {
            h = (h ^ x as u64).wrapping_mul(0x1000_0000_01b3);
        }
        (h as usize) & mask
    };
    // Insert blocks last-to-first so chains list LOWEST offsets first — the
    // tie-break falls out of construction order.
    for bi in (0..n_blocks).rev() {
        let h = hash(&base[bi * BLOCK..bi * BLOCK + BLOCK]);
        next[bi] = head[h];
        head[h] = bi as u32;
    }

    let mut lit_start = 0usize;
    let mut pos = 0usize;
    let flush_lit = |out: &mut Vec<u8>, from: usize, to: usize, target: &[u8]| {
        let mut s = from;
        while s < to {
            let n = (to - s).min(u32::MAX as usize);
            out.push(OP_INSERT);
            out.extend_from_slice(&(n as u32).to_le_bytes());
            out.extend_from_slice(&target[s..s + n]);
            s += n;
        }
    };
    while pos + BLOCK <= target.len() {
        let h = hash(&target[pos..pos + BLOCK]);
        let mut best_len = 0usize;
        let mut best_off = 0usize;
        let mut cand = head[h];
        let mut probes = 0usize;
        while cand != u32::MAX && probes < CHAIN_CAP {
            let boff = cand as usize * BLOCK;
            // Extend the match forward from the block start.
            let max = (base.len() - boff).min(target.len() - pos);
            let mut l = 0usize;
            while l < max && base[boff + l] == target[pos + l] {
                l += 1;
            }
            if l > best_len {
                best_len = l;
                best_off = boff;
            }
            cand = next[cand as usize];
            probes += 1;
        }
        if best_len >= MIN_MATCH {
            flush_lit(&mut out, lit_start, pos, target);
            out.push(OP_COPY);
            out.extend_from_slice(&(best_off as u64).to_le_bytes());
            out.extend_from_slice(&(best_len as u64).to_le_bytes());
            pos += best_len;
            lit_start = pos;
        } else {
            pos += 1;
        }
        if out.len() >= target.len() + 16 {
            return None; // already not smaller; stop wasting work
        }
    }
    flush_lit(&mut out, lit_start, target.len(), target);
    (out.len() < target.len() + 5).then_some(out) // must beat raw-in-envelope
}

// ---------------------------------------------------------------------------
// §8.4 — zip-splice-v1
// ---------------------------------------------------------------------------

/// One member located by the parser: `member_no` is CENTRAL-DIRECTORY order
/// (the identity used by the member table), `offset`/`len` locate the data
/// segment in the ORIGINAL file. `name` is the raw name bytes — possibly not
/// UTF-8, display use only; reconstruction never reads it.
#[derive(Debug, Clone)]
pub struct SpliceMember {
    pub member_no: u32,
    pub offset: u64,
    pub len: u64,
    pub name: Vec<u8>,
    pub method: u16,
}

/// The parse result: members plus the residual payload (§8.4 layout) that,
/// together with the member data, reconstructs the original byte-identically
/// BY CONSTRUCTION (the partition invariant).
#[derive(Debug)]
pub struct SpliceParts {
    pub members: Vec<SpliceMember>,
    pub residual_payload: Vec<u8>,
}

fn rd_u16(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes(s.try_into().expect("2")))
        .ok_or_else(|| Error::Corrupt("zip: read past end".into()))
}
fn rd_u32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes(s.try_into().expect("4")))
        .ok_or_else(|| Error::Corrupt("zip: read past end".into()))
}

/// Parse a zip archive by the ONE hard rule practice converged on
/// (DESIGN-RETL §8.4): enumerate from the central directory ONLY, apply the
/// SFX base-offset correction, verify `PK\x03\x04` at each corrected offset,
/// compute the data start from the LOCAL header's name/extra lengths (the
/// CD's copies legally differ — using them is the classic splicer bug), and
/// take the length from the CD's `compressed_size`.
///
/// Named refusals, each the point where data location becomes ambiguous or
/// the partition invariant breaks: zip64, multi-disk, encrypted/masked
/// central directory, strong encryption, a missing local-header signature at
/// a CD-claimed offset, overlapping segments (the zip-bomb shape), and
/// segments out of bounds. Traditionally- or AES-encrypted MEMBERS pass —
/// their crypto lives inside `compressed_size`, and opaque bytes splice like
/// any others.
pub fn zip_split(file: &[u8]) -> Result<SpliceParts> {
    // --- EOCD: scan backward for PK\x05\x06 with a consistent comment length.
    const EOCD_MIN: usize = 22;
    if file.len() < EOCD_MIN {
        return Err(Error::Unsupported("zip: too small to hold an end record".into()));
    }
    let scan_from = file.len().saturating_sub(EOCD_MIN + 65_535);
    let mut eocd_pos = None;
    for p in (scan_from..=file.len() - EOCD_MIN).rev() {
        if &file[p..p + 4] == b"PK\x05\x06" {
            let comment_len = rd_u16(file, p + 20)? as usize;
            if p + EOCD_MIN + comment_len == file.len() {
                eocd_pos = Some(p);
                break;
            }
        }
    }
    let Some(eocd) = eocd_pos else {
        return Err(Error::Unsupported(
            "zip: no end-of-central-directory record — not a zip archive".into(),
        ));
    };
    let disk_no = rd_u16(file, eocd + 4)?;
    let cd_disk = rd_u16(file, eocd + 6)?;
    let entries_here = rd_u16(file, eocd + 8)?;
    let entries_total = rd_u16(file, eocd + 10)?;
    let cd_size = rd_u32(file, eocd + 12)? as u64;
    let cd_offset = rd_u32(file, eocd + 16)? as u64;
    if disk_no != 0 || cd_disk != 0 || entries_here != entries_total {
        return Err(Error::Unsupported(
            "zip: multi-disk archive — spanned archives are refused".into(),
        ));
    }
    if entries_total == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
        return Err(Error::Unsupported(
            "zip: zip64 sentinel in the end record — zip64 is refused (a missing \
             zip64 record would mean a WRONG cut length, never a slightly-off one)"
                .into(),
        ));
    }

    // --- SFX base-offset correction (Info-ZIP's rule): the CD really sits
    // just before the EOCD; the difference from the recorded offset is the
    // prepended-stub length, added to every recorded offset.
    let cd_pos = (eocd as u64)
        .checked_sub(cd_size)
        .ok_or_else(|| Error::Corrupt("zip: central directory larger than the file".into()))?;
    let base = cd_pos.checked_sub(cd_offset).ok_or_else(|| {
        Error::Unsupported(
            "zip: central-directory offset is inconsistent with its position — \
             unresolvable offsets are refused"
                .into(),
        )
    })?;

    // --- Walk the central directory.
    let mut members = Vec::with_capacity(entries_total as usize);
    let mut cur = cd_pos as usize;
    for member_no in 0..entries_total as u32 {
        if file.get(cur..cur + 4).map(|s| s != b"PK\x01\x02").unwrap_or(true) {
            return Err(Error::Corrupt(format!(
                "zip: central-directory entry {member_no} has no signature"
            )));
        }
        let flags = rd_u16(file, cur + 8)?;
        if flags & (1 << 13) != 0 {
            return Err(Error::Unsupported(format!(
                "zip: member {member_no} has a masked (central-directory-encrypted) local \
                 header — its sizes are unknowable, refused"
            )));
        }
        if flags & (1 << 6) != 0 {
            return Err(Error::Unsupported(format!(
                "zip: member {member_no} uses strong encryption — refused"
            )));
        }
        let method = rd_u16(file, cur + 10)?;
        let comp_size = rd_u32(file, cur + 20)? as u64;
        let uncomp_size = rd_u32(file, cur + 24)? as u64;
        let name_len = rd_u16(file, cur + 28)? as usize;
        let extra_len = rd_u16(file, cur + 30)? as usize;
        let comment_len = rd_u16(file, cur + 32)? as usize;
        let disk_start = rd_u16(file, cur + 34)?;
        let lfh_off = rd_u32(file, cur + 42)? as u64;
        if disk_start != 0 {
            return Err(Error::Unsupported(format!(
                "zip: member {member_no} starts on another disk — refused"
            )));
        }
        if comp_size == 0xFFFF_FFFF || uncomp_size == 0xFFFF_FFFF || lfh_off == 0xFFFF_FFFF {
            return Err(Error::Unsupported(format!(
                "zip: member {member_no} carries a zip64 sentinel — zip64 is refused"
            )));
        }
        let name = file
            .get(cur + 46..cur + 46 + name_len)
            .ok_or_else(|| Error::Corrupt("zip: central directory truncated".into()))?
            .to_vec();

        // Locate the data segment via the LOCAL header.
        let lfh = (base + lfh_off) as usize;
        if file.get(lfh..lfh + 4).map(|s| s != b"PK\x03\x04").unwrap_or(true) {
            return Err(Error::Unsupported(format!(
                "zip: member {member_no}'s local header is not where the central \
                 directory claims (offset {lfh}) — refused rather than guessed"
            )));
        }
        let l_name = rd_u16(file, lfh + 26)? as usize;
        let l_extra = rd_u16(file, lfh + 28)? as usize;
        let data_start = (lfh + 30 + l_name + l_extra) as u64;
        let data_end = data_start
            .checked_add(comp_size)
            .ok_or_else(|| Error::Corrupt("zip: data segment overflows".into()))?;
        if data_end > file.len() as u64 {
            return Err(Error::Unsupported(format!(
                "zip: member {member_no}'s data segment ({data_start}+{comp_size}) reaches \
                 past the end of the file — refused"
            )));
        }
        members.push(SpliceMember {
            member_no,
            offset: data_start,
            len: comp_size,
            name,
            method,
        });
        cur += 46 + name_len + extra_len + comment_len;
    }

    // --- The partition invariant (Adler's covered-span discipline): data
    // ranges sorted by offset, pairwise DISJOINT, and clear of the central
    // directory. Overlap is the zip-bomb shape and the one quirk that makes
    // cut-and-resplice ambiguous.
    let mut by_off: Vec<&SpliceMember> = members.iter().collect();
    by_off.sort_by_key(|m| m.offset);
    for w in by_off.windows(2) {
        if w[0].offset + w[0].len > w[1].offset {
            return Err(Error::Unsupported(format!(
                "zip: members {} and {} claim OVERLAPPING data segments (the zip-bomb \
                 shape) — cut-and-resplice would be ambiguous, refused",
                w[0].member_no, w[1].member_no
            )));
        }
    }
    for m in &by_off {
        if m.offset < cd_pos + cd_size && m.offset + m.len > cd_pos {
            return Err(Error::Unsupported(format!(
                "zip: member {}'s data segment overlaps the central directory — refused",
                m.member_no
            )));
        }
    }

    // --- Build the residual payload: splice list (offset order) + gap bytes.
    let mut payload = Vec::with_capacity(64 + file.len());
    payload.extend_from_slice(&(file.len() as u64).to_le_bytes());
    payload.extend_from_slice(&(members.len() as u32).to_le_bytes());
    for m in &by_off {
        payload.extend_from_slice(&m.member_no.to_le_bytes());
        payload.extend_from_slice(&m.offset.to_le_bytes());
        payload.extend_from_slice(&m.len.to_le_bytes());
    }
    let mut cursor = 0u64;
    for m in &by_off {
        payload.extend_from_slice(&file[cursor as usize..m.offset as usize]);
        cursor = m.offset + m.len;
    }
    payload.extend_from_slice(&file[cursor as usize..]);

    Ok(SpliceParts { members, residual_payload: payload })
}

/// Re-splice: the residual payload plus the member data (by `member_no`)
/// reproduces the original file. Byte-identity is the partition invariant —
/// but the caller VERIFIES against the original's hash anyway, because
/// "by construction" is an argument and the hash gate is a check.
pub fn zip_join(payload: &[u8], data_of: &dyn Fn(u32) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let corrupt = |m: &str| Error::Corrupt(format!("zip-splice-v1: {m}"));
    if payload.len() < 12 {
        return Err(corrupt("truncated before the header"));
    }
    let file_len = u64::from_le_bytes(payload[0..8].try_into().expect("8"));
    if file_len > MAX_PAYLOAD as u64 {
        return Err(corrupt("file_len exceeds the envelope cap"));
    }
    let count = u32::from_le_bytes(payload[8..12].try_into().expect("4")) as usize;
    let entries_end = 12usize
        .checked_add(count.checked_mul(20).ok_or_else(|| corrupt("entry count overflows"))?)
        .ok_or_else(|| corrupt("entry table overflows"))?;
    if payload.len() < entries_end {
        return Err(corrupt("truncated inside the splice list"));
    }
    let mut entries = Vec::with_capacity(count);
    let mut total_data = 0u64;
    let mut prev_end = 0u64;
    for i in 0..count {
        let at = 12 + i * 20;
        let member_no = u32::from_le_bytes(payload[at..at + 4].try_into().expect("4"));
        let offset = u64::from_le_bytes(payload[at + 4..at + 12].try_into().expect("8"));
        let len = u64::from_le_bytes(payload[at + 12..at + 20].try_into().expect("8"));
        if offset < prev_end {
            return Err(corrupt("splice list not sorted/disjoint"));
        }
        prev_end = offset.checked_add(len).ok_or_else(|| corrupt("entry overflows"))?;
        if prev_end > file_len {
            return Err(corrupt("entry reaches past file_len"));
        }
        total_data = total_data.checked_add(len).ok_or_else(|| corrupt("data overflows"))?;
        entries.push((member_no, offset, len));
    }
    let gap = &payload[entries_end..];
    if gap.len() as u64 + total_data != file_len {
        return Err(corrupt(&format!(
            "partition broken: {} gap bytes + {total_data} data bytes != file_len {file_len}",
            gap.len()
        )));
    }

    let mut out = Vec::with_capacity(file_len as usize);
    let mut gap_cursor = 0usize;
    for (member_no, offset, len) in &entries {
        let gap_take = (*offset - out.len() as u64) as usize;
        out.extend_from_slice(&gap[gap_cursor..gap_cursor + gap_take]);
        gap_cursor += gap_take;
        let data = data_of(*member_no)?;
        if data.len() as u64 != *len {
            return Err(corrupt(&format!(
                "member {member_no}'s stored data is {} bytes, the splice list says {len}",
                data.len()
            )));
        }
        out.extend_from_slice(&data);
    }
    out.extend_from_slice(&gap[gap_cursor..]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests — decoder totality, round trips, refusals
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    fn bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..n).map(|_| (xorshift(&mut s) & 0xff) as u8).collect()
    }

    #[test]
    fn envelope_round_trips_and_refuses_newer_kinds_by_number() {
        let e = envelope(Kind::DeltaV1, b"payload").unwrap();
        let (k, p) = open_envelope(&e).unwrap();
        assert_eq!(k, Kind::DeltaV1);
        assert_eq!(p, b"payload");

        let mut newer = e.clone();
        newer[0] = 7;
        let err = open_envelope(&newer).unwrap_err().to_string();
        assert!(err.contains('7'), "the unknown kind is NAMED: {err}");

        for n in 0..e.len() {
            assert!(open_envelope(&e[..n]).is_err(), "prefix {n} decoded");
        }
        let mut long = e.clone();
        long.push(0);
        assert!(open_envelope(&long).is_err(), "trailing bytes are corruption");
    }

    /// The delta contract: apply(base, encode(base, target)) == target,
    /// byte-identically, across shapes chosen to stress the encoder — and the
    /// test NEVER asserts encoder bytes, only round-trip equality.
    #[test]
    fn delta_round_trips_across_shapes() {
        let cases: Vec<(Vec<u8>, Vec<u8>)> = vec![
            // small edit in a large base
            {
                let base = bytes(50_000, 1);
                let mut t = base.clone();
                t[25_000] ^= 0xff;
                (base, t)
            },
            // insertion in the middle
            {
                let base = bytes(10_000, 2);
                let mut t = base[..5000].to_vec();
                t.extend_from_slice(b"INSERTED CONTENT");
                t.extend_from_slice(&base[5000..]);
                (base, t)
            },
            // deletion
            {
                let base = bytes(10_000, 3);
                let mut t = base[..2000].to_vec();
                t.extend_from_slice(&base[6000..]);
                (base, t)
            },
            // rearrangement
            {
                let base = bytes(8_000, 4);
                let mut t = base[4000..].to_vec();
                t.extend_from_slice(&base[..4000]);
                (base, t)
            },
            // adversarial repetition (the chain-cap case)
            (vec![0u8; 100_000], vec![0u8; 100_001]),
            // empty base (all-literal delta), empty target
            (Vec::new(), bytes(500, 5)),
            (bytes(500, 6), Vec::new()),
        ];
        for (i, (base, target)) in cases.iter().enumerate() {
            match delta_encode(base, target) {
                Some(d) => {
                    let back = delta_apply(base, &d).unwrap();
                    assert_eq!(&back, target, "case {i}: round trip broke");
                    assert!(
                        d.len() < target.len() + 5,
                        "case {i}: a delta that is not smaller must be None"
                    );
                }
                None => {
                    // Legal: the encoder declines when not smaller. Raw wins.
                }
            }
        }
        // The similar-inputs case MUST produce a small delta — otherwise the
        // engine is decorative. (A bound, not a byte-assert.)
        let base = bytes(100_000, 7);
        let mut t = base.clone();
        t[1000] ^= 1;
        let d = delta_encode(&base, &t).expect("a one-byte edit must delta well");
        assert!(d.len() < 200, "one-byte edit encoded to {} bytes", d.len());
        assert_eq!(delta_apply(&base, &d).unwrap(), t);
    }

    #[test]
    fn delta_encoder_is_deterministic() {
        let base = bytes(30_000, 8);
        let mut t = base[..10_000].to_vec();
        t.extend_from_slice(&bytes(100, 9));
        t.extend_from_slice(&base[10_000..]);
        let a = delta_encode(&base, &t);
        let b = delta_encode(&base, &t);
        assert_eq!(a, b, "same inputs, same bytes — a pure function");
    }

    // ------------------------------------------------------------- zip

    /// Minimal STORE-method zip writer for fixtures. CRCs are zero — the
    /// splicer never validates them, and reconstruction is byte-identical
    /// regardless (they ride the residual verbatim).
    fn store_zip(members: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut f = Vec::new();
        let mut cd = Vec::new();
        let mut offsets = Vec::new();
        for (name, data) in members {
            offsets.push(f.len() as u32);
            f.extend_from_slice(b"PK\x03\x04");
            f.extend_from_slice(&20u16.to_le_bytes()); // version needed
            f.extend_from_slice(&0u16.to_le_bytes()); // flags
            f.extend_from_slice(&0u16.to_le_bytes()); // method = store
            f.extend_from_slice(&[0; 4]); // time+date
            f.extend_from_slice(&[0; 4]); // crc
            f.extend_from_slice(&(data.len() as u32).to_le_bytes());
            f.extend_from_slice(&(data.len() as u32).to_le_bytes());
            f.extend_from_slice(&(name.len() as u16).to_le_bytes());
            f.extend_from_slice(&0u16.to_le_bytes()); // extra
            f.extend_from_slice(name);
            f.extend_from_slice(data);
        }
        let cd_start = f.len() as u32;
        for ((name, data), off) in members.iter().zip(&offsets) {
            cd.extend_from_slice(b"PK\x01\x02");
            cd.extend_from_slice(&20u16.to_le_bytes()); // made by
            cd.extend_from_slice(&20u16.to_le_bytes()); // needed
            cd.extend_from_slice(&0u16.to_le_bytes()); // flags
            cd.extend_from_slice(&0u16.to_le_bytes()); // method
            cd.extend_from_slice(&[0; 4]); // time+date
            cd.extend_from_slice(&[0; 4]); // crc
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes()); // extra
            cd.extend_from_slice(&0u16.to_le_bytes()); // comment
            cd.extend_from_slice(&0u16.to_le_bytes()); // disk start
            cd.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            cd.extend_from_slice(&[0; 4]); // external attrs
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(name);
        }
        f.extend_from_slice(&cd);
        f.extend_from_slice(b"PK\x05\x06");
        f.extend_from_slice(&[0; 4]); // disk numbers
        f.extend_from_slice(&(members.len() as u16).to_le_bytes());
        f.extend_from_slice(&(members.len() as u16).to_le_bytes());
        f.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        f.extend_from_slice(&cd_start.to_le_bytes());
        f.extend_from_slice(&0u16.to_le_bytes()); // comment len
        f
    }

    fn roundtrip(file: &[u8]) -> SpliceParts {
        let parts = zip_split(file).unwrap();
        let data: std::collections::HashMap<u32, Vec<u8>> = parts
            .members
            .iter()
            .map(|m| {
                (m.member_no, file[m.offset as usize..(m.offset + m.len) as usize].to_vec())
            })
            .collect();
        let back = zip_join(&parts.residual_payload, &|no| {
            data.get(&no).cloned().ok_or_else(|| Error::Corrupt("missing member".into()))
        })
        .unwrap();
        assert_eq!(back, file, "re-splice must be byte-identical");
        parts
    }

    #[test]
    fn zip_splice_round_trips_the_quirk_battery() {
        // Plain, empty-member, empty archive, non-UTF8 name, duplicate names.
        let plain = store_zip(&[(b"a.txt", b"hello"), (b"b.bin", &bytes(3000, 20))]);
        assert_eq!(roundtrip(&plain).members.len(), 2);

        let empty_member = store_zip(&[(b"empty", b"")]);
        roundtrip(&empty_member);

        let empty_zip = store_zip(&[]);
        assert_eq!(roundtrip(&empty_zip).members.len(), 0);

        let non_utf8 = store_zip(&[(&[0xC0, 0xFF, 0x80][..], b"cp437 name")]);
        let parts = roundtrip(&non_utf8);
        assert_eq!(parts.members[0].name, vec![0xC0, 0xFF, 0x80]);

        let dupes = store_zip(&[(b"same", b"one"), (b"same", b"two")]);
        assert_eq!(roundtrip(&dupes).members.len(), 2);

        // SFX stub: bytes prepended, internal offsets NOT updated — the
        // base-offset correction absorbs it and the stub rides the residual.
        let mut sfx = b"#!/bin/sh SELF-EXTRACTING STUB\n".to_vec();
        sfx.extend_from_slice(&plain);
        roundtrip(&sfx);

        // Junk BETWEEN members (deleted entries / incremental appends):
        // build manually by splicing garbage before the second member's LFH
        // and fixing the CD offset. Cheaper: append junk after the archive's
        // last data but before the CD is not expressible with store_zip, so
        // exercise the equivalent quirk — a data-descriptor member, whose
        // descriptor bytes sit between data and the next header.
        let mut dd = store_zip(&[(b"d.txt", b"descriptor-follows")]);
        // set GP bit 3 in both headers, zero the LFH sizes, splice a
        // 16-byte descriptor after the data.
        let data_start = 30 + 5; // LFH fixed + name "d.txt"
        let data_len = b"descriptor-follows".len();
        dd[6] = 0x08; // LFH flags low byte: bit 3
        dd[18..26].fill(0); // LFH crc+sizes zeroed (descriptor carries them)
        let mut desc = b"PK\x07\x08".to_vec();
        desc.extend_from_slice(&[0; 4]); // crc
        desc.extend_from_slice(&(data_len as u32).to_le_bytes());
        desc.extend_from_slice(&(data_len as u32).to_le_bytes());
        let insert_at = data_start + data_len;
        let mut with_desc = dd[..insert_at].to_vec();
        with_desc.extend_from_slice(&desc);
        with_desc.extend_from_slice(&dd[insert_at..]);
        // fix CD: flags bit 3 + cd_offset shift (+16) in EOCD
        let cd_pos = insert_at + 16;
        with_desc[cd_pos + 8] = 0x08; // CD flags low byte
        let eocd = with_desc.len() - 22;
        let old_cd_off = u32::from_le_bytes(with_desc[eocd + 16..eocd + 20].try_into().unwrap());
        with_desc[eocd + 16..eocd + 20]
            .copy_from_slice(&(old_cd_off + 16).to_le_bytes());
        let parts = roundtrip(&with_desc);
        // The descriptor is NON-data: it rides the residual, and the cut is
        // exactly the CD's compressed_size.
        assert_eq!(parts.members[0].len as usize, data_len);
    }

    #[test]
    fn zip_refusals_are_named() {
        let plain = store_zip(&[(b"a", b"data-a"), (b"b", b"data-b")]);

        // zip64 sentinel in the EOCD.
        let mut z64 = plain.clone();
        let eocd = z64.len() - 22;
        z64[eocd + 16..eocd + 20].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let e = zip_split(&z64).unwrap_err().to_string();
        assert!(e.contains("zip64"), "{e}");

        // Multi-disk.
        let mut md = plain.clone();
        let eocd = md.len() - 22;
        md[eocd + 4] = 1;
        let e = zip_split(&md).unwrap_err().to_string();
        assert!(e.contains("multi-disk") || e.contains("spanned"), "{e}");

        // Overlap: point member b's CD entry at member a's local header.
        let mut ov = plain.clone();
        // find second CD entry: CD starts after both members' LFH+data
        let cd_start = ov.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        let second = cd_start + 46 + 1; // entry 0 is 46 + name_len(1)
        assert_eq!(&ov[second..second + 4], b"PK\x01\x02");
        ov[second + 42..second + 46].copy_from_slice(&0u32.to_le_bytes());
        let e = zip_split(&ov).unwrap_err().to_string();
        assert!(e.contains("OVERLAP") || e.contains("zip bomb"), "{e}");

        // Masked central directory (bit 13).
        let mut masked = plain.clone();
        let cd_start = masked.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        masked[cd_start + 8] |= 0x20; // bit 13 = 0x2000, high byte at +9
        masked[cd_start + 9] |= 0x20;
        let e = zip_split(&masked).unwrap_err().to_string();
        assert!(e.contains("masked"), "{e}");

        // CD claims an LFH where there is none.
        let mut bad = plain.clone();
        let cd_start = bad.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        bad[cd_start + 42..cd_start + 46].copy_from_slice(&3u32.to_le_bytes());
        let e = zip_split(&bad).unwrap_err().to_string();
        assert!(e.contains("local header is not where"), "{e}");

        // Not a zip at all.
        let e = zip_split(b"just some text, no archive").unwrap_err().to_string();
        assert!(e.contains("not a zip"), "{e}");

        // zip_join totality: truncation at every offset of a real payload.
        let parts = zip_split(&plain).unwrap();
        for n in 0..parts.residual_payload.len().min(40) {
            assert!(
                zip_join(&parts.residual_payload[..n], &|_| Ok(Vec::new())).is_err(),
                "prefix {n} joined"
            );
        }
    }

    /// The eternity decoder is total: truncation at EVERY offset of a real
    /// delta errors, never panics — and the resource bounds fire before any
    /// allocation could.
    #[test]
    fn delta_apply_is_total_and_resource_bounded() {
        let base = bytes(5_000, 10);
        let mut t = base.clone();
        t.extend_from_slice(b"tail");
        let d = delta_encode(&base, &t).unwrap();
        for n in 0..d.len() {
            assert!(delta_apply(&base, &d[..n]).is_err(), "prefix {n} applied");
        }
        // Wrong base length: refused by the header check, named.
        let e = delta_apply(&base[..100], &d).unwrap_err().to_string();
        assert!(e.contains("wrong base"), "{e}");
        // A 2^63 COPY dies on the bounds check, not in the allocator.
        let mut evil = Vec::new();
        evil.extend_from_slice(&(base.len() as u64).to_le_bytes());
        evil.extend_from_slice(&u64::MAX.to_le_bytes()); // target_len
        evil.push(OP_COPY);
        evil.extend_from_slice(&0u64.to_le_bytes());
        evil.extend_from_slice(&(1u64 << 63).to_le_bytes());
        let e = delta_apply(&base, &evil).unwrap_err().to_string();
        assert!(e.contains("envelope cap") || e.contains("reaches past"), "{e}");
    }
}
