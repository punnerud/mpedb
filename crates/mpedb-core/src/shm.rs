//! The shared-memory layer: file creation/attach, the lock area, meta pages,
//! and the reader table. Implements design/DESIGN.md §3–§4 exactly; every protocol
//! here was adversarially reviewed — read the design before changing ordering
//! or lock semantics.
//!
//! Safety model: all cross-process mutable state is accessed through atomics
//! or under the robust writer mutex. We never form `&`/`&mut` references to
//! memory another process may concurrently mutate non-atomically; page slices
//! handed out by transactions cover only pages that are immutable for the
//! borrow's duration (committed pages pinned by MVCC, or dirty pages owned
//! exclusively by the writer holding the lock).

mod pages;
mod wal;
mod readers;
mod notify;
mod optimistic;
mod open;

use mpedb_types::{Durability, Error, FilePerms, Result, PAGE_SIZE};
use std::cell::UnsafeCell;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::{File, OpenOptions};
#[cfg(all(unix, not(target_arch = "wasm32")))]
use std::os::unix::fs::{FileExt, OpenOptionsExt};
#[cfg(all(unix, not(target_arch = "wasm32")))]
use std::os::unix::io::{AsRawFd, RawFd};
// Windows: `crate::wincompat` supplies the same names over real Win32 calls —
// the extension traits in the Unix spelling, and a `libc` module that SHADOWS
// the real crate for this file. Same trick as the wasm32 arm below, except
// nothing here is emulated: every call is a genuine cross-process primitive.
#[cfg(windows)]
use crate::wincompat::{libc, AsRawFd, FileExt, OpenOptionsExt, RawFd};
// wasm32 has no filesystem and no fds. `crate::wasmcompat` supplies the same
// names over a process-private byte buffer — including a `libc` module that
// SHADOWS the real crate for this file, so every `libc::…` call site below is
// compiled unchanged rather than forked per platform. Its header documents why
// each emulated call is sound in a single-threaded browser tab.
#[cfg(target_arch = "wasm32")]
use crate::wasmcompat::{libc, File, OpenOptions, RawFd};
// `FromRawFd` is used only by the Linux `memfd_create` arm of
// `memory_backing_file` — importing it unconditionally is an unused import
// everywhere else, which `-D warnings` rejects (found by the first macOS CI run).
#[cfg(target_os = "linux")]
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

