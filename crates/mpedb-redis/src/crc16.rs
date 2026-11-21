//! Redis Cluster's key hash: CRC-16/XMODEM, then modulo 16384.
//!
//! The polynomial and the hash-tag rule are the protocol. A client computes
//! the same slot we do, and a `MOVED` answer is only meaningful if the two
//! agree. The table is generated from the polynomial rather than copied, and
//! `crc16(b"123456789") == 0x31C3` is the check that the generator matches
//! CRC-16/XMODEM.

/// Number of hash slots in the Redis Cluster keyspace.
pub const SLOT_COUNT: u16 = 16384;

const TABLE: [u16; 256] = crc_table();

const fn crc_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            if crc & 0x8000 != 0 {
                crc = crc.wrapping_shl(1) ^ 0x1021;
            } else {
                crc = crc.wrapping_shl(1);
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// CRC-16/XMODEM of `data`.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        let idx = ((crc >> 8) ^ u16::from(byte)) as u8 as usize;
        crc = (crc << 8) ^ TABLE[idx];
    }
    crc
}

/// The bytes Redis hashes.
///
/// When the key contains `{` and a later `}` with at least one byte between
/// them, only that interior is hashed. `{}` and a `{` with no `}` hash the
/// whole key. That is what puts `{user}.a` and `{user}.b` on one slot, which
/// is what makes a multi-key command legal.
pub fn hash_tag(key: &[u8]) -> &[u8] {
    if let Some(open) = key.iter().position(|b| *b == b'{') {
        let rest = &key[open + 1..];
        if let Some(close) = rest.iter().position(|b| *b == b'}') {
            if close > 0 {
                return &rest[..close];
            }
        }
    }
    key
}

/// Cluster slot of a Redis key, hash tag included.
pub fn slot(key: &[u8]) -> u16 {
    crc16(hash_tag(key)) % SLOT_COUNT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmodem_check_value() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn hash_tag_groups_keys_and_ignores_an_empty_tag() {
        assert_eq!(slot(b"{user}.a"), slot(b"{user}.b"));
        assert_eq!(slot(b"{user}.a"), slot(b"user"));
        assert_ne!(slot(b"{user}.a"), slot(b"other"));
        assert_eq!(hash_tag(b"no-braces"), b"no-braces");
        assert_eq!(hash_tag(b"{}empty"), b"{}empty");
        assert_eq!(hash_tag(b"{"), b"{");
        assert_eq!(hash_tag(b"a{b}c{d}"), b"b");
    }

    #[test]
    fn slot_stays_inside_the_cluster_space() {
        for key in [b"" as &[u8], b"a", b"foo", b"{x}y", &[0xff, 0x00]] {
            assert!(slot(key) < SLOT_COUNT);
        }
    }
}
