//! Power-loss-simulator instrumentation for `durability = commit` (#121).
//!
//! **Test-only, off unless `MPEDB_COMMIT_SYNC_LOG` names a file.** Everything
//! here is one cached env lookup and a predictable branch on the commit path.
//!
//! # Why this exists
//!
//! `mpedb powerloss --durability wal|async` models power loss as a *truncated
//! tail*, because a WAL only ever appends. `commit` publishes by mutating a
//! mapped file **in place**, so its power-loss image is not a tail: it is "an
//! arbitrary subset of the dirty pages never reached the platter". There is no
//! way to construct that image from outside the process, because from outside
//! you cannot see which pages a commit dirtied, when each `msync` returned, or
//! what the bytes were at that instant. This module is that window.
//!
//! # What it records
//!
//! The *actual sequence of durability syscalls the engine made*, not a model of
//! what it was supposed to make. That distinction is the whole point: the
//! simulator reads §4.1's ordering out of the implementation and then tries to
//! falsify the recovery property, rather than assuming the ordering and proving
//! itself. Reorder the data and meta flushes in the engine and the trace
//! reorders with them — which is exactly how the simulator's non-vacuity is
//! demonstrated (`powerloss --durability commit --sabotage`).
//!
//! Four event kinds, appended in real time to the log file:
//!
//! | kind | payload | emitted from |
//! |------|---------|--------------|
//! | `1` FLUSH   | changed pages made durable | after `msync(MS_SYNC)` **returns** |
//! | `2` BARRIER | — | after [`Shm::sync_barrier`] returns |
//! | `3` PUBLISH | `txn_id`, meta slot | end of [`Shm::write_meta_slot`] |
//! | `4` MARK    | a `u64` tag | [`mark`], called by the workload driver |
//!
//! A FLUSH carries page *contents*, delta-coded: a page is written to the log
//! only when its bytes differ from the last bytes this process logged for it
//! (page → xxh3 shadow map). Without that, one commit's msync span — which on
//! Linux is deliberately the whole live data region (`engine/commit.rs`) —
//! would put megabytes in the log per commit. A page never seen before is
//! always emitted, so replaying every FLUSH in order over the pre-workload file
//! image reproduces the file byte for byte.
//!
//! Note the emission point: **after the syscall returns**. "msync returned"
//! is the durability edge the fault model is built on, and logging before the
//! call would record bytes as durable that a power loss during the call could
//! still lose.
//!
//! # Format
//!
//! Little-endian, no alignment, no header:
//!
//! ```text
//! FLUSH:   u8=1  u32 count  count × ( u64 file_offset, 4096 bytes )
//! BARRIER: u8=2
//! PUBLISH: u8=3  u64 txn_id  u64 slot
//! MARK:    u8=4  u64 tag
//! ```
//!
//! Each event is one `write_all` on an `O_APPEND` fd, so a multi-process run
//! interleaves whole events (this simulator drives a single writer anyway —
//! device semantics, not concurrency; the SIGKILL harness owns concurrency).

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use mpedb_types::{Error, Result, PAGE_SIZE};

pub const EV_FLUSH: u8 = 1;
pub const EV_BARRIER: u8 = 2;
pub const EV_PUBLISH: u8 = 3;
pub const EV_MARK: u8 = 4;

struct Logger {
    file: Mutex<File>,
    /// page id → xxh3 of the bytes last written to the log for it.
    shadow: Mutex<HashMap<u64, u64>>,
}

fn logger() -> Option<&'static Logger> {
    static L: OnceLock<Option<Logger>> = OnceLock::new();
    L.get_or_init(|| {
        let path = std::env::var_os("MPEDB_COMMIT_SYNC_LOG")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Logger {
            file: Mutex::new(file),
            shadow: Mutex::new(HashMap::new()),
        })
    })
    .as_ref()
}

/// Is the instrumentation armed? Cheap enough to guard the slice construction
/// in `msync_range_nobarrier` with.
#[inline]
pub fn active() -> bool {
    logger().is_some()
}

fn emit(rec: &[u8]) -> Result<()> {
    let Some(l) = logger() else { return Ok(()) };
    let mut f = l.file.lock().unwrap_or_else(|e| e.into_inner());
    f.write_all(rec).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("write(commit sync log): {e}"),
        ))
    })
}

/// A range whose `msync(MS_SYNC)` has just returned — i.e. bytes that survive
/// power loss from this instant. `off` is the file offset of `bytes[0]`; both
/// are rounded out to whole pages by the caller.
pub(crate) fn record_flush(off: u64, bytes: &[u8]) -> Result<()> {
    let Some(l) = logger() else { return Ok(()) };
    debug_assert!(off.is_multiple_of(PAGE_SIZE as u64));
    let mut shadow = l.shadow.lock().unwrap_or_else(|e| e.into_inner());
    let mut rec = vec![EV_FLUSH, 0, 0, 0, 0];
    let mut count = 0u32;
    for (i, page) in bytes.chunks_exact(PAGE_SIZE).enumerate() {
        let id = off / PAGE_SIZE as u64 + i as u64;
        let h = xxhash_rust::xxh3::xxh3_64(page);
        if shadow.insert(id, h) == Some(h) {
            continue; // platter already holds these bytes
        }
        rec.extend_from_slice(&(id * PAGE_SIZE as u64).to_le_bytes());
        rec.extend_from_slice(page);
        count += 1;
    }
    drop(shadow);
    rec[1..5].copy_from_slice(&count.to_le_bytes());
    emit(&rec)
}

/// The platter barrier returned (`F_FULLFSYNC` on Darwin; a no-op on Linux,
/// where `msync(MS_SYNC)` *is* `vfs_fsync_range` and each FLUSH above is
/// already a durability edge). Logged on both, so the trace shows where the
/// engine *intended* the ordering class to break.
pub(crate) fn record_barrier() -> Result<()> {
    emit(&[EV_BARRIER])
}

/// A meta slot was stored into the mapping (not yet flushed). This is the
/// commit-boundary marker the simulator groups the trace by: every FLUSH
/// between `PUBLISH(T-1)`'s meta flush and `PUBLISH(T)` is commit `T`'s data.
pub(crate) fn record_publish(txn: u64, slot: u64) -> Result<()> {
    let mut rec = [0u8; 17];
    rec[0] = EV_PUBLISH;
    rec[1..9].copy_from_slice(&txn.to_le_bytes());
    rec[9..17].copy_from_slice(&slot.to_le_bytes());
    emit(&rec)
}

/// Workload-driver hook: stamp a tag into the trace at the point the caller has
/// observed a commit as acknowledged. The simulator uses it to line commits up
/// with the externally-recorded expected states. No-op when the log is off.
pub fn mark(tag: u64) {
    let mut rec = [0u8; 9];
    rec[0] = EV_MARK;
    rec[1..9].copy_from_slice(&tag.to_le_bytes());
    let _ = emit(&rec);
}