/// Anonymous process-private file for [`Shm::open_memory`]: Linux `memfd_create`
/// (never hits a path); elsewhere create+unlink a temp file so the fd has no
/// directory entry.
fn memory_backing_file() -> Result<File> {
    // wasm32: no fds, no temp dir — the anonymous buffer IS the backing store.
    // This is the ONLY `Shm` entry point a browser build can reach; every
    // file-backed one fails at `OpenOptions::open` (crate::wasmcompat).
    #[cfg(target_arch = "wasm32")]
    {
        crate::wasmcompat::File::anonymous().map_err(Error::Io)
    }
    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    {
        // MFD_CLOEXEC = 1
        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                c"mpedb".as_ptr(),
                1u32,
            )
        };
        if fd < 0 {
            return Err(io_err("memfd_create"));
        }
        Ok(unsafe { File::from_raw_fd(fd as RawFd) })
    }
    #[cfg(all(not(target_os = "linux"), not(target_arch = "wasm32")))]
    {
        // pid + timestamp is NOT unique: two `:memory:` opens on two threads
        // of one process can land on the same clock tick (macOS quantizes
        // SystemTime), and `create_new` then fails AlreadyExists. The atomic
        // counter is what actually disambiguates within a process; pid covers
        // across processes, the timestamp covers a recycled pid racing a
        // crash-orphaned name in the unlink window below.
        static MEM_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        // This arm IS the `:memory:` path on macOS and Windows, so it is
        // production code — see `scratch_dir` for why that helper is not
        // `#[cfg(test)]`.
        let dir = crate::scratch_dir();
        let path = dir.join(format!(
            "mpedb-mem-{}-{}-{}",
            crate::os::process_id(),
            // via os: std::time panics on wasm32 (the :memory: namer IS the
            // browser boot path)
            crate::os::wall_clock_micros(),
            MEM_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let _ = std::fs::remove_file(&path); // unlink; fd keeps the inode
        Ok(file)
    }
}

pub const FORMAT_VERSION: u32 = 9; // v9: ring entry carries a splice length delta (#151)
                                   //     (DESIGN-BLOBEXTENT §3.4); WAL3 trailer.
                                   // v2: intent-ring region between reader table and data

/// How many `trylock` attempts before the writer lock blocks. See
/// [`Shm::writer_lock`] for why this exists and why it is small.
///
/// Linux only, and #164 is where that stopped being an accident and became a
/// measurement: mirroring the spin onto the FLD-2 platforms bought nothing on
/// Windows and cost 0.4–0.6 % on macOS, against +10–14 % here. The non-Linux
/// `writer_lock` carries the table.
#[cfg(target_os = "linux")]
const WRITER_LOCK_SPINS: u32 = 64;

/// A/B switch for the acquire spin, off-by-default meaning "spin" (#164).
///
/// This exists because the first attempt to measure the spin on Windows was
/// INVALID and said so through its own control: two `bench-noise` dispatches,
/// one per revision, landed on different runner hardware — the byte-identical
/// sqlite3 control arm came out 25 % faster in the second, and the 1-writer
/// arm (where an uncontended `trylock` succeeds first try, so the spin cannot
/// execute at all) moved MORE than any contended arm. A ratio cannot rescue
/// that; when the null control moves most, the run measured the host.
///
/// So the arms have to share a process, the way `MPEDB_NO_MPEE` puts the
/// solver's two arms in one binary. Then a repetition alternates spin/no-spin
/// microseconds apart on one core, and the host cancels for real.
///
/// Process-global and deliberately not synchronized with in-flight acquires:
/// it is for a probe that flips between timed arms, not for per-transaction
/// control. Relaxed load — the value is a benchmarking choice, it guards no
/// data, and the acquire it precedes is a syscall pair that dwarfs it.
static WRITER_SPIN_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn the writer-lock acquire spin on (`true`) or off (`false`). Returns the
/// previous setting so a probe can restore it.
///
/// A NO-OP where [`has_writer_spin`] is false. Check that first: a probe that
/// toggles this on a platform with no spin to toggle measures one arm twice
/// and reports a confident 1.00 — "no effect" where the truth is "no
/// mechanism". Those two readings look identical and mean opposite things.
pub fn set_writer_spin_enabled(on: bool) -> bool {
    !WRITER_SPIN_OFF.swap(!on, Ordering::Relaxed)
}

/// Whether this build's writer lock has an acquire spin for the switch to
/// reach. Linux only — see [`Shm::writer_lock`]'s non-Linux twin for the
/// measurement that decided the FLD-2 platforms do without one (#164).
pub const fn has_writer_spin() -> bool {
    cfg!(target_os = "linux")
}

#[cfg(target_os = "linux")]
#[inline]
fn writer_spins() -> u32 {
    if WRITER_SPIN_OFF.load(Ordering::Relaxed) {
        0
    } else {
        WRITER_LOCK_SPINS
    }
}
/// Reserves at or below this size get their extents pre-written (zeroed) at
/// create so per-commit COW writes never pay an unwritten→written extent
/// conversion (`crate::os::prewrite_zeros`). Above it, the reserve is left
/// fallocate'd-but-unwritten: zeroing e.g. an 800 GiB `size_mb` at create would
/// dwarf the per-commit saving, and the conversion cost amortizes as it fills.
const PREZERO_MAX_BYTES: u64 = 2 << 30; // 2 GiB
const MAGIC: &[u8; 8] = b"MPEDB1\0\0";

// ---- meta page field offsets (pages 0 and 1) ----
const M_MAGIC: usize = 0; // [u8; 8], init-frozen
const M_FORMAT_VERSION: usize = 8; // u32, init-frozen
const M_PAGE_SIZE: usize = 12; // u32, init-frozen
const M_PAGE_COUNT: usize = 16; // u64, init-frozen
const M_MAX_READERS: usize = 24; // u32, init-frozen
const M_DURABILITY: usize = 28; // u32, init-frozen
const M_SCHEMA_HASH: usize = 32; // [u8; 32], init-frozen
const M_TXN_ID: usize = 64; // AtomicU64
const M_CATALOG_ROOT: usize = 72; // AtomicU64
const M_FREELIST_ROOT: usize = 80; // AtomicU64
const M_HIGH_WATER: usize = 88; // AtomicU64
const M_EXTENT_MAP_ROOT: usize = 96; // AtomicU64 (v3, DESIGN-BLOBEXTENT §3.4)
/// Schema generation (v4, #47): bumped by every DDL commit, IN the flipping
/// snapshot — the staleness signal a writer compares against its cached
/// schema. The FROZEN M_SCHEMA_HASH stays the SEED hash forever (the
/// config-drift check target); the live schema lives in the catalog.
const M_SCHEMA_GEN: usize = 104; // AtomicU64 (v4, #47 DDL)
const M_CHECKSUM: usize = 112; // AtomicU64 (xxh3 of logical bytes 0..112)
const META_LOGICAL_LEN: usize = 112;

// ---- lock area field offsets (page 2) ----
const LA_INIT_STATE: usize = 0; // AtomicU32: 0 empty, 1 formatting, 2 READY
#[cfg(target_os = "linux")]
const LA_MUTEX: usize = 64; // pthread_mutex_t, 128 bytes reserved (Linux)
// macOS reuses the ex-mutex region (Linux never compiles these): the tri-state
// recovered signal for the flock-based writer lock (design/DESIGN-MACOS-LOCK.md).
// 0 = CLEAN, 1 = a holder is in its critical section, 2 = POISONED (a prior
// owner-death recovery failed = ENOTRECOVERABLE, fail-closed).
#[cfg(not(target_os = "linux"))]
const LA_WRITER_DIRTY: usize = 64; // AtomicU32
const LA_DURABLE_TXN: usize = 192; // AtomicU64
const LA_OLDEST_PINNED: usize = 200; // AtomicU64 (monotone cache)
const LA_PID_NS_INO: usize = 208; // u64, frozen per boot epoch
const LA_BOOT_ID: usize = 216; // [u8; 16], frozen per boot epoch
const LA_WAL_LEN: usize = 232; // AtomicU64: bytes of DURABLE log (§5.4 wal);
                               // advanced only AFTER fdatasync(wal)
const LA_WAL_CKPT: usize = 240; // AtomicU64: log offset below which records are
                                // already checkpointed into the main file;
                                // advanced only AFTER a full-mapping MS_SYNC
const LA_WAL_APPENDED: usize = 248; // AtomicU64: append cursor — bytes written to
                                    // the log, synced or not (§5.4.2 async).
                                    // ONLY used in `async`; `wal` appends at
                                    // wal_len and leaves this zero. Written under
                                    // the writer lock; the deferred flusher only
                                    // reads it. Not a durability watermark — reboot
                                    // recovery ignores it (scans from wal_ckpt,
                                    // cross-checks wal_len).

// ---- committed-footprint ring (concurrency = optimistic; DESIGN-PHASE3) ----
//
// A fixed ring of recent committed writes' footprints, living in the free tail
// of the lock page (bytes 256.. ; never touched in `serial` mode, so serial
// on-disk bytes are unchanged). Written under the writer lock at commit BEFORE
// the meta flip; read under the writer lock by an optimistic committer to
// decide first-committer-wins. It is pure *validation* state — never part of
// durable/recovery state (after a reboot no pre-reboot snapshot survives, so a
// stale ring only ever yields empty conflict windows). Each committed txn N
// writes slot `N % OPT_RING_SLOTS`; a validator scans the exact txn ids in its
// window and treats any missing/overwritten entry as a conflict, which also
// makes the ring self-protecting against a foreign serial-mode writer (its
// commits leave gaps → conservative conflict → serial fallback, never a missed
// conflict).
/// Committed-footprint ring capacity (also the max snapshot-age a validator can
/// trust before conservatively conflicting).
///
/// **This bounds think time, in commits.** A guarded action holds a snapshot
/// while it works; if more than this many commits land in the meantime the ring
/// can no longer witness the window and the guard refuses conservatively. At 64
/// that was milliseconds of think time under load — fine for a handler, useless
/// for a person editing a document. Arm F never reached it (measured: zero
/// `SnapshotTooOld` at 8 editors), but arm F thinks for 10 ms, and the workload
/// it stands for thinks for seconds.
pub const OPT_RING_SLOTS: u64 = 256;
/// Exact key hashes a ring entry can carry before falling back to the summary.
///
/// **The width that actually bit.** Before #149 the comparison folded every key
/// through `region_bit` — a 64-bit Bloom with k = 1 — even when both sides
/// named a single exact key. Eight editors on eight distinct rows collide on a
/// bit with probability ≈ 35 %, and arm F measured exactly that: seats 2 and 5
/// share bit 19, conflict on every commit, and the whole gap to PostgreSQL was
/// two rows that are not related at all.
///
/// Eight covers a real action: arm E's networked profile touches one account
/// row plus five audit rows. Beyond eight the entry falls back to the Bloom,
/// which is correct and simply coarser — the fail-safe direction.
pub const OFP_MAX_KEYS: usize = 8;
const OPT_RING_ENTRY: usize = 128; // 16 u64: header (5) ‖ keys (8) ‖ spare (3)
// entry field offsets
const OFP_TXN: usize = 0;
const OFP_KIND: usize = 8;
const OFP_TBITS: usize = 16;
/// The key summary: an exact hash for `POINT`, a shard hash for `SHARD`, a
/// Bloom for `REGION`. `OFP_NKEYS` says whether the exact set below is usable.
const OFP_KHASH: usize = 24;
/// Columns this commit wrote, as a bitmask over column index folded `& 63`
/// (#146 K1). `u64::MAX` means "unknown / all", which is what anything that
/// cannot name its columns must publish.
///
/// **Why a fourth dimension.** Moving a document block writes `ord`; editing
/// its text writes `body`. Those are the same row, so every coarser rule
/// conflicts them — and a concurrent edit then cannot *follow* a move, which is
/// the hard case in collaborative editing. Distinguishing columns is what makes
/// the move and the edit independent.
const OFP_COLS: usize = 32;
/// How many of the `OFP_MAX_KEYS` exact slots below are valid. `0` means the
/// commit could not name its keys exactly and only `OFP_KHASH` applies.
const OFP_NKEYS: usize = 40;
/// First of `OFP_MAX_KEYS` exact key-hash slots.
const OFP_KEYS: usize = 48;
/// The BYTE RANGE inside the value this commit rewrote, packed `lo << 32 | hi`
/// (#151), or [`RANGE_ANY`] when it could not name one.
///
/// **The fifth dimension, and the one that makes a cell divisible.** Four
/// editors typing in four paragraphs of one `body` are, to every coarser rule,
/// four writes to the same cell. `splice()` already makes them compose at the
/// storage layer; this is what stops the guard refusing them anyway.
///
/// The range is known from the statement's own parameters, so it is decided
/// before any work happens — the same "declared, not accumulated" property the
/// key has (#148), and the reason a collision is one integer comparison.
const OFP_RANGE: usize = 112;
/// How much this commit changed the value's LENGTH, as `i64` bits (#151).
///
/// The range says *where*; this says *by how much everything after it moved*.
/// Together they are enough to carry a pending edit's offset forward across
/// commits it never saw — which is what stops an edit near the start of a cell
/// invalidating every pending edit after it.
const OFP_DELTA: usize = 120;
/// The mask meaning "every column" — the fail-safe value.
pub const COLS_ANY: u64 = u64::MAX;
/// The byte range meaning "anywhere in the value" — what anything that cannot
/// name a range must publish, and what a zeroed slot reads as.
pub const RANGE_ANY: u64 = u64::MAX;
/// Footprint kinds recorded per committed txn.
pub const OFP_KIND_EMPTY: u64 = 0; // touched no user table (catalog/sys only)
pub const OFP_KIND_POINT: u64 = 1; // exactly one table, one PK (table_bits+key_hash)
pub const OFP_KIND_TABLE: u64 = 2; // table-level write set (table_bits), any keys
/// Table set PLUS a key-region summary in `khash` (#143). Same 32-byte entry —
/// `khash` was simply unused for `TABLE`, so this costs no format change.
///
/// **Why this kind had to exist.** Arm E measured what `TABLE` costs the shard
/// guard: two workers writing different rows of the same table conflicted on
/// every commit, so "one shard per user" sharded nothing and throughput was
/// flat at 42 actions/s while the unguarded control scaled to 307
/// (benchmarks/notify.md). The write set alone is the wrong resolution for a
/// workload keyed by row.
pub const OFP_KIND_REGION: u64 = 3;
/// Table set plus a **shard identity** in `khash` (#144). Compared for
/// EQUALITY, not overlap: two commits under different shards touched disjoint
/// rows by construction.
///
/// **Why equality and not a key set.** `REGION` summarises the rows a commit
/// touched, and that summary saturates — six keys in a 64-bit filter collide
/// better than half the time, which arm E measured as the guard failing to
/// scale. A per-user shard has the wrong shape for a key set: whether two
/// actions conflict should depend on *whether it is the same user*, never on
/// how many users exist. One value compared for equality gives exactly that,
/// and 10 users cost what 10 million do.
///
/// The identity is the session context an action ran under (`app.tenant`), and
/// it is recorded ONLY when every table the plan touches carries a policy on
/// that key — the policy is what makes "different tenant ⇒ different rows" a
/// fact rather than a hope. Anything less records `REGION` or `TABLE`.
pub const OFP_KIND_SHARD: u64 = 4;

// ---- change-notification region (#139) ----
//
// A fixed table of per-table change generations in the free tail of the lock
// page, so a process can WAIT for a table to change instead of polling. This
// is what lets mpedb answer LISTEN/NOTIFY without Postgres's global commit
// lock: Postgres serializes commits to deliver notifications in commit order
// across every channel, and that ordering is a promise mpedb deliberately does
// not make. Per table, order comes free — the writer lock already serializes
// commits, so a counter bumped under it is monotone.
//
// Slot = `table_id % NOTIFY_SLOTS`, with the EXACT table id stored alongside.
// A collision therefore costs a spurious wakeup and never a missed one. That
// direction is deliberate: the committed-footprint ring above folds ids `& 63`
// and is sound because a false conflict only costs a retry, but the same fold
// used as *identity* would wake the wrong listener — the bug class `cdc.rs`
// documents having already had to fix once.
// ---- change notification (#139–#147), page-relative ----
//
// Offsets below are within the notification PAGE (`notify_start_page`), not
// the lock page. It moved off the lock page in #147: the tail there had ~230
// bytes left, enough for the slots but not for the waiter registry, and
// without a registry a SIGKILLed listener leaked the parked-waiter count
// forever (review finding R1).
const NF_SLOTS_OFF: usize = 0;
/// Notification slots. Collisions are false wakeups, so this trades memory for
/// wakeup precision, not for correctness.
pub const NOTIFY_SLOTS: u32 = 64;
const NOTIFY_ENTRY: usize = 24;
const NF_GEN: usize = 0; // AtomicU64: bumped once per commit that wrote the table
const NF_SEQ: usize = 8; // AtomicU32: the futex word; changes on every publish
const NF_TABLE: usize = 12; // AtomicU32: exact table id (not folded)
const NF_KEY: usize = 16; // AtomicU64: digest of the key touched, 0 = whole table

const NF_WAITERS: usize = NF_SLOTS_OFF + NOTIFY_SLOTS as usize * NOTIFY_ENTRY; // 1536
const NF_ANY: usize = NF_WAITERS + 8; // 1544
const NF_EPOCH: usize = NF_ANY + 8; // 1552

/// Waiter registry (#147). One slot per parked listener, carrying the same
/// identity the reader table uses — pid plus `/proc` start time — so a slot
/// whose owner died can be told from one whose owner is merely quiet.
///
/// **Why a registry at all.** `NF_WAITERS` is the hot path: a commit reads it
/// once to decide whether the wake syscalls are worth issuing. A bare counter
/// can only be repaired by someone who knows which increments are stale, and
/// that is what identity buys.
///
/// **Ownership decides who decrements.** A slot is cleared by exactly one
/// party — its owner on the way out, or a sweeper that wins the CAS — and only
/// that party decrements. Without that rule a sweep racing a clean exit would
/// double-decrement, and an undercount is a MISSED WAKEUP, the one direction
/// this whole subsystem may not fail in.
const NF_REG: usize = NF_EPOCH + 8; // 1560
pub const NOTIFY_WAITER_SLOTS: usize = 128;
const NFW_ENTRY: usize = 16;
const NFW_PID: usize = 0; // AtomicU32: 0 = free
const NFW_START: usize = 8; // AtomicU64: /proc start time, the incarnation tag

const _: () = assert!(NF_REG + NOTIFY_WAITER_SLOTS * NFW_ENTRY <= PAGE_SIZE);

/// Why a guard's conflict check answered yes (#142 G1).
///
/// Three of these are the OPT ring's documented limits rather than a real
/// overlap, and they are separated because the difference decides whether the
/// ring is worth widening. A guard that mostly fails on `SnapshotTooOld` says
/// callers think for too long; one that mostly fails on `Overlap` says the
/// workload genuinely contends. Guessing which without counting is how a
/// format change gets built for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuardVerdict {
    /// No conflict.
    #[default]
    Clear,
    /// A committed txn in the window wrote a table in the guarded surface.
    /// This is the only verdict that is an actual conflict.
    Overlap,
    /// The snapshot is older than the ring can witness (> `OPT_RING_SLOTS`
    /// commits ago), so the question is unanswerable and answered safely.
    SnapshotTooOld,
    /// A ring slot did not hold the txn id it should — overwritten, or written
    /// by a process whose commits we cannot see. Conservative.
    RingGap,
}

