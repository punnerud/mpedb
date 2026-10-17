//! Native exception scopes. Only Oracle semantic errors can be handled;
//! host I/O, budgets, corruption and unsupported operations always escape.
use super::*;

pub(super) const MAX_SCOPES: usize = 128;
pub(super) struct Frame {
    pub start: usize,
    pub end: usize,
    pub handler: usize,
    pub handler_end: usize,
}
pub(super) struct Caught {
    pub start: usize,
    pub end: usize,
    pub error: Error,
}
pub(super) fn named(code: u16) -> Error {
    let message = match code {
        1 => "unique constraint violated",
        1403 => "no data found",
        1422 => "exact fetch returns more than requested number of rows",
        1476 => "divisor is equal to zero",
        6502 => "PL/SQL: numeric or value error",
        _ => "PL/SQL exception",
    };
    Error::Oracle {
        code: code as u32,
        message: message.into(),
    }
}
pub(super) fn semantic(error: Error, op: Op) -> std::result::Result<Error, Error> {
    let code = match &error {
        Error::Oracle { .. } => return Ok(error),
        Error::DivisionByZero => 1476,
        Error::PrimaryKeyViolation { .. } | Error::UniqueViolation { .. } => 1,
        Error::NotNullViolation { .. } => 1400,
        Error::CheckViolation { .. } => 2290,
        Error::ArithmeticOverflow
            if matches!(
                op,
                Op::OracleIntAdd
                    | Op::OracleIntSub
                    | Op::OracleIntMul
                    | Op::OracleIntNeg
                    | Op::OraclePlSqlInteger
                    | Op::OraclePlSqlLoopBound
            ) =>
        {
            6502
        }
        Error::TypeMismatch(_)
            if matches!(
                op,
                Op::OracleVarchar(_)
                    | Op::OraclePlSqlNumber
                    | Op::OraclePlSqlInteger
                    | Op::OraclePlSqlLoopBound
                    | Op::SqlBool
            ) =>
        {
            6502
        }
        _ => return Err(error),
    };
    Ok(Error::Oracle {
        code,
        message: error.to_string(),
    })
}

pub(super) fn sqlcode(caught: &[Caught]) -> Result<i64> {
    match caught.last().map(|c| &c.error) {
        None => Ok(0),
        Some(Error::Oracle { code: 1403, .. }) => Ok(100),
        Some(Error::Oracle { code, .. }) => Ok(-i64::from(*code)),
        _ => Err(Error::Corrupt("proc: non-Oracle exception context".into())),
    }
}
pub(super) fn sqlerrm(caught: &[Caught]) -> Result<String> {
    let mut text = match caught.last().map(|c| &c.error) {
        None => "ORA-0000: normal, successful completion".into(),
        // Legacy handlers retain their original error detail. SQLERRM's new
        // operation renders Oracle's canonical divisor message separately.
        Some(Error::Oracle { code: 1476, .. }) => named(1476).to_string(),
        Some(e @ Error::Oracle { .. }) => e.to_string(),
        _ => return Err(Error::Corrupt("proc: non-Oracle exception context".into())),
    };
    let mut end = text.len().min(512);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    Ok(text)
}

pub(super) fn application(code: PValue, message: PValue) -> Result<Error> {
    let code = match code {
        PValue::Scalar(Value::Int(n)) => n,
        PValue::Scalar(Value::Null) => 0,
        _ => return Err(type_err("RAISE_APPLICATION_ERROR requires an integer code")),
    };
    if !(-20999..=-20000).contains(&code) {
        return Ok(Error::Oracle {
            code: 21000,
            message: format!(
                "error number argument to raise_application_error of {code} is out of range"
            ),
        });
    }
    let mut message = match message {
        PValue::Scalar(Value::Text(text)) => text,
        PValue::Scalar(Value::Null) => String::new(),
        _ => {
            return Err(type_err(
                "RAISE_APPLICATION_ERROR requires a character message",
            ))
        }
    };
    // Native 23.26.3 reference: the English error envelope retains at most
    // 2034 message bytes (2045 including ORA prefix), at a UTF-8 boundary.
    let mut end = message.len().min(2034);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    Ok(Error::Oracle {
        code: code.unsigned_abs() as u32,
        message,
    })
}
