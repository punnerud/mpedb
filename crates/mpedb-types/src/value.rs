use crate::error::{Error, Result};
use std::cmp::Ordering;
use std::fmt;

/// Column types. Rigid by default and by design: unlike sqlite, a column only
/// ever stores its declared type (or NULL where permitted), and writes with the
/// wrong type are rejected — that is the dev/prod parity this project exists for.
///
/// [`ColumnType::Any`] opts a SINGLE column out of that, sqlite-affinity style.
/// It is per column on purpose: "rigid schema" is the product, and making it a
/// database-wide switch would turn a property you can rely on into one you have
/// to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ColumnType {
    Int64 = 1,
    Float64 = 2,
    Bool = 3,
    Text = 4,
    Blob = 5,
    /// Microseconds since the Unix epoch, UTC.
    Timestamp = 6,
    /// Any scalar, decided per VALUE rather than per column (sqlite affinity).
    ///
    /// The discriminant lives in the row's FIXED section, not the varlen body —
    /// `row::fixed_width(Any) == 9`, a tag byte plus the eight the other types
    /// use. That is not an arbitrary layout choice: prefixing the body with a tag
    /// would make it `[tag] ++ bytes`, which does not exist as one contiguous
    /// slice to borrow, and both `btree::Payload::Parts` (#42) and the streaming
    /// insert (#43) borrow varlen bodies straight out of the caller's `Value`.
    /// Keeping the tag in the fixed slot leaves those untouched, and a rigid
    /// column pays nothing for a feature it does not use.
    Any = 7,
}

impl ColumnType {
    pub fn from_tag(tag: u8) -> Option<ColumnType> {
        Some(match tag {
            1 => ColumnType::Int64,
            2 => ColumnType::Float64,
            3 => ColumnType::Bool,
            4 => ColumnType::Text,
            5 => ColumnType::Blob,
            6 => ColumnType::Timestamp,
            7 => ColumnType::Any,
            _ => return None,
        })
    }

