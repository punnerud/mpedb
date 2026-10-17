//! Oracle PL/SQL NUMBER specials, measured through public SQL DUMP and runtime
//! arithmetic on Oracle 23.26.3. The initial anonymous-block arithmetic profile
//! is experimental: the broader function-wrapper matrix has known mismatches.
//! These instructions deliberately differ from IEEE and
//! PostgreSQL numeric. Legacy finite instructions retain their old semantics.

use super::{oracle_number, type_err, Op, PValue};
use mpedb_types::{Error, Result, Value};

pub(super) fn number(value: PValue) -> Result<Value> {
    if let PValue::Scalar(Value::Numeric(text)) = &value {
        let text = mpedb_types::value::parse_numeric(text)
            .ok_or_else(|| type_err("invalid PL/SQL NUMBER"))?;
        if matches!(text.as_str(), "Infinity" | "-Infinity") {
            return Ok(Value::Numeric(text));
        }
    }
    oracle_number::number(value)
}

fn infinity(negative: bool) -> Value {
    Value::Numeric(if negative { "-Infinity" } else { "Infinity" }.into())
}

fn zero() -> Value {
    Value::Numeric("0".into())
}

fn special(value: &Value) -> Option<bool> {
    match value {
        Value::Numeric(s) if s == "Infinity" => Some(false),
        Value::Numeric(s) if s == "-Infinity" => Some(true),
        _ => None,
    }
}

fn negative(value: &Value) -> bool {
    matches!(value, Value::Numeric(s) if s.starts_with('-'))
}

fn is_zero(value: &Value) -> bool {
    matches!(value, Value::Numeric(s) if s == "0")
}

pub(super) fn integer(value: PValue, loop_bound: bool) -> Result<Value> {
    let value = number(value)?;
    if special(&value).is_some() {
        return Err(Error::ArithmeticOverflow);
    }
    if loop_bound {
        oracle_number::loop_bound(PValue::Scalar(value))
    } else {
        oracle_number::integer(PValue::Scalar(value))
    }
}

pub(super) fn negate(value: PValue) -> Result<Value> {
    let value = number(value)?;
    match special(&value) {
        Some(negative) => Ok(infinity(!negative)),
        None => oracle_number::negate(PValue::Scalar(value)),
    }
}

pub(super) fn binary(op: Op, a: PValue, b: PValue) -> Result<Value> {
    let (a, b) = (number(a)?, number(b)?);
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    let (sa, sb) = (special(&a), special(&b));
    if sa.is_some() || sb.is_some() {
        return match op {
            Op::OraclePlSqlAdd => match (sa, sb) {
                // Opposite NUMBER infinities add to negative infinity on
                // the measured Oracle reference, rather than IEEE NaN.
                (Some(a), Some(b)) => Ok(infinity(a || b)),
                (Some(a), _) | (_, Some(a)) => Ok(infinity(a)),
                _ => unreachable!(),
            },
            Op::OraclePlSqlSub => match (sa, sb) {
                (Some(a), Some(b)) if a == b => Ok(zero()),
                (Some(a), _) => Ok(infinity(a)),
                (_, Some(b)) => Ok(infinity(!b)),
                _ => unreachable!(),
            },
            Op::OraclePlSqlMul => {
                if is_zero(&a) || is_zero(&b) {
                    Ok(zero())
                } else {
                    Ok(infinity(negative(&a) ^ negative(&b)))
                }
            }
            Op::OraclePlSqlDivide => {
                if is_zero(&b) {
                    Err(Error::DivisionByZero)
                } else if sa.is_none() {
                    Ok(zero())
                } else {
                    Ok(infinity(negative(&a) ^ negative(&b)))
                }
            }
            _ => unreachable!("PL/SQL NUMBER dispatch"),
        };
    }
    let finite_op = match op {
        Op::OraclePlSqlAdd => Op::OracleAdd,
        Op::OraclePlSqlSub => Op::OracleSub,
        Op::OraclePlSqlMul => Op::OracleMul,
        Op::OraclePlSqlDivide => Op::OraclePlSqlDiv,
        _ => unreachable!("PL/SQL NUMBER dispatch"),
    };
    // Both inputs are normalized finite NUMBER values before this call, so
    // ArithmeticOverflow here means the result overflowed, not an invalid
    // argument that should be silently converted into a special value.
    let sign = match op {
        Op::OraclePlSqlAdd | Op::OraclePlSqlSub => negative(&a),
        _ => negative(&a) ^ negative(&b),
    };
    match oracle_number::binary(finite_op, PValue::Scalar(a), PValue::Scalar(b)) {
        Err(Error::ArithmeticOverflow) => Ok(infinity(sign)),
        answer => answer,
    }
}
