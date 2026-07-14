use crate::footprint::PlanHash;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// The single error type shared across the mpedb workspace.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// Invalid configuration file (TOML syntax or semantic validation).
    Config(String),
    /// Invalid schema definition, or schema mismatch between config and database.
    Schema(String),
    /// The on-file state is inconsistent (bad magic, checksum, page linkage...).
    Corrupt(String),
    /// A value did not match the rigid column type, or an expression mixed types.
    TypeMismatch(String),
    NotNullViolation {
        table: String,
        column: String,
    },
    UniqueViolation {
        table: String,
        constraint: String,
    },
    CheckViolation {
        table: String,
        column: String,
        expr: String,
    },
    /// A row with the same primary key already exists.
    PrimaryKeyViolation {
        table: String,
    },
    /// An INSERT/UPDATE row failed a row-level-security `WITH CHECK` policy
    /// (DESIGN-MULTIDB.md §3.7). Deliberately carries NO predicate text — the
    /// policy source may embed thresholds/allow-lists (§6.6).
    PolicyViolation {
        table: String,
    },
    /// A write to an **RLS-enabled** table was rejected by a constraint, with the
    /// variant and column deliberately withheld (DESIGN-MULTIDB.md §6.5).
    ///
    /// Uniqueness pre-checks run over the whole B+tree with no RLS awareness, so
    /// a caller inserting a row valid under its own policy still collides with
    /// rows it cannot see. The distinct variants — `PrimaryKeyViolation` vs
    /// `UniqueViolation{constraint}` vs `CheckViolation{column, expr}` vs success
    /// — then let a probe learn not just THAT a hidden row exists but WHICH
    /// attribute matches a probed value (the error even names the column),
    /// enabling attribute-by-attribute reconstruction of invisible rows. This
    /// variant collapses them into one indistinguishable failure.
    ///
    /// It does NOT close the existence oracle (rejected-vs-success still leaks
    /// that *something* collided); that cannot be closed while a single global
    /// unique domain is preserved, and its mitigation is §6.4 — make the policy
    /// discriminator a leading part of every UNIQUE/PK so collisions can only
    /// happen inside the caller's own visible partition.
    ///
    /// Only RLS-enabled tables lose the detail; everywhere else the precise
    /// variants remain, because they are what makes a constraint failure
    /// debuggable.
    WriteRejected {
        table: String,
    },
    /// SQL tokenizer/parser error with a byte offset into the statement.
    Parse {
        pos: usize,
        msg: String,
    },
    /// Name resolution / type-checking of a parsed statement failed.
    Bind(String),
    /// `execute(hash, ...)` for a hash absent from both the local cache and
    /// the shared plan registry.
    UnknownPlan(PlanHash),
    /// The plan was built against a different schema; re-prepare.
    PlanInvalidated,
    WrongParamCount {
        expected: usize,
        got: usize,
    },
    /// All reader slots are occupied by live processes.
    ReadersFull,
    /// The fixed-size region is out of pages.
    DbFull,
    /// This reader's slot was reclaimed (max-pin-age eviction or theft);
    /// the snapshot is no longer protected and the read must be retried.
    SnapshotEvicted,
    /// Optimistic-concurrency (`concurrency = "optimistic"`) first-committer-
    /// wins abort: a conflicting commit landed on this write's footprint since
    /// its snapshot. Retryable — the caller re-prepares against a fresh
    /// snapshot (the facade autocommit path does this transparently).
    WriteConflict,
    DivisionByZero,
    ArithmeticOverflow,
    Unsupported(String),
    /// A write targeted a table that is currently write-blocked (frozen) by the
    /// CDC control record — e.g. the mirror froze it during an authority switch
    /// (DESIGN-MIRROR §3.9). Not a bug; the caller must not write it now.
    Frozen {
        table_id: u32,
    },
    /// Invariant violation inside the engine itself; always a bug.
    Internal(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Config(m) => write!(f, "config error: {m}"),
            Error::Schema(m) => write!(f, "schema error: {m}"),
            Error::Corrupt(m) => write!(f, "database corrupt: {m}"),
            Error::TypeMismatch(m) => write!(f, "type mismatch: {m}"),
            Error::NotNullViolation { table, column } => {
                write!(f, "NOT NULL violation: {table}.{column}")
            }
            Error::UniqueViolation { table, constraint } => {
                write!(f, "UNIQUE violation: {table} ({constraint})")
            }
            Error::CheckViolation { table, column, expr } => {
                write!(f, "CHECK violation: {table}.{column} failed `{expr}`")
            }
            Error::PrimaryKeyViolation { table } => {
                write!(f, "PRIMARY KEY violation in {table}")
            }
            Error::WriteRejected { table } => {
                write!(f, "write to {table} rejected by a constraint")
            }
            Error::PolicyViolation { table } => {
                write!(f, "row violates row-level security policy on {table}")
            }
            Error::Parse { pos, msg } => write!(f, "SQL parse error at byte {pos}: {msg}"),
            Error::Bind(m) => write!(f, "bind error: {m}"),
            Error::UnknownPlan(h) => write!(f, "unknown plan hash {h}"),
            Error::PlanInvalidated => {
                write!(f, "plan was built against a different schema; re-prepare")
            }
            Error::WrongParamCount { expected, got } => {
                write!(f, "wrong parameter count: expected {expected}, got {got}")
            }
            Error::ReadersFull => write!(f, "all reader slots are in use"),
            Error::DbFull => write!(f, "database is out of space"),
            Error::SnapshotEvicted => {
                write!(f, "read snapshot was evicted; retry the read transaction")
            }
            Error::WriteConflict => {
                write!(f, "optimistic write conflict; retry the transaction")
            }
            Error::DivisionByZero => write!(f, "division by zero"),
            Error::ArithmeticOverflow => write!(f, "arithmetic overflow"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::Frozen { table_id } => {
                write!(f, "table {table_id} is write-blocked (mirror frozen)")
            }
            Error::Internal(m) => write!(f, "internal error (bug in mpedb): {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