    pub fn parse(name: &str) -> Option<ColumnType> {
        Some(match name {
            "int64" | "int" | "integer" => ColumnType::Int64,
            "float64" | "float" | "real" | "double" => ColumnType::Float64,
            "bool" | "boolean" => ColumnType::Bool,
            "text" | "string" => ColumnType::Text,
            "blob" | "bytes" => ColumnType::Blob,
            "timestamp" => ColumnType::Timestamp,
            "any" => ColumnType::Any,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            ColumnType::Int64 => "int64",
            ColumnType::Float64 => "float64",
            ColumnType::Bool => "bool",
            ColumnType::Text => "text",
            ColumnType::Blob => "blob",
            ColumnType::Timestamp => "timestamp",
            ColumnType::Any => "any",
        }
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A single SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Blob(Vec<u8>),
    /// Microseconds since the Unix epoch, UTC.
    Timestamp(i64),
    /// **A session-context list — a parameter value only, never a stored one**
    /// (design/DESIGN-MULTIDB.md §2.6). It exists so `col IN (current_setting('k'))`
    /// can bind a variable-length membership set to ONE reserved slot: the
    /// arity lives in the data, not the plan bytes, so the plan hash stays
    /// context-independent and one plan still serves every session (§4.1).
    ///
    /// There is deliberately no `ColumnType::List`: a list has no column to be
    /// stored in, no key encoding, and no ordering. Every path that would need
    /// one rejects it — `column_type()` returns `None`-like behaviour via
    /// `fits`, `sql_cmp` refuses it, and the row/key codecs error rather than
    /// inventing a representation. The ONLY thing it supports is membership.
    List(Vec<Value>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The column type this value stores into, or `None` for NULL.
    pub fn column_type(&self) -> Option<ColumnType> {
        Some(match self {
            Value::Null => return None,
            Value::Int(_) => ColumnType::Int64,
            Value::Float(_) => ColumnType::Float64,
            Value::Bool(_) => ColumnType::Bool,
            Value::Text(_) => ColumnType::Text,
            Value::Blob(_) => ColumnType::Blob,
            Value::Timestamp(_) => ColumnType::Timestamp,
            // A list is not storable, so it has no column type. `fits` uses this
            // to reject it from every column, which is what we want: the only
            // legal home for a List is a context param slot.
            Value::List(_) => return None,
        })
    }

    /// Whether this value may be stored in a column of type `ty`
    /// (NULL is accepted here; nullability is checked separately).
    /// Whether this value may be stored in a `ty` column.
    ///
    /// NULL fits anything (nullability is checked separately), and `Any` accepts
    /// anything — that is what it is for. Everything else must match exactly:
    /// mpedb does not convert, because a conversion that succeeds locally and
    /// fails in production is the whole problem this project is aimed at.
    pub fn fits(&self, ty: ColumnType) -> bool {
        if ty == ColumnType::Any {
            // ...except a context list, which is param-only (DESIGN-MULTIDB
            // §2.6) and has no encoding in any column, loose or not.
            return !matches!(self, Value::List(_));
        }
        match self.column_type() {
            None => true,
            Some(t) => t == ty,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self.column_type() {
            None => "null",
            Some(t) => t.name(),
        }
    }

    /// SQL comparison. Returns `None` if either side is NULL (three-valued
    /// logic); errors on cross-type comparison (the binder inserts explicit
    /// coercions, so a runtime mix is a bug or a corrupt plan blob).
    ///
    /// Text and blob compare bytewise (binary collation). Floats use IEEE
    /// total order with -0.0 == 0.0 and all NaNs equal, matching the key
    /// encoding in [`crate::keycode`].
    pub fn sql_cmp(&self, other: &Value) -> Result<Option<Ordering>> {
        use Value::*;
        Ok(Some(match (self, other) {
            (Null, _) | (_, Null) => return Ok(None),
            (Int(a), Int(b)) => a.cmp(b),
            (Float(a), Float(b)) => float_total_cmp(*a, *b),
            (Bool(a), Bool(b)) => a.cmp(b),
            (Text(a), Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Blob(a), Blob(b)) => a.cmp(b),
            (Timestamp(a), Timestamp(b)) => a.cmp(b),
            // Lists have no ordering and comparing one is always a bug in the
            // caller, not a NULL: say so rather than silently yielding NULL,
            // which in a policy predicate would read as "row not visible" and
            // hide the mistake.
            (List(_), _) | (_, List(_)) => {
                return Err(Error::TypeMismatch(
                    "a context list supports only `IN` membership, not comparison".into(),
                ))
            }
            (a, b) => {
                return Err(Error::TypeMismatch(format!(
                    "cannot compare {} with {}",
                    a.type_name(),
                    b.type_name()
                )))
            }
        }))
    }

    /// SQL comparison under an explicit collating sequence (task: COLLATE).
    ///
    /// Collation affects TEXT–TEXT comparison ONLY (sqlite's rule): every other
    /// type — and any NULL — falls straight through to [`Value::sql_cmp`], so a
    /// numeric or blob comparison is never perturbed by a stray `COLLATE`. For
    /// two texts the bytes are ordered by `coll`. [`Collation::Binary`] is
    /// byte-identical to `sql_cmp`, so a Binary-tagged comparison and an
    /// untagged one can never disagree.
    pub fn sql_cmp_collated(&self, other: &Value, coll: Collation) -> Result<Option<Ordering>> {
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => Ok(Some(coll.compare_str(a, b))),
            _ => self.sql_cmp(other),
        }
    }
}

/// A collating sequence: how two TEXT values are ordered for comparison and
/// sorting. mpedb ships sqlite's three built-ins and nothing else; the tag is
/// carried in plan bytes (comparison [`Instr`](crate::Instr)s and ORDER BY
/// keys), so it is a closed enum with a stable wire tag like
/// [`ColumnType`]/`ScalarFn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Collation {
    /// Compare by memcmp of the raw UTF-8 bytes — mpedb's native order and the
    /// keycode order. The default when no `COLLATE` is in force.
    #[default]
    Binary = 0,
    /// Case-insensitive, but ONLY for the 26 ASCII letters (sqlite does NOT
    /// casefold Unicode): each byte in `A'..='Z'` is folded to lowercase before
    /// comparison, everything else compared as-is.
    NoCase = 1,
    /// Like [`Collation::Binary`] but trailing ASCII spaces (`0x20`) are ignored
    /// on both sides: `'abc'` == `'abc   '`.
    Rtrim = 2,
}

impl Collation {
    /// Decode a wire tag; `None` (→ `Corrupt`) for an unknown byte.
    pub fn from_tag(t: u8) -> Option<Collation> {
        Some(match t {
            0 => Collation::Binary,
            1 => Collation::NoCase,
            2 => Collation::Rtrim,
            _ => return None,
        })
    }

    /// The SQL name, as written after `COLLATE` and rendered by EXPLAIN.
    pub fn name(self) -> &'static str {
        match self {
            Collation::Binary => "BINARY",
            Collation::NoCase => "NOCASE",
            Collation::Rtrim => "RTRIM",
        }
    }

    /// Resolve a collation name (case-insensitive), or `None` if unknown.
    pub fn parse(name: &str) -> Option<Collation> {
        if name.eq_ignore_ascii_case("BINARY") {
            Some(Collation::Binary)
        } else if name.eq_ignore_ascii_case("NOCASE") {
            Some(Collation::NoCase)
        } else if name.eq_ignore_ascii_case("RTRIM") {
            Some(Collation::Rtrim)
        } else {
            None
        }
    }

    /// Order two strings under this collation. `Binary` is exactly
    /// `a.as_bytes().cmp(b.as_bytes())`.
    pub fn compare_str(self, a: &str, b: &str) -> Ordering {
        match self {
            Collation::Binary => a.as_bytes().cmp(b.as_bytes()),
            Collation::NoCase => nocase_cmp(a.as_bytes(), b.as_bytes()),
            Collation::Rtrim => a
                .trim_end_matches(' ')
                .as_bytes()
                .cmp(b.trim_end_matches(' ').as_bytes()),
        }
    }
}

/// sqlite NOCASE: fold each ASCII uppercase byte to lowercase and compare the
/// folded byte streams, breaking a tie on length. Bytes outside `A'..='Z'`
/// (including all non-ASCII UTF-8 continuation bytes) are compared unchanged —
/// which is exactly why NOCASE does not casefold Unicode.
fn nocase_cmp(a: &[u8], b: &[u8]) -> Ordering {
    #[inline]
    fn fold(x: u8) -> u8 {
        if x.is_ascii_uppercase() {
            x + 32
        } else {
            x
        }
    }
    let n = a.len().min(b.len());
    for i in 0..n {
        let c = fold(a[i]).cmp(&fold(b[i]));
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// Total order over f64 matching the memcmp key encoding: -0.0 and 0.0 are
/// equal, all NaNs are equal and sort above +inf.
pub fn float_total_cmp(a: f64, b: f64) -> Ordering {
    normalize_float_bits(a).cmp(&normalize_float_bits(b))
}

/// Order-preserving u64 image of an f64: flips the sign bit for positives and
/// all bits for negatives, after canonicalizing -0.0 and NaN.
pub fn normalize_float_bits(v: f64) -> u64 {
    let v = if v == 0.0 { 0.0 } else { v }; // -0.0 -> 0.0
    let bits = if v.is_nan() { f64::NAN.to_bits() } else { v.to_bits() };
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{v:?}"),
            Value::Bool(v) => f.write_str(if *v { "true" } else { "false" }),
            Value::List(items) => {
                f.write_str("(")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str(")")
            }
            Value::Text(v) => write!(f, "'{}'", v.replace('\'', "''")),
            Value::Blob(v) => {
                f.write_str("x'")?;
                for b in v {
                    write!(f, "{b:02x}")?;
                }
                f.write_str("'")
            }
            Value::Timestamp(v) => write!(f, "timestamp({v})"),
        }
    }
}

/// Deterministic (non-ordered) serialization of a value, used inside plan
/// blobs and schema canonicalization. Length-prefixed, bounds-checked decode.
pub fn write_value(buf: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => buf.push(0),
        Value::Int(x) => {
            buf.push(1);
            buf.extend_from_slice(&x.to_le_bytes());
        }
        Value::Float(x) => {
            buf.push(2);
            buf.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        Value::Bool(x) => {
            buf.push(3);
            buf.push(*x as u8);
        }
        Value::Text(s) => {
            buf.push(4);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Blob(b) => {
            buf.push(5);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Timestamp(x) => {
            buf.push(6);
            buf.extend_from_slice(&x.to_le_bytes());
        }
        // A context list DOES have to serialize: the intent ring encodes params
        // with this function (ring_exec::encode_params) and context values are
        // params, so without this `col IN (current_setting(..))` would work
        // alone and break the moment a second writer contended. Nested lists are
        // impossible by construction (Session::set_list takes scalars), but the
        // encoding is recursive anyway so a decoder can never be surprised.
        Value::List(items) => {
            buf.push(7);
            buf.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for it in items {
                write_value(buf, it);
            }
        }
    }
}

/// Decode a value written by [`write_value`], advancing `*pos`. All reads are
/// bounds-checked so corrupt/hostile input yields `Error::Corrupt`, never a
/// panic or out-of-bounds access.
pub fn read_value(buf: &[u8], pos: &mut usize) -> Result<Value> {
    fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
        let end = pos
            .checked_add(n)
            .filter(|&e| e <= buf.len())
            .ok_or_else(|| Error::Corrupt("truncated value".into()))?;
        let s = &buf[*pos..end];
        *pos = end;
        Ok(s)
    }
    let tag = take(buf, pos, 1)?[0];
    Ok(match tag {
        0 => Value::Null,
        1 => Value::Int(i64::from_le_bytes(take(buf, pos, 8)?.try_into().unwrap())),
        2 => Value::Float(f64::from_bits(u64::from_le_bytes(
            take(buf, pos, 8)?.try_into().unwrap(),
        ))),
        3 => Value::Bool(match take(buf, pos, 1)?[0] {
            0 => false,
            1 => true,
            _ => return Err(Error::Corrupt("invalid bool".into())),
        }),
        4 => {
            let len = u32::from_le_bytes(take(buf, pos, 4)?.try_into().unwrap()) as usize;
            let bytes = take(buf, pos, len)?;
            Value::Text(
                std::str::from_utf8(bytes)
                    .map_err(|_| Error::Corrupt("invalid utf-8 in text value".into()))?
                    .to_owned(),
            )
        }
        5 => {
            let len = u32::from_le_bytes(take(buf, pos, 4)?.try_into().unwrap()) as usize;
            Value::Blob(take(buf, pos, len)?.to_vec())
        }
        6 => Value::Timestamp(i64::from_le_bytes(take(buf, pos, 8)?.try_into().unwrap())),
        7 => {
            let n = u32::from_le_bytes(take(buf, pos, 4)?.try_into().unwrap()) as usize;
            // A hostile length must not pre-allocate: each element is decoded
            // (and bounds-checked) before the next, so a lie about `n` runs out
            // of buffer instead of out of memory.
            let mut items = Vec::new();
            for _ in 0..n {
                let v = read_value(buf, pos)?;
                // Reject nesting on the way IN, so nothing downstream ever has
                // to reason about a list of lists.
                if matches!(v, Value::List(_)) {
                    return Err(Error::Corrupt("nested context list".into()));
                }
                items.push(v);
            }
            Value::List(items)
        }
        _ => return Err(Error::Corrupt(format!("invalid value tag {tag}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_variants() {
        let values = vec![
            Value::Null,
            Value::Int(-42),
            Value::Int(i64::MIN),
            Value::Float(3.75),
            Value::Float(f64::NEG_INFINITY),
            Value::Bool(true),
            Value::Text("hløl \0 zero".into()),
            Value::Blob(vec![0, 255, 0, 1]),
            Value::Timestamp(1_720_000_000_000_000),
        ];
        let mut buf = Vec::new();
        for v in &values {
            write_value(&mut buf, v);
        }
        let mut pos = 0;
        for v in &values {
            assert_eq!(&read_value(&buf, &mut pos).unwrap(), v);
        }
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn truncated_input_is_error_not_panic() {
        let mut buf = Vec::new();
        write_value(&mut buf, &Value::Text("hello".into()));
        for cut in 0..buf.len() {
            assert!(read_value(&buf[..cut], &mut 0).is_err());
        }
    }

    #[test]
    fn float_order_semantics() {
        assert_eq!(float_total_cmp(0.0, -0.0), Ordering::Equal);
        assert_eq!(float_total_cmp(f64::NAN, f64::NAN), Ordering::Equal);
        assert_eq!(float_total_cmp(f64::INFINITY, f64::NAN), Ordering::Less);
        assert_eq!(float_total_cmp(-1.0, 1.0), Ordering::Less);
        assert_eq!(
            float_total_cmp(f64::NEG_INFINITY, f64::MIN),
            Ordering::Less
        );
    }
}
