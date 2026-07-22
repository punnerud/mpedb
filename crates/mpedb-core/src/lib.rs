//! mpedb storage engine: shared-memory COW B+tree with MVCC snapshots.
//!
//! Module map (see /DESIGN.md for the full architecture):
//! - [`pagestore`] — page pool abstraction (COW discipline)
//! - [`btree`] — copy-on-write B+tree
//! - [`row`] — row payload codec
//! - shm mapping, meta pages, reader table, transactions: in progress

pub mod btree;
pub mod cdc;
pub mod engine;
// Kept PRIVATE: several of these take raw pointers, and clippy's
// `not_unsafe_ptr_arg_deref` (rightly) only tolerates that behind a private
// module. The facade needs exactly one of them, re-exported below.
mod os;

/// Wall-clock microseconds since the Unix epoch — see [`os::wall_clock_micros`].
///
/// Re-exported so the SQL facade reads the SAME clock the engine does. That
/// matters on `wasm32`, where `SystemTime::now()` panics and the real time has
/// to come from a host import: two clock sources would mean the engine and the
/// executor could disagree about what `'now'` is.
pub use os::wall_clock_micros;
pub mod pagestore;
pub mod plsim;
pub mod ring;
pub mod row;
pub mod shm;
/// The `wasm32` OS emulation for the process-private (`:memory:`) path. Empty
/// on every native target; read its header for why each stub is sound.
pub mod wasmcompat;

/// The platform's **real** durability barrier — the one an acked durable commit
/// waits on: `fdatasync` on Linux, `fcntl(F_FULLFSYNC)` on macOS (where plain
/// `fsync()` does not flush the drive's write cache). Returns 0 on success.
///
/// Exposed so a tool that must make the *same* promise as the engine — the
/// benchmark's raw-Rust baseline, which exists to say what the medium can do
/// under our durability class — calls the identical thing instead of keeping a
/// copy that can drift. A baseline using plain `fsync()` on Apple hardware would
/// beat a truly durable engine by ~10x and report it as a result.
///
/// # Safety
/// `fd` must be a valid open file descriptor.
#[cfg(not(target_arch = "wasm32"))]
pub fn durability_barrier(fd: std::os::unix::io::RawFd) -> libc::c_int {
    os::fdatasync(fd)
}

/// wasm32: there is no fd and no durability class to make a promise about — the
/// browser build refuses anything but `Durability::None`. Kept only so the
/// symbol exists; it barriers nothing because nothing is at risk.
#[cfg(target_arch = "wasm32")]
pub fn durability_barrier(fd: crate::wasmcompat::RawFd) -> core::ffi::c_int {
    os::fdatasync(fd)
}

pub use cdc::{CaptureConfig, DirtyEntry, DirtyOp};
pub use engine::{
    CheckPrograms, Engine, FoldOpts, FoldStop, ReadTxn, RowCursor, TxnSavepoint, TxnSavepointFull,
    WorkMeter, WriteTxn,
};
pub use ring::{IntentRing, PendingIntent, RingResult, RING_PARAMS_CAP};