/// Which bytes of which cell a pending sub-edit is about (#151) — the question
/// [`Shm::opt_rebase`] answers, as one value rather than five arguments.
#[derive(Debug, Clone, Copy)]
pub struct CellEdit {
    /// The table, folded `& 63` like the ring's own bitmap.
    pub table_bit: u64,
    pub key: u64,
    /// The column, folded `& 63`.
    pub col_bit: u64,
    pub at: u64,
    pub len: u64,
}

/// The answer to "where is my sub-edit now, and does it collide" (#151).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// The offset, carried forward through every commit that moved it.
    At(u64),
    /// Another commit is about the same bytes. Not a coordinate problem — the
    /// two edits genuinely disagree, and one of them has to lose.
    Collision,
}

/// What a committing txn publishes about what it wrote — the mirror image of
/// [`GuardSurface`], and the reason both are types: the two travel together
/// through every dimension, and passing them as loose arguments was eight
/// parameters and counting, which is the shape of a struct that has not been
/// written down yet.
pub struct CommitFootprint<'a> {
    pub kind: u64,
    pub table_bits: u64,
    /// Point key, shard hash, or region Bloom — whichever `kind` says.
    pub key_hash: u64,
    pub cols: u64,
    /// The exact keys written, when there were few enough to name (#149).
    pub keys: Option<&'a [u64]>,
    /// Packed byte range (#151), or [`RANGE_ANY`].
    pub range: u64,
    /// Length change this commit made to the value, in bytes (#151).
    pub delta: i64,
}

