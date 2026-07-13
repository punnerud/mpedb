use crate::error::{Error, Result};
use std::cmp::Ordering;
use std::fmt;

/// Rigid column types. Unlike sqlite, a column only ever stores its declared
/// type (or NULL where permitted); writes with the wrong type are rejected.
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
        })
    }

    /// Whether this value may be stored in a column of type `ty`
    /// (NULL is accepted here; nullability is checked separately).
    pub fn fits(&self, ty: ColumnType) -> bool {
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
            (a, b) => {
                return Err(Error::TypeMismatch(format!(
                    "cannot compare {} with {}",
                    a.type_name(),
                    b.type_name()
                )))
            }
        }))
    }
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
