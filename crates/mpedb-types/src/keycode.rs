//! memcmp-ordered key encoding.
//!
//! `encode_key(a) < encode_key(b)` (bytewise) iff `a < b` in SQL ORDER BY
//! semantics with NULLS FIRST, binary text collation, IEEE total order for
//! floats (-0.0 == 0.0, NaNs equal and greater than +inf).
//!
//! Layout per value: a tag byte (0x00 = NULL, 0x01 = present) followed by the
//! type-specific payload. Composite keys are simply concatenated; because
//! every payload is either fixed-size or 0x00-terminated with escaping, no
//! separator is needed and prefix ordering is preserved.

use crate::error::{Error, Result};
use crate::value::{normalize_float_bits, ColumnType, Value};

const TAG_NULL: u8 = 0x00;
const TAG_PRESENT: u8 = 0x01;
/// Escape for a literal 0x00 inside text/blob payloads; the terminator is a
/// bare 0x00 followed by any byte < 0xff (i.e. the next tag or end-of-key).
const ESCAPE: u8 = 0xff;
const TERMINATOR: u8 = 0x00;

/// Append the ordered encoding of one value.
pub fn encode_value(buf: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => buf.push(TAG_NULL),
        Value::Int(x) | Value::Timestamp(x) => {
            buf.push(TAG_PRESENT);
            buf.extend_from_slice(&((*x as u64) ^ (1 << 63)).to_be_bytes());
        }
        Value::Float(x) => {
            buf.push(TAG_PRESENT);
            buf.extend_from_slice(&normalize_float_bits(*x).to_be_bytes());
        }
        Value::Bool(x) => {
            buf.push(TAG_PRESENT);
            buf.push(*x as u8);
        }
        Value::Text(s) => {
            buf.push(TAG_PRESENT);
            encode_bytes(buf, s.as_bytes());
        }
        Value::Blob(b) => {
            buf.push(TAG_PRESENT);
            encode_bytes(buf, b);
        }
        // A context list can never be a key: it has no ordering (`sql_cmp`
        // refuses it) and no column to live in (`column_type()` is None, so
        // `fits` rejects it from every column, and validate_row rejects the
        // row before the engine ever builds a key). Reaching here means an
        // earlier validation was removed, so say so loudly rather than encode
        // something that would silently corrupt an index — this signature
        // cannot return an error, and a wrong key is worse than a crash.
        Value::List(_) => unreachable!(
            "a context list reached key encoding — it is param-only (DESIGN-MULTIDB §2.6)"
        ),
    }
}

fn encode_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        buf.push(b);
        if b == 0x00 {
            buf.push(ESCAPE);
        }
    }
    buf.push(TERMINATOR);
}

/// Encode a composite key.
pub fn encode_key(values: &[Value]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(values.len() * 12);
    for v in values {
        encode_value(&mut buf, v);
    }
    buf
}

/// Decode one value of declared type `ty`, advancing `*pos`. Bounds-checked;
/// corrupt input yields `Error::Corrupt`, never a panic.
pub fn decode_value(buf: &[u8], pos: &mut usize, ty: ColumnType) -> Result<Value> {
    let err = || Error::Corrupt("truncated key".into());
    let tag = *buf.get(*pos).ok_or_else(err)?;
    *pos += 1;
    match tag {
        TAG_NULL => return Ok(Value::Null),
        TAG_PRESENT => {}
        t => return Err(Error::Corrupt(format!("invalid key tag {t:#x}"))),
    }
    match ty {
        ColumnType::Int64 | ColumnType::Timestamp => {
            let raw = buf.get(*pos..*pos + 8).ok_or_else(err)?;
            *pos += 8;
            let x = (u64::from_be_bytes(raw.try_into().unwrap()) ^ (1 << 63)) as i64;
            Ok(if ty == ColumnType::Int64 {
                Value::Int(x)
            } else {
                Value::Timestamp(x)
            })
        }
        ColumnType::Float64 => {
            let raw = buf.get(*pos..*pos + 8).ok_or_else(err)?;
            *pos += 8;
            let n = u64::from_be_bytes(raw.try_into().unwrap());
            let bits = if n >> 63 == 1 { n & !(1 << 63) } else { !n };
            Ok(Value::Float(f64::from_bits(bits)))
        }
        ColumnType::Bool => {
            let b = *buf.get(*pos).ok_or_else(err)?;
            *pos += 1;
            match b {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                _ => Err(Error::Corrupt("invalid bool in key".into())),
            }
        }
        ColumnType::Text | ColumnType::Blob => {
            let bytes = decode_bytes(buf, pos)?;
            if ty == ColumnType::Text {
                Ok(Value::Text(String::from_utf8(bytes).map_err(|_| {
                    Error::Corrupt("invalid utf-8 in key".into())
                })?))
            } else {
                Ok(Value::Blob(bytes))
            }
        }
    }
}