/// The five dimensions a guarded action is checked on (#142–#151).
///
/// They are one concept and travel together: a conflict needs an overlap in
/// EVERY dimension, and each one that can be named narrows the answer. Passing
/// them separately was seven arguments and counting, which is the shape of a
/// type that has not been written down yet.
///
/// Each field's "unknown" value is the widest one, so a surface built from
/// nothing conflicts with everything:
///
/// | field | unknown | meaning |
/// |---|---|---|
/// | `tables` | — | folded `& 63`; empty conflicts with nothing |
/// | `regions` | [`Shm::REGION_ANY`] | which keys, as a 64-bit Bloom |
/// | `keys` | `None` | which keys, EXACTLY, when there are few enough |
/// | `shard` | `None` | which tenant, when provable |
/// | `cols` | [`COLS_ANY`] | which columns |
/// | `range` | `None` | which bytes inside the value |
///
/// `keys` and `regions` describe the same thing at two precisions, and both are
/// carried because either side of a comparison may have lost exactness. When
/// both sides can name their keys the test is a set intersection with no false
/// positives; when either cannot, it falls back to the Bloom. Keeping only the
/// Bloom is what made two unrelated rows conflict 35 % of the time at eight
/// editors (#149).
#[derive(Debug, Clone)]
pub struct GuardSurface {
    pub tables: u64,
    pub regions: u64,
    /// Every key this surface touches, or `None` once there were more than
    /// [`OFP_MAX_KEYS`] of them and exactness was given up.
    pub keys: Option<Vec<u64>>,
    pub shard: Option<u64>,
    pub cols: u64,
    /// What this action claims about WHERE inside the value it writes (#151).
    pub range: RangeClaim,
}

/// A guarded action's claim on the bytes of a value (#151).
///
/// Three states, not two, and the third is the one that earns the type:
/// `Settled` says a rebase walk already decided this dimension at execution
/// time, so the commit-time test must not decide it again against coordinates
/// that have since moved. Collapsing it into "no range" would read as *the
/// whole value* — the exact opposite — which is what it did before this was
/// spelled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeClaim {
    /// No range could be named: the whole value, conflicting with any sub-edit.
    Whole,
    /// These bytes.
    Bytes(u32, u32),
    /// Already decided by [`Shm::opt_rebase`] at execution.
    Settled,
}

const INIT_READY: u32 = 2;
const INIT_FORMATTING: u32 = 1;

// ---- reader slot offsets (64-byte slots, page 3..) ----
const RS_WORD: usize = 0; // AtomicU64: {pid: high u32, seq: low u32}
const RS_TXN: usize = 8; // AtomicU64: pinned txn; u64::MAX = not yet pinned
const RS_PID_START: usize = 16; // AtomicU64: /proc/<pid>/stat starttime
pub const READER_SLOT_SIZE: usize = 64;

pub const META_PAGE_A: u64 = 0;
pub const META_PAGE_B: u64 = 1;
pub const LOCK_PAGE: u64 = 2;
pub const READER_TABLE_PAGE: u64 = 3;

pub fn reader_table_pages(max_readers: u32) -> u64 {
    ((max_readers as usize * READER_SLOT_SIZE).div_ceil(PAGE_SIZE)) as u64
}

/// Intent-ring region (design/DESIGN.md §5.3): fixed geometry, directly after the
/// reader table.
pub const RING_SLOTS: u32 = 256;
pub const RING_SLOT_SIZE: usize = 1024;
pub const RING_PAGES: u64 = (RING_SLOTS as usize * RING_SLOT_SIZE / PAGE_SIZE) as u64;

pub fn ring_start_page(max_readers: u32) -> u64 {
    READER_TABLE_PAGE + reader_table_pages(max_readers)
}

/// Change-notification region (#139–#147): its own page, directly after the
/// intent ring.
///
/// It lived in the lock page's free tail until #147. That tail had ~230 bytes
/// left, which was enough for the per-table slots but not for the waiter
/// registry a SIGKILLed listener needs in order to be cleaned up — so the
/// counter leaked forever and every later commit paid wake syscalls for a
/// process that was gone. One page buys the registry and leaves room; the cost
/// is 4 KiB per database and one format version.
pub const NOTIFY_PAGES: u64 = 1;

pub fn notify_start_page(max_readers: u32) -> u64 {
    ring_start_page(max_readers) + RING_PAGES
}

/// Committed-footprint ring (#142–#149): its own pages, directly after the
/// notification region.
///
/// It lived in the lock page's free tail until #149, which capped it at 64
/// entries of 40 bytes. Both of those numbers were limits with consequences —
/// 64 entries bounds how long a guarded action may think, and 40 bytes left
/// room for exactly one key, which forced every comparison through a 64-bit
/// Bloom. Its own pages buy both: 256 entries and eight exact keys each, for
/// 32 KiB per database.
pub const OPT_RING_PAGES: u64 =
    ((OPT_RING_SLOTS as usize * OPT_RING_ENTRY).div_ceil(PAGE_SIZE)) as u64;

pub fn opt_ring_start_page(max_readers: u32) -> u64 {
    notify_start_page(max_readers) + NOTIFY_PAGES
}

pub fn data_start_page(max_readers: u32) -> u64 {
    opt_ring_start_page(max_readers) + OPT_RING_PAGES
}

fn durability_tag(d: Durability) -> u32 {
    match d {
        Durability::None => 0,
        Durability::Commit => 1,
        Durability::Async => 2,
        // Tag 3 in the frozen meta field. FORMAT_VERSION deliberately stays:
        // an old engine sees an unknown tag and refuses the attach, which is
        // the correct failure for a file whose durability protocol it cannot
        // honor.
        Durability::Wal => 3,
    }
}

fn durability_from_tag(t: u32) -> Option<Durability> {
    Some(match t {
        0 => Durability::None,
        1 => Durability::Commit,
        2 => Durability::Async,
        3 => Durability::Wal,
        _ => return None,
    })
}

// ---- write-ahead log (durability = wal / async, design/DESIGN.md §5.4) ----
//
// The WAL is a separate append-only file at `<db-path>-wal`, shared by both
// wal-class modes: `wal` (fdatasync per commit, durable-on-ack) and `async`
// (deferred/coalesced fdatasync, crash-consistent — §5.4.2). Record layout
// (all fields little-endian):
//
// ```text
//   0   magic        u32   WAL_MAGIC ("WAL3")
//   4   txn_id       u64
//  12   n_pages      u32
//  16   rec_len      u32   total record length in bytes (header..=checksum)
//  20   n_pages × page_entry (variable length — see below)
//   +   catalog_root    u64 ┐
//   +   freelist_root   u64 │ the commit's MetaSnapshot body — recovery
//   +   high_water      u64 │ rebuilds the meta FROM THIS TRAILER, which is
//   +   extent_map_root u64 ┘ why v3 had to bump the framing (BLOBEXTENT §3.4)
//   +   checksum     u64   xxh3_64(file_offset LE ‖ record bytes 0..here)
//
//   page_entry:
//     0  page_id  u64
//     8  enc      u8    0 = FULL, 1 = SPLIT
//     FULL:   9  4096 page-image bytes
//     SPLIT:  9  prefix_len u16 ‖ suffix_start u16
//               ‖ prefix[prefix_len] ‖ suffix[PAGE_SIZE - suffix_start]
// ```
//
// SPLIT is the "lean record" encoding (design/DESIGN.md §5.4.1): a B+tree node uses
// only a header+slot prefix and a packed-cell suffix, so the unread middle
// (or an overflow page's unused tail) is omitted from the log and zero-filled
// on replay (`btree::used_span` proves which bytes are never read back). It is
// chosen per page only when it is strictly smaller than FULL, so a record is
// never larger than the old fixed-page format. `rec_len` in the header lets a
// recovery scan skip a variable-length record without decoding it.
//
// The record's own file offset is part of the checksum preimage: a
// checksum-valid record is valid ONLY at the offset it was appended at, so a
// recovery scan can never resync onto a stale copy of a record embedded in
// page data (or left behind at a different offset). Recovery additionally
// requires consecutive records to carry consecutive txn ids.

