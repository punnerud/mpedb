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
        Error::Oracle { .. } | Error::OracleUserException { .. } => return Ok(error),
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
            1426
        }
        Error::ArithmeticOverflow if matches!(op, Op::OracleNumberConstraint { .. }) => 6502,
        Error::TypeMismatch(_)
            if matches!(
                op,
                Op::OracleNumberConstraint { .. }
                    | Op::OracleVarchar(_)
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
        message: if code == 1426 {
            "numeric overflow".into()
        } else {
            error.to_string()
        },
    })
}

pub(super) fn sqlcode(caught: &[Caught]) -> Result<i64> {
    match caught.last().map(|c| &c.error) {
        None => Ok(0),
        Some(Error::OracleUserException { .. }) => Ok(1),
        Some(Error::Oracle { code: 1403, .. }) => Ok(100),
        Some(Error::Oracle { code, .. }) => Ok(-i64::from(*code)),
        _ => Err(Error::Corrupt("proc: non-Oracle exception context".into())),
    }
}
pub(super) fn sqlerrm(caught: &[Caught]) -> Result<String> {
    let mut text = match caught.last().map(|c| &c.error) {
        None => "ORA-0000: normal, successful completion".into(),
        Some(Error::OracleUserException { .. }) => "User-Defined Exception".into(),
        // Legacy handlers retain their original error detail. SQLERRM's new
        // operation renders Oracle's canonical divisor message separately.
        Some(Error::Oracle { code: 1476, .. }) => named(1476).to_string(),
        Some(Error::Oracle { code, .. }) if *code > 65535 => {
            format!(" {code}: non-ORACLE exception ")
        }
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

/// Proc validation checks the constant even when its instruction is unreachable.
pub(super) fn user_identity(proc: &Proc, index: u16) -> [u8; 32] {
    let Value::Blob(bytes) = &proc.consts[index as usize] else {
        unreachable!("validated")
    };
    bytes.as_slice().try_into().expect("validated identity")
}

/// Locally caught large exception codes retain SQLCODE; only an unhandled
/// exception crossing the PL/SQL entry boundary is converted to ORA-06515.
pub(super) fn entry_boundary(error: Error) -> Error {
    match error {
        Error::Oracle { code, .. } if code > 65535 => Error::Oracle {
            code: 6515,
            message: format!("PL/SQL: unhandled exception {code}"),
        },
        other => other,
    }
}

pub(super) fn bound(code: u32) -> Error {
    let message = crate::oracle_exceptions::message(code)
        .unwrap_or("PL/SQL exception (Oracle message catalog entry unavailable)");
    Error::Oracle {
        code,
        message: message.into(),
    }
}

/// Explicit arguments use the active error's detail when the rounded code
/// matches it. Other codes use catalog text; positive non-Oracle values keep
/// Oracle's signed non-ORACLE envelope (including the i32::MIN wrap case).
pub(super) fn explicit(value: PValue, caught: &[Caught]) -> Result<Value> {
    let value = super::plsql_number::integer(value, false).map_err(|error| match error {
        Error::ArithmeticOverflow => bound(1426),
        Error::TypeMismatch(_) => bound(6502),
        other => other,
    })?;
    let code = match value {
        Value::Null => return Ok(Value::Null),
        Value::Int(code) => code as i32,
        _ => unreachable!("integer conversion"),
    };
    let active = sqlcode(caught)?;
    if i64::from(code) == active || (code == -1403 && active == 100) {
        return sqlerrm(caught).map(Value::Text);
    }
    let text = match code {
        0 => "ORA-0000: normal, successful completion".into(),
        1 => "User-Defined Exception".into(),
        100 => bound(1403).to_string(),
        -65535..=-1 => bound(code.unsigned_abs()).to_string(),
        other => format!(" {}: non-ORACLE exception ", other.wrapping_neg()),
    };
    Ok(Value::Text(text))
}
