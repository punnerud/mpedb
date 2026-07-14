//! Row payload encoding.
//!
//! Layout: null bitmap (ceil(ncols/8) bytes) → fixed-width section (8 bytes
//! per numeric/timestamp column, 1 per bool, 8 per text/blob as offset+len
//! into the varlen section) → varlen section. Enables decoding a single
//! column without materializing the whole row (filter fast path).

use mpedb_types::{ColumnType, Error, Result, Value};

fn fixed_width(ty: ColumnType) -> usize {
    match ty {
        ColumnType::Bool => 1,
        _ => 8,
    }
}

fn bitmap_len(ncols: usize) -> usize {
    ncols.div_ceil(8)
}

/// Encode a full row. `values.len()` must equal `types.len()`; type fit and
/// nullability are validated by the caller (engine) beforehand.
pub fn encode_row(values: &[Value], types: &[ColumnType]) -> Result<Vec<u8>> {
    debug_assert_eq!(values.len(), types.len());
    let nbm = bitmap_len(types.len());
    let fixed_total: usize = types.iter().map(|&t| fixed_width(t)).sum();
    let var_total: usize = values
        .iter()
        .map(|v| match v {
            Value::Text(s) => s.len(),
            Value::Blob(b) => b.len(),
            _ => 0,
        })
        .sum();
    let mut buf = vec![0u8; nbm + fixed_total];
    buf.reserve(var_total);
    let mut off = nbm;
    for (i, (v, &ty)) in values.iter().zip(types).enumerate() {
        let w = fixed_width(ty);
        match v {
            Value::Null => buf[i / 8] |= 1 << (i % 8),
            Value::Int(x) | Value::Timestamp(x) => {
                buf[off..off + 8].copy_from_slice(&x.to_le_bytes())
            }
            Value::Float(x) => buf[off..off + 8].copy_from_slice(&x.to_bits().to_le_bytes()),
            Value::Bool(x) => buf[off] = *x as u8,
            Value::Text(_) | Value::Blob(_) => {
                let bytes: &[u8] = match v {
                    Value::Text(s) => s.as_bytes(),
                    Value::Blob(b) => b,
                    _ => unreachable!(),
                };
                if bytes.len() > u32::MAX as usize {
                    return Err(Error::Unsupported("value larger than 4 GiB".into()));
                }
                let var_off = (buf.len() - (nbm + fixed_total)) as u32;
                buf[off..off + 4].copy_from_slice(&var_off.to_le_bytes());
                buf[off + 4..off + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(bytes);
            }
            // A context list is param-only (DESIGN-MULTIDB §2.6): it has no
            // ColumnType, so `fits` rejects it from every column and
            // validate_row fails the row long before encoding. This arm returns
            // an error rather than asserting, because unlike key encoding this
            // signature CAN report it — and a row codec should never be the
            // thing that panics.
            Value::List(_) => {
                return Err(Error::TypeMismatch(
                    "a context list cannot be stored in a column".into(),
                ))
            }
        }
        off += w;
    }
    Ok(buf)
}

/// Decode one column from an encoded row without touching the others.
pub fn decode_column(buf: &[u8], types: &[ColumnType], col: usize) -> Result<Value> {
    let err = || Error::Corrupt("truncated row".into());
    if col >= types.len() {
        return Err(Error::Internal(format!("column {col} out of range")));
    }
    let nbm = bitmap_len(types.len());
    let bm_byte = *buf.get(col / 8).ok_or_else(err)?;
    if bm_byte & (1 << (col % 8)) != 0 {
        return Ok(Value::Null);
    }
    let mut off = nbm;
    for &t in &types[..col] {
        off += fixed_width(t);
    }
    let ty = types[col];
    let fixed_end: usize = nbm + types.iter().map(|&t| fixed_width(t)).sum::<usize>();
    match ty {
        ColumnType::Bool => match *buf.get(off).ok_or_else(err)? {
            0 => Ok(Value::Bool(false)),
            1 => Ok(Value::Bool(true)),
            _ => Err(Error::Corrupt("invalid bool in row".into())),
        },
        ColumnType::Int64 | ColumnType::Timestamp => {
            let raw = buf.get(off..off + 8).ok_or_else(err)?;
            let x = i64::from_le_bytes(raw.try_into().unwrap());
            Ok(if ty == ColumnType::Int64 {
                Value::Int(x)
            } else {
                Value::Timestamp(x)
            })
        }
        ColumnType::Float64 => {
            let raw = buf.get(off..off + 8).ok_or_else(err)?;
            Ok(Value::Float(f64::from_bits(u64::from_le_bytes(
                raw.try_into().unwrap(),
            ))))
        }
        ColumnType::Text | ColumnType::Blob => {
            let raw = buf.get(off..off + 8).ok_or_else(err)?;
            let var_off = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
            let start = fixed_end.checked_add(var_off).ok_or_else(err)?;
            let bytes = buf
                .get(start..start.checked_add(len).ok_or_else(err)?)
                .ok_or_else(err)?;
            if ty == ColumnType::Text {
                Ok(Value::Text(
                    std::str::from_utf8(bytes)
                        .map_err(|_| Error::Corrupt("invalid utf-8 in row".into()))?
                        .to_owned(),
                ))
            } else {
                Ok(Value::Blob(bytes.to_vec()))
            }
        }
    }
}

/// Decode the whole row.
pub fn decode_row(buf: &[u8], types: &[ColumnType]) -> Result<Vec<Value>> {
    (0..types.len())
        .map(|i| decode_column(buf, types, i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_mixed_row() {
        let types = [
            ColumnType::Int64,
            ColumnType::Text,
            ColumnType::Float64,
            ColumnType::Bool,
            ColumnType::Blob,
            ColumnType::Timestamp,
            ColumnType::Text,
        ];
        let row = vec![
            Value::Int(-5),
            Value::Text("hei på deg".into()),
            Value::Null,
            Value::Bool(true),
            Value::Blob(vec![0, 1, 2, 0]),
            Value::Timestamp(1_700_000_000_000_000),
            Value::Null,
        ];
        let enc = encode_row(&row, &types).unwrap();
        assert_eq!(decode_row(&enc, &types).unwrap(), row);
        for (i, expected) in row.iter().enumerate() {
            assert_eq!(&decode_column(&enc, &types, i).unwrap(), expected);
        }
    }

    #[test]
    fn nine_columns_cross_bitmap_byte() {
        let types = [ColumnType::Bool; 9];
        let row: Vec<Value> = (0..9)
            .map(|i| {
                if i % 3 == 0 {
                    Value::Null
                } else {
                    Value::Bool(i % 2 == 0)
                }
            })
            .collect();
        let enc = encode_row(&row, &types).unwrap();
        assert_eq!(decode_row(&enc, &types).unwrap(), row);
    }

    #[test]
    fn truncation_errors_not_panics() {
        let types = [ColumnType::Text, ColumnType::Int64];
        let enc = encode_row(
            &[Value::Text("abcdef".into()), Value::Int(7)],
            &types,
        )
        .unwrap();
        for cut in 0..enc.len() {
            let _ = decode_row(&enc[..cut], &types);
        }
    }
}
