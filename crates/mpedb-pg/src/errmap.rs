//! mpedb errors → PostgreSQL SQLSTATE codes.
//!
//! This is not cosmetic. Every serious client BRANCHES on SQLSTATE rather than
//! on the message: psycopg raises a different exception class per code, Django
//! turns `23505` into `IntegrityError` and everything else into
//! `DatabaseError`, and SQLAlchemy's retry logic keys on the serialization
//! codes. A server that returns `XX000` (internal error) for a unique-violation
//! makes an ORM report a crash where the application expected a duplicate.
//!
//! So the mapping is by MEANING, and where mpedb has no equivalent concept the
//! code chosen is the one whose CLIENT BEHAVIOUR is right, not the one whose
//! name reads best.

use mpedb_types::Error;

/// `ERROR` severity, the only one mpedb produces for a failed statement.
pub const SEVERITY_ERROR: &str = "ERROR";

/// SQLSTATE for a statement mpedb refused to compile.
///
/// `0A000` is `feature_not_supported`, and it is the honest code for mpedb's
/// named refusals: the SQL is valid PostgreSQL, this server does not implement
/// it. A syntax error proper gets `42601`.
pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
pub const SYNTAX_ERROR: &str = "42601";
pub const UNDEFINED_TABLE: &str = "42P01";
pub const UNDEFINED_COLUMN: &str = "42703";
pub const UNDEFINED_FUNCTION: &str = "42883";
pub const DATATYPE_MISMATCH: &str = "42804";
pub const UNIQUE_VIOLATION: &str = "23505";
pub const NOT_NULL_VIOLATION: &str = "23502";
pub const FOREIGN_KEY_VIOLATION: &str = "23503";
pub const CHECK_VIOLATION: &str = "23514";
pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
pub const DIVISION_BY_ZERO: &str = "22012";
pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
pub const INVALID_PARAMETER_VALUE: &str = "22023";
pub const DISK_FULL: &str = "53100";
pub const OUT_OF_MEMORY: &str = "53200";
pub const QUERY_CANCELED: &str = "57014";
pub const LOCK_NOT_AVAILABLE: &str = "55P03";
pub const SERIALIZATION_FAILURE: &str = "40001";
pub const DATA_CORRUPTED: &str = "XX001";
pub const INTERNAL_ERROR: &str = "XX000";
pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";
pub const INVALID_CURSOR_NAME: &str = "34000";
pub const READ_ONLY_SQL_TRANSACTION: &str = "25006";
pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";
pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
pub const RAISE_EXCEPTION: &str = "P0001";
pub const PROTOCOL_VIOLATION: &str = "08P01";
pub const TOO_MANY_CONNECTIONS: &str = "53300";
pub const IO_ERROR: &str = "58030";

/// The SQLSTATE and message for an mpedb error.
pub fn sqlstate(e: &Error) -> (&'static str, String) {
    let msg = e.to_string();
    let code = match e {
        // ---- constraint violations, the codes ORMs branch on ---------------
        Error::UniqueViolation { .. } | Error::PrimaryKeyViolation { .. } => UNIQUE_VIOLATION,
        Error::NotNullViolation { .. } => NOT_NULL_VIOLATION,
        Error::CheckViolation { .. } => CHECK_VIOLATION,
        Error::ForeignKeyViolation { .. } => FOREIGN_KEY_VIOLATION,
        // An RLS refusal is not a constraint failure: the row exists (or would
        // have), and the caller is not allowed to see or write it. PG reports
        // its own policy violations under insufficient_privilege.
        Error::PolicyViolation { .. } => INSUFFICIENT_PRIVILEGE,
        Error::Raise(_) => RAISE_EXCEPTION,

        // ---- compile-time ---------------------------------------------------
        Error::Parse { .. } => SYNTAX_ERROR,
        // A bind error is one of several PG codes depending on WHAT did not
        // bind, and the message is the only thing that can tell them apart.
        // Guessing from the text is unlovely, but the alternative — reporting
        // `42601` for a missing table — makes every ORM's "does this table
        // exist" probe fail as a syntax error, which is unrecoverable for it.
        Error::Bind(m) => bind_code(m),
        Error::Unsupported(_) => FEATURE_NOT_SUPPORTED,
        Error::TypeMismatch(_) | Error::NonUtf8Concat { .. } => DATATYPE_MISMATCH,
        Error::WrongParamCount { .. } => PROTOCOL_VIOLATION,
        Error::Schema(_) | Error::Config(_) => FEATURE_NOT_SUPPORTED,

        // ---- runtime --------------------------------------------------------
        Error::ArithmeticOverflow => NUMERIC_VALUE_OUT_OF_RANGE,
        Error::DivisionByZero => DIVISION_BY_ZERO,
        // 22023 invalid_parameter_value. PostgreSQL uses the narrower 2201E /
        // 2201F for the logarithm and power cases; one class-22 code that is
        // correct for every domain error beats five that have to be kept in
        // step with which function raised.
        Error::DomainError(_) => INVALID_PARAMETER_VALUE,
        Error::RuntimeBudget { .. } => QUERY_CANCELED,
        Error::OutOfMemory { .. } => OUT_OF_MEMORY,
        Error::DbFull => DISK_FULL,
        Error::ReadersFull => TOO_MANY_CONNECTIONS,
        Error::Busy => LOCK_NOT_AVAILABLE,
        // Both of these mean "your snapshot lost" — the one class a client is
        // expected to RETRY. 40001 is what makes SQLAlchemy's and Django's
        // retry helpers fire; anything else makes them give up.
        Error::WriteConflict | Error::WriteRejected { .. } | Error::SnapshotEvicted => {
            SERIALIZATION_FAILURE
        }
        Error::Corrupt(_) => DATA_CORRUPTED,
        Error::Frozen { .. } => READ_ONLY_SQL_TRANSACTION,
        Error::Io(_) => IO_ERROR,

        // ---- plan lifecycle -------------------------------------------------
        // `PlanInvalidated` means the schema moved under a cached plan. PG's
        // nearest equivalent is a cached-plan invalidation, which it reports as
        // `0A000` with "cached plan must not change result type" — clients
        // (notably psycopg and JDBC) know to re-prepare on it.
        Error::PlanInvalidated => FEATURE_NOT_SUPPORTED,
        Error::UnknownPlan(_) => INVALID_SQL_STATEMENT_NAME,

        _ => INTERNAL_ERROR,
    };
    (code, msg)
}