fn decode_bytes(buf: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let b = *buf
            .get(*pos)
            .ok_or_else(|| Error::Corrupt("unterminated key bytes".into()))?;
        *pos += 1;
        if b != 0x00 {
            out.push(b);
            continue;
        }
        // 0x00 + ESCAPE = literal zero byte; 0x00 + anything else = terminator
        // (the next byte belongs to the following field and is never peeked
        // past the end of the buffer).
        match buf.get(*pos) {
            Some(&ESCAPE) => {
                out.push(0x00);
                *pos += 1;
            }
            _ => return Ok(out),
        }
    }
}

/// Decode a full composite key given the declared column types.
pub fn decode_key(buf: &[u8], types: &[ColumnType]) -> Result<Vec<Value>> {
    let mut pos = 0;
    let mut out = Vec::with_capacity(types.len());
    for &ty in types {
        out.push(decode_value(buf, &mut pos, ty)?);
    }
    if pos != buf.len() {
        return Err(Error::Corrupt("trailing bytes in key".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    /// Deterministic xorshift so tests need no external RNG crate.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn random_value(rng: &mut Rng, ty: ColumnType) -> Value {
        if rng.next().is_multiple_of(10) {
            return Value::Null;
        }
        match ty {
            ColumnType::Int64 => Value::Int(rng.next() as i64 >> (rng.next() % 64)),
            ColumnType::Timestamp => Value::Timestamp(rng.next() as i64 >> (rng.next() % 64)),
            ColumnType::Float64 => {
                let choices = [
                    f64::from_bits(rng.next()),
                    (rng.next() as i64 >> 40) as f64 / 8.0,
                    0.0,
                    -0.0,
                    f64::NAN,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                ];
                Value::Float(choices[(rng.next() % choices.len() as u64) as usize])
            }
            ColumnType::Bool => Value::Bool(rng.next().is_multiple_of(2)),
            ColumnType::Text => {
                let len = (rng.next() % 12) as usize;
                let s: String = (0..len)
                    .map(|_| {
                        let alphabet = ['a', 'b', '\u{0}', 'ø', 'z'];
                        alphabet[(rng.next() % alphabet.len() as u64) as usize]
                    })
                    .collect();
                Value::Text(s)
            }
            ColumnType::Blob => {
                let len = (rng.next() % 12) as usize;
                Value::Blob((0..len).map(|_| (rng.next() % 4) as u8 * 85).collect())
            }
        }
    }

    /// Reference order: NULLS FIRST, then sql_cmp.
    fn semantic_cmp(a: &Value, b: &Value) -> Ordering {
        match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => a.sql_cmp(b).unwrap().unwrap(),
        }
    }

    #[test]
    fn encoding_order_matches_semantic_order() {
        let types = [
            ColumnType::Int64,
            ColumnType::Float64,
            ColumnType::Bool,
            ColumnType::Text,
            ColumnType::Blob,
            ColumnType::Timestamp,
        ];
        let mut rng = Rng(0x9e3779b97f4a7c15);
        for &ty in &types {
            for _ in 0..2000 {
                let a = random_value(&mut rng, ty);
                let b = random_value(&mut rng, ty);
                let ea = encode_key(std::slice::from_ref(&a));
                let eb = encode_key(std::slice::from_ref(&b));
                assert_eq!(
                    ea.cmp(&eb),
                    semantic_cmp(&a, &b),
                    "order mismatch for {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn composite_prefix_ordering() {
        // "ab" followed by anything must sort before "ab\0..." (embedded zero).
        let k1 = encode_key(&[Value::Text("ab".into()), Value::Int(i64::MAX)]);
        let k2 = encode_key(&[Value::Text("ab\0".into()), Value::Int(i64::MIN)]);
        assert!(k1 < k2);
    }

    #[test]
    fn roundtrip_composite() {
        let types = [ColumnType::Text, ColumnType::Int64, ColumnType::Blob];
        let vals = vec![
            Value::Text("a\0b".into()),
            Value::Int(-7),
            Value::Blob(vec![0, 0, 255]),
        ];
        let enc = encode_key(&vals);
        assert_eq!(decode_key(&enc, &types).unwrap(), vals);
    }

    #[test]
    fn corrupt_keys_error_not_panic() {
        let enc = encode_key(&[Value::Text("hei".into()), Value::Int(1)]);
        for cut in 0..enc.len() {
            let _ = decode_key(&enc[..cut], &[ColumnType::Text, ColumnType::Int64]);
        }
        let _ = decode_key(&[0x02], &[ColumnType::Int64]);
        let _ = decode_key(&[0x01, 0x05], &[ColumnType::Bool]);
    }
}
