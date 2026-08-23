//! mpedb storage engine: shared-memory COW B+tree with MVCC snapshots.
//!
//! Module map (see /DESIGN.md for the full architecture):
//! - [`pagestore`] — page pool abstraction (COW discipline)
//! - [`btree`] — copy-on-write B+tree
//! - [`row`] — row payload codec
//! - shm mapping, meta pages, reader table, transactions: in progress

/// Where this crate's own tests put their files.
///
/// The same `MPEDB_TEST_DIR` knob `mpedb_testkit::scratch_base` reads, spelled
/// again here rather than shared — and the duplication is forced, not lazy.
/// `mpedb-testkit` depends on the facade, which depends on THIS crate, so using
/// it here is a dependency cycle.
///
/// It exists because the alternative was measured: `std::env::temp_dir()` put
/// every core test database on the root filesystem, and one run of the suite
/// left 1 189 directories and 652 MB behind. That volume is 38 GB on the
/// machine this is developed on; when it filled, `ld` died with a bus error
/// mid-link and the failure looked like a compiler bug rather than a full disk.
///
/// NOT `#[cfg(test)]`, and that is load-bearing in two directions. The
/// `:memory:` path off Linux is production code that needs a scratch directory,
/// so gating this to test builds did not compile there at all. And an
/// INTEGRATION test links the library WITHOUT `cfg(test)`, so gating it would
/// send those runs' ephemeral files to the OS temp dir — which is the litter
/// this exists to prevent, just somewhere else. The knob is opt-in: unset means
/// `temp_dir()`, which is what a real deployment gets.
// Unused on Linux outside tests: there the `:memory:` path is `memfd_create`
// and never touches a directory. Kept unconditional rather than cfg'd to the
// platforms that call it, because that cfg would be a third place to keep in
// step with `shm.rs`'s own two arms.
#[allow(dead_code)]
pub(crate) fn scratch_dir() -> std::path::PathBuf {
    match std::env::var_os("MPEDB_TEST_DIR") {
        Some(d) if !d.is_empty() => {
            let d = std::path::PathBuf::from(d);
            let _ = std::fs::create_dir_all(&d);
            d
        }
        _ => std::env::temp_dir(),
    }
}

pub mod backup;
pub mod btree;
pub mod compact;
pub mod cdc;
pub mod engine;
// Kept PRIVATE: several of these take raw pointers, and clippy's
// `not_unsafe_ptr_arg_deref` (rightly) only tolerates that behind a private
// module. The facade needs exactly one of them, re-exported below.
mod os;
/// Which kind of storage a path sits on. Just these two out of `os` — the
/// module is otherwise internal (hole punching, madvise hints), but the
/// design's "local filesystem" precondition is something CALLERS have to be
/// able to check, so the check is part of the surface.
pub use os::{fs_kind, FsKind};
pub use os::Instant;

/// Wall-clock microseconds since the Unix epoch — see [`os::wall_clock_micros`].
///
/// Re-exported so the SQL facade reads the SAME clock the engine does. That
/// matters on `wasm32`, where `SystemTime::now()` panics and the real time has
/// to come from a host import: two clock sources would mean the engine and the
/// executor could disagree about what `'now'` is.
pub use os::wall_clock_micros;
/// The crash harnesses' two primitives (#159 stage 4). Re-exported rather than
/// making `os` public: those harnesses are the only thing outside the engine
/// with a reason to reach into the platform layer, and they need exactly two
/// functions.
pub use os::{died_by_hard_kill, hard_kill_child, hard_kill_self};

/// TOML-escape a path so it can be interpolated into a `path = "..."` line.
///
/// Only tests build config text from a live path, and on Unix this is the
/// identity — which is exactly why it was missing until the Windows port ran
/// them: `C:\Users\...` makes `\U` a TOML unicode escape, so an unescaped
/// path is a *parse error*, not a path.
#[cfg(test)]
pub(crate) fn toml_escape_path(p: &std::path::Path) -> String {
    mpedb_types::toml_escape(&p.display().to_string())
}

pub mod pagestore;
pub mod plsim;
pub mod ring;
pub mod row;
pub mod shm;
/// The `wasm32` OS emulation for the process-private (`:memory:`) path. Empty
/// on every native target; read its header for why each stub is sound.
pub mod wasmcompat;
/// The Windows arm of the same idea (#159): Unix-shaped names over real Win32
/// calls, so the engine's call sites compile unchanged. Empty on Unix.
pub mod wincompat;

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
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub fn durability_barrier(fd: std::os::unix::io::RawFd) -> libc::c_int {
    os::fdatasync(fd)
}

/// Windows: `FlushFileBuffers`, which is the real platter barrier there —
/// `FlushViewOfFile` (the `msync` position) only reaches the filesystem cache,
/// so the two compose exactly as `msync` + `F_FULLFSYNC` do on macOS.
#[cfg(windows)]
pub fn durability_barrier(fd: crate::wincompat::RawFd) -> core::ffi::c_int {
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
    CheckPrograms, Engine, FoldOpts, FoldStop, ReadTxn, RowCursor, SchemaPrograms, TxnSavepoint,
    TxnSavepointFull,
    WorkMeter, WriteTxn,
};
pub use ring::{IntentRing, PendingIntent, RingResult, RING_PARAMS_CAP};
