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
        // An `any` column carries its discriminant HERE, not in the varlen body:
        // a tag byte plus the eight everything else uses (the scalar itself, or
        // the (offset, len) pair when the tag says text/blob).
        //
        // Prefixing the body instead would make it `[tag] ++ bytes`, which does
        // not exist as one contiguous slice to borrow — and `btree::Payload::Parts`
        // (#42) and the streaming insert (#43) both borrow varlen bodies straight
        // out of the caller's `Value`. This layout leaves them untouched, and a
        // rigid column pays nothing for a feature it does not use.
        ColumnType::Any => 9,
        _ => 8,
    }
}

/// The tag byte in an `any` column's fixed slot: which type this VALUE is.
/// Reuses `ColumnType`'s own discriminants so there is one numbering to get
/// wrong, plus 0 for NULL (the bitmap already covers NULL, so 0 should never be
/// read — it is here so a zeroed slot decodes as corrupt rather than as Int(0)).
fn any_tag(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Int(_) => ColumnType::Int64 as u8,
        Value::Float(_) => ColumnType::Float64 as u8,
        Value::Bool(_) => ColumnType::Bool as u8,
        Value::Text(_) => ColumnType::Text as u8,
        Value::Blob(_) => ColumnType::Blob as u8,
        Value::Timestamp(_) => ColumnType::Timestamp as u8,
        Value::List(_) => 0, // rejected by `fits` long before here
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
    let mut off: usize =
        nbm + types[..stream_col].iter().map(|&t| fixed_width(t)).sum::<usize>();
    // An `any` column's slot is [tag][off][len]; `encode_row_parts` already wrote
    // the tag from the placeholder value (which is why the placeholder must be a
    // Blob/Text of the right KIND even though its length is ignored). Step over it.
    if types[stream_col] == ColumnType::Any {
        off += 1;
    }
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
    let mut cursor = nbm;
    for (i, (v, &ty)) in values.iter().zip(types).enumerate() {
        let w = fixed_width(ty);
        // THE TRAP: everything below matches on the VALUE, not the column type.
        // An `any` column's slot is [tag][8], so without this an Int in an `any`
        // column would take the Int arm and write its eight bytes where the tag
        // belongs — a row that decodes as garbage and never fails a type check.
        // Write the tag and shift; the arms then write exactly where they always
        // did, one byte along.
        let slot = if ty == ColumnType::Any && !v.is_null() {
            head[cursor] = any_tag(v);
            cursor + 1
        } else {
            cursor
        };
        let off = slot; // shadow for the arms; the real cursor advances by `w` below
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
        cursor += w;
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
/// Bytes of the row image the header occupies: null bitmap + fixed section.
/// This prefix is all [`varlen_window`] needs — the chunked blob reader
/// (#50 B4) reads it once and never materializes the varlen body.
pub fn head_len(types: &[ColumnType]) -> usize {
    bitmap_len(types.len()) + types.iter().map(|&t| fixed_width(t)).sum::<usize>()
}

/// The byte window of varlen column `col` inside the FULL row image:
/// `Ok(None)` for NULL. `head` must hold at least [`head_len`] bytes (a
/// prefix of the row image). Rigid `text`/`blob` columns only — an `any`
/// column's payload layout is per value, and the reader refuses it by name.
pub fn varlen_window(
    head: &[u8],
    types: &[ColumnType],
    col: usize,
) -> Result<Option<(u64, u64)>> {
    let err = || Error::Corrupt("truncated row head".into());
    let ty = *types
        .get(col)
        .ok_or_else(|| Error::Internal(format!("column {col} out of range")))?;
    if !matches!(ty, ColumnType::Text | ColumnType::Blob) {
        return Err(Error::Unsupported(format!(
            "chunked reads need a rigid text/blob column, got {ty}"
        )));
    }
    let bm_byte = *head.get(col / 8).ok_or_else(err)?;
    if bm_byte & (1 << (col % 8)) != 0 {
        return Ok(None);
    }
    let mut off = bitmap_len(types.len());
    for &t in &types[..col] {
        off += fixed_width(t);
    }
    let raw = head.get(off..off + 8).ok_or_else(err)?;
    let var_off = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as u64;
    let len = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as u64;
    Ok(Some((head_len(types) as u64 + var_off, len)))
}

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
    // An `any` column's tag says what THIS value is; decode it as that type
    // from the eight bytes after the tag. Everything below is the rigid path,
    // untouched.
    let (ty, off) = if ty == ColumnType::Any {
        let tag = *buf.get(off).ok_or_else(err)?;
        let t = ColumnType::from_tag(tag).filter(|t| *t != ColumnType::Any).ok_or_else(|| {
            // 0 is NULL's tag, and the null bitmap already returned above — so a
            // 0 here means a zeroed slot, not a null value. Say so rather than
            // decoding it as Int(0).
            Error::Corrupt(format!("invalid `any` value tag {tag:#x} in row"))
        })?;
        (t, off + 1)
    } else {
        (ty, off)
    };
    match ty {
        // Unreachable: rewritten above. Kept explicit so adding a type to
        // ColumnType makes this fail to compile rather than silently fall through.
        ColumnType::Any => Err(Error::Internal("`any` tag resolved to `any`".into())),
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
    decode_row_masked(buf, types, None)
}

/// [`decode_row`] with decode-time column pruning — the scan-level half of
/// #125's width analysis. `keep[i]` false yields `Value::Null` WITHOUT
/// touching the column's bytes, and the row is truncated to `keep.len()`
/// slots (so a `count(*)`'s all-false mask decodes an EMPTY row and the only
/// work left is the walk itself). `None` keeps every column: byte-identical
/// to the per-column path, differential-pinned by `roundtrip_mixed_row`.
///
/// One pass on purpose. The per-column [`decode_column`] recomputes its
/// offset by summing `types[..col]` and the fixed-section end by summing ALL
/// of `types` — O(ncols²) per row, measured at 170 of the 293 ns/row a
/// `SELECT count(*)` fold cost (examples/agg_prof.rs). Here the offset is
/// carried forward and the fixed end computed once.
pub fn decode_row_masked(
    buf: &[u8],
    types: &[ColumnType],
    keep: Option<&[bool]>,
) -> Result<Vec<Value>> {
    let err = || Error::Corrupt("truncated row".into());
    let n_out = keep.map_or(types.len(), |k| k.len().min(types.len()));
    let nbm = bitmap_len(types.len());
    // Needed only by varlen columns, but O(ncols) once per row is noise —
    // and a masked decode with no varlen column never reads past the head.
    let fixed_end = nbm + types.iter().map(|&t| fixed_width(t)).sum::<usize>();
    let mut out = Vec::with_capacity(n_out);
    let mut off = nbm;
    for (i, &ty) in types.iter().enumerate().take(n_out) {
        let w = fixed_width(ty);
        if keep.is_some_and(|k| !k[i]) {
            out.push(Value::Null);
            off += w;
            continue;
        }
        let bm_byte = *buf.get(i / 8).ok_or_else(err)?;
        if bm_byte & (1 << (i % 8)) != 0 {
            out.push(Value::Null);
            off += w;
            continue;
        }
        // An `any` column's tag says what THIS value is — the same resolution
        // `decode_column` performs, including the zeroed-slot refusal.
        let (vty, voff) = if ty == ColumnType::Any {
            let tag = *buf.get(off).ok_or_else(err)?;
            let t = ColumnType::from_tag(tag)
                .filter(|t| *t != ColumnType::Any)
                .ok_or_else(|| {
                    Error::Corrupt(format!("invalid `any` value tag {tag:#x} in row"))
                })?;
            (t, off + 1)
        } else {
            (ty, off)
        };
        out.push(match vty {
            ColumnType::Any => {
                return Err(Error::Internal("`any` tag resolved to `any`".into()))
            }
            ColumnType::Bool => match *buf.get(voff).ok_or_else(err)? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return Err(Error::Corrupt("invalid bool in row".into())),
            },
            ColumnType::Int64 | ColumnType::Timestamp => {
                let raw = buf.get(voff..voff + 8).ok_or_else(err)?;
                let x = i64::from_le_bytes(raw.try_into().unwrap());
                if vty == ColumnType::Int64 {
                    Value::Int(x)
                } else {
                    Value::Timestamp(x)
                }
            }
            ColumnType::Float64 => {
                let raw = buf.get(voff..voff + 8).ok_or_else(err)?;
                Value::Float(f64::from_bits(u64::from_le_bytes(raw.try_into().unwrap())))
            }
            ColumnType::Text | ColumnType::Blob => {
                let raw = buf.get(voff..voff + 8).ok_or_else(err)?;
                let var_off = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
                let len = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
                let start = fixed_end.checked_add(var_off).ok_or_else(err)?;
                let bytes = buf
                    .get(start..start.checked_add(len).ok_or_else(err)?)
                    .ok_or_else(err)?;
                if vty == ColumnType::Text {
                    Value::Text(
                        std::str::from_utf8(bytes)
                            .map_err(|_| Error::Corrupt("invalid utf-8 in row".into()))?
                            .to_owned(),
                    )
                } else {
                    Value::Blob(bytes.to_vec())
                }
            }
        });
        off += w;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `any` column round-trips every scalar kind, and the tag decides —
    /// not the column.
    #[test]
    fn any_column_roundtrips_every_kind() {
        let types = [ColumnType::Int64, ColumnType::Any, ColumnType::Text];
        for v in [
            Value::Int(-42),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Bool(false),
            Value::Text("hei".into()),
            Value::Blob(vec![0, 1, 255]),
            Value::Timestamp(1_700_000_000_000_000),
            Value::Null,
        ] {
            let row = [Value::Int(7), v.clone(), Value::Text("tail".into())];
            let buf = encode_row(&row, &types).unwrap();
            // every column, not just the loose one: the `any` slot is 9 bytes
            // wide, so a wrong width silently shifts its NEIGHBOURS
            assert_eq!(decode_column(&buf, &types, 0).unwrap(), Value::Int(7), "{v:?}");
            assert_eq!(decode_column(&buf, &types, 1).unwrap(), v, "any col: {v:?}");
            assert_eq!(
                decode_column(&buf, &types, 2).unwrap(),
                Value::Text("tail".into()),
                "column AFTER the any slot: {v:?}"
            );
        }
    }

    /// `encode_row` matches on the VALUE, so an Int in an `any` column would
    /// take the Int arm and write its eight bytes where the tag belongs. That
    /// row decodes as garbage and never fails a type check — the exact trap this
    /// feature had to avoid. Pin it.
    #[test]
    fn any_column_writes_the_tag_not_the_bare_scalar() {
        let types = [ColumnType::Any];
        let buf = encode_row(&[Value::Int(1)], &types).unwrap();
        // bitmap(1) then the any slot: tag first
        assert_eq!(buf[1], ColumnType::Int64 as u8, "tag byte missing");
        assert_eq!(buf.len(), 1 + 9, "any slot must be 9 bytes");
        assert_eq!(decode_column(&buf, &types, 0).unwrap(), Value::Int(1));
    }

    /// A zeroed `any` slot must be Corrupt, not Int(0). The null bitmap already
    /// covers NULL, so tag 0 can only mean a slot nobody wrote.
    #[test]
    fn zeroed_any_tag_is_corrupt_not_int_zero() {
        let types = [ColumnType::Any];
        let mut buf = encode_row(&[Value::Int(0)], &types).unwrap();
        buf[1] = 0; // clobber the tag
        assert!(matches!(
            decode_column(&buf, &types, 0),
            Err(Error::Corrupt(_))
        ));
    }

    /// Truncation at every offset must be `Corrupt`, never a panic — the house
    /// rule for every decoder, and the `any` slot adds a new width to get wrong.
    #[test]
    fn truncated_any_row_never_panics() {
        let types = [ColumnType::Any, ColumnType::Any];
        let row = [Value::Text("abc".into()), Value::Int(9)];
        let full = encode_row(&row, &types).unwrap();
        for cut in 0..full.len() {
            for col in 0..types.len() {
                // must not panic; wrong-but-typed answers are fine, panics are not
                let _ = decode_column(&full[..cut], &types, col);
            }
        }
    }

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