pub const WAL_MAGIC: u32 = 0x334C_4157; // "WAL3" (trailer carries extent_map_root)
/// Fixed record overhead: magic + txn + n_pages + rec_len + 4 meta + checksum.
pub const WAL_RECORD_FIXED: usize = 4 + 8 + 4 + 4 + 4 * 8 + 8;
/// Bytes of a FULL page entry: page id + enc byte + page image. A SPLIT entry
/// is always smaller; this is the per-page upper bound used to size the read
/// buffer and to bound `rec_len` on decode.
pub const WAL_PAGE_ENTRY: usize = 8 + 1 + PAGE_SIZE;
const WAL_ENC_FULL: u8 = 0;
const WAL_ENC_SPLIT: u8 = 1;
/// Byte offset of the fixed 20-byte record header prefix (magic..rec_len).
const WAL_HDR_LEN: usize = 4 + 8 + 4 + 4;
/// The meta+checksum trailer after the last page entry.
const WAL_TRAILER_LEN: usize = 4 * 8 + 8;
/// Checkpoint when this many un-checkpointed log bytes have accumulated.
pub const WAL_CKPT_THRESHOLD_DEFAULT: u64 = 16 * 1024 * 1024;
/// The log is preallocated (never sparse-appended) in chunks of this size.
const WAL_GROW_CHUNK: u64 = 4 * 1024 * 1024;

/// A/B knob (`MPEDB_WAL_FULL_PAGES=1`): force every logged page to the FULL
/// encoding, disabling the lean SPLIT elision. Lets the benchmark measure the
/// lean-record contribution in isolation. Read once per process.
fn wal_full_pages() -> bool {
    static F: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("MPEDB_WAL_FULL_PAGES").is_ok());
    *F
}

/// A/B knob (`MPEDB_WAL_CKPT_FULL=1`): force the whole-mapping msync arm of
/// `wal_checkpoint_if` even on Darwin (where the default is page-targeted).
fn wal_ckpt_full_msync() -> bool {
    static F: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("MPEDB_WAL_CKPT_FULL").is_ok_and(|v| v == "1"));
    *F
}

/// Checkpoint threshold, env-overridable for tests and simulations
/// (`MPEDB_WAL_CKPT_BYTES`); read once per process.
pub fn wal_ckpt_threshold() -> u64 {
    static T: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        std::env::var("MPEDB_WAL_CKPT_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(WAL_CKPT_THRESHOLD_DEFAULT)
    });
    *T
}

/// The WAL file that accompanies a `durability = wal` database.
pub fn wal_path(db_path: &Path) -> PathBuf {
    let mut os = db_path.as_os_str().to_owned();
    os.push("-wal");
    PathBuf::from(os)
}

/// File-absolute byte offsets of the lock-area WAL fields and the boot id,
/// exported for crash/power-loss tooling that manipulates cold files.
pub const WAL_LEN_FILE_OFFSET: u64 = LOCK_PAGE * PAGE_SIZE as u64 + LA_WAL_LEN as u64;
pub const WAL_CKPT_FILE_OFFSET: u64 = LOCK_PAGE * PAGE_SIZE as u64 + LA_WAL_CKPT as u64;
pub const BOOT_ID_FILE_OFFSET: u64 = LOCK_PAGE * PAGE_SIZE as u64 + LA_BOOT_ID as u64;

fn wal_checksum(offset: u64, record_without_checksum: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&offset.to_le_bytes());
    h.update(record_without_checksum);
    h.digest()
}

/// One logged page image, borrowing from the read buffer. `write_into`
/// reconstructs the full `PAGE_SIZE` byte image; SPLIT zero-fills the elided
/// span (`btree::used_span` guarantees no reader observes the difference).
pub enum WalPageImg<'a> {
    Full(&'a [u8]),
    Split {
        prefix: &'a [u8],
        suffix_start: usize,
        suffix: &'a [u8],
    },
}

impl WalPageImg<'_> {
    /// Materialize the page into `dst` (must be `PAGE_SIZE`).
    pub fn write_into(&self, dst: &mut [u8]) {
        debug_assert_eq!(dst.len(), PAGE_SIZE);
        match self {
            WalPageImg::Full(img) => dst.copy_from_slice(img),
            WalPageImg::Split {
                prefix,
                suffix_start,
                suffix,
            } => {
                dst.fill(0);
                dst[..prefix.len()].copy_from_slice(prefix);
                dst[*suffix_start..].copy_from_slice(suffix);
            }
        }
    }
}

/// A decoded, checksum-verified WAL record (pages borrow from the read buffer).
pub struct WalRecord<'a> {
    pub txn_id: u64,
    pub pages: Vec<(u64, WalPageImg<'a>)>,
    pub catalog_root: u64,
    pub freelist_root: u64,
    pub high_water: u64,
    pub extent_map_root: u64,
    /// Total encoded length in the log.
    pub len: u64,
}

/// Encode one commit record destined for file offset `offset`. Each page is
/// stored FULL, or SPLIT (lean) when `btree::used_span` shows SPLIT is
/// strictly smaller — so a record never exceeds the old fixed-page size.
pub fn encode_wal_record(
    offset: u64,
    txn_id: u64,
    pages: &[(u64, &[u8])],
    catalog_root: u64,
    freelist_root: u64,
    high_water: u64,
    extent_map_root: u64,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(WAL_RECORD_FIXED + pages.len() * 256);
    encode_wal_record_into(
        &mut buf,
        offset,
        txn_id,
        pages.iter().map(|&(id, img)| (id, img)),
        catalog_root,
        freelist_root,
        high_water,
        extent_map_root,
    );
    buf
}

/// Append one page image to a WAL record body (id + FULL/SPLIT payload).
fn append_wal_page_img(buf: &mut Vec<u8>, id: u64, img: &[u8]) {
    debug_assert_eq!(img.len(), PAGE_SIZE);
    buf.extend_from_slice(&id.to_le_bytes());
    let (prefix_end, suffix_start) = crate::btree::used_span(img);
    // SPLIT beats FULL only when the elided gap saves more than its 4-byte
    // header overhead (id+enc are common to both); otherwise store FULL.
    // MPEDB_WAL_FULL_PAGES forces FULL for A/B measurement.
    if img.len() == PAGE_SIZE
        && suffix_start.saturating_sub(prefix_end) > 4
        && !wal_full_pages()
    {
        buf.push(WAL_ENC_SPLIT);
        buf.extend_from_slice(&(prefix_end as u16).to_le_bytes());
        buf.extend_from_slice(&(suffix_start as u16).to_le_bytes());
        buf.extend_from_slice(&img[..prefix_end]);
        buf.extend_from_slice(&img[suffix_start..]);
    } else {
        buf.push(WAL_ENC_FULL);
        buf.extend_from_slice(img);
    }
}

