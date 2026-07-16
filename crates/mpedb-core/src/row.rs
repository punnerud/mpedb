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
/// ⚠ **This materialises the WHOLE row, blob included, and for a large value
/// that buffer is the single most expensive thing in an insert** — 10.1 ms of a
/// 23.5 ms 16 MiB insert (~42%), measured 2026-07-16 with
/// `examples/blob_warm --features leakstat`. It exists only to be handed to
/// `btree::insert`, which copies it straight back out into overflow pages: the
/// blob crosses memory three times (`params!`'s `to_vec` → here → the pages) and
/// two of the three do no work. The cost is the fresh 16 MiB malloc — anonymous
/// pages fault exactly like the file mapping does — plus the memcpy.
///
/// The fix is a scatter-gather payload so the overflow path consumes the row's
/// parts without concatenating them; see task #42. Not a format change — the
/// pages come out byte-for-byte identical.
/// Encode a row whose column `stream_col` is not in `values` yet — it will be
/// streamed in (#43). Returns the head (bitmap + fixed + every OTHER varlen
/// body, concatenated) and the total row length.
///
/// The streamed column must be the LAST varlen column, because the varlen
/// section is laid out in column order and the stream has to land at its end.
/// Callers get an error rather than a silently mis-laid row.
pub fn encode_row_head_for_stream(
    values: &[Value],
    types: &[ColumnType],
    stream_col: usize,
    stream_len: usize,
) -> Result<(Vec<u8>, usize)> {
    let last_varlen = values
        .iter()
        .enumerate()
        .filter(|(_, v)| matches!(v, Value::Text(_) | Value::Blob(_)))
        .map(|(i, _)| i)
        .next_back();
    if last_varlen.is_some_and(|i| i > stream_col) {
        return Err(Error::Unsupported(
            "the streamed column must be the last variable-length column in the row".into(),
        ));
    }
    let (mut head, bodies) = encode_row_parts(values, types)?;
    // `encode_row_parts` recorded stream_col's length as whatever placeholder
    // the caller passed (empty); patch in the real one, and its offset is the
    // end of the varlen section since it is last.
    let nbm = bitmap_len(types.len());
    let off: usize = nbm + types[..stream_col].iter().map(|&t| fixed_width(t)).sum::<usize>();
    let var_off: usize = bodies.iter().map(|b| b.len()).sum();
    if stream_len > u32::MAX as usize {
        return Err(Error::Unsupported("value larger than 4 GiB".into()));
    }
    head[off..off + 4].copy_from_slice(&(var_off as u32).to_le_bytes());
    head[off + 4..off + 8].copy_from_slice(&(stream_len as u32).to_le_bytes());
    for b in bodies {
        head.extend_from_slice(b);
    }
    let total = head.len() + stream_len;
    Ok((head, total))
}

/// The byte length `encode_row` would produce, without encoding anything.
///
/// Lets a caller pick the payload form by size (#42) before paying for either:
/// an inline row wants the flat buffer its leaf cell needs anyway, a spilling
/// one wants the parts. `encode_row` sums the same thing internally, so this is
/// not extra work being invented — it is that sum, hoisted.
pub fn encoded_len(values: &[Value], types: &[ColumnType]) -> usize {
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
    nbm + fixed_total + var_total
}

pub fn encode_row_parts<'a>(
    values: &'a [Value],
    types: &[ColumnType],
) -> Result<(Vec<u8>, Vec<&'a [u8]>)> {
    debug_assert_eq!(values.len(), types.len());
    let nbm = bitmap_len(types.len());
    let fixed_total: usize = types.iter().map(|&t| fixed_width(t)).sum();
    let mut head = vec![0u8; nbm + fixed_total];
    let mut bodies: Vec<&'a [u8]> = Vec::new();
    let mut var_len = 0usize; // running length of the varlen section
    let mut off = nbm;
    for (i, (v, &ty)) in values.iter().zip(types).enumerate() {
        let w = fixed_width(ty);
        match v {
            Value::Null => head[i / 8] |= 1 << (i % 8),
            Value::Int(x) | Value::Timestamp(x) => {
                head[off..off + 8].copy_from_slice(&x.to_le_bytes())
            }
            Value::Float(x) => head[off..off + 8].copy_from_slice(&x.to_bits().to_le_bytes()),
            Value::Bool(x) => head[off] = *x as u8,
            Value::Text(_) | Value::Blob(_) => {
                let bytes: &'a [u8] = match v {
                    Value::Text(s) => s.as_bytes(),
                    Value::Blob(b) => b,
                    _ => unreachable!(),
                };
                if bytes.len() > u32::MAX as usize {
                    return Err(Error::Unsupported("value larger than 4 GiB".into()));
                }
                head[off..off + 4].copy_from_slice(&(var_len as u32).to_le_bytes());
                head[off + 4..off + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
                var_len += bytes.len();
                bodies.push(bytes);
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
    Ok((head, bodies))
}

/// Encode a full row into one contiguous buffer.
///
/// A thin wrapper over [`encode_row_parts`] ON PURPOSE: the two must produce the
/// same bytes, and the only way to guarantee that is for one to be the other.
/// Prefer the parts form for anything that might be large — this one has to
/// materialise the whole row, and for a big value that malloc + memcpy measured
/// **10.1 ms of a 23.5 ms 16 MiB insert (~42%)** before #42 (the fresh heap
/// pages fault exactly like the file mapping does).
pub fn encode_row(values: &[Value], types: &[ColumnType]) -> Result<Vec<u8>> {
    let (mut buf, bodies) = encode_row_parts(values, types)?;
    buf.reserve(bodies.iter().map(|b| b.len()).sum::<usize>());
    for b in bodies {
        buf.extend_from_slice(b);
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
