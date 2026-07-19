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
use crate::value::{normalize_float_bits, Collation, ColumnType, Value};

const TAG_NULL: u8 = 0x00;
const TAG_PRESENT: u8 = 0x01;
/// Escape for a literal 0x00 inside text/blob payloads; the terminator is a
/// bare 0x00 followed by any byte < 0xff (i.e. the next tag or end-of-key).
const ESCAPE: u8 = 0xff;
const TERMINATOR: u8 = 0x00;

/// Append the ordered encoding of one value, folding TEXT under `coll` first
/// (so two texts equal under the collation encode to identical bytes). Every
/// non-TEXT value — and any value under [`Collation::Binary`] — is byte-for-byte
/// identical to [`encode_value`], so a non-collated key never changes shape.
pub fn encode_value_collated(buf: &mut Vec<u8>, v: &Value, coll: Collation) {
    match v {
        Value::Text(s) if coll != Collation::Binary => {
            buf.push(TAG_PRESENT);
            encode_bytes(buf, coll.fold_key(s).as_bytes());
        }
        _ => encode_value(buf, v),
    }
}

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

/// Encode a composite key where TEXT columns are FOLDED under a per-column
/// collating sequence before encoding, so two values that are equal under the
/// collation produce identical bytes. This is what makes a collated PRIMARY KEY
/// / secondary index collapse `'abc'` and `'ABC'` (NOCASE) into one on-disk key
/// — the same folding also drives collation-aware GROUP BY / DISTINCT.
///
/// `collations[i]` governs `values[i]`; a shorter slice (or `Binary`) leaves the
/// value bytewise, so `encode_key_collated(v, &[])` equals [`encode_key`]. Only
/// TEXT is folded — every other type is collation-independent.
pub fn encode_key_collated(values: &[Value], collations: &[Collation]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(values.len() * 12);
    for (i, v) in values.iter().enumerate() {
        let coll = collations.get(i).copied().unwrap_or(Collation::Binary);
        encode_value_collated(&mut buf, v, coll);
    }
    buf
}

// ---------------------------------------------------------------------------
// The GROUP key: sqlite's storage-class equality, as bytes.
// ---------------------------------------------------------------------------

/// Storage-class tags. Their order IS sqlite's: NULL < numbers < TEXT < BLOB.
/// `Bool`/`Timestamp` are mpedb-native with no sqlite class, so they get ranks
/// of their own above BLOB — a bool column holds only bools, so the rank never
/// decides anything a differential can see, and giving them a rank keeps the
/// key a TOTAL order (which [`Value::sort_cmp`] deliberately is not: it answers
/// `None` for such a pair, meaning "peers", and peers must not merge into one
/// group).
const CLASS_NULL: u8 = 0x00;
const CLASS_NUM: u8 = 0x01;
const CLASS_TEXT: u8 = 0x02;
const CLASS_BLOB: u8 = 0x03;
const CLASS_BOOL: u8 = 0x04;
const CLASS_TS: u8 = 0x05;

/// Numeric sub-tags, ordered. A number is keyed as `(floor, sub, [bits])`:
/// `NUM_EXACT` means the value IS that integer (9 bytes, no `bits`), the other
/// three carry the f64 image so equal-floor floats still order among themselves.
const NUM_BELOW: u8 = 0x00; // real below i64::MIN — sorts under every integer
const NUM_EXACT: u8 = 0x01; // integral: an i64, or an f64 that IS one
const NUM_ABOVE: u8 = 0x02; // floor < value < floor+1, or a real above i64::MAX
const NUM_NAN: u8 = 0x03; // NaN, above everything (see `int_float_cmp`)

