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
    Concurrency, Config, DbOptions, Durability, FilePerms, WorkspaceConfig, WorkspaceMember,
    DEFAULT_MAX_WORK_ROWS,
};
pub use error::{Error, Result};
pub mod agg;
pub use agg::Accum;
pub use expr::{CmpKind, ExprProgram, Instr, ScalarFn};

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
}
pub use footprint::{Footprint, KeyAccess, KeyBound, KeyPart, PlanHash};
pub use fts::{Doclist, Tokenizer};
pub use policy::{PolicyCmd, PolicyDef};
pub use schema::{ColumnDef, DefaultExpr, IndexDef, Schema, TableDef, TableKind, MAX_INDEXES};
pub use value::{Collation, ColumnType, Value};

/// Maximum number of tables (user + system) in one database. Bounded so that
/// plan footprints can use a single `u64` bitmap per access kind.
pub const MAX_TABLES: usize = 64;

/// Maximum number of columns per table (bounded by `u16` column indices in
/// the expression IR and row format, kept small for sane page layouts).
pub const MAX_COLUMNS: usize = 1024;

/// On-file page size in bytes. Fixed at format time.
pub const PAGE_SIZE: usize = 4096;

/// mpedb on-file format version. Bumped on any incompatible layout change and
/// mixed into both the file header and every plan hash.
pub const FORMAT_VERSION: u32 = 1;
