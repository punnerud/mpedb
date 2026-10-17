//! Oracle NUMBER arithmetic with twenty base-100 groups, rounded using the
//! real Oracle 23.26.3 reference measurements. No binary float intermediates.

use super::{tyname, type_err, Op, PValue};
use mpedb_types::{Error, Result, Value};
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Signed, Zero};

pub(super) fn number(value: PValue) -> Result<Value> {
    match value {
        PValue::Scalar(Value::Null) => Ok(Value::Null),
        PValue::Scalar(Value::Int(n)) => Ok(Value::Numeric(n.to_string())),
        PValue::Scalar(Value::Numeric(text)) => Ok(Value::Numeric(normalize(&text)?)),
        other => Err(type_err(format!(
            "PL/SQL NUMBER conversion from {} is not implemented",
            tyname(&other)
        ))),
    }
}

pub(super) fn integer(value: PValue) -> Result<Value> {
    let Value::Numeric(text) = number(value)? else {
        return Ok(Value::Null);
    };
    let negative = text.starts_with('-');
    let unsigned = text.trim_start_matches('-');
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let mut magnitude: i64 = whole.parse().map_err(|_| Error::ArithmeticOverflow)?;
    if fraction.as_bytes().first().is_some_and(|&c| c >= b'5') {
        magnitude = magnitude.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
    }
    let rounded = if negative { -magnitude } else { magnitude };
    if !(i32::MIN as i64..=i32::MAX as i64).contains(&rounded) {
        return Err(Error::ArithmeticOverflow);
    }
    Ok(Value::Int(rounded))
}

pub(super) fn loop_bound(value: PValue) -> Result<Value> {
    match integer(value)? {
        Value::Null => Err(type_err("PL/SQL numeric FOR bounds cannot be NULL")),
        value => Ok(value),
    }
}

fn integer_result(n: i64) -> Result<Value> {
    if !(i32::MIN as i64..=i32::MAX as i64).contains(&n) {
        return Err(Error::ArithmeticOverflow);
    }
    Ok(Value::Int(n))
}

pub(super) fn integer_negate(value: PValue) -> Result<Value> {
    match integer(value)? {
        Value::Null => Ok(Value::Null),
        Value::Int(n) => integer_result(-n),
        _ => unreachable!("PLS_INTEGER normalization"),
    }
}

pub(super) fn integer_binary(op: Op, a: PValue, b: PValue) -> Result<Value> {
    let (Value::Int(a), Value::Int(b)) = (integer(a)?, integer(b)?) else {
        return Ok(Value::Null);
    };
    integer_result(match op {
        Op::OracleIntAdd => a + b,
        Op::OracleIntSub => a - b,
        Op::OracleIntMul => a * b,
        _ => unreachable!("PLS_INTEGER arithmetic dispatch"),
    })
}

struct Decimal {
    coefficient: BigInt,
    scale: u32,
}

fn decimal(value: PValue) -> Result<Option<Decimal>> {
    match number(value)? {
        Value::Null => Ok(None),
        Value::Numeric(text) => {
            // Strip insignificant fractional zeros BEFORE computing powers,
            // so hostile values such as 1.000...[100000 zeros] cannot turn a
            // bounded NUMBER operation into an unbounded exponentiation.
            let text = mpedb_types::value::canonical_numeric(&text);
            let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
            let coefficient = format!("{whole}{fraction}")
                .parse::<BigInt>()
                .map_err(|_| type_err("invalid finite NUMBER representation"))?;
            Ok(Some(Decimal {
                coefficient,
                scale: fraction.len() as u32,
            }))
        }
        _ => unreachable!("NUMBER normalization"),
    }
}

fn pow10(n: u32) -> BigInt {
    BigInt::from(10).pow(n)
}

fn render(coefficient: BigInt, scale: i32) -> String {
    let coefficient = if scale < 0 {
        coefficient * pow10((-scale) as u32)
    } else {
        coefficient
    };
    let digits = coefficient.abs().to_string();
    let scale = scale.max(0) as usize;
    let unsigned = if scale == 0 {
        digits
    } else if digits.len() <= scale {
        format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
    } else {
        let split = digits.len() - scale;
        format!("{}.{}", &digits[..split], &digits[split..])
    };
    let text = if coefficient.is_negative() {
        format!("-{unsigned}")
    } else {
        unsigned
    };
    mpedb_types::value::canonical_numeric(&text)
}