/// Encode a WAL record into `buf` (cleared first). Reused by the hot commit
/// path so each durable ack does not allocate a fresh multi-page buffer.
///
/// The eight arguments ARE the WAL record's fields (design/DESIGN.md §5): bundling
/// them into a struct would only move the same list one line up, and this is
/// commit-path code whose shape is load-bearing.
#[allow(clippy::too_many_arguments)]
fn encode_wal_record_into<'a, I>(
    buf: &mut Vec<u8>,
    offset: u64,
    txn_id: u64,
    pages: I,
    catalog_root: u64,
    freelist_root: u64,
    high_water: u64,
    extent_map_root: u64,
) where
    I: IntoIterator<Item = (u64, &'a [u8])>,
    I::IntoIter: ExactSizeIterator,
{
    let pages = pages.into_iter();
    let n = pages.len();
    buf.clear();
    buf.reserve(WAL_RECORD_FIXED + n * 256);
    buf.extend_from_slice(&WAL_MAGIC.to_le_bytes());
    buf.extend_from_slice(&txn_id.to_le_bytes());
    buf.extend_from_slice(&(n as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // rec_len placeholder (byte 16)
    for (id, img) in pages {
        append_wal_page_img(buf, id, img);
    }
    buf.extend_from_slice(&catalog_root.to_le_bytes());
    buf.extend_from_slice(&freelist_root.to_le_bytes());
    buf.extend_from_slice(&high_water.to_le_bytes());
    buf.extend_from_slice(&extent_map_root.to_le_bytes());
    // rec_len includes the not-yet-appended 8-byte checksum, and is part of
    // the checksum preimage so a torn/altered length cannot validate.
    let rec_len = (buf.len() + 8) as u32;
    buf[16..20].copy_from_slice(&rec_len.to_le_bytes());
    let sum = wal_checksum(offset, buf);
    buf.extend_from_slice(&sum.to_le_bytes());
}

/// Decode + verify the record starting at byte 0 of `buf`, which sits at file
/// offset `offset`. `page_count` bounds page ids and the page count. Returns
/// None for anything torn, truncated, or invalid — recovery stops there.
pub fn decode_wal_record(buf: &[u8], offset: u64, page_count: u64) -> Option<WalRecord<'_>> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(buf.get(o..o + 2)?.try_into().unwrap()));
    let u32_at = |o: usize| Some(u32::from_le_bytes(buf.get(o..o + 4)?.try_into().unwrap()));
    let u64_at = |o: usize| Some(u64::from_le_bytes(buf.get(o..o + 8)?.try_into().unwrap()));
    if u32_at(0)? != WAL_MAGIC {
        return None;
    }
    let txn_id = u64_at(4)?;
    let n_pages = u32_at(12)? as usize;
    if n_pages as u64 > page_count {
        return None; // a real commit cannot dirty more pages than exist
    }
    let rec_len = u32_at(16)? as usize;
    // A record is at least the fixed overhead and at most one FULL entry per
    // page — a corrupt length outside that band is rejected before allocating.
    if rec_len < WAL_RECORD_FIXED || rec_len > WAL_RECORD_FIXED + n_pages * WAL_PAGE_ENTRY {
        return None;
    }
    if buf.len() < rec_len {
        return None; // truncated tail
    }
    let body = &buf[..rec_len - 8];
    if wal_checksum(offset, body) != u64_at(rec_len - 8)? {
        return None;
    }
    let mut pages = Vec::with_capacity(n_pages);
    let mut pos = WAL_HDR_LEN;
    for _ in 0..n_pages {
        let id = u64_at(pos)?;
        if id >= page_count {
            return None; // checksum-valid but foreign/corrupt: not our record
        }
        let enc = *buf.get(pos + 8)?;
        pos += 9;
        match enc {
            WAL_ENC_FULL => {
                let img = buf.get(pos..pos + PAGE_SIZE)?;
                pages.push((id, WalPageImg::Full(img)));
                pos += PAGE_SIZE;
            }
            WAL_ENC_SPLIT => {
                let prefix_len = u16_at(pos)? as usize;
                let suffix_start = u16_at(pos + 2)? as usize;
                pos += 4;
                if prefix_len > suffix_start || suffix_start > PAGE_SIZE {
                    return None;
                }
                let suffix_len = PAGE_SIZE - suffix_start;
                let prefix = buf.get(pos..pos + prefix_len)?;
                pos += prefix_len;
                let suffix = buf.get(pos..pos + suffix_len)?;
                pos += suffix_len;
                pages.push((
                    id,
                    WalPageImg::Split {
                        prefix,
                        suffix_start,
                        suffix,
                    },
                ));
            }
            _ => return None,
        }
    }
    // The page entries plus the meta+checksum trailer must account for exactly
    // rec_len — reject a length that disagrees with the parsed structure.
    if pos + WAL_TRAILER_LEN != rec_len {
        return None;
    }
    Some(WalRecord {
        txn_id,
        pages,
        catalog_root: u64_at(pos)?,
        freelist_root: u64_at(pos + 8)?,
        high_water: u64_at(pos + 16)?,
        extent_map_root: u64_at(pos + 24)?,
        len: rec_len as u64,
    })
}

/// Powerloss-sim instrumentation: append `(start_page, npages)` pairs of
/// extent ranges whose msync RETURNED — i.e. ranges that survive real power
/// loss — to the file named by `MPEDB_EXTENT_SYNC_LOG`, fdatasync'd so the
/// log itself is as durable as what it records. No-op when unset. Test-only
/// by construction (one env lookup, cached).
pub fn extent_sync_log(ranges: &[(u64, u32)]) -> Result<()> {
    // wasm32: no env to read the opt-in from and no file to append to. The
    // native body is unreachable rather than stubbed — this log records a
    // durability barrier, and the private path promises no durability at all.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = ranges;
        return Ok(());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
    use std::io::Write;
    use std::sync::OnceLock;
    static LOG: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let Some(path) = LOG.get_or_init(|| std::env::var_os("MPEDB_EXTENT_SYNC_LOG").map(Into::into))
    else {
        return Ok(());
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|_| io_err("open(extent sync log)"))?;
    let mut buf = Vec::with_capacity(ranges.len() * 12);
    for &(start, npages) in ranges {
        buf.extend_from_slice(&start.to_le_bytes());
        buf.extend_from_slice(&npages.to_le_bytes());
    }
    f.write_all(&buf).map_err(|_| io_err("write(extent sync log)"))?;
    #[cfg(unix)]
    let raw = std::os::fd::AsRawFd::as_raw_fd(&f);
    #[cfg(windows)]
    let raw = crate::wincompat::AsRawFd::as_raw_fd(&f);
    if crate::os::fdatasync(raw) != 0 {
        return Err(io_err("fdatasync(extent sync log)"));
    }
    Ok(())
    }
}

/// Per-process handle on the WAL file. `alloc` caches the preallocated
/// (logical) size — stale-low is harmless (we re-fstat before growing).
struct WalFile {
    file: File,
    alloc: AtomicU64,
}

/// A validated snapshot of one meta slot's per-commit fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaSnapshot {
    pub slot: u64, // which meta page it came from
    pub txn_id: u64,
    pub catalog_root: u64,
    pub freelist_root: u64,
    pub high_water: u64,
    /// Root of the extent map (DESIGN-BLOBEXTENT §3.2); 0 = empty tree.
    pub extent_map_root: u64,
    /// Schema generation (#47): 0 at seed, +1 per DDL commit. Writers compare
    /// against their cached value and reload the schema from the catalog on
    /// mismatch; the frozen M_SCHEMA_HASH never changes (it is the seed hash).
    pub schema_gen: u64,
}

/// Free marker for [`Shm::priv_pins`] (private in-memory reader pins).
const PRIV_PIN_FREE: u64 = u64::MAX;
/// Capacity of the process-local private pin table (`:memory:` only).
const PRIV_PIN_SLOTS: usize = 64;

