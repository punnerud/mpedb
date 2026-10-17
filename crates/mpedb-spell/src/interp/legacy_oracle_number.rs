//! Legacy format-1 NUMBER instruction semantics. Preserve existing hashes.

//! Exact NUMBER arithmetic over bounded Oracle-range operands. This is distinct
//! from PostgreSQL numeric division and Python/Rust floating-point arithmetic.
//! Oracle's rounding-dependent cases remain explicit refusals until measured
//! against a real Oracle reference server.

use super::{sql_logic, type_err, Op, PValue};
use mpedb_types::{Error, Result, Value};
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{One, Signed, Zero};

struct Decimal {
    coefficient: BigInt,
    scale: u32,
}

fn decimal(value: PValue) -> Result<Option<Decimal>> {
    match sql_logic::number(value)? {
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

fn value(coefficient: BigInt, scale: u32) -> Result<Value> {
    let digits = coefficient.abs().to_string();
    let scale = scale as usize;
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
    sql_logic::number(PValue::Scalar(Value::Numeric(
        mpedb_types::value::canonical_numeric(&text),
    )))
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
        Op::SqlAdd | Op::SqlSub => {
            let scale = a.scale.max(b.scale);
            let a = a.coefficient * pow10(scale - a.scale);
            let b = b.coefficient * pow10(scale - b.scale);
            value(if op == Op::SqlAdd { a + b } else { a - b }, scale)
        }
        Op::SqlMul => value(a.coefficient * b.coefficient, a.scale + b.scale),
        Op::SqlDiv => {
            if b.coefficient.is_zero() {
                return Err(Error::DivisionByZero);
            }
            let numerator = a.coefficient * pow10(b.scale);
            let denominator = b.coefficient * pow10(a.scale);
            let gcd = numerator.gcd(&denominator);
            let mut numerator = numerator / &gcd;
            let mut denominator = denominator / gcd;
            if denominator.is_negative() {
                numerator = -numerator;
                denominator = -denominator;
            }
            let mut twos = 0;
            let mut fives = 0;
            while denominator.is_even() {
                denominator /= 2;
                twos += 1;
            }
            while (&denominator % 5u8).is_zero() {
                denominator /= 5;
                fives += 1;
            }
            if !denominator.is_one() {
                return Err(Error::Unsupported("Oracle NUMBER division requires a rounded recurring decimal; reference-verified Oracle rounding is not implemented".into()));
            }
            let scale = twos.max(fives);
            numerator *= BigInt::from(2).pow(scale - twos);
            numerator *= BigInt::from(5).pow(scale - fives);
            value(numerator, scale)
        }
        _ => unreachable!("NUMBER arithmetic dispatch"),
    }
}