/// Encode a composite **grouping** key: the key of `GROUP BY`, `DISTINCT`,
/// `PARTITION BY`, `UNION`/`INTERSECT`/`EXCEPT` dedup and `f(DISTINCT x)`.
///
/// **This is NOT [`encode_key`], and the difference is a wrong answer.** The
/// on-disk key encodes a value's mpedb TYPE, which is right for a tree over a
/// rigidly typed column: `1` and `1.0` are different entries there because the
/// column can only hold one of them. A grouping key is asked a different
/// question — "did sqlite's comparison call these two the same value?" — and
/// over a typeless (`any`) column the answer differs: sqlite groups by
/// STORAGE CLASS, so integer `1` and real `1.0` are ONE key while the text
/// `'1'` is another, and `count(DISTINCT v)` over `1, 1.0, '1'` is 2, not 3.
///
/// The contract, pinned by `group_key_matches_sort_cmp`:
///
/// - `enc(a) == enc(b)` **iff** [`Value::sort_cmp`] says `Equal` (or both are
///   NULL, which sort_cmp reports as `None` and every grouping context treats
///   as one value — sqlite's rule for `GROUP BY`/`DISTINCT`/set ops alike).
/// - `enc(a).cmp(enc(b))` **equals** `sort_cmp` whenever that is `Some`, so a
///   `BTreeMap` keyed by these bytes iterates in sqlite's order and the
///   byte-hash paths (`HashSet` dedup) and the comparator paths (`sort_rows`)
///   can never disagree. That agreement is the point of having one encoder.
///
/// `collations[i]` folds `values[i]` when it is TEXT, exactly as in
/// [`encode_key_collated`]; a short slice means `Binary`.
pub fn encode_group_key(values: &[Value], collations: &[Collation]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(values.len() * 12);
    for (i, v) in values.iter().enumerate() {
        let coll = collations.get(i).copied().unwrap_or(Collation::Binary);
        encode_group_value(&mut buf, v, coll);
    }
    buf
}

/// Append one value's grouping key. See [`encode_group_key`].
pub fn encode_group_value(buf: &mut Vec<u8>, v: &Value, coll: Collation) {
    match v {
        Value::Null => buf.push(CLASS_NULL),
        Value::Int(_) | Value::Float(_) => {
            buf.push(CLASS_NUM);
            encode_number(buf, v);
        }
        Value::Text(s) => {
            buf.push(CLASS_TEXT);
            encode_bytes(buf, coll.fold_key(s).as_bytes());
        }
        Value::Blob(b) => {
            buf.push(CLASS_BLOB);
            encode_bytes(buf, b);
        }
        Value::Bool(x) => {
            buf.push(CLASS_BOOL);
            buf.push(*x as u8);
        }
        Value::Timestamp(x) => {
            buf.push(CLASS_TS);
            buf.extend_from_slice(&((*x as u64) ^ (1 << 63)).to_be_bytes());
        }
        // Same reasoning as `encode_value`: a context list is param-only and
        // can never be a key.
        Value::List(_) => unreachable!(
            "a context list reached group-key encoding — it is param-only (DESIGN-MULTIDB §2.6)"
        ),
    }
}