// Nesting depth of exclusive in-place write on this thread. Non-zero means
// claim_and_pin_private must not spin on exclusive_write (avoids deadlock when
// the writer itself opens a nested read for triggers/catalog).
std::thread_local! {
    static EXCLUSIVE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Distinguishes concurrent writers' half-written files within one process.
/// Only ever part of a temporary name, so it never reaches anything durable.
static EXTERNAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// One payload on its way to becoming an object.
///
/// ## Why a rename is the whole crash argument
///
/// The bytes go to a temporary name and are hashed as they pass. Only once
/// they are all written and durable does the file get the name that says what
/// it is. So there is no instant at which an object exists under its own name
/// holding anything other than the bytes that name describes: a crash before
/// the rename leaves a `.writing-*` file that belongs to nobody and can be
/// swept, and a crash after it leaves a complete object.
///
/// That is a stronger guarantee than the in-file extent gets, and it comes for
/// free rather than by effort — an extent has to reach its final place as it is
/// written, because its place is what names it, so its integrity rests on the
/// ordering of an msync against a meta flip. An object is named by its
/// contents, so it can be assembled somewhere anonymous first.
///
/// The hash is computed on the way through rather than by reading the file
/// back, so a payload of any size costs one pass and no second resident copy.
pub struct ExternalWriter {
    // `std::fs::File` SPELLED OUT, not the ambient `File`. On wasm32 that alias
    // is `wasmcompat::File` — the anonymous memfd stand-in, which deliberately
    // has no path constructor because a tab has no filesystem — so the ambient
    // name did not compile for that target at all. An external payload is a
    // real object beside the database, so the real type is the right one, and
    // on wasm32 `std::fs` then refuses at RUNTIME: the same "return failure
    // rather than pretend success" the compat layer promises for `fchmod`.
    file: std::fs::File,
    temp: PathBuf,
    dir: PathBuf,
    hasher: xxhash_rust::xxh3::Xxh3,
    len: u64,
}

impl ExternalWriter {
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        self.file
            .write_all(bytes)
            .map_err(|_| io_err("write(external payload)"))?;
        self.hasher.update(bytes);
        self.len += bytes.len() as u64;
        Ok(())
    }

    /// Publish it, and hand back the reference the leaf cell will carry.
    pub fn finish(self) -> Result<(u128, u64)> {
        self.file
            .sync_all()
            .map_err(|_| io_err("fsync(external payload)"))?;
        let hash = self.hasher.digest128();
        let final_path = self.dir.join(format!("{hash:032x}.ext"));
        // Renaming over an object that is already there is not a conflict, it
        // is the same bytes arriving twice — which is exactly what content
        // addressing is for, and what makes writing the same page on two
        // devices cost one object instead of two.
        std::fs::rename(&self.temp, &final_path).map_err(|_| io_err("rename(external payload)"))?;
        // The rename itself has to survive, not just the bytes: a directory
        // entry lives in the directory, and an unsynced one can be lost while
        // the file it names is perfectly durable.
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok((hash, self.len))
    }
}

impl Drop for ExternalWriter {
    fn drop(&mut self) {
        // An abandoned payload leaves nothing behind. `finish` renamed the file
        // away, so this only ever removes one nobody claimed.
        let _ = std::fs::remove_file(&self.temp);
    }
}

pub struct Shm {
    map: *mut u8,
    len: usize,
    file: File,
    /// Where the database file is, which is the only thing that says where its
    /// external payloads are (`<path>.extents/`). The mapping cannot answer
    /// this — it is bytes, not a name — and an fd cannot portably be turned
    /// back into a path.
    path: PathBuf,
    pub page_count: u64,
    pub max_readers: u32,
    pub durability: Durability,
    pub data_start: u64,
    /// This process's start time, for reader-slot identity.
    my_pid_start: u64,
    /// Set when open() performed EOWNERDEAD/boot recovery (informational).
    pub recovered: bool,
    /// The append-only log; Some iff durability = wal.
    wal: Option<WalFile>,
    /// Process-private in-memory DB (`path = ":memory:"`): no multi-process
    /// attach. Reader pins use [`Self::priv_pins`] instead of the mapped
    /// multi-process reader table (cheaper: no CAS claim scan / SeqCst pin
    /// recheck). Multi-thread same-process still safe via the pin array.
    pub is_private: bool,
    /// Process-local pins for `is_private`: each entry is `PRIV_PIN_FREE` or a
    /// pinned `txn_id`. Index is returned as `ReadTxn.slot`.
    priv_pins: [AtomicU64; PRIV_PIN_SLOTS],
    /// Active index (0|1) into [`Self::priv_meta`] for private DBs. Readers
    /// Acquire-load this, then copy the snapshot — no dual-page xxh3.
    priv_meta_active: AtomicU64,
    /// Double-buffered published meta for `is_private`. Writer fills the
    /// inactive slot under the writer lock, then Release-stores the active
    /// index. Same-process only; Sync via exclusive writer + Acquire/Release.
    priv_meta: [UnsafeCell<MetaSnapshot>; 2],
    /// Private `:memory:` only: a write txn holds exclusive mutability (no
    /// *foreign* reader pins) and may mutate committed pages in place.
    /// Other threads spin while this is set; the owning thread may nest
    /// `begin_read` (trigger/catalog) via TLS depth. Cleared on writer unlock.
    exclusive_write: AtomicU64, // 0 = off, 1 = on
    /// macOS-only: the crash-safe writer lock (sidecar flock + ERRORCHECK
    /// mutex). Linux uses the robust pthread mutex in the mapped lock area.
    #[cfg(not(target_os = "linux"))]
    wl: crate::os::WriterLock,
}

// The raw pointer is to a MAP_SHARED region; all concurrent access goes
// through atomics or the writer lock as documented per method.
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.map as *mut libc::c_void, self.len);
        }
    }
}

fn io_err(context: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::Error::last_os_error().kind(),
        format!("{context}: {}", std::io::Error::last_os_error()),
    ))
}

/// Apply the configured file permissions to a freshly-formatted database file
/// (or its `-wal`). The file was born 0o600; this widens it to `perms.mode`
/// (default: leave it 0o600) and, if requested, `chown`s it. A configured
/// owner/group that cannot be applied is a hard error — a silently-unenforced
/// isolation boundary is worse than a loud failure (design/DESIGN-MULTIDB.md §1.4).
/// Windows: NTFS has no POSIX mode bits and no uid/gid, so `mode` is ignored —
/// a fresh file is already owner-private by inheritance, which is what `0o600`
/// asks for. A CONFIGURED `owner`/`group` is a hard error rather than a silent
/// no-op: the whole reason this function fails loudly on Unix is that an
/// unenforced isolation boundary is worse than a refused one
/// (design/DESIGN-MULTIDB.md §1.4), and that argument does not weaken by
/// changing platform.
#[cfg(windows)]
fn apply_file_perms(_fd: RawFd, perms: &FilePerms) -> Result<()> {
    if perms.owner.is_some() || perms.group.is_some() {
        return Err(Error::Config(
            "database.owner / database.group are POSIX concepts and cannot be applied on \
             Windows; remove them or run on Unix"
                .into(),
        ));
    }
    Ok(())
}

// `not(windows)`, not `unix`: wasm32 is neither, and `crate::wasmcompat::libc`
// shadows the real crate in this file with honest no-op `fchmod`/`fchown` —
// so the one body covers both. Splitting this on `unix` silently dropped the
// wasm32 target, which no test run in the workspace would have noticed.
#[cfg(not(windows))]
fn apply_file_perms(fd: RawFd, perms: &FilePerms) -> Result<()> {
    let mode = perms.mode.unwrap_or(0o600);
    if unsafe { libc::fchmod(fd, mode as libc::mode_t) } != 0 {
        return Err(io_err("fchmod (applying database file mode)"));
    }
    if perms.owner.is_some() || perms.group.is_some() {
        // (uid_t)-1 / (gid_t)-1 means "leave unchanged".
        let uid = match perms.owner.as_deref() {
            Some(s) => resolve_id(s, true)?,
            None => u32::MAX,
        };
        let gid = match perms.group.as_deref() {
            Some(s) => resolve_id(s, false)?,
            None => u32::MAX,
        };
        if unsafe { libc::fchown(fd, uid as libc::uid_t, gid as libc::gid_t) } != 0 {
            return Err(io_err("fchown (applying database file owner/group)"));
        }
    }
    Ok(())
}

