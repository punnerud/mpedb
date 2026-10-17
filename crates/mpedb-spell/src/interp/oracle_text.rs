//! Oracle VARCHAR2 in an AL32UTF8 database with binary comparison and BYTE
//! default length semantics. Bounds apply at assignment and parameter edges.

use super::{tyname, type_err, Op, PValue};
use mpedb_types::{Error, Result, Value};
use std::cmp::Ordering;

pub(super) fn varchar(value: PValue, constraint: u16) -> Result<Value> {
    match value {
        PValue::Scalar(Value::Null) => Ok(Value::Null),
        PValue::Scalar(Value::Text(text)) => {
            let limit = (constraint & 0x7fff) as usize;
            let length = if constraint & 0x8000 != 0 {
                text.chars().count()
            } else {
                text.len()
            };
            if text.len() > 32767 || (limit != 0 && length > limit) {
                return Err(type_err(
                    "PL/SQL VARCHAR2 character string buffer too small",
                ));
            }
            Ok(if text.is_empty() {
                Value::Null
            } else {
                Value::Text(text)
            })
        }
        other => Err(type_err(format!(
            "PL/SQL VARCHAR2 conversion from {} is not implemented",
            tyname(&other)
        ))),
    }
}

pub(super) fn binary(op: Op, a: PValue, b: PValue) -> Result<Value> {
    let (a, b) = (varchar(a, 0)?, varchar(b, 0)?);
    if op == Op::OracleConcat {
        let a = match &a {
            Value::Text(s) => s.as_str(),
            _ => "",
        };
        let b = match &b {
            Value::Text(s) => s.as_str(),
            _ => "",
        };
        let length = a.len() + b.len();
        if length > 32767 {
            return Err(type_err(
                "PL/SQL VARCHAR2 concatenation exceeds 32767 bytes",
            ));
        }
        if length == 0 {
            return Ok(Value::Null);
        }
        let mut text = String::new();
        text.try_reserve_exact(length)
            .map_err(|_| Error::OutOfMemory {
                what: "PL/SQL VARCHAR2 concatenation",
            })?;
        text.push_str(a);
        text.push_str(b);
        return Ok(Value::Text(text));
    }
    let (Value::Text(a), Value::Text(b)) = (a, b) else {
        return Ok(Value::Null);
    };
    // Compare virtual U+0020 padding without allocating. Simply trimming
    // spaces would order a prefix incorrectly against a trailing control byte.
    let width = a.len().max(b.len());
    let order = a
        .bytes()
        .chain(std::iter::repeat(b' '))
        .take(width)
        .cmp(b.bytes().chain(std::iter::repeat(b' ')).take(width));
    Ok(Value::Bool(match op {
        Op::OracleCharEq => order == Ordering::Equal,
        Op::OracleCharNe => order != Ordering::Equal,
        Op::OracleCharLt => order == Ordering::Less,
        Op::OracleCharLe => order != Ordering::Greater,
        Op::OracleCharGt => order == Ordering::Greater,
        Op::OracleCharGe => order != Ordering::Less,
        _ => unreachable!("Oracle character comparison dispatch"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Proc;

    #[test]
    fn invalid_character_constraint_is_rejected_in_memory_and_on_decode() {
        let make = |constraint| {
            Proc::new(
                "text_check".into(),
                0,
                0,
                vec![],
                vec![Value::Text("a".into())],
                vec![Op::LoadConst(0), Op::OracleVarchar(constraint), Op::Return],
            )
        };
        assert!(make(0x8000).is_err());
        let p = make(0).unwrap();
        let mut bytes = p.encode();
        let n = bytes.len();
        bytes[n - 3] = 0;
        bytes[n - 2] = 0x80;
        assert!(Proc::decode(&bytes).is_err());
    }
}
