//! Shared, dependency-light types for the mpedb workspace.
//!
//! Everything here is usable by both the storage engine (`mpedb-core`) and the
//! SQL front-end (`mpedb-sql`) without either depending on the other.

pub mod config;
pub mod error;
pub mod expr;
pub mod footprint;
pub mod fts;
pub mod keycode;
pub mod policy;
pub mod schema;
pub mod value;

pub use config::{
    BareGroupBy, Concurrency, Config, DbOptions, Durability, FilePerms, WorkspaceConfig,
    WorkspaceMember,
    DEFAULT_MAX_JOIN_CELLS, DEFAULT_MAX_WORK_ROWS, MAX_DB_SIZE_MB,
};
pub use error::{BudgetKind, Error, Result};
pub mod agg;
pub use agg::{Accum, HostAggState, HostAggs};
pub use expr::{sqlite_now_string, CmpKind, ExprProgram, HostFns, Instr, ScalarFn};

/// The aggregate functions.
///
/// A closed enum, like [`ScalarFn`]: the tag goes in the plan bytes, so it must
/// be stable and exhaustively decodable — an unknown tag is `Corrupt`, never a
/// silently-missing aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AggFn {
    /// `COUNT(*)` when the arg is None, `COUNT(expr)` otherwise. COUNT(expr)
    /// skips NULLs; COUNT(*) counts rows.
    Count = 1,
    Sum = 2,
    Avg = 3,
    Min = 4,
    Max = 5,
    /// `total(x)` — like `sum` but always a float and **0.0 over an empty group**
    /// (never NULL), matching sqlite.
    Total = 6,
    /// `group_concat(x)` — concatenate the non-NULL values' text with a `,`
    /// separator, in scan order; NULL over an empty group. The two-argument
    /// (custom separator) form is refused by the parser in v1.
    GroupConcat = 7,
}

impl AggFn {
    pub fn from_tag(t: u8) -> Option<AggFn> {
        Some(match t {
            1 => AggFn::Count,
            2 => AggFn::Sum,
            3 => AggFn::Avg,
            4 => AggFn::Min,
            5 => AggFn::Max,
            6 => AggFn::Total,
            7 => AggFn::GroupConcat,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            AggFn::Count => "count",
            AggFn::Sum => "sum",
            AggFn::Avg => "avg",
            AggFn::Min => "min",
            AggFn::Max => "max",
            AggFn::Total => "total",
            AggFn::GroupConcat => "group_concat",
        }
    }

    /// For MIN / MAX only: does `candidate` STRICTLY beat the running `incumbent`
    /// extreme? The single source of the min/max keep-rule, shared by
    /// [`Accum::push`](crate::Accum) (which decides the aggregate value) and the
    /// executor's sqlite "bare column" witness (which decides which input row a
    /// bare column takes its value from). An incomparable pair keeps the
    /// incumbent (`Ordering::None` ⇒ `false`), so on a tie the FIRST occurrence
    /// wins — matching sqlite. Meaningless (and always `false`) for non-min/max.
    pub fn min_max_prefers(self, incumbent: &Value, candidate: &Value) -> Result<bool> {
        // `sort_cmp`: MIN/MAX over an `any` column meets mixed storage classes,
        // and sqlite's extremum is taken in its class order (a number always
        // beats a string to MIN). Comparing stored values only, so no
        // comparison affinity is involved.
        let ord = incumbent.sort_cmp(candidate, crate::Collation::Binary);
        Ok(matches!(
            (self, ord),
            (AggFn::Min, Some(std::cmp::Ordering::Greater))
                | (AggFn::Max, Some(std::cmp::Ordering::Less))
        ))
    }
}
/// WHICH aggregate a call names: one of the closed built-ins, or a HOST
/// aggregate registered on the connection through the C-API `xStep`/`xFinal`
/// path (design/DESIGN-UDF.md stage 2).
///
/// A host aggregate is carried BY NAME, exactly as [`Instr::HostCall`] carries a
/// host scalar: the accumulator is a live C callback pair, which is not
/// serializable, and a plan naming one is valid only for the connection that
/// registered it (so it never reaches the shared plan registry). The name is
/// already lowercased by the parser — SQL function names are case-insensitive
/// and the registry stores the same spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggTarget {
    Native(AggFn),
    Host(String),
}

impl AggTarget {
    /// The function name as written in SQL (for EXPLAIN and error messages).
    pub fn name(&self) -> &str {
        match self {
            AggTarget::Native(f) => f.name(),
            AggTarget::Host(n) => n,
        }
    }
    /// The built-in this call names, or `None` for a host aggregate. Every rule
    /// that is about a SPECIFIC built-in (the `count(*)` argument shape, the
    /// min/max bare-column witness) goes through this, so a host aggregate can
    /// never be mistaken for one of them.
    pub fn native(&self) -> Option<AggFn> {
        match self {
            AggTarget::Native(f) => Some(*f),
            AggTarget::Host(_) => None,
        }
    }
    pub fn host(&self) -> Option<&str> {
        match self {
            AggTarget::Native(_) => None,
            AggTarget::Host(n) => Some(n),
        }
    }
}

pub use footprint::{Footprint, KeyAccess, KeyBound, KeyPart, PlanHash, TableSet};
pub use fts::{Doclist, Tokenizer};
pub use policy::{PolicyCmd, PolicyDef};
pub use schema::{
    store_into, ColumnDef, DefaultExpr, IndexDef, Schema, TableDef, TableKind,
    MAX_IDENTIFIER_LEN, MAX_INDEXES,
};
pub use value::{
    exact_float_as_int, exact_int_as_float, Affinity, Collation, ColumnType, Value,
};

/// Maximum number of tables (user + system) in one database — a **resource**
/// bound, no longer a representation one (design/DESIGN-TABLE-CAP.md).
///
/// Footprints and the CDC capture config used to be per-table bitmaps, so this
/// constant *was* an integer width (u64 → 64, then u128 → 128). Both are now
/// sparse [`TableSet`]s, which impose no ceiling at all. What still bounds the
/// count is cost, not encoding:
///
/// 1. **Tombstone bloat** — table ids are never reused (DESIGN-DROP-TABLE §0),
///    so `Schema::tables` keeps a dead slot (~17 encoded bytes) per LIFETIME
///    create, and the whole schema is one catalog record re-encoded on every
///    DDL. That is the real cost curve, and it is proportional to actual use.
/// 2. **Decode safety** — `Schema::from_canonical_bytes` reads `ntables` from
///    untrusted bytes and must bound it before allocating.
/// 3. Schema validation reserves 8 slots for system tables, so 4088 are
///    user-visible — ~34× what Django's `queries` label needs, which is the
///    workload this number was raised for.
///
/// Raising it further is now a one-constant change: no format bump, no bit
/// audit.
pub const MAX_TABLES: usize = 4096;

/// Maximum number of columns per table (bounded by `u16` column indices in
/// the expression IR and row format, kept small for sane page layouts).
pub const MAX_COLUMNS: usize = 1024;

/// On-file page size in bytes. Fixed at format time.
pub const PAGE_SIZE: usize = 4096;

/// mpedb on-file format version. Bumped on any incompatible layout change and
/// mixed into both the file header and every plan hash.
pub const FORMAT_VERSION: u32 = 1;
