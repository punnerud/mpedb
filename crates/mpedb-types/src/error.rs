use crate::footprint::PlanHash;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// Which deterministic per-execution budget tripped (#74,
/// design/DESIGN-RUNTIME-BUDGET.md). Each kind carries its own unit and its
/// own `[runtime]` config knob, so [`Error::RuntimeBudget`]'s Display always
/// tells the user the RIGHT knob to raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    /// The work-row counter: rows yielded by scans, nested-loop join
    /// candidates, correlated-subquery re-evaluations, recursive-CTE rows.
    /// Knob: `[runtime] max_work_rows`.
    WorkRows,
    /// The join-materialization counter: `Value` cells LIVE in a nested-loop
    /// join's held intermediate product (`rows × row width`). Work-rows bound
    /// how much a query reads; this bounds how much a join HOLDS — the
    /// memory-proportional guard that stops an N-way cross join from taking
    /// the process down. Knob: `[runtime] max_join_cells`.
    JoinCells,
}

impl BudgetKind {
    /// The unit the `used`/`limit` counts are in, for the error message.
    pub fn unit(self) -> &'static str {
        match self {
            BudgetKind::WorkRows => "work-rows",
            BudgetKind::JoinCells => "live joined cells",
        }
    }

    /// The `[runtime]` config knob that raises this budget.
    pub fn knob(self) -> &'static str {
        match self {
            BudgetKind::WorkRows => "max_work_rows",
            BudgetKind::JoinCells => "max_join_cells",
        }
    }
}

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
    /// `RAISE(ABORT, 'msg')` fired in a trigger body (DESIGN-TRIGGERS §4.3):
    /// the statement aborts and unwinds atomically. The payload is the raise
    /// message VERBATIM — sqlite reports exactly the user's text.
    Raise(String),
    PrimaryKeyViolation {
        table: String,
    },
    /// An INSERT/UPDATE row failed a row-level-security `WITH CHECK` policy
    /// (design/DESIGN-MULTIDB.md §3.7). Deliberately carries NO predicate text — the
    /// policy source may embed thresholds/allow-lists (§6.6).
    PolicyViolation {
        table: String,
    },
    /// A write to an **RLS-enabled** table was rejected by a constraint, with the
    /// variant and column deliberately withheld (design/DESIGN-MULTIDB.md §6.5).
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
    /// The single writer lock was not acquired within the caller's busy
    /// deadline (`Engine::begin_write_deadline` / the facade's
    /// `Database::set_busy_timeout`). Another process holds a write
    /// transaction; nothing was executed or enqueued. This is sqlite's
    /// `SQLITE_BUSY` ("database is locked") — a liveness answer, not a fault:
    /// the database is fine and the call may be retried.
    Busy,
    DivisionByZero,
    ArithmeticOverflow,
    /// A statement exceeded one of its deterministic per-execution budgets
    /// (#74): `used` units of `kind` crossed `limit` while evaluating `which`
    /// (a coarse but correct attribution of where the work went — a scan, a
    /// nested-loop join, a correlated subquery, or a recursive CTE). Distinct
    /// from `Corrupt`: the data is fine, the query is a runaway. Deterministic
    /// — the same query over the same data trips at the same `used` on every
    /// machine — so it is reproducible and CI-stable, unlike a wall-clock
    /// timeout. The Display hint names the `[runtime]` knob for `kind`.
    RuntimeBudget {
        kind: BudgetKind,
        limit: u64,
        used: u64,
        which: String,
    },
    /// An allocation inside a query's own row materialization failed — the
    /// process is under a memory rlimit / cgroup cap, or genuinely out. The
    /// statement aborts cleanly instead of taking the host process down.
    /// Best-effort: only the bulk join-materialization allocations are
    /// fallible; a small allocation elsewhere at the very wall can still
    /// abort the process — the deterministic `[runtime] max_join_cells`
    /// budget is the primary guard, this is the backstop for the opted-out
    /// (`0` = unlimited) case.
    OutOfMemory {
        what: &'static str,
    },
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
            Error::Raise(m) => write!(f, "{m}"),
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
            Error::Busy => write!(
                f,
                "database is busy: another process held the writer lock past \
                 the busy timeout"
            ),
            Error::DivisionByZero => write!(f, "division by zero"),
            Error::ArithmeticOverflow => write!(f, "arithmetic overflow"),
            Error::RuntimeBudget { kind, limit, used, which } => write!(
                f,
                "runtime budget exceeded: {used} {} > limit {limit} while \
                 evaluating {which}; raise [runtime] {} in the config to \
                 allow more",
                kind.unit(),
                kind.knob()
            ),
            Error::OutOfMemory { what } => write!(
                f,
                "out of memory: allocation failed while materializing {what}; \
                 the statement was aborted (set [runtime] max_join_cells to \
                 fail deterministically before memory pressure)"
            ),
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
