//! mpedb-mirror: bidirectional sqlite3/PostgreSQL ⇄ mpedb mirroring.
//!
//! Implements the design in `/DESIGN-MIRROR.md` (v1.1, review-hardened): source
//! adapters (sqlite, PostgreSQL), the epoch-fenced sync protocol, type mapping,
//! and the `mir\0` mirror-state / `cdc\0` capture sys-record codecs.
//!
//! The capture plane (dirty-set, write-block, reserved pages) lives in
//! mpedb-core as a generic CDC primitive; this crate owns the mirror semantics
//! on top of it.

pub mod adapter;
pub mod apply;
pub mod export;
pub mod import;
pub mod pg;
pub mod reconcile;
pub mod sqlite;
pub mod sqlite_adapter;
pub mod sqlite_track;
pub mod state;

#[cfg(test)]
mod pg_harness;

pub use reconcile::{check_source_not_restored, reconcile, ReconcileStats};
pub use sqlite_adapter::SqliteAdapter;

pub use adapter::{Cursor, NetOp, NetOpKind, PullBatch, SourceAdapter};
pub use apply::{apply_batch, ApplyStats};
pub use export::{diff_sqlite_data, export_sqlite, ExportReport};
pub use import::{import_sqlite, ImportOptions, ImportReport};

pub use state::{
    Authority, CaptureMode, Epoch, MirrorConfig, MirrorState, SourceKind, MIR_NS,
};
