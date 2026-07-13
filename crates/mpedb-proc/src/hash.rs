//! Procedure content hashes: `blake3(canonical proc blob)`, where the blob
//! embeds the format version, the full IR, the plan-hash table (with kinds
//! and arities), the arity and the name. Two processes that compute the same
//! `ProcHash` hold byte-identical procedures — the same protocol plans use
//! (DESIGN.md §7), one level up.

use mpedb_types::{Error, Result};
use std::fmt;

/// Content hash of a compiled procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcHash(pub [u8; 32]);

impl fmt::Display for ProcHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for ProcHash {
    type Err = Error;
    fn from_str(s: &str) -> Result<ProcHash> {
        let s = s.trim();
        if s.len() != 64 || !s.is_ascii() {
            return Err(Error::Config("proc hash must be 64 hex chars".into()));
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hex = std::str::from_utf8(chunk).unwrap();
            out[i] = u8::from_str_radix(hex, 16)
                .map_err(|_| Error::Config("invalid hex in proc hash".into()))?;
        }
        Ok(ProcHash(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_roundtrip() {
        let h = ProcHash([0xAB; 32]);
        let s = h.to_string();
        assert_eq!(s.parse::<ProcHash>().unwrap(), h);
        assert!("zz".parse::<ProcHash>().is_err());
        assert!("g".repeat(64).parse::<ProcHash>().is_err());
    }
}