/// Oracle stores twenty base-100 groups: the leading group has one or two
/// decimal digits according to the exponent parity. Round half away from zero.
/// Only the retained (at most forty) digits enter BigInt parsing.
pub(super) fn normalize(input: &str) -> Result<String> {
    let text = mpedb_types::value::parse_numeric(input)
        .filter(|s| !matches!(s.as_str(), "NaN" | "Infinity" | "-Infinity"))
        .ok_or_else(|| type_err("PL/SQL NUMBER requires a finite decimal"))?;
    let unsigned = text.trim_start_matches('-');
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let whole = whole.trim_start_matches('0');
    let exponent = if !whole.is_empty() {
        whole.len() as i32 - 1
    } else if let Some(first) = fraction.find(|c| c != '0') {
        -(first as i32) - 1
    } else {
        return Ok("0".into());
    };
    if exponent < -130 {
        return Ok("0".into());
    }
    if exponent >= 126 {
        return Err(Error::ArithmeticOverflow);
    }
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    let precision = (39 + exponent.rem_euclid(2)) as usize;
    let retained = precision.min(digits.len());
    let mut coefficient: BigInt = digits[..retained]
        .parse()
        .map_err(|_| type_err("invalid NUMBER digits"))?;
    if digits.as_bytes().get(retained).is_some_and(|&d| d >= b'5') {
        coefficient += 1;
    }
    let scale = retained as i32 - 1 - exponent;
    if coefficient.to_string().len() as i32 - scale > 126 {
        return Err(Error::ArithmeticOverflow);
    }
    if text.starts_with('-') {
        coefficient = -coefficient;
    }
    Ok(render(coefficient, scale))
}

fn value(coefficient: BigInt, scale: u32) -> Result<Value> {
    Ok(Value::Numeric(normalize(&render(
        coefficient,
        scale as i32,
    ))?))
}

pub(super) fn negate(input: PValue) -> Result<Value> {
    match decimal(input)? {
        None => Ok(Value::Null),
        Some(d) => value(-d.coefficient, d.scale),
    }
}

pub(super) fn binary(op: Op, a: PValue, b: PValue) -> Result<Value> {
    let (a, b) = (decimal(a)?, decimal(b)?);
    let (Some(a), Some(b)) = (a, b) else {
        return Ok(Value::Null);
    };
    match op {
        Op::OracleAdd | Op::OracleSub => {
            let scale = a.scale.max(b.scale);
            let a = a.coefficient * pow10(scale - a.scale);
            let b = b.coefficient * pow10(scale - b.scale);
            value(if op == Op::OracleAdd { a + b } else { a - b }, scale)
        }
        Op::OracleMul => value(a.coefficient * b.coefficient, a.scale + b.scale),
        Op::OracleDiv => {
            if b.coefficient.is_zero() {
                return Err(Error::DivisionByZero);
            }
            let numerator = a.coefficient * pow10(b.scale);
            let denominator = b.coefficient * pow10(a.scale);
            let mut numerator = numerator;
            let mut denominator = denominator;
            if denominator.is_negative() {
                numerator = -numerator;
                denominator = -denominator;
            }
            let negative = numerator.is_negative();
            numerator = numerator.abs();
            if numerator.is_zero() {
                return Ok(Value::Numeric("0".into()));
            }
            let mut exponent =
                numerator.to_string().len() as i32 - denominator.to_string().len() as i32;
            let below = if exponent >= 0 {
                numerator < &denominator * pow10(exponent as u32)
            } else {
                &numerator * pow10((-exponent) as u32) < denominator
            };
            if below {
                exponent -= 1;
            }
            if exponent < -130 {
                // Unlike literal conversion/multiplication, Oracle division
                // of nonzero operands raises ORA-01426 on underflow.
                return Err(Error::ArithmeticOverflow);
            }
            if exponent >= 126 {
                return Err(Error::ArithmeticOverflow);
            }
            let precision = 39 + exponent.rem_euclid(2);
            let scale = precision - 1 - exponent;
            if scale >= 0 {
                numerator *= pow10(scale as u32);
            } else {
                denominator *= pow10((-scale) as u32);
            }
            let (mut quotient, remainder) = numerator.div_rem(&denominator);
            if remainder * 2 >= denominator {
                quotient += 1;
            }
            if negative {
                quotient = -quotient;
            }
            // Already rounded at the base-100 quantum; normalize catches a
            // carry beyond the NUMBER exponent limit and canonicalizes zeros.
            Ok(Value::Numeric(normalize(&render(quotient, scale))?))
        }
        _ => unreachable!("NUMBER arithmetic dispatch"),
    }
}
