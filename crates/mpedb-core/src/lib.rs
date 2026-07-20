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
mod os;
pub mod pagestore;
pub mod plsim;
pub mod ring;
pub mod row;
pub mod shm;

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
pub fn durability_barrier(fd: std::os::unix::io::RawFd) -> libc::c_int {
    os::fdatasync(fd)
}

pub use cdc::{CaptureConfig, DirtyEntry, DirtyOp};
pub use engine::{
    CheckPrograms, Engine, ReadTxn, RowCursor, TxnSavepoint, TxnSavepointFull, WorkMeter, WriteTxn,
};
pub use ring::{IntentRing, PendingIntent, RingResult, RING_PARAMS_CAP};