/// Resolve a user (`is_user`) or group name to a numeric id. A purely-numeric
/// string is taken as the id directly; otherwise it is looked up via
/// `getpwnam_r`/`getgrnam_r` (thread-safe, buffer-growing on ERANGE).
///
/// Unix-only: Windows has no uid/gid, and `apply_file_perms` refuses a
/// configured owner/group there rather than reaching this.
#[cfg(unix)]
#[cfg(not(target_arch = "wasm32"))]
fn resolve_id(name: &str, is_user: bool) -> Result<u32> {
    if let Ok(n) = name.parse::<u32>() {
        return Ok(n);
    }
    let c_name = std::ffi::CString::new(name)
        .map_err(|_| Error::Config(format!("invalid owner/group name `{name}`")))?;
    let mut cap: usize = 4096;
    loop {
        let mut buf = vec![0 as libc::c_char; cap];
        let (rc, found, id) = if is_user {
            let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut res: *mut libc::passwd = std::ptr::null_mut();
            let rc = unsafe {
                libc::getpwnam_r(c_name.as_ptr(), &mut pwd, buf.as_mut_ptr(), cap, &mut res)
            };
            (rc, !res.is_null(), pwd.pw_uid)
        } else {
            let mut grp: libc::group = unsafe { std::mem::zeroed() };
            let mut res: *mut libc::group = std::ptr::null_mut();
            let rc = unsafe {
                libc::getgrnam_r(c_name.as_ptr(), &mut grp, buf.as_mut_ptr(), cap, &mut res)
            };
            (rc, !res.is_null(), grp.gr_gid)
        };
        if rc == libc::ERANGE {
            cap *= 2;
            continue;
        }
        if rc != 0 {
            return Err(Error::Io(std::io::Error::from_raw_os_error(rc)));
        }
        if !found {
            let kind = if is_user { "user" } else { "group" };
            return Err(Error::Config(format!("unknown {kind} `{name}`")));
        }
        return Ok(id);
    }
}

/// wasm32: there is no user/group database to resolve against. A numeric id
/// still parses; a NAME cannot be resolved, and inventing one would silently
/// misapply an isolation boundary — so it is refused. Unreachable in practice:
/// the only reachable path here is `:memory:`, which owns no file to chown.
#[cfg(target_arch = "wasm32")]
fn resolve_id(name: &str, is_user: bool) -> Result<u32> {
    if let Ok(n) = name.parse::<u32>() {
        return Ok(n);
    }
    let kind = if is_user { "user" } else { "group" };
    Err(Error::Config(format!(
        "cannot resolve {kind} name `{name}`: the wasm32 build has no user database \
         (use a numeric id)"
    )))
}

/// Overwrite `[from, to)` with literal zeros (1 MiB chunks). Used when
/// growing the WAL so appends land in already-written extents (see
/// `wal_ensure_alloc`); the region is always beyond every logged byte.
fn prezero(file: &File, from: u64, to: u64) -> Result<()> {
    const CHUNK: usize = 1 << 20;
    let zeros = vec![0u8; CHUNK];
    let mut off = from;
    while off < to {
        let n = ((to - off) as usize).min(CHUNK);
        file.write_all_at(&zeros[..n], off)?;
        off += n as u64;
    }
    Ok(())
}

/// pread as much of `buf` as the file has, retrying short reads; returns the
/// bytes read (< buf.len() only at EOF). The WAL recovery scan treats a short
/// read as a torn tail, never as an error.
fn read_full_at(file: &File, buf: &mut [u8], offset: u64) -> Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(done)
}

/// This process's start time; the pair (pid, start time) survives PID reuse.
/// Platform-specific (Linux `/proc`; macOS `sysctl`) — see `crate::os`.
fn proc_start_time(pid: u32) -> Option<u64> {
    crate::os::proc_start_time(pid)
}

/// Is `pid`, started at `recorded_start`, still the process we recorded?
///
/// Public because admission control (#150) needs exactly this judgement for
/// editor seats, and re-deriving it elsewhere is how the direction gets
/// inverted: **this errs toward ALIVE**. `EPERM` — a process owned by someone
/// else — reads as alive, because declaring a live process dead evicts a real
/// editor, while leaving a dead one costs a seat until its lease expires.
pub fn pid_is_alive(pid: u32, recorded_start: u64) -> bool {
    pid_alive_identity(pid, recorded_start)
}

/// This process's `/proc` start time, the second half of the identity that
/// makes pid reuse harmless.
///
/// Cached KEYED BY PID, not in a plain `OnceLock`: our own start time is a
/// per-process constant (one `/proc` read per open was part of the connect
/// cell's cost, #N2 0.2.9), but Python consumers FORK — a fork's child must
/// not inherit the parent's identity, so a pid change invalidates the slot.
/// Other pids' start times (recovery, reader scans) are never cached.
pub fn own_process_start_time() -> Option<u64> {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    // pid == 0 marks the slot empty; start == 0 encodes "read failed" so a
    // failure is re-tried rather than cached.
    static PID: AtomicU32 = AtomicU32::new(0);
    static START: AtomicU64 = AtomicU64::new(0);
    let me = crate::os::process_id();
    if PID.load(Ordering::Acquire) == me {
        let cached = START.load(Ordering::Acquire);
        if cached != 0 {
            return Some(cached);
        }
    }
    let fresh = proc_start_time(me);
    if let Some(v) = fresh {
        // Store the value BEFORE publishing the pid, so a racing reader that
        // matches on pid can only see the matching start time.
        START.store(v, Ordering::Release);
        PID.store(me, Ordering::Release);
    }
    fresh
}

fn pid_alive_identity(pid: u32, recorded_start: u64) -> bool {
    if crate::os::pid_definitely_dead(pid) {
        return false;
    }
    // pid exists — but is it the same incarnation?
    match proc_start_time(pid) {
        Some(st) => st == recorded_start,
        // Raced with its own exit (died between the liveness check and the
        // start-time read): dead only if a second check agrees.
        None => !crate::os::pid_definitely_dead(pid),
    }
}

/// Hard error on failure: a degraded process must refuse to attach rather
/// than proceed with a zero identity — a zeroed boot id would trigger
/// spurious boot recovery (mutex re-init + reader-table wipe) on a LIVE
/// database, and a zeroed ns would defeat the namespace check.
fn my_pid_ns_ino() -> Result<u64> {
    crate::os::pid_namespace_id()
        .ok_or_else(|| Error::Config("cannot read PID-namespace identity".into()))
}

/// Hard error on failure (see [`my_pid_ns_ino`]).
fn boot_id() -> Result<[u8; 16]> {
    crate::os::boot_id().ok_or_else(|| Error::Config("cannot read boot identity".into()))
}

/// Hard error on failure (see [`my_pid_ns_ino`]). This is a PREDICATE, not a
/// comparison — [`crate::os::boot_id_matches`] documents why the two are not
/// the same question on every platform.
fn boot_id_matches(stored: &[u8; 16]) -> Result<bool> {
    crate::os::boot_id_matches(stored)
        .ok_or_else(|| Error::Config("cannot read boot identity".into()))
}

struct FlockGuard<'a>(&'a File);

impl<'a> FlockGuard<'a> {
    fn exclusive(f: &'a File) -> Result<FlockGuard<'a>> {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io_err("flock"));
        }
        Ok(FlockGuard(f))
    }
}

impl Drop for FlockGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}


#[cfg(test)]
mod tests;
