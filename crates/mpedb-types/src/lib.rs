//! Shared, dependency-light types for the mpedb workspace.
//!
//! Everything here is usable by both the storage engine (`mpedb-core`) and the
//! SQL front-end (`mpedb-sql`) without either depending on the other.

pub mod config;
pub mod error;
pub mod expr;
pub mod footprint;
pub mod keycode;
pub mod policy;
pub mod schema;
pub mod value;

pub use config::{
    Concurrency, Config, DbOptions, Durability, FilePerms, WorkspaceConfig, WorkspaceMember,
};
pub use error::{Error, Result};
pub use expr::{ExprProgram, Instr};
pub use footprint::{Footprint, KeyAccess, KeyBound, KeyPart, PlanHash};
pub use policy::{PolicyCmd, PolicyDef};
pub use schema::{ColumnDef, DefaultExpr, Schema, TableDef};
pub use value::{ColumnType, Value};

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
