//! Versioned, schema-bound receipt images; source transport never translates
//! data directly to another database's representations.
use super::*;
use crate::adapter::{NetOp, NetOpKind};
use mpedb_core::row;
const LIMIT: usize = 64 * 1024 * 1024;
fn types(table: &OracleTable) -> Result<Vec<ColumnType>> {
    table
        .columns
        .iter()
        .map(|c| {
            c.mapped()
                .ok_or_else(|| Error::Unsupported("unmapped Oracle receipt type".into()))
        })
        .collect()
}
fn same_key(table: &OracleTable, op: &NetOp, values: &[Value]) -> bool {
    op.pk.len() == table.pk.len()
        && !op.pk.contains(&Value::Null)
        && table
            .pk
            .iter()
            .zip(&op.pk)
            .all(|(&k, v)| values.get(k) == Some(v))
}
fn frame(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    if out
        .len()
        .checked_add(8)
        .and_then(|n| n.checked_add(bytes.len()))
        .is_none_or(|n| n > LIMIT)
    {
        return Err(Error::Unsupported(
            "Oracle write receipt exceeds 64 MiB; reduce the batch".into(),
        ));
    }
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}
pub(super) fn payload(tables: &[OracleTable], ops: &[NetOp], domain: &[u8]) -> Result<[u8; 32]> {
    let tables = crate::oracle::carrier_tables(tables)?;
    let mut bytes = b"MPEdb Oracle write payload v1\0".to_vec();
    frame(&mut bytes, domain)?;
    for op in ops {
        let table = tables
            .get(op.table_id as usize)
            .ok_or_else(|| Error::Config("unknown Oracle receipt table".into()))?;
        bytes.extend_from_slice(&op.table_id.to_be_bytes());
        frame(&mut bytes, &mpedb_types::keycode::encode_key_exact(&op.pk))?;
        match &op.kind {
            NetOpKind::Delete => bytes.push(0),
            NetOpKind::Upsert(values) => {
                bytes.push(1);
                if values.len() != table.columns.len() {
                    return Err(Error::Config("Oracle receipt row width mismatch".into()));
                }
                frame(&mut bytes, &row::encode_row(values, &types(table)?)?)?;
            }
        }
    }
    if bytes.len() > LIMIT {
        return Err(Error::Unsupported(
            "Oracle write payload exceeds 64 MiB".into(),
        ));
    }
    Ok(*blake3::hash(&bytes).as_bytes())
}
pub(super) fn encode(
    tables: &[OracleTable],
    ops: &[NetOp],
    images: &[Option<Vec<Value>>],
) -> Result<Vec<u8>> {
    if ops.len() != images.len() || ops.len() > u32::MAX as usize {
        return Err(Error::Config("Oracle receipt result count mismatch".into()));
    }
    let tables = crate::oracle::carrier_tables(tables)?;
    let mut out = vec![1];
    out.extend_from_slice(&(ops.len() as u32).to_be_bytes());
    for (op, image) in ops.iter().zip(images) {
        let table = tables
            .get(op.table_id as usize)
            .ok_or_else(|| Error::Config("unknown Oracle receipt table".into()))?;
        out.extend_from_slice(&op.table_id.to_be_bytes());
        match (&op.kind, image) {
            (NetOpKind::Delete, None) => out.push(0),
            (NetOpKind::Upsert(_), Some(values))
                if values.len() == table.columns.len() && same_key(table, op, values) =>
            {
                out.push(1);
                frame(&mut out, &row::encode_row(values, &types(table)?)?)?;
            }
            _ => {
                return Err(Error::Corrupt(
                    "Oracle receipt result does not match its operation".into(),
                ))
            }
        }
    }
    if out.len() > LIMIT {
        return Err(Error::Unsupported(
            "Oracle write receipt exceeds 64 MiB".into(),
        ));
    }
    Ok(out)
}
pub(super) fn decode(
    tables: &[OracleTable],
    ops: &[NetOp],
    bytes: &[u8],
) -> Result<Vec<Option<Vec<Value>>>> {
    if bytes.len() > LIMIT
        || bytes.len() < 5
        || bytes[0] != 1
        || u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize != ops.len()
    {
        return Err(Error::Corrupt(
            "invalid Oracle receipt version or count".into(),
        ));
    }
    let tables = crate::oracle::carrier_tables(tables)?;
    let mut pos = 5;
    let mut images = Vec::with_capacity(ops.len());
    for op in ops {
        let head = bytes
            .get(pos..pos + 5)
            .ok_or_else(|| Error::Corrupt("truncated Oracle receipt operation".into()))?;
        pos += 5;
        if u32::from_be_bytes(head[..4].try_into().unwrap()) != op.table_id {
            return Err(Error::Corrupt(
                "Oracle receipt table identity mismatch".into(),
            ));
        }
        let table = tables
            .get(op.table_id as usize)
            .ok_or_else(|| Error::Corrupt("unknown Oracle receipt table".into()))?;
        match (&op.kind, head[4]) {
            (NetOpKind::Delete, 0) => images.push(None),
            (NetOpKind::Upsert(_), 1) => {
                let size = bytes
                    .get(pos..pos + 8)
                    .ok_or_else(|| Error::Corrupt("truncated Oracle receipt frame".into()))?;
                pos += 8;
                let size = u64::from_be_bytes(size.try_into().unwrap());
                if size > LIMIT as u64 {
                    return Err(Error::Corrupt("Oracle receipt frame exceeds bounds".into()));
                }
                let data = bytes
                    .get(pos..pos + size as usize)
                    .ok_or_else(|| Error::Corrupt("truncated Oracle receipt image".into()))?;
                pos += size as usize;
                let ty = types(table)?;
                let values = row::decode_row(data, &ty)?;
                if !same_key(table, op, &values) {
                    return Err(Error::Corrupt(
                        "Oracle receipt image has another row identity".into(),
                    ));
                }
                if row::encode_row(&values, &ty)? != data {
                    return Err(Error::Corrupt("noncanonical Oracle receipt image".into()));
                }
                images.push(Some(values));
            }
            _ => {
                return Err(Error::Corrupt(
                    "Oracle receipt operation kind mismatch".into(),
                ))
            }
        }
    }
    if pos != bytes.len() {
        return Err(Error::Corrupt("Oracle receipt has trailing bytes".into()));
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_codec_rejects_truncation_forged_frames_and_other_row_identities() {
        let column = OracleColumn {
            name: "ID".into(),
            data_type: "NUMBER".into(),
            type_owner: None,
            precision: None,
            scale: None,
            data_length: 22,
            char_length: 0,
            char_used: None,
            not_null: true,
            generated: false,
            identity: false,
            default_source: None,
            collation: None,
        };
        let table = OracleTable {
            owner: "APP".into(),
            name: "T".into(),
            object_id: 7,
            ddl_source: "CREATE TABLE T".into(),
            schema_ddl: "CREATE TABLE T".into(),
            columns: vec![column],
            pk: vec![0],
            pk_validated: true,
            temporary: false,
            nested: false,
            external: false,
        };
        let tables = [table];
        let values = vec![Value::Numeric("1".into())];
        let ops = [NetOp {
            table_id: 0,
            pk: values.clone(),
            kind: NetOpKind::Upsert(values.clone()),
        }];
        let images = vec![Some(values)];
        let bytes = encode(&tables, &ops, &images).unwrap();
        assert_eq!(decode(&tables, &ops, &bytes).unwrap(), images);
        for end in 0..bytes.len() {
            assert!(
                decode(&tables, &ops, &bytes[..end]).is_err(),
                "prefix {end}"
            );
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(decode(&tables, &ops, &extra).is_err());
        for index in [0, 4, 5, 9] {
            let mut forged = bytes.clone();
            forged[index] ^= 128;
            assert!(decode(&tables, &ops, &forged).is_err());
        }
        let mut huge = bytes.clone();
        huge[10..18].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(decode(&tables, &ops, &huge).is_err());
        let other = vec![Some(vec![Value::Numeric("2".into())])];
        assert!(encode(&tables, &ops, &other).is_err());
        let other_ops = [NetOp {
            pk: vec![Value::Numeric("2".into())],
            ..ops[0].clone()
        }];
        assert!(decode(&tables, &other_ops, &bytes).is_err());
        assert_ne!(
            payload(&tables, &ops, b"first incarnation").unwrap(),
            payload(&tables, &ops, b"second incarnation").unwrap()
        );
    }
}
