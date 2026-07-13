//! mpedb-mirror: bidirectional sqlite3/PostgreSQL ⇄ mpedb mirroring.
//!
//! Implements the design in `/DESIGN-MIRROR.md` (v1.1, review-hardened): source
//! adapters (sqlite, PostgreSQL), the epoch-fenced sync protocol, type mapping,
//! and the `mir\0` mirror-state / `cdc\0` capture sys-record codecs.
//!
//! The capture plane (dirty-set, write-block, reserved pages) lives in
//! mpedb-core as a generic CDC primitive; this crate owns the mirror semantics
//! on top of it.

pub mod state;

pub use state::{
    Authority, CaptureMode, Epoch, MirrorConfig, MirrorState, SourceKind, MIR_NS,
};