/// The numeric payload: `(floor as i64, sub-tag, [f64 image])`.
///
/// Integers and reals must INTERLEAVE exactly — `9007199254740992.0` sorts
/// below the integer `9007199254740993`, and no cast can be used to decide that
/// (`as f64` rounds past 2^53, `as i64` truncates). Keying on the floor plus a
/// sub-tag does it without any lossy conversion: an exact integer is
/// `(n, EXACT)`; a fractional real is `(floor, ABOVE, bits)`, which lands
/// strictly between `(floor, EXACT)` and `(floor+1, EXACT)`; and a real outside
/// i64's range is pinned to the extreme integer with `BELOW`/`ABOVE`.
///
/// The image is [`normalize_float_bits`], monotone in the real value, so
/// same-floor reals order correctly among themselves. Self-delimiting: the
/// sub-tag says whether 8 more bytes follow, so composite keys concatenate.
fn encode_number(buf: &mut Vec<u8>, v: &Value) {
    let push_i = |buf: &mut Vec<u8>, i: i64| {
        buf.extend_from_slice(&((i as u64) ^ (1 << 63)).to_be_bytes())
    };
    let push_f =
        |buf: &mut Vec<u8>, f: f64| buf.extend_from_slice(&normalize_float_bits(f).to_be_bytes());
    match v {
        Value::Int(i) => {
            push_i(buf, *i);
            buf.push(NUM_EXACT);
        }
        Value::Float(f) => {
            let f = *f;
            if f.is_nan() {
                push_i(buf, i64::MAX);
                buf.push(NUM_NAN);
                push_f(buf, f);
            } else if f >= 9223372036854775808.0 {
                // Above every i64 (2^63 is exactly representable; i64::MAX is
                // 2^63-1, so `>=` is the right boundary).
                push_i(buf, i64::MAX);
                buf.push(NUM_ABOVE);
                push_f(buf, f);
            } else if f < -9223372036854775808.0 {
                push_i(buf, i64::MIN);
                buf.push(NUM_BELOW);
                push_f(buf, f);
            } else {
                let fl = f.floor();
                // In range by the guards, so the cast is exact. `-0.0` floors to
                // `-0.0`, which equals `0.0` and casts to 0 — so `-0.0` and `0`
                // are ONE group, as sqlite has them.
                let i = fl as i64;
                push_i(buf, i);
                if fl == f {
                    buf.push(NUM_EXACT);
                } else {
                    buf.push(NUM_ABOVE);
                    push_f(buf, f);
                }
            }
        }
        _ => unreachable!("encode_number over a non-number"),
    }
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
        // Refused at schema validation, so reaching here means a corrupt or
        // hand-built catalog rather than a user mistake. Ordering ACROSS types
        // is the reason: a key must be memcmp-ordered, and deciding whether
        // Int(5) sorts before Text("a") means inventing a cross-type order.
        // sqlite has one; adopting it would hand back exactly the kind of
        // surprise this project exists to remove. See `Schema::validate`.
        ColumnType::Any => Err(Error::Corrupt(
            "an `any` column cannot be part of a key".into(),
        )),
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
            // Keys are never `any` (Schema::validate refuses it — a key is
            // memcmp-ordered and `any` has no order across types), so the
            // round-trip property this generator feeds does not apply to it.
            ColumnType::Any => unreachable!("`any` cannot be a key column"),
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

    /// Every value a grouping key can meet, including the pairs the whole
    /// exercise is about: `1`/`1.0`/`'1'`, `0`/`-0.0`, and an integer that no
    /// f64 can hold next to the real that rounds to it.
    fn group_key_zoo() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Int(0),
            Value::Float(0.0),
            Value::Float(-0.0),
            Value::Int(1),
            Value::Float(1.0),
            Value::Float(1.5),
            Value::Int(2),
            Value::Int(-1),
            Value::Float(-1.5),
            Value::Int(-2),
            Value::Int(i64::MAX),
            Value::Int(i64::MIN),
            Value::Float(9223372036854775808.0),  // 2^63: above every i64
            Value::Float(-9223372036854775808.0), // -2^63: IS i64::MIN
            Value::Float(-9223372036854777000.0), // below every i64
            Value::Int(9007199254740993),         // not representable as f64
            Value::Float(9007199254740992.0),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(f64::NAN),
            Value::Text("1".into()),
            Value::Text("".into()),
            Value::Text("abc".into()),
            Value::Text("ABC".into()),
            Value::Blob(vec![0x31]),
            Value::Blob(vec![]),
            Value::Bool(false),
            Value::Bool(true),
            Value::Timestamp(0),
            Value::Timestamp(7),
        ]
    }

    /// **The contract of [`encode_group_key`].** Byte order must equal
    /// [`Value::sort_cmp`] wherever that answers, and bytes must be equal
    /// exactly when the two values are one group (sort_cmp `Equal`, or two
    /// NULLs). This is what keeps the hash-keyed paths (DISTINCT dedup) and the
    /// comparator paths (ORDER BY, window peers) from disagreeing.
    #[test]
    fn group_key_matches_sort_cmp() {
        let zoo = group_key_zoo();
        for a in &zoo {
            for b in &zoo {
                let ord = encode_group_key(std::slice::from_ref(a), &[])
                    .cmp(&encode_group_key(std::slice::from_ref(b), &[]));
                match a.sort_cmp(b, Collation::Binary) {
                    Some(o) => assert_eq!(ord, o, "order mismatch: {a:?} vs {b:?}"),
                    // `None` = NULL involved, or an mpedb-native pair sort_cmp
                    // calls peers. Only two NULLs may share a key.
                    None => {
                        let both_null = a.is_null() && b.is_null();
                        assert_eq!(
                            ord == Ordering::Equal,
                            both_null,
                            "grouping verdict wrong: {a:?} vs {b:?}"
                        );
                    }
                }
            }
        }
    }

    /// The repro that started this: sqlite's `count(DISTINCT v)` over
    /// `1, 1.0, '1'` is 2. The on-disk key says 3 — and is right to, for a
    /// typed column — so the two encoders must differ exactly here.
    #[test]
    fn group_key_folds_int_and_real_but_not_text() {
        let k = |v: Value| encode_group_key(&[v], &[]);
        assert_eq!(k(Value::Int(1)), k(Value::Float(1.0)));
        assert_ne!(k(Value::Int(1)), k(Value::Text("1".into())));
        assert_ne!(k(Value::Text("1".into())), k(Value::Blob(vec![0x31])));
        assert_eq!(k(Value::Int(0)), k(Value::Float(-0.0)));
        // …and the on-disk encoder still separates them.
        assert_ne!(
            encode_key(&[Value::Int(1)]),
            encode_key(&[Value::Float(1.0)])
        );
    }

    /// Collation folds TEXT in a grouping key exactly as it does on disk.
    #[test]
    fn group_key_folds_text_under_collation() {
        let a = encode_group_key(&[Value::Text("ABC".into())], &[Collation::NoCase]);
        let b = encode_group_key(&[Value::Text("abc".into())], &[Collation::NoCase]);
        assert_eq!(a, b);
        assert_ne!(
            encode_group_key(&[Value::Text("ABC".into())], &[]),
            encode_group_key(&[Value::Text("abc".into())], &[])
        );
    }

    /// Composite grouping keys concatenate without ambiguity: the numeric
    /// payload is variable-length (9 or 17 bytes), so the sub-tag has to be
    /// self-delimiting or a two-column key could alias.
    #[test]
    fn group_key_composite_is_unambiguous() {
        let mut seen = std::collections::HashSet::new();
        let zoo = group_key_zoo();
        for a in &zoo {
            for b in &zoo {
                let k = encode_group_key(&[a.clone(), b.clone()], &[]);
                // Two DIFFERENT groups must never produce the same bytes; two
                // equal ones must. Canonicalize by the single-value keys.
                let canon = (
                    encode_group_key(std::slice::from_ref(a), &[]),
                    encode_group_key(std::slice::from_ref(b), &[]),
                );
                if let Some(prev) = seen.replace((k.clone(), canon.clone())) {
                    assert_eq!(prev.1, canon, "composite key aliased: {a:?}, {b:?}");
                }
            }
        }
    }

    /// A randomized cross-check of the same contract over the value generator,
    /// mixing types the way an `any` column does.
    #[test]
    fn group_key_order_is_total_and_matches_sort_cmp_randomized() {
        let types = [
            ColumnType::Int64,
            ColumnType::Float64,
            ColumnType::Text,
            ColumnType::Blob,
        ];
        let mut rng = Rng(0x243f6a8885a308d3);
        for _ in 0..20000 {
            let ta = types[(rng.next() % types.len() as u64) as usize];
            let tb = types[(rng.next() % types.len() as u64) as usize];
            let a = random_value(&mut rng, ta);
            let b = random_value(&mut rng, tb);
            let ord = encode_group_key(std::slice::from_ref(&a), &[])
                .cmp(&encode_group_key(std::slice::from_ref(&b), &[]));
            match a.sort_cmp(&b, Collation::Binary) {
                Some(o) => assert_eq!(ord, o, "order mismatch: {a:?} vs {b:?}"),
                None => assert_eq!(
                    ord == Ordering::Equal,
                    a.is_null() && b.is_null(),
                    "grouping verdict wrong: {a:?} vs {b:?}"
                ),
            }
        }
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
