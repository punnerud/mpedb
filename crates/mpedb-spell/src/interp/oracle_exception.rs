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
