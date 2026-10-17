//! Bounded exact-decimal assignment for local Oracle NUMBER(p,s) variables.
use super::{plsql_number, PValue};
use mpedb_types::{Error, Result, Value};
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Signed, Zero};

pub(super) fn number(value: PValue, precision: u8, scale: i16) -> Result<Value> {
    let Value::Numeric(text) = plsql_number::number(value)? else {
        return Ok(Value::Null);
    };
    if matches!(text.as_str(), "Infinity" | "-Infinity") {
        return Err(Error::ArithmeticOverflow);
    }
    let negative = text.starts_with('-');
    let unsigned = text.trim_start_matches('-');
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    // NUMBER normalization bounds the input to Oracle's exponent/precision;
    // all subsequent powers are bounded by the validated scale limits.
    let mut coefficient: BigInt = format!("{whole}{fraction}")
        .parse()
        .map_err(|_| Error::Corrupt("invalid normalized Oracle NUMBER".into()))?;
    let shift = fraction.len() as i32 - i32::from(scale);
    if shift > 0 {
        let divisor = BigInt::from(10).pow(shift as u32);
        let (mut quotient, remainder) = coefficient.div_rem(&divisor);
        if remainder * 2 >= divisor {
            quotient += 1;
        }
        coefficient = quotient;
    } else if shift < 0 {
        coefficient *= BigInt::from(10).pow((-shift) as u32);
    }
    if coefficient.to_string().len() > precision as usize {
        return Err(Error::ArithmeticOverflow);
    }
    if coefficient.is_zero() {
        return Ok(Value::Numeric("0".into()));
    }
    let digits = coefficient.abs().to_string();
    let unsigned = if scale < 0 {
        format!("{digits}{}", "0".repeat((-scale) as usize))
    } else if scale == 0 {
        digits
    } else if digits.len() <= scale as usize {
        format!("0.{}{digits}", "0".repeat(scale as usize - digits.len()))
    } else {
        let split = digits.len() - scale as usize;
        format!("{}.{}", &digits[..split], &digits[split..])
    };
    let rendered = if negative {
        format!("-{unsigned}")
    } else {
        unsigned
    };
    Ok(Value::Numeric(mpedb_types::value::canonical_numeric(
        &rendered,
    )))
}