/// Pick the code for a bind error from what it says went wrong.
///
/// The order matters: "no such column" contains neither the word table nor
/// function, but a message about a missing table in a JOIN can mention columns.
/// Table is checked first because a missing table is the more consequential
/// misdiagnosis — it is the probe every migration tool runs.
fn bind_code(m: &str) -> &'static str {
    let l = m.to_ascii_lowercase();
    if l.contains("no such table") || l.contains("unknown table") || l.contains("does not exist: table")
    {
        UNDEFINED_TABLE
    } else if l.contains("unknown function") || l.contains("no such function") {
        UNDEFINED_FUNCTION
    } else if l.contains("no such column") || l.contains("unknown column") || l.contains("ambiguous")
    {
        UNDEFINED_COLUMN
    } else if l.contains("type") && (l.contains("mismatch") || l.contains("expected")) {
        DATATYPE_MISMATCH
    } else {
        // A bind failure that is none of the above is still a statement this
        // server will not run — `0A000` tells the client "not supported here"
        // rather than "your SQL is malformed", which is usually the truth.
        FEATURE_NOT_SUPPORTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dup() -> Error {
        Error::UniqueViolation {
            table: "users".into(),
            constraint: "users.email".into(),
        }
    }
    fn notnull() -> Error {
        Error::NotNullViolation {
            table: "users".into(),
            column: "id".into(),
        }
    }
    fn check() -> Error {
        Error::CheckViolation {
            table: "users".into(),
            column: "age".into(),
            expr: "age >= 0".into(),
        }
    }
    fn fk() -> Error {
        Error::ForeignKeyViolation {
            table: "memberships".into(),
            constraint: None,
        }
    }
    fn budget() -> Error {
        Error::RuntimeBudget {
            kind: mpedb_types::error::BudgetKind::WorkRows,
            limit: 10,
            used: 11,
            which: "scan".into(),
        }
    }

    #[test]
    fn constraint_violations_get_the_codes_orms_branch_on() {
        // Django turns 23505 into IntegrityError and everything else into a
        // DatabaseError; getting this wrong reports a crash where the
        // application expected a duplicate.
        assert_eq!(sqlstate(&dup()).0, "23505");
        assert_eq!(sqlstate(&notnull()).0, "23502");
        assert_eq!(sqlstate(&check()).0, "23514");
        assert_eq!(sqlstate(&fk()).0, "23503");
    }

    #[test]
    fn a_missing_table_is_42p01_and_not_a_syntax_error() {
        // This is the probe every migration tool runs. Reporting 42601 makes it
        // unrecoverable, because a syntax error is not something to retry.
        let (c, _) = sqlstate(&Error::Bind("no such table `users`".into()));
        assert_eq!(c, UNDEFINED_TABLE);
        let (c, _) = sqlstate(&Error::Bind("NO SUCH TABLE `Users`".into()));
        assert_eq!(c, UNDEFINED_TABLE);
    }

    #[test]
    fn missing_columns_and_functions_are_told_apart() {
        assert_eq!(
            sqlstate(&Error::Bind("no such column `nick`".into())).0,
            UNDEFINED_COLUMN
        );
        assert_eq!(
            sqlstate(&Error::Bind("unknown function `pg_typeof`".into())).0,
            UNDEFINED_FUNCTION
        );
    }

    #[test]
    fn a_named_refusal_is_feature_not_supported_not_an_internal_error() {
        // mpedb's whole compatibility posture is "a named refusal, never a
        // wrong answer". XX000 would tell the client the server broke.
        let (c, m) = sqlstate(&Error::Unsupported("generate_series() is …".into()));
        assert_eq!(c, FEATURE_NOT_SUPPORTED);
        assert!(m.contains("generate_series"));
    }

    #[test]
    fn a_bind_error_that_matches_nothing_still_avoids_syntax_error() {
        let (c, _) = sqlstate(&Error::Bind("something else entirely".into()));
        assert_eq!(c, FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn a_parse_error_is_the_one_thing_that_is_a_syntax_error() {
        assert_eq!(
            sqlstate(&Error::Parse {
                pos: 3,
                msg: "unexpected token".into()
            })
            .0,
            SYNTAX_ERROR
        );
    }

    #[test]
    fn the_runtime_budget_reports_as_a_cancelled_query() {
        // 57014 is what a client shows as "statement timed out / cancelled",
        // which is what a work-row budget actually means to the caller.
        assert_eq!(sqlstate(&budget()).0, QUERY_CANCELED);
    }
}
