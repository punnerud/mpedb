//! The shared-memory layer: file creation/attach, the lock area, meta pages,
//! and the reader table. Implements DESIGN.md §3–§4 exactly; every protocol
//! here was adversarially reviewed — read the design before changing ordering
//! or lock semantics.
//!
//! Safety model: all cross-process mutable state is accessed through atomics
//! or under the robust writer mutex. We never form `&`/`&mut` references to
//! memory another process may concurrently mutate non-atomically; page slices
//! handed out by transactions cover only pages that are immutable for the
//! borrow's duration (committed pages pinned by MVCC, or dirty pages owned
//! exclusively by the writer holding the lock).

use mpedb_types::{Durability, Error, FilePerms, Result, PAGE_SIZE};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

pub const FORMAT_VERSION: u32 = 2; // v2: intent-ring region between reader table and data
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
const M_CHECKSUM: usize = 96; // AtomicU64 (xxh3 of logical bytes 0..96)
const META_LOGICAL_LEN: usize = 96;

// ---- lock area field offsets (page 2) ----
const LA_INIT_STATE: usize = 0; // AtomicU32: 0 empty, 1 formatting, 2 READY
const LA_MUTEX: usize = 64; // pthread_mutex_t, 128 bytes reserved
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
const LA_OPT_RING: usize = 256;
/// Committed-footprint ring capacity (also the max snapshot-age a validator can
/// trust before conservatively conflicting).
pub const OPT_RING_SLOTS: u64 = 64;
const OPT_RING_ENTRY: usize = 32; // txn_id ‖ kind ‖ table_bits ‖ key_hash
// entry field offsets
const OFP_TXN: usize = 0;
const OFP_KIND: usize = 8;
const OFP_TBITS: usize = 16;
const OFP_KHASH: usize = 24;
/// Footprint kinds recorded per committed txn.
pub const OFP_KIND_EMPTY: u64 = 0; // touched no user table (catalog/sys only)
pub const OFP_KIND_POINT: u64 = 1; // exactly one table, one PK (table_bits+key_hash)
pub const OFP_KIND_TABLE: u64 = 2; // table-level write set (table_bits), any keys

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

/// Intent-ring region (DESIGN.md §5.3): fixed geometry, directly after the
/// reader table.
pub const RING_SLOTS: u32 = 256;
pub const RING_SLOT_SIZE: usize = 1024;
pub const RING_PAGES: u64 = (RING_SLOTS as usize * RING_SLOT_SIZE / PAGE_SIZE) as u64;

pub fn ring_start_page(max_readers: u32) -> u64 {
    READER_TABLE_PAGE + reader_table_pages(max_readers)
}

pub fn data_start_page(max_readers: u32) -> u64 {
    ring_start_page(max_readers) + RING_PAGES
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

// ---- write-ahead log (durability = wal / async, DESIGN.md §5.4) ----
//
// The WAL is a separate append-only file at `<db-path>-wal`, shared by both
// wal-class modes: `wal` (fdatasync per commit, durable-on-ack) and `async`
// (deferred/coalesced fdatasync, crash-consistent — §5.4.2). Record layout
// (all fields little-endian):
//
// ```text
//   0   magic        u32   WAL_MAGIC ("WAL2")
//   4   txn_id       u64
//  12   n_pages      u32
//  16   rec_len      u32   total record length in bytes (header..=checksum)
//  20   n_pages × page_entry (variable length — see below)
//   +   catalog_root u64 ┐
//   +   freelist_root u64 │ the commit's MetaSnapshot body
//   +   high_water   u64 ┘
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
// SPLIT is the "lean record" encoding (DESIGN.md §5.4.1): a B+tree node uses
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

pub const WAL_MAGIC: u32 = 0x324C_4157; // "WAL2" (lean-record framing)
/// Fixed record overhead: magic + txn + n_pages + rec_len + 3 meta + checksum.
pub const WAL_RECORD_FIXED: usize = 4 + 8 + 4 + 4 + 3 * 8 + 8;
/// Bytes of a FULL page entry: page id + enc byte + page image. A SPLIT entry
/// is always smaller; this is the per-page upper bound used to size the read
/// buffer and to bound `rec_len` on decode.
pub const WAL_PAGE_ENTRY: usize = 8 + 1 + PAGE_SIZE;
const WAL_ENC_FULL: u8 = 0;
const WAL_ENC_SPLIT: u8 = 1;
/// Byte offset of the fixed 20-byte record header prefix (magic..rec_len).
const WAL_HDR_LEN: usize = 4 + 8 + 4 + 4;
/// The meta+checksum trailer after the last page entry.
const WAL_TRAILER_LEN: usize = 3 * 8 + 8;
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
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(WAL_RECORD_FIXED + pages.len() * 256);
    buf.extend_from_slice(&WAL_MAGIC.to_le_bytes());
    buf.extend_from_slice(&txn_id.to_le_bytes());
    buf.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // rec_len placeholder (byte 16)
    for &(id, img) in pages {
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
    buf.extend_from_slice(&catalog_root.to_le_bytes());
    buf.extend_from_slice(&freelist_root.to_le_bytes());
    buf.extend_from_slice(&high_water.to_le_bytes());
    // rec_len includes the not-yet-appended 8-byte checksum, and is part of
    // the checksum preimage so a torn/altered length cannot validate.
    let rec_len = (buf.len() + 8) as u32;
    buf[16..20].copy_from_slice(&rec_len.to_le_bytes());
    let sum = wal_checksum(offset, &buf);
    buf.extend_from_slice(&sum.to_le_bytes());
    buf
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
        len: rec_len as u64,
    })
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
}

pub struct Shm {
    map: *mut u8,
    len: usize,
    file: File,
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
/// isolation boundary is worse than a loud failure (DESIGN-MULTIDB.md §1.4).
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

fn pid_alive_identity(pid: u32, recorded_start: u64) -> bool {
    let alive = unsafe { libc::kill(pid as i32, 0) };
    if alive != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::ESRCH {
            return false; // definitely dead
        }
        // EPERM or anything else: process exists → alive (never sweep on EPERM)
        return true;
    }
    // pid exists — but is it the same incarnation?
    match proc_start_time(pid) {
        Some(st) => st == recorded_start,
        // /proc race (died between kill and read): treat as dead only if
        // kill confirms
        None => unsafe { libc::kill(pid as i32, 0) == 0 },
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

impl Shm {
    // ---------- raw access helpers ----------

    #[inline]
    fn base(&self) -> *mut u8 {
        self.map
    }

    #[inline]
    fn at(&self, offset: usize) -> *mut u8 {
        debug_assert!(offset < self.len);
        unsafe { self.base().add(offset) }
    }

    #[inline]
    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        debug_assert!(offset.is_multiple_of(8));
        unsafe { &*(self.at(offset) as *const AtomicU64) }
    }

    #[inline]
    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        debug_assert!(offset.is_multiple_of(4));
        unsafe { &*(self.at(offset) as *const AtomicU32) }
    }

    /// Read-only view of a page. Caller contract (MVCC): the page must be
    /// immutable for the borrow's duration — either committed and pinned, or
    /// dirty and owned by the calling writer.
    pub fn page(&self, id: u64) -> Result<&[u8]> {
        if id >= self.page_count {
            return Err(Error::Corrupt(format!("page id {id} out of bounds")));
        }
        Ok(unsafe {
            std::slice::from_raw_parts(self.at(id as usize * PAGE_SIZE), PAGE_SIZE)
        })
    }

    /// Mutable view of a page. Caller contract: writer lock held AND the page
    /// is unreachable from any committed meta (freshly allocated this txn).
    #[allow(clippy::mut_from_ref)]
    pub fn page_mut_unchecked(&self, id: u64) -> Result<&mut [u8]> {
        if id >= self.page_count {
            return Err(Error::Corrupt(format!("page id {id} out of bounds")));
        }
        Ok(unsafe {
            std::slice::from_raw_parts_mut(self.at(id as usize * PAGE_SIZE), PAGE_SIZE)
        })
    }

    // ---------- meta pages ----------

    fn meta_off(slot: u64) -> usize {
        slot as usize * PAGE_SIZE
    }

    /// Assemble the checksum preimage from explicitly loaded field values
    /// (never a raw memcpy of the page, which could mix torn writes).
    fn meta_checksum_input(&self, slot: u64, s: &MetaSnapshot) -> [u8; META_LOGICAL_LEN] {
        let base = Self::meta_off(slot);
        let mut buf = [0u8; META_LOGICAL_LEN];
        // init-frozen fields are stable after the init handshake: plain copy
        unsafe {
            std::ptr::copy_nonoverlapping(self.at(base), buf.as_mut_ptr(), M_TXN_ID);
        }
        buf[M_TXN_ID..M_TXN_ID + 8].copy_from_slice(&s.txn_id.to_le_bytes());
        buf[M_CATALOG_ROOT..M_CATALOG_ROOT + 8].copy_from_slice(&s.catalog_root.to_le_bytes());
        buf[M_FREELIST_ROOT..M_FREELIST_ROOT + 8]
            .copy_from_slice(&s.freelist_root.to_le_bytes());
        buf[M_HIGH_WATER..M_HIGH_WATER + 8].copy_from_slice(&s.high_water.to_le_bytes());
        buf
    }

    /// Try to read one meta slot; None if its checksum does not validate.
    fn read_meta_slot(&self, slot: u64) -> Option<MetaSnapshot> {
        let base = Self::meta_off(slot);
        // Acquire on the checksum pairs with the Release store in
        // write_meta_slot, making all data pages of that commit visible.
        let checksum = self.atomic_u64(base + M_CHECKSUM).load(Ordering::Acquire);
        let snap = MetaSnapshot {
            slot,
            txn_id: self.atomic_u64(base + M_TXN_ID).load(Ordering::Relaxed),
            catalog_root: self.atomic_u64(base + M_CATALOG_ROOT).load(Ordering::Relaxed),
            freelist_root: self
                .atomic_u64(base + M_FREELIST_ROOT)
                .load(Ordering::Relaxed),
            high_water: self.atomic_u64(base + M_HIGH_WATER).load(Ordering::Relaxed),
        };
        let expect = xxhash_rust::xxh3::xxh3_64(&self.meta_checksum_input(slot, &snap));
        // A torn read (fields from two different commits) fails the checksum
        // and the slot is skipped; the double buffer guarantees the other
        // slot is a complete older commit.
        if expect == checksum {
            Some(snap)
        } else {
            None
        }
    }

    /// The newest valid committed meta. In durability=commit and wal modes,
    /// gated by the durable_txn watermark so no process observes a commit
    /// that a power failure could still erase (identical gate semantics in
    /// both durable modes; wal advances the watermark after its fdatasync).
    pub fn newest_meta(&self) -> Result<MetaSnapshot> {
        if !matches!(self.durability, Durability::Commit | Durability::Wal) {
            return self.newest_meta_gated(u64::MAX);
        }
        // The durable gate is monotone, but a reader racing two consecutive
        // durable commits can load a STALE gate and then find both slots
        // newer than it — a spurious "no valid meta". Reload the gate and
        // retry: a fresh gate only ever admits more, and the newest slot is
        // always <= the gate its committer advanced.
        let mut last_gate = 0;
        for _ in 0..64 {
            let gate = self.durable_txn().load(Ordering::Acquire);
            match self.newest_meta_gated(gate) {
                Ok(m) => return Ok(m),
                Err(e) => {
                    if gate == last_gate {
                        return Err(e); // stable: genuine corruption
                    }
                    last_gate = gate;
                }
            }
            std::hint::spin_loop();
        }
        Err(Error::Corrupt(
            "no durably-gated meta page after retries".into(),
        ))
    }

    fn newest_meta_gated(&self, gate: u64) -> Result<MetaSnapshot> {
        let a = self.read_meta_slot(META_PAGE_A).filter(|m| m.txn_id <= gate);
        let b = self.read_meta_slot(META_PAGE_B).filter(|m| m.txn_id <= gate);
        match (a, b) {
            (Some(a), Some(b)) => Ok(if a.txn_id >= b.txn_id { a } else { b }),
            (Some(m), None) | (None, Some(m)) => Ok(m),
            (None, None) => Err(Error::Corrupt(
                "no valid meta page (both checksums invalid)".into(),
            )),
        }
    }

    /// Ignore the durability gate (used by recovery under the writer lock).
    fn newest_meta_ungated(&self) -> Result<MetaSnapshot> {
        let a = self.read_meta_slot(META_PAGE_A);
        let b = self.read_meta_slot(META_PAGE_B);
        match (a, b) {
            (Some(a), Some(b)) => Ok(if a.txn_id >= b.txn_id { a } else { b }),
            (Some(m), None) | (None, Some(m)) => Ok(m),
            (None, None) => Err(Error::Corrupt(
                "no valid meta page (both checksums invalid)".into(),
            )),
        }
    }

    /// Publish a commit into the alternate meta slot. Caller: writer lock
    /// held; all COW data pages already written via plain stores.
    /// Ordering per DESIGN.md §4.1: fence(Release) → body fields (Relaxed) →
    /// checksum (Release).
    pub fn write_meta_slot(&self, prev_slot: u64, s: &MetaSnapshot) -> u64 {
        let slot = 1 - prev_slot;
        let base = Self::meta_off(slot);
        fence(Ordering::Release); // orders the plain data-page stores
        self.atomic_u64(base + M_TXN_ID).store(s.txn_id, Ordering::Relaxed);
        self.atomic_u64(base + M_CATALOG_ROOT)
            .store(s.catalog_root, Ordering::Relaxed);
        self.atomic_u64(base + M_FREELIST_ROOT)
            .store(s.freelist_root, Ordering::Relaxed);
        self.atomic_u64(base + M_HIGH_WATER)
            .store(s.high_water, Ordering::Relaxed);
        let mut stamped = *s;
        stamped.slot = slot;
        let sum = xxhash_rust::xxh3::xxh3_64(&self.meta_checksum_input(slot, &stamped));
        self.atomic_u64(base + M_CHECKSUM).store(sum, Ordering::Release);
        slot
    }

    // ---------- lock area ----------

    fn lock_area_off(field: usize) -> usize {
        LOCK_PAGE as usize * PAGE_SIZE + field
    }

    fn mutex_ptr(&self) -> *mut libc::pthread_mutex_t {
        self.at(Self::lock_area_off(LA_MUTEX)) as *mut libc::pthread_mutex_t
    }

    pub fn durable_txn(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_DURABLE_TXN))
    }

    pub fn oldest_pinned_cache(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_OLDEST_PINNED))
    }

    /// Bytes of durable log (advanced only after fdatasync; §5.4 wal).
    pub fn wal_len(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_WAL_LEN))
    }

    /// Log offset below which records are checkpointed into the main file.
    pub fn wal_ckpt(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_WAL_CKPT))
    }

    /// Append cursor: bytes written to the log, synced or not (§5.4.2 async).
    /// Advanced under the writer lock by `wal_append_async`; read by the
    /// deferred flusher. Unused (stays 0) outside `async`.
    pub fn wal_appended(&self) -> &AtomicU64 {
        self.atomic_u64(Self::lock_area_off(LA_WAL_APPENDED))
    }

    fn init_state(&self) -> &AtomicU32 {
        self.atomic_u32(Self::lock_area_off(LA_INIT_STATE))
    }

    /// Acquire the global writer mutex. Returns `true` if EOWNERDEAD recovery
    /// ran (a previous writer died holding the lock). Recovery per §5.2:
    /// msync both meta pages, refresh durable_txn — nothing else, by COW
    /// construction.
    pub fn writer_lock(&self) -> Result<bool> {
        let rc = unsafe { libc::pthread_mutex_lock(self.mutex_ptr()) };
        match rc {
            0 => Ok(false),
            libc::EOWNERDEAD => {
                unsafe { crate::os::mutex_make_consistent(self.mutex_ptr()) };
                if let Err(e) = self.recover_after_owner_death() {
                    // we DO hold the (now-consistent) mutex; never leak it
                    self.writer_unlock();
                    return Err(e);
                }
                Ok(true)
            }
            libc::EDEADLK => Err(Error::Internal(
                "writer lock re-entered by its owner (nested write transaction)".into(),
            )),
            _ => Err(Error::Internal(format!("pthread_mutex_lock failed: {rc}"))),
        }
    }

    /// Non-blocking writer-lock attempt: Ok(Some(recovered)) on success,
    /// Ok(None) if another process holds it.
    pub fn try_writer_lock(&self) -> Result<Option<bool>> {
        let rc = unsafe { libc::pthread_mutex_trylock(self.mutex_ptr()) };
        match rc {
            0 => Ok(Some(false)),
            libc::EBUSY => Ok(None),
            libc::EOWNERDEAD => {
                unsafe { crate::os::mutex_make_consistent(self.mutex_ptr()) };
                if let Err(e) = self.recover_after_owner_death() {
                    self.writer_unlock();
                    return Err(e);
                }
                Ok(Some(true))
            }
            libc::EDEADLK => Err(Error::Internal(
                "writer lock re-entered by its owner (nested write transaction)".into(),
            )),
            _ => Err(Error::Internal(format!("pthread_mutex_trylock failed: {rc}"))),
        }
    }

    /// Base byte offset of intent-ring slot `i` (see `crate::ring`).
    pub fn ring_slot_off(&self, i: u32) -> usize {
        debug_assert!(i < RING_SLOTS);
        ring_start_page(self.max_readers) as usize * PAGE_SIZE + i as usize * RING_SLOT_SIZE
    }

    /// Atomic accessor into the mapping at an absolute byte offset (used by
    /// the ring; offset must be 8-aligned and in-bounds).
    pub fn atomic_u64_at(&self, offset: usize) -> &AtomicU64 {
        self.atomic_u64(offset)
    }

    pub fn atomic_u32_at(&self, offset: usize) -> &AtomicU32 {
        self.atomic_u32(offset)
    }

    /// Plain byte access for owner-exclusive ring payload areas.
    /// Caller contract: the region is written only by the slot owner between
    /// RESERVED and READY, and read only by the lock-holding leader.
    #[allow(clippy::mut_from_ref)]
    pub fn bytes_at_unchecked(&self, offset: usize, len: usize) -> &mut [u8] {
        debug_assert!(offset + len <= self.len);
        unsafe { std::slice::from_raw_parts_mut(self.at(offset), len) }
    }

    pub fn my_pid_start(&self) -> u64 {
        self.my_pid_start
    }

    pub fn writer_unlock(&self) {
        unsafe {
            libc::pthread_mutex_unlock(self.mutex_ptr());
        }
    }

    fn recover_after_owner_death(&self) -> Result<()> {
        match self.durability {
            // Re-establish the double-buffer durability invariant: the dead
            // writer may have published a meta it never msynced. Without this
            // msync, the NEXT commit overwrites the other slot — the only one
            // holding a durable copy of the last acknowledged commit — and a
            // torn write to it during power loss would regress the database
            // below the durable watermark (§5.2).
            Durability::Commit => self.msync_range(0, 2 * PAGE_SIZE)?,
            // WAL-class modes (wal AND async) do NOT need the meta-page msync,
            // checked against the same §5.2 invariant: power-loss recovery
            // replays the log, not the mapping metas. An acknowledged (wal) or
            // appended (async) commit's pages AND meta fields live in a log
            // record at ≥ wal_ckpt BEFORE its meta slot is written in the
            // mapping, and wal_ckpt only advances after a full-mapping MS_SYNC
            // that makes both meta slots durable (wal_checkpoint_if). So
            // overwriting a never-msynced meta slot cannot lose a recorded
            // commit — wal_recover() reconstructs it from the log. (For async
            // the "acknowledged" set is only the deferred-flushed prefix; a
            // dead writer's un-flushed tail is the mode's declared loss window,
            // §5.4.2 — not something the double buffer ever protected.)
            Durability::Wal | Durability::Async | Durability::None => {}
        }
        // Refresh durable_txn from the newest valid meta. Sound in wal mode
        // for the same reason: a meta exists in the mapping only after its
        // record's fdatasync returned, so the newest mapping meta is always
        // durable-by-log and may be acknowledged to readers.
        let newest = self.newest_meta_ungated()?;
        self.durable_txn().fetch_max(newest.txn_id, Ordering::Release);
        Ok(())
    }

    pub fn msync_range(&self, offset: usize, len: usize) -> Result<()> {
        // round down to page boundary as required by msync
        let start = offset & !(PAGE_SIZE - 1);
        let len = len + (offset - start);
        let rc = unsafe {
            libc::msync(
                self.at(start) as *mut libc::c_void,
                len,
                libc::MS_SYNC,
            )
        };
        if rc != 0 {
            return Err(io_err("msync"));
        }
        Ok(())
    }

    pub fn msync_page(&self, id: u64) -> Result<()> {
        self.msync_range(id as usize * PAGE_SIZE, PAGE_SIZE)
    }

    // ---------- write-ahead log (durability = wal) ----------

    fn wal_file(&self) -> Result<&WalFile> {
        self.wal
            .as_ref()
            .ok_or_else(|| Error::Internal("wal operation on a non-wal database".into()))
    }

    /// Open (creating if absent) the companion `<path>-wal` file. Called on
    /// every attach path of a wal-mode database, before format/recovery.
    fn attach_wal(&mut self, db_path: &Path) -> Result<()> {
        debug_assert!(self.durability.uses_wal());
        // Born owner-only for the same reason as the main file; the formatter
        // widens it to the configured mode (see `Shm::open`).
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(wal_path(db_path))?;
        let alloc = file.metadata()?.len();
        self.wal = Some(WalFile {
            file,
            alloc: AtomicU64::new(alloc),
        });
        Ok(())
    }

    /// Format-time reset: drop any debris from a previous incarnation. A
    /// fresh database starts with wal_ckpt = wal_len = 0 (the zeroed lock
    /// area), so stale checksum-valid records at low offsets would otherwise
    /// be replayed into the new database on the first post-reboot attach.
    fn wal_reset_for_format(&self) -> Result<()> {
        let wal = self.wal_file()?;
        let fd = wal.file.as_raw_fd();
        if unsafe { libc::ftruncate(fd, 0) } != 0 {
            return Err(io_err("ftruncate(wal)"));
        }
        // real preallocation, never sparse-append (ENOSPC surfaces here)
        if crate::os::preallocate(fd, 0, WAL_GROW_CHUNK as i64) != 0 {
            return Err(io_err("fallocate(wal)"));
        }
        prezero(&wal.file, 0, WAL_GROW_CHUNK)?;
        wal.alloc.store(WAL_GROW_CHUNK, Ordering::Release);
        wal.file.sync_all()?; // the empty log must be durable before READY
        Ok(())
    }

    /// Grow the preallocated region to cover `need` bytes. The cached alloc
    /// may be stale-low (another process grew the file), so re-fstat before
    /// allocating and extend only the true tail — never re-allocate blocks
    /// below the checkpoint that a hole punch reclaimed.
    fn wal_ensure_alloc(&self, need: u64) -> Result<()> {
        let wal = self.wal_file()?;
        if need <= wal.alloc.load(Ordering::Acquire) {
            return Ok(());
        }
        let cur = wal.file.metadata()?.len();
        let target = need.max(cur).div_ceil(WAL_GROW_CHUNK) * WAL_GROW_CHUNK;
        if target > cur {
            let rc = crate::os::preallocate(wal.file.as_raw_fd(), cur as i64, (target - cur) as i64);
            if rc != 0 {
                return Err(io_err("fallocate(wal grow)"));
            }
            // Convert the fresh extents from "unwritten" to "written" once,
            // here, instead of on every append: an fdatasync over an append
            // into unwritten extents must journal the extent conversion,
            // which measured 958 µs vs 350 µs per append+fdatasync on this
            // host's ext4 — 2.7× the whole commit-path flush cost.
            prezero(&wal.file, cur, target)?;
        }
        wal.alloc.store(target, Ordering::Release);
        Ok(())
    }

    /// Append + fdatasync one commit record; advance `wal_len` only after the
    /// sync returns (§5.4 wal commit path). Caller: writer lock held, all COW
    /// page stores done, `dirty_sorted` are this commit's dirty page ids.
    ///
    /// The append offset is the trusted in-memory `wal_len`: it is only ever
    /// advanced post-fdatasync under the writer lock, so a successor after
    /// EOWNERDEAD simply appends over any torn/orphan bytes a dead writer
    /// left beyond it (such bytes belong to a commit that was never
    /// acknowledged — its fdatasync never returned or its meta never flipped).
    pub fn wal_commit(&self, dirty_sorted: &[u64], snap: &MetaSnapshot) -> Result<()> {
        let wal = self.wal_file()?;
        let off = self.wal_len().load(Ordering::Acquire);
        let mut pages = Vec::with_capacity(dirty_sorted.len());
        for &id in dirty_sorted {
            pages.push((id, self.page(id)?));
        }
        let buf = encode_wal_record(
            off,
            snap.txn_id,
            &pages,
            snap.catalog_root,
            snap.freelist_root,
            snap.high_water,
        );
        self.wal_ensure_alloc(off + buf.len() as u64)?;
        wal.file.write_all_at(&buf, off)?;
        if crate::os::fdatasync(wal.file.as_raw_fd()) != 0 {
            return Err(io_err("fdatasync(wal)"));
        }
        self.wal_len().store(off + buf.len() as u64, Ordering::Release);
        Ok(())
    }

    /// Append one commit record WITHOUT fdatasync (`durability = async`,
    /// §5.4.2). Advances the `wal_appended` cursor (the next append position),
    /// NOT `wal_len` (the durable watermark, which only the deferred flusher
    /// or a checkpoint advances). Caller: writer lock held, all COW page
    /// stores done. The record is crash-consistent the instant its bytes reach
    /// the kernel: a torn tail truncates it on recovery, a whole record may
    /// replay. It is NOT durable-on-ack — a power loss before the next flush
    /// loses it. `wal_appended` lives in the lock area so a successor after
    /// EOWNERDEAD appends after it, not over it.
    pub fn wal_append_async(&self, dirty_sorted: &[u64], snap: &MetaSnapshot) -> Result<()> {
        let wal = self.wal_file()?;
        let off = self.wal_appended().load(Ordering::Acquire);
        let mut pages = Vec::with_capacity(dirty_sorted.len());
        for &id in dirty_sorted {
            pages.push((id, self.page(id)?));
        }
        let buf = encode_wal_record(
            off,
            snap.txn_id,
            &pages,
            snap.catalog_root,
            snap.freelist_root,
            snap.high_water,
        );
        self.wal_ensure_alloc(off + buf.len() as u64)?;
        wal.file.write_all_at(&buf, off)?;
        self.wal_appended().store(off + buf.len() as u64, Ordering::Release);
        Ok(())
    }

    /// The deferred flush (`durability = async`, §5.4.2): fdatasync the log up
    /// to the current append cursor and publish that as the new durable
    /// watermark. Runs OFF the writer lock — writers append concurrently; we
    /// only ever claim `[0, a)` durable, and `a` was published (Release) after
    /// its pwrite completed, so fdatasync flushes those bytes. `wal_len`
    /// advances monotonically (`fetch_max`). Returns the bytes newly made
    /// durable (0 if already caught up). Callers: the background flusher on
    /// its interval, and clean shutdown.
    pub fn wal_flush_deferred(&self) -> Result<u64> {
        if self.durability != Durability::Async {
            return Ok(0);
        }
        let wal = self.wal_file()?;
        let appended = self.wal_appended().load(Ordering::Acquire);
        let durable = self.wal_len().load(Ordering::Acquire);
        if appended <= durable {
            return Ok(0);
        }
        if crate::os::fdatasync(wal.file.as_raw_fd()) != 0 {
            return Err(io_err("fdatasync(wal async flush)"));
        }
        let prev = self.wal_len().fetch_max(appended, Ordering::AcqRel);
        Ok(appended.saturating_sub(prev.max(durable)))
    }

    /// Checkpoint when the un-checkpointed log exceeds the configured
    /// threshold. Caller: writer lock held, after the meta flip.
    pub fn wal_maybe_checkpoint(&self) -> Result<()> {
        self.wal_checkpoint_if(wal_ckpt_threshold())
    }

    /// Checkpoint if `wal_len - wal_ckpt >= threshold` (§5.4 wal):
    ///
    /// 1. `msync` the WHOLE mapping (MS_SYNC): every commit ≤ the current
    ///    meta — data pages and both meta slots — is now durable in the main
    ///    file, so no log record below the current `wal_len` is needed for
    ///    recovery any more.
    /// 2. Advance `wal_ckpt = wal_len` and msync the lock page, so the new
    ///    checkpoint offset is durable BEFORE any log bytes below it are
    ///    reclaimed (a reboot recovery scans from the on-disk `wal_ckpt`;
    ///    reclaiming first could zero bytes a stale on-disk `wal_ckpt` still
    ///    points into).
    /// 3. Reclaim the space below the checkpoint with
    ///    `FALLOC_FL_PUNCH_HOLE | KEEP_SIZE` (best-effort).
    ///
    /// Deliberate deviation from the sketched "ftruncate + reset both to 0":
    /// punching a hole keeps `wal_len` strictly monotone, so a log offset is
    /// never reused across the file's lifetime. That removes an entire hazard
    /// class: no mixed-epoch (`wal_ckpt`, `wal_len`) pair is ever observable
    /// (a writer dying between the two zero-stores of a truncate-reset leaves
    /// exactly such a pair), and no stale-but-checksum-valid record can ever
    /// sit at an offset a later scan will visit — the offset-bound checksum
    /// plus monotone offsets make scan resync impossible by construction.
    /// Space cost is identical (the punched blocks are freed); the logical
    /// file size keeps growing but is sparse below `wal_ckpt`.
    pub fn wal_checkpoint_if(&self, threshold: u64) -> Result<()> {
        // `wal` checkpoints up to the durable watermark (every logged commit
        // was fdatasync'd before its meta flipped). `async` checkpoints up to
        // the APPEND cursor: the full-mapping msync below makes every appended
        // commit's pages+meta durable directly in the main file, which is a
        // strictly stronger durability than the deferred log fdatasync — so
        // those log bytes become redundant and `wal_len` may jump to the
        // checkpoint too (keeping wal_ckpt <= wal_len for the recovery
        // cross-check).
        let target = if self.durability == Durability::Async {
            self.wal_appended().load(Ordering::Acquire)
        } else {
            self.wal_len().load(Ordering::Acquire)
        };
        let ckpt = self.wal_ckpt().load(Ordering::Acquire);
        if target.saturating_sub(ckpt) < threshold {
            return Ok(());
        }
        self.msync_range(0, self.len)?; // 1. main file catches up to the log
        if self.durability == Durability::Async {
            self.wal_len().fetch_max(target, Ordering::AcqRel);
        }
        self.wal_ckpt().store(target, Ordering::Release);
        self.msync_page(LOCK_PAGE)?; // 2. durable ckpt before reclaim
        let wal = self.wal_file()?;
        // 3. best-effort space reclaim; failure (exotic fs / macOS) only means
        // the space is not reclaimed — correctness is unaffected.
        crate::os::punch_hole(wal.file.as_raw_fd(), 0, target as i64);
        Ok(())
    }

    /// Reboot-path WAL recovery (§5.4 wal). Caller: exclusive flock held, no
    /// other process attached (first attach after a reboot).
    ///
    /// After power loss the lock area itself is only as durable as the
    /// mapping, so nothing in it can be trusted except `wal_ckpt` — which is
    /// safe BY CONSTRUCTION to scan from: any value the on-disk `wal_ckpt`
    /// can hold was stored (program order) after a full-mapping MS_SYNC
    /// completed, so the main file durably contains every commit whose record
    /// lies below it. Records at ≥ `wal_ckpt` are exactly the commits the
    /// main file may be missing, and replaying page images is idempotent, so:
    /// scan from `wal_ckpt`, replay every checksum-valid record in order onto
    /// the mapping (page images + meta), stop at the first invalid/partial
    /// record (torn tail), msync, then set `wal_ckpt = wal_len =`
    /// end-of-valid-prefix.
    ///
    /// Sufficiency of scanning from `wal_ckpt` (the §5.4 invariant): the main
    /// file's durable state is always ≤ the log — a meta is flipped in the
    /// mapping only AFTER its record's fdatasync returned, so any meta the
    /// kernel may have written back has a durable log record; and by COW,
    /// every page whose content changed after the checkpoint txn was freshly
    /// allocated by a post-checkpoint commit and therefore appears in a
    /// record ≥ `wal_ckpt`. Replay therefore reconstructs a state ≥ anything
    /// the main file could hold, ending exactly at the newest durable commit.
    ///
    /// Returns the end of the valid prefix. Errors with `Corrupt` if the log
    /// ends BELOW the on-disk `wal_len` — every `wal_len` value that can
    /// reach disk was stored after its fdatasync returned, so bytes below it
    /// were durable once and their absence means the WAL file was truncated
    /// or replaced behind our back (acknowledged commits are gone; refusing
    /// is the honest outcome).
    pub fn wal_recover(&self) -> Result<u64> {
        if !self.durability.uses_wal() {
            return Ok(0);
        }
        let wal = self.wal_file()?;
        let start = self.wal_ckpt().load(Ordering::Acquire);
        let disk_len = self.wal_len().load(Ordering::Acquire);
        let mut off = start;
        let mut prev_txn: Option<u64> = None;
        let mut last_meta: Option<MetaSnapshot> = None;
        let mut header = [0u8; WAL_HDR_LEN];
        let mut buf: Vec<u8> = Vec::new();
        loop {
            if read_full_at(&wal.file, &mut header, off)? < header.len() {
                break; // EOF inside a header: torn tail
            }
            let n_pages = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            let rec_len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
            // Reject a bogus header before allocating: the length must sit in
            // the valid band (checked again, offset-bound, in decode).
            if u32::from_le_bytes(header[0..4].try_into().unwrap()) != WAL_MAGIC
                || n_pages as u64 > self.page_count
                || rec_len < WAL_RECORD_FIXED
                || rec_len > WAL_RECORD_FIXED + n_pages * WAL_PAGE_ENTRY
            {
                break;
            }
            buf.clear();
            buf.resize(rec_len, 0);
            if read_full_at(&wal.file, &mut buf, off)? < rec_len {
                break; // truncated record
            }
            let Some(rec) = decode_wal_record(&buf, off, self.page_count) else {
                break; // bad checksum / structure: end of valid prefix
            };
            if prev_txn.is_some_and(|p| rec.txn_id != p + 1) {
                break; // writers are serialized; txn ids are consecutive
            }
            // replay: page images below the data region would clobber the
            // control pages — a checksum-valid record from a foreign file
            // ends the prefix rather than corrupting this one
            if rec.pages.iter().any(|(id, _)| *id < self.data_start) {
                break;
            }
            for (id, img) in &rec.pages {
                img.write_into(self.page_mut_unchecked(*id)?);
            }
            prev_txn = Some(rec.txn_id);
            last_meta = Some(MetaSnapshot {
                slot: META_PAGE_A,
                txn_id: rec.txn_id,
                catalog_root: rec.catalog_root,
                freelist_root: rec.freelist_root,
                high_water: rec.high_water,
            });
            off += rec.len;
        }
        if off < disk_len {
            return Err(Error::Corrupt(format!(
                "wal valid prefix ends at byte {off} but the lock area records \
                 {disk_len} durable bytes — the wal file was truncated or replaced"
            )));
        }
        if let Some(m) = last_meta {
            // install the newest replayed meta into BOTH slots (like the
            // genesis write): the pre-recovery slot contents are unspecified
            self.write_meta_slot(META_PAGE_A, &m); // writes slot B
            self.write_meta_slot(META_PAGE_B, &m); // writes slot A
        }
        self.msync_range(0, self.len)?;
        self.wal_ckpt().store(off, Ordering::Release);
        self.wal_len().store(off, Ordering::Release);
        // async's append cursor restarts at the recovered prefix end (the
        // un-flushed tail beyond it was the declared loss window and is now
        // truncated); harmless to set in wal mode, where it stays unread.
        self.wal_appended().store(off, Ordering::Release);
        self.msync_page(LOCK_PAGE)?;
        Ok(off)
    }

    // ---------- reader table ----------

    fn slot_off(&self, idx: u32) -> usize {
        READER_TABLE_PAGE as usize * PAGE_SIZE + idx as usize * READER_SLOT_SIZE
    }

    fn slot_word(&self, idx: u32) -> &AtomicU64 {
        self.atomic_u64(self.slot_off(idx) + RS_WORD)
    }

    fn slot_txn(&self, idx: u32) -> &AtomicU64 {
        self.atomic_u64(self.slot_off(idx) + RS_TXN)
    }

    fn slot_pid_start(&self, idx: u32) -> &AtomicU64 {
        self.atomic_u64(self.slot_off(idx) + RS_PID_START)
    }

    /// Set in the pid half of a slot word while its claimer is still
    /// initializing txn/pid_start. Safe: Linux PID_MAX_LIMIT is 2^22, so
    /// bit 31 is never part of a real pid.
    const CLAIMING: u32 = 1 << 31;

    #[inline]
    fn pack(pid: u32, seq: u32) -> u64 {
        ((pid as u64) << 32) | seq as u64
    }

    #[inline]
    fn unpack(word: u64) -> (u32, u32) {
        ((word >> 32) as u32, word as u32)
    }

    /// Claim a reader slot and pin the current snapshot (DESIGN.md §4.3 pin
    /// protocol, with the paired-SeqCst-fence fix). Returns (slot index,
    /// owned word value, pinned meta).
    pub fn claim_and_pin(&self) -> Result<(u32, u64, MetaSnapshot)> {
        let pid = std::process::id();
        let (idx, word) = match self.claim_slot(pid) {
            Some(x) => x,
            None => {
                // slot exhaustion: sweep dead slots ourselves, then retry once
                self.sweep_dead_readers();
                self.claim_slot(pid).ok_or(Error::ReadersFull)?
            }
        };
        // pin loop; on any error the slot must not leak (a leaked slot owned
        // by a live process is unsweepable)
        loop {
            let meta = match self.newest_meta() {
                Ok(m) => m,
                Err(e) => {
                    self.release_slot(idx, word);
                    return Err(e);
                }
            };
            self.slot_txn(idx).store(meta.txn_id, Ordering::Release);
            // SC fence pairs with the writer's SC fence after lock acquisition;
            // forbids the store-buffering outcome where our pin store and the
            // writer's reader-table scan both pass each other.
            fence(Ordering::SeqCst);
            match self.newest_meta() {
                Ok(recheck) if recheck.txn_id == meta.txn_id => {
                    return Ok((idx, word, meta));
                }
                Ok(_) => continue, // a commit landed in the window: re-pin
                Err(e) => {
                    self.release_slot(idx, word);
                    return Err(e);
                }
            }
        }
    }

    /// Claim protocol (reviewed): every store into a slot must be owner-only.
    /// 1. CAS {0, s} → {pid|CLAIMING, s+1}: reservation and identity
    ///    publication are one atomic step.
    /// 2. As owner, initialize txn = u64::MAX and pid_start.
    /// 3. CAS {pid|CLAIMING, s+1} → {pid, s+1} to go live; if that fails the
    ///    slot was reclaimed from us — walk away without touching it.
    ///
    /// A stale txn value visible during step 1-2 is safe: it is ≤ the newest
    /// committed txn, so a concurrent oldest-pinned scan only becomes more
    /// conservative (delays reclaim, never corrupts).
    fn claim_slot(&self, pid: u32) -> Option<(u32, u64)> {
        static CLAIM_SALT: AtomicU32 = AtomicU32::new(0);
        let n = self.max_readers;
        // randomized start offset decorrelates claim scans across processes
        // and across threads within one process
        let salt = CLAIM_SALT.fetch_add(1, Ordering::Relaxed);
        let start = std::process::id()
            .wrapping_mul(2654435761)
            .wrapping_add(salt.wrapping_mul(40503))
            % n;
        for i in 0..n {
            let idx = (start + i) % n;
            // a benign CAS failure (racing claimer, sweep seq-bump) must not
            // abandon a still-free slot: retry a few times before moving on
            for _ in 0..4 {
                let w = self.slot_word(idx).load(Ordering::Acquire);
                let (pid_half, seq) = Self::unpack(w);
                if pid_half != 0 {
                    break; // genuinely occupied (or mid-claim): next slot
                }
                let claiming = Self::pack(pid | Self::CLAIMING, seq.wrapping_add(1));
                if self
                    .slot_word(idx)
                    .compare_exchange(w, claiming, Ordering::AcqRel, Ordering::Relaxed)
                    .is_err()
                {
                    continue; // lost the race; re-read, slot may still be free
                }
                // owner-only initialization while marked CLAIMING
                self.slot_txn(idx).store(u64::MAX, Ordering::Release);
                self.slot_pid_start(idx)
                    .store(self.my_pid_start, Ordering::Release);
                let owned = Self::pack(pid, seq.wrapping_add(1));
                if self
                    .slot_word(idx)
                    .compare_exchange(claiming, owned, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some((idx, owned));
                }
                // reclaimed from us mid-claim: never touch this slot again
                break;
            }
        }
        None
    }

    /// Release an owned slot. Fails with `SnapshotEvicted` semantics (returns
    /// false) if the generation no longer matches (eviction / theft).
    pub fn release_slot(&self, idx: u32, owned_word: u64) -> bool {
        let (_, seq) = Self::unpack(owned_word);
        self.slot_word(idx)
            .compare_exchange(
                owned_word,
                Self::pack(0, seq.wrapping_add(1)),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Verify an owned slot is still ours (long scans call this periodically).
    pub fn slot_still_owned(&self, idx: u32, owned_word: u64) -> bool {
        self.slot_word(idx).load(Ordering::Acquire) == owned_word
    }

    /// Free slots whose owning process is provably gone. Safe to run from any
    /// process at any time: every free is a generation-CAS of the exact word
    /// observed dead, so racing a re-claim is harmless.
    pub fn sweep_dead_readers(&self) {
        for idx in 0..self.max_readers {
            let w = self.slot_word(idx).load(Ordering::Acquire);
            let (pid_half, seq) = Self::unpack(w);
            if pid_half == 0 {
                continue;
            }
            let pid = pid_half & !Self::CLAIMING;
            let dead = if pid_half & Self::CLAIMING != 0 {
                // mid-claim: pid_start is not yet trustworthy, so only a
                // definite ESRCH counts as dead (a claimer dying in this
                // µs-window whose pid is instantly recycled leaks one slot
                // until that pid exits — accepted residual, it pins nothing)
                let kill_failed = unsafe { libc::kill(pid as i32, 0) } != 0;
                kill_failed
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            } else {
                let recorded_start = self.slot_pid_start(idx).load(Ordering::Acquire);
                !pid_alive_identity(pid, recorded_start)
            };
            if !dead {
                continue;
            }
            let _ = self.slot_word(idx).compare_exchange(
                w,
                Self::pack(0, seq.wrapping_add(1)),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }

    /// Compute the oldest pinned snapshot bound. Caller: writer lock held.
    /// `current_txn` is the newest committed txn (claimed-but-unpinned slots
    /// count as pinning it — they can never pin older). Publishes the result
    /// monotonically into the cache and returns it.
    pub fn compute_oldest_pinned(&self, current_txn: u64) -> u64 {
        // pairs with the readers' pin-protocol SC fence
        fence(Ordering::SeqCst);
        let mut oldest = current_txn;
        for idx in 0..self.max_readers {
            let w = self.slot_word(idx).load(Ordering::Acquire);
            let (pid, _) = Self::unpack(w);
            if pid == 0 {
                continue;
            }
            let t = self.slot_txn(idx).load(Ordering::Acquire);
            let effective = if t == u64::MAX { current_txn } else { t };
            oldest = oldest.min(effective);
        }
        self.oldest_pinned_cache().fetch_max(oldest, Ordering::AcqRel);
        oldest
    }

    // ---------- committed-footprint ring (optimistic concurrency) ----------

    #[inline]
    fn opt_field(&self, txn_id: u64, field: usize) -> &AtomicU64 {
        let slot = (txn_id % OPT_RING_SLOTS) as usize;
        self.atomic_u64(Self::lock_area_off(LA_OPT_RING) + slot * OPT_RING_ENTRY + field)
    }

    /// *Writer-lock holder.* Record this commit's footprint BEFORE the meta
    /// flip. `kind`/`table_bits`/`key_hash` describe what txn `txn_id` wrote.
    /// The txn_id field is stored LAST (Release) so a reader that sees it can
    /// trust the payload; in practice every read is also serialized by the
    /// writer mutex, which is a full barrier.
    pub fn opt_record(&self, txn_id: u64, kind: u64, table_bits: u64, key_hash: u64) {
        self.opt_field(txn_id, OFP_KIND).store(kind, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_TBITS).store(table_bits, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_KHASH).store(key_hash, Ordering::Relaxed);
        self.opt_field(txn_id, OFP_TXN).store(txn_id, Ordering::Release);
    }

    /// *Writer-lock holder.* First-committer-wins conflict test for an
    /// optimistic write of `(table_id, key_hash)` prepared against snapshot
    /// `snap_txn`, committing under a lock whose newest committed txn is
    /// `current_txn`. Returns true (conflict) if any committed txn in
    /// `(snap_txn, current_txn]` wrote our table at table granularity, wrote
    /// our exact key at point granularity, OR is missing from the ring (an
    /// overwritten/too-old window, or a foreign serial-mode committer's gap —
    /// both handled conservatively). Sound by construction: it never returns
    /// false when a real conflicting commit exists in the window.
    pub fn opt_conflict(
        &self,
        snap_txn: u64,
        current_txn: u64,
        table_id: u32,
        key_hash: u64,
    ) -> bool {
        if current_txn <= snap_txn {
            return false; // nothing committed since our snapshot
        }
        if current_txn - snap_txn > OPT_RING_SLOTS {
            return true; // snapshot older than the ring can witness: conservative
        }
        let my_bit = 1u64 << (table_id & 63);
        for t in (snap_txn + 1)..=current_txn {
            if self.opt_field(t, OFP_TXN).load(Ordering::Acquire) != t {
                return true; // gap / overwritten / foreign writer: conservative
            }
            let kind = self.opt_field(t, OFP_KIND).load(Ordering::Relaxed);
            let tbits = self.opt_field(t, OFP_TBITS).load(Ordering::Relaxed);
            match kind {
                OFP_KIND_EMPTY => {}
                OFP_KIND_TABLE => {
                    if tbits & my_bit != 0 {
                        return true;
                    }
                }
                _ => {
                    // point: conflict only on the same table AND same key
                    if tbits & my_bit != 0
                        && self.opt_field(t, OFP_KHASH).load(Ordering::Relaxed) == key_hash
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    // ---------- open / create ----------

    pub fn open(
        path: &Path,
        size_bytes: u64,
        max_readers: u32,
        durability: Durability,
        schema_hash: &[u8; 32],
        perms: &FilePerms,
    ) -> Result<Shm> {
        let size = (size_bytes / PAGE_SIZE as u64) * PAGE_SIZE as u64;
        let page_count = size / PAGE_SIZE as u64;
        let min_pages = data_start_page(max_readers) + 8;
        if page_count < min_pages {
            return Err(Error::Config(format!(
                "size_mb too small: need at least {min_pages} pages for geometry"
            )));
        }
        // Born-restrictive: create owner-only (umask can only tighten, never
        // widen 0o600), so there is no world/group-readable instant even under
        // a concurrent open(). Widened to the configured mode by the formatter
        // below (DESIGN-MULTIDB.md §1.4). `.mode()` only affects *creation*, so
        // attaching to an existing file leaves its perms untouched.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;

        // Fast path: fully-sized file that reports READY attaches lock-free.
        let st_size = file.metadata()?.len();
        if st_size == size {
            let mut shm = Self::map(&file, size, max_readers, durability)?;
            if shm.init_state().load(Ordering::Acquire) == INIT_READY {
                shm.validate_frozen(page_count, max_readers, durability, schema_hash)?;
                if durability.uses_wal() {
                    shm.attach_wal(path)?;
                }
                shm.post_attach(schema_hash)?;
                return Ok(shm);
            }
            drop(shm);
        }

        // Slow path: adopt/format under the kernel-cleaned file lock. Any
        // previous creator death (post-create, mid-fallocate, mid-format)
        // lands here and is repaired.
        let _guard = FlockGuard::exclusive(&file)?;
        let st_size = file.metadata()?.len();
        if st_size != size {
            // NEVER resize a READY database (in either direction): a size
            // mismatch on a live db is config drift, and extending/shrinking
            // it would brick every correctly-configured process. Probe first.
            if st_size >= (LOCK_PAGE + 1) * PAGE_SIZE as u64 {
                let probe = Self::map(&file, st_size, max_readers, durability)?;
                let ready = probe.init_state().load(Ordering::Acquire) == INIT_READY;
                drop(probe);
                if ready {
                    return Err(Error::Config(format!(
                        "database file is {st_size} bytes but config says {size}; \
                         the file is authoritative — fix the config"
                    )));
                }
            }
            // non-READY debris: safe to resize and re-format
            if st_size > size
                && unsafe { libc::ftruncate(file.as_raw_fd(), size as i64) } != 0
            {
                return Err(io_err("ftruncate (shrinking debris file)"));
            }
            // real preallocation: ENOSPC surfaces here instead of as SIGBUS
            // on first touch of a hole mid-commit
            let rc = crate::os::preallocate(file.as_raw_fd(), 0, size as i64);
            if rc != 0 {
                return Err(io_err("fallocate (preallocating database file)"));
            }
        }
        let mut shm = Self::map(&file, size, max_readers, durability)?;
        if durability.uses_wal() {
            shm.attach_wal(path)?; // before format: format truncates debris
        }
        let state = shm.init_state().load(Ordering::Acquire);
        if state != INIT_READY {
            // fresh format (state 0) or a dead initializer's debris (state 1)
            shm.format(page_count, max_readers, durability, schema_hash)?;
            // We are the formatter and hold the exclusive flock: widen the
            // born-0o600 file (and its `-wal`, an equal isolation asset holding
            // recent page images) to the configured mode/owner exactly once.
            apply_file_perms(shm.file.as_raw_fd(), perms)?;
            if let Some(w) = shm.wal.as_ref() {
                apply_file_perms(w.file.as_raw_fd(), perms)?;
            }
        } else {
            shm.validate_frozen(page_count, max_readers, durability, schema_hash)?;
        }
        shm.post_attach(schema_hash)?;
        Ok(shm)
    }

    /// Open an existing, READY database using the file's own frozen
    /// geometry — no config required. For tooling (`mpedb dump`); skips the
    /// schema-hash check (the schema itself is read from the catalog).
    pub fn open_existing(path: &Path) -> Result<Shm> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let st_size = file.metadata()?.len();
        if st_size < 4 * PAGE_SIZE as u64 || st_size % PAGE_SIZE as u64 != 0 {
            return Err(Error::Corrupt(
                "file size is not a valid mpedb database".into(),
            ));
        }
        let mut shm = Self::map(&file, st_size, 1, Durability::None)?;
        if shm.init_state().load(Ordering::Acquire) != INIT_READY {
            return Err(Error::Corrupt(
                "database file is not initialized (READY marker absent)".into(),
            ));
        }
        let base = Self::meta_off(META_PAGE_A);
        let magic = unsafe { std::slice::from_raw_parts(shm.at(base + M_MAGIC), 8) };
        if magic != MAGIC {
            return Err(Error::Corrupt("bad magic (not an mpedb file)".into()));
        }
        let read_u32 = |off: usize| unsafe { (shm.at(base + off) as *const u32).read() };
        let read_u64 = |off: usize| unsafe { (shm.at(base + off) as *const u64).read() };
        if read_u32(M_FORMAT_VERSION) != FORMAT_VERSION {
            return Err(Error::Schema(format!(
                "file format version {} != engine version {FORMAT_VERSION}",
                read_u32(M_FORMAT_VERSION)
            )));
        }
        if read_u32(M_PAGE_SIZE) as usize != PAGE_SIZE
            || read_u64(M_PAGE_COUNT) != st_size / PAGE_SIZE as u64
        {
            return Err(Error::Corrupt("stored geometry disagrees with file size".into()));
        }
        let max_readers = read_u32(M_MAX_READERS);
        if !(1..=65_536).contains(&max_readers) {
            return Err(Error::Corrupt(format!(
                "stored max_readers {max_readers} out of range"
            )));
        }
        if data_start_page(max_readers) + 1 > st_size / PAGE_SIZE as u64 {
            return Err(Error::Corrupt(
                "stored max_readers implies a reader table beyond the file".into(),
            ));
        }
        let durability = durability_from_tag(read_u32(M_DURABILITY))
            .ok_or_else(|| Error::Corrupt("bad durability tag".into()))?;
        shm.max_readers = max_readers;
        shm.durability = durability;
        shm.data_start = data_start_page(max_readers);
        if durability.uses_wal() {
            shm.attach_wal(path)?;
        }
        let hash_unused = [0u8; 32];
        shm.post_attach(&hash_unused)?;
        Ok(shm)
    }

    fn map(file: &File, size: u64, max_readers: u32, durability: Durability) -> Result<Shm> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io_err("mmap"));
        }
        // opportunistic; harmless where unsupported (macOS: no-op)
        crate::os::madvise_hugepage(ptr, size as usize);
        Ok(Shm {
            map: ptr as *mut u8,
            len: size as usize,
            file: file.try_clone()?,
            page_count: size / PAGE_SIZE as u64,
            max_readers,
            durability,
            data_start: data_start_page(max_readers),
            my_pid_start: proc_start_time(std::process::id()).ok_or_else(|| {
                Error::Config("cannot read this process's start time for identity".into())
            })?,
            recovered: false,
            wal: None,
        })
    }

    /// One-time format. Caller: exclusive flock held. The LAST store, with
    /// Release, is init_state = READY; death at any earlier point leaves a
    /// state the next flock holder re-formats.
    fn format(
        &mut self,
        page_count: u64,
        max_readers: u32,
        durability: Durability,
        schema_hash: &[u8; 32],
    ) -> Result<()> {
        self.init_state().store(INIT_FORMATTING, Ordering::Release);

        // zero the control region (metas, lock area, reader table)
        let ctl_pages = data_start_page(max_readers) as usize;
        unsafe {
            std::ptr::write_bytes(self.at(0), 0, ctl_pages * PAGE_SIZE);
        }
        // re-set state: the zeroing above cleared it
        self.init_state().store(INIT_FORMATTING, Ordering::Release);

        // robust, error-checking, process-shared writer mutex
        unsafe {
            let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
            libc::pthread_mutexattr_init(&mut attr);
            libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED);
            crate::os::mutexattr_set_robust(&mut attr);
            libc::pthread_mutexattr_settype(&mut attr, libc::PTHREAD_MUTEX_ERRORCHECK);
            let rc = libc::pthread_mutex_init(self.mutex_ptr(), &attr);
            libc::pthread_mutexattr_destroy(&mut attr);
            if rc != 0 {
                return Err(Error::Internal(format!("pthread_mutex_init: {rc}")));
            }
        }

        let ns = my_pid_ns_ino()?;
        let bid = boot_id()?;
        unsafe {
            (self.at(Self::lock_area_off(LA_PID_NS_INO)) as *mut u64).write(ns);
            std::ptr::copy_nonoverlapping(
                bid.as_ptr(),
                self.at(Self::lock_area_off(LA_BOOT_ID)),
                16,
            );
        }
        self.durable_txn().store(0, Ordering::Release);
        self.oldest_pinned_cache().store(0, Ordering::Release);

        // frozen fields into both meta pages
        for slot in [META_PAGE_A, META_PAGE_B] {
            let base = Self::meta_off(slot);
            unsafe {
                std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), self.at(base + M_MAGIC), 8);
                (self.at(base + M_FORMAT_VERSION) as *mut u32).write(FORMAT_VERSION);
                (self.at(base + M_PAGE_SIZE) as *mut u32).write(PAGE_SIZE as u32);
                (self.at(base + M_PAGE_COUNT) as *mut u64).write(page_count);
                (self.at(base + M_MAX_READERS) as *mut u32).write(max_readers);
                (self.at(base + M_DURABILITY) as *mut u32).write(durability_tag(durability));
                std::ptr::copy_nonoverlapping(
                    schema_hash.as_ptr(),
                    self.at(base + M_SCHEMA_HASH),
                    32,
                );
            }
        }
        if durability.uses_wal() {
            // any leftover log belongs to a previous incarnation of this
            // file; wal_ckpt = wal_len = 0 in the freshly zeroed lock area
            self.wal_reset_for_format()?;
        }

        // genesis commit: txn 0, empty trees, high_water at data start.
        // Written into slot B so the first real commit goes to slot A.
        let genesis = MetaSnapshot {
            slot: META_PAGE_B,
            txn_id: 0,
            catalog_root: 0,
            freelist_root: 0,
            high_water: data_start_page(max_readers),
        };
        self.write_meta_slot(META_PAGE_A, &genesis);
        // slot A gets the same genesis so both slots validate
        self.write_meta_slot(META_PAGE_B, &genesis);

        if durability != Durability::None {
            self.msync_range(0, data_start_page(max_readers) as usize * PAGE_SIZE)?;
        }
        self.init_state().store(INIT_READY, Ordering::Release);
        if durability != Durability::None {
            self.msync_page(LOCK_PAGE)?;
        }
        Ok(())
    }

    fn validate_frozen(
        &self,
        page_count: u64,
        max_readers: u32,
        durability: Durability,
        schema_hash: &[u8; 32],
    ) -> Result<()> {
        let base = Self::meta_off(META_PAGE_A);
        let read_u32 = |off: usize| unsafe { (self.at(base + off) as *const u32).read() };
        let read_u64 = |off: usize| unsafe { (self.at(base + off) as *const u64).read() };
        let magic = unsafe { std::slice::from_raw_parts(self.at(base + M_MAGIC), 8) };
        if magic != MAGIC {
            return Err(Error::Corrupt("bad magic (not an mpedb file)".into()));
        }
        if read_u32(M_FORMAT_VERSION) != FORMAT_VERSION {
            return Err(Error::Schema(format!(
                "file format version {} != engine version {FORMAT_VERSION}",
                read_u32(M_FORMAT_VERSION)
            )));
        }
        if read_u32(M_PAGE_SIZE) as usize != PAGE_SIZE {
            return Err(Error::Corrupt("page size mismatch".into()));
        }
        // the file is authoritative for geometry: any drift is a hard error
        if read_u64(M_PAGE_COUNT) != page_count {
            return Err(Error::Config(format!(
                "file has {} pages, config implies {page_count}; fix the config",
                read_u64(M_PAGE_COUNT)
            )));
        }
        if read_u32(M_MAX_READERS) != max_readers {
            return Err(Error::Config(format!(
                "file was created with max_readers={}, config says {max_readers}; \
                 the file is authoritative",
                read_u32(M_MAX_READERS)
            )));
        }
        if read_u32(M_DURABILITY) != durability_tag(durability) {
            return Err(Error::Config(
                "durability mode differs from the one the file was created with".into(),
            ));
        }
        let stored_hash =
            unsafe { std::slice::from_raw_parts(self.at(base + M_SCHEMA_HASH), 32) };
        if stored_hash != schema_hash {
            return Err(Error::Schema(
                "schema hash mismatch: the config schema differs from the database's \
                 (run `mpedb dump --schema` to see the stored schema)"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Identity checks + per-boot recovery. Runs on every attach path.
    fn post_attach(&mut self, _schema_hash: &[u8; 32]) -> Result<()> {
        let ns_off = Self::lock_area_off(LA_PID_NS_INO);
        let stored_ns = unsafe { (self.at(ns_off) as *const u64).read() };
        let my_ns = my_pid_ns_ino()?;

        let bid_off = Self::lock_area_off(LA_BOOT_ID);
        let stored_bid: [u8; 16] = unsafe {
            let mut b = [0u8; 16];
            std::ptr::copy_nonoverlapping(self.at(bid_off), b.as_mut_ptr(), 16);
            b
        };
        let my_bid = boot_id()?;

        if stored_bid != my_bid {
            // First attach since a reboot: pids in the reader table are from a
            // previous boot and the robust mutex's kernel state is gone (a
            // mutex left "locked" by a pre-reboot process would deadlock
            // forever — robust lists do not survive power loss). Reinitialize
            // the volatile control state under the file lock.
            let _guard = FlockGuard::exclusive(&self.file)?;
            let still_stored: [u8; 16] = unsafe {
                let mut b = [0u8; 16];
                std::ptr::copy_nonoverlapping(self.at(bid_off), b.as_mut_ptr(), 16);
                b
            };
            if still_stored != my_bid {
                // WAL replay FIRST, before any volatile reinit: after power
                // loss the mapping (incl. metas) is whatever the kernel wrote
                // back; the log is the source of truth. Replay is idempotent
                // and the boot id is only updated below, so dying anywhere in
                // here makes the next attacher redo the whole sequence.
                if self.durability.uses_wal() {
                    self.wal_recover()?;
                }
                unsafe {
                    let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
                    libc::pthread_mutexattr_init(&mut attr);
                    libc::pthread_mutexattr_setpshared(
                        &mut attr,
                        libc::PTHREAD_PROCESS_SHARED,
                    );
                    crate::os::mutexattr_set_robust(&mut attr);
                    libc::pthread_mutexattr_settype(&mut attr, libc::PTHREAD_MUTEX_ERRORCHECK);
                    libc::pthread_mutex_init(self.mutex_ptr(), &attr);
                    libc::pthread_mutexattr_destroy(&mut attr);
                    // clear the reader table: all pre-reboot pins are void
                    std::ptr::write_bytes(
                        self.at(READER_TABLE_PAGE as usize * PAGE_SIZE),
                        0,
                        reader_table_pages(self.max_readers) as usize * PAGE_SIZE,
                    );
                    (self.at(ns_off) as *mut u64).write(my_ns);
                    std::ptr::copy_nonoverlapping(my_bid.as_ptr(), self.at(bid_off), 16);
                }
                let newest = self.newest_meta_ungated()?;
                self.durable_txn().store(newest.txn_id, Ordering::Release);
                self.oldest_pinned_cache()
                    .store(newest.txn_id, Ordering::Release);
                self.recovered = true;
            }
        } else if stored_ns != my_ns {
            return Err(Error::Config(format!(
                "attach from a different PID namespace (file: {stored_ns}, \
                 process: {my_ns}); kill(2)-based liveness would be unsound — \
                 mpedb requires all processes in one PID namespace"
            )));
        }
        self.sweep_dead_readers();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("mpedb-shm-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}-{}", name, std::process::id()))
    }

    fn open_test(name: &str) -> (Shm, std::path::PathBuf) {
        let p = tmp_path(name);
        let _ = std::fs::remove_file(&p);
        let shm = Shm::open(&p, 4 * 1024 * 1024, 64, Durability::None, &[7u8; 32], &mpedb_types::FilePerms::default()).unwrap();
        (shm, p)
    }

    #[test]
    fn format_and_reattach() {
        let (shm, p) = open_test("format");
        let m = shm.newest_meta().unwrap();
        assert_eq!(m.txn_id, 0);
        assert_eq!(m.high_water, shm.data_start);
        drop(shm);
        // reattach: fast path, same geometry
        let shm2 = Shm::open(&p, 4 * 1024 * 1024, 64, Durability::None, &[7u8; 32], &mpedb_types::FilePerms::default()).unwrap();
        assert_eq!(shm2.newest_meta().unwrap().txn_id, 0);
        // wrong schema hash refused
        let err = Shm::open(&p, 4 * 1024 * 1024, 64, Durability::None, &[8u8; 32], &mpedb_types::FilePerms::default());
        assert!(matches!(err, Err(Error::Schema(_))));
        // wrong max_readers refused (geometry is file-authoritative)
        let err = Shm::open(&p, 4 * 1024 * 1024, 128, Durability::None, &[7u8; 32], &mpedb_types::FilePerms::default());
        assert!(matches!(err, Err(Error::Config(_))));
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn born_restrictive_default_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp_path("perms-default");
        let _ = std::fs::remove_file(&p);
        // No mode configured ⇒ file stays owner-only (0o600), never the
        // umask-default 0o644 that would be group/world-readable.
        let shm =
            Shm::open(&p, 4 * 1024 * 1024, 64, Durability::None, &[7u8; 32], &FilePerms::default())
                .unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "default-created db must be owner-only");
        drop(shm);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn configured_mode_widens_main_and_wal() {
        use std::os::unix::fs::PermissionsExt;
        let p = tmp_path("perms-wal");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(wal_path(&p));
        let perms = FilePerms { mode: Some(0o640), owner: None, group: None };
        // wal durability so the `-wal` companion is created and widened too.
        let shm =
            Shm::open(&p, 4 * 1024 * 1024, 64, Durability::Wal, &[7u8; 32], &perms).unwrap();
        let main_mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        let wal_mode = std::fs::metadata(wal_path(&p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(main_mode, 0o640, "main file widened to configured mode");
        assert_eq!(wal_mode, 0o640, "the -wal companion must inherit the same mode");
        drop(shm);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(wal_path(&p));
    }

    #[test]
    fn resolve_id_accepts_numeric_and_rejects_unknown_name() {
        assert_eq!(resolve_id("0", true).unwrap(), 0);
        assert_eq!(resolve_id("1234", false).unwrap(), 1234);
        assert!(matches!(
            resolve_id("no_such_user_zzq_9182", true),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn adopts_zero_size_and_short_files() {
        let p = tmp_path("short");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, b"").unwrap(); // dead creator: zero-size debris
        let shm = Shm::open(&p, 4 * 1024 * 1024, 64, Durability::None, &[7u8; 32], &mpedb_types::FilePerms::default()).unwrap();
        assert_eq!(shm.newest_meta().unwrap().txn_id, 0);
        drop(shm);
        // short/garbage file (dead mid-fallocate)
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, vec![0xAAu8; 3 * PAGE_SIZE]).unwrap();
        let shm = Shm::open(&p, 4 * 1024 * 1024, 64, Durability::None, &[7u8; 32], &mpedb_types::FilePerms::default()).unwrap();
        assert_eq!(shm.newest_meta().unwrap().txn_id, 0);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn meta_double_buffer_commit_cycle() {
        let (shm, p) = open_test("meta");
        let m0 = shm.newest_meta().unwrap();
        let mut next = MetaSnapshot {
            slot: 0,
            txn_id: 1,
            catalog_root: 100,
            freelist_root: 101,
            high_water: m0.high_water + 5,
        };
        let slot1 = shm.write_meta_slot(m0.slot, &next);
        assert_ne!(slot1, m0.slot);
        let m1 = shm.newest_meta().unwrap();
        assert_eq!(m1.txn_id, 1);
        assert_eq!(m1.catalog_root, 100);
        next.txn_id = 2;
        next.catalog_root = 200;
        let slot2 = shm.write_meta_slot(slot1, &next);
        assert_ne!(slot2, slot1);
        assert_eq!(shm.newest_meta().unwrap().catalog_root, 200);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn corrupt_meta_falls_back_to_other_slot() {
        let (shm, p) = open_test("corrupt-meta");
        let m0 = shm.newest_meta().unwrap();
        let next = MetaSnapshot {
            txn_id: 1,
            ..m0
        };
        let slot1 = shm.write_meta_slot(m0.slot, &next);
        // scribble over the NEWER meta's checksum: readers must fall back
        shm.atomic_u64(Shm::meta_off(slot1) + M_CHECKSUM)
            .store(0xDEAD_BEEF, Ordering::Release);
        let m = shm.newest_meta().unwrap();
        assert_eq!(m.txn_id, 0, "must fall back to the older valid slot");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn reader_slots_claim_pin_release_sweep() {
        let (shm, p) = open_test("slots");
        let (idx, word, meta) = shm.claim_and_pin().unwrap();
        assert_eq!(meta.txn_id, 0);
        assert!(shm.slot_still_owned(idx, word));
        // oldest_pinned accounts for us
        assert_eq!(shm.compute_oldest_pinned(5), 0);
        assert!(shm.release_slot(idx, word));
        assert!(!shm.release_slot(idx, word), "double release must fail");
        // with no pins, bound rises to current
        assert_eq!(shm.compute_oldest_pinned(5), 5);

        // forge a dead reader: fake pid claimed a slot then "died"
        let dead_pid = 4_000_000u32; // beyond pid_max on default systems
        let w = Shm::pack(dead_pid, 9);
        shm.slot_word(1).store(w, Ordering::Release);
        shm.slot_txn(1).store(3, Ordering::Release);
        shm.slot_pid_start(1).store(12345, Ordering::Release);
        assert_eq!(shm.compute_oldest_pinned(10), 3);
        shm.sweep_dead_readers();
        assert_eq!(
            shm.compute_oldest_pinned(10),
            10,
            "dead reader's pin must be reclaimed by the sweep"
        );
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn pid_reuse_detected_via_start_time() {
        let (shm, p) = open_test("pid-reuse");
        // claim a slot under OUR live pid but a WRONG recorded start time:
        // simulates a recycled pid — sweep must free it despite kill()==0
        let w = Shm::pack(std::process::id(), 1);
        shm.slot_word(2).store(w, Ordering::Release);
        shm.slot_txn(2).store(1, Ordering::Release);
        shm.slot_pid_start(2).store(1, Ordering::Release); // bogus start time
        shm.sweep_dead_readers();
        let (pid, _) = Shm::unpack(shm.slot_word(2).load(Ordering::Acquire));
        assert_eq!(pid, 0, "recycled-pid slot must be freed");

        // and with the CORRECT start time it must survive the sweep
        let w = Shm::pack(std::process::id(), 3);
        shm.slot_word(3).store(w, Ordering::Release);
        shm.slot_pid_start(3)
            .store(proc_start_time(std::process::id()).unwrap(), Ordering::Release);
        shm.sweep_dead_readers();
        let (pid, _) = Shm::unpack(shm.slot_word(3).load(Ordering::Acquire));
        assert_eq!(pid, std::process::id(), "live slot must survive");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn claiming_slots_swept_only_on_definite_death() {
        let (shm, p) = open_test("claiming");
        // live pid stuck in CLAIMING (e.g. preempted mid-claim): must survive
        // sweeps even though pid_start is stale garbage
        let w = Shm::pack(std::process::id() | Shm::CLAIMING, 5);
        shm.slot_word(1).store(w, Ordering::Release);
        shm.slot_pid_start(1).store(0xDEAD, Ordering::Release);
        shm.sweep_dead_readers();
        assert_eq!(
            shm.slot_word(1).load(Ordering::Acquire),
            w,
            "live CLAIMING slot must not be swept on start-time mismatch"
        );
        // dead pid in CLAIMING: swept via ESRCH
        let dead = Shm::pack(4_000_000 | Shm::CLAIMING, 7);
        shm.slot_word(2).store(dead, Ordering::Release);
        shm.sweep_dead_readers();
        let (pid_half, _) = Shm::unpack(shm.slot_word(2).load(Ordering::Acquire));
        assert_eq!(pid_half, 0, "dead CLAIMING slot must be reclaimed");
        // cleanup our synthetic live slot so other tests are unaffected
        shm.slot_word(1).store(0, Ordering::Release);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn claimed_slot_never_clobbers_other_claimants() {
        // claim_and_pin stores txn/pid_start only AFTER owning the word, so a
        // concurrent claimant's pin can never be overwritten (reviewed
        // finding: the old pre-CAS MAX store clobbered published pins).
        let (shm, p) = open_test("no-clobber");
        let (i1, w1, m1) = shm.claim_and_pin().unwrap();
        let (i2, w2, m2) = shm.claim_and_pin().unwrap();
        assert_ne!(i1, i2);
        assert_eq!(m1.txn_id, 0);
        assert_eq!(m2.txn_id, 0);
        // both pins visible to the oldest-pinned scan
        assert_eq!(shm.compute_oldest_pinned(9), 0);
        assert!(shm.release_slot(i1, w1));
        assert!(shm.release_slot(i2, w2));
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn writer_lock_basic_and_errorcheck() {
        let (shm, p) = open_test("lock");
        let recovered = shm.writer_lock().unwrap();
        assert!(!recovered);
        // ERRORCHECK: relocking from the same thread errors instead of hanging
        assert!(shm.writer_lock().is_err());
        shm.writer_unlock();
        std::fs::remove_file(&p).unwrap();
    }

    // ------------------------------------------------- write-ahead log tests

    /// wal tests run on /dev/shm when available: the torn-tail sweep calls
    /// wal_recover (a full-mapping msync) thousands of times.
    fn wal_tmp_path(name: &str) -> std::path::PathBuf {
        let base = std::path::Path::new("/dev/shm");
        let dir = if base.is_dir() {
            base.join("mpedb-wal-tests")
        } else {
            std::env::temp_dir().join("mpedb-wal-tests")
        };
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}-{}", name, std::process::id()))
    }

    fn wal_open_test(name: &str) -> (Shm, std::path::PathBuf) {
        wal_open_test_mode(name, Durability::Wal)
    }

    /// Open a WAL-class db (`Wal` or `Async`). Note: `Shm::open` never spawns
    /// the async flusher — that lives in the `Engine` layer — so async
    /// contract tests here are fully deterministic (we drive
    /// `wal_append_async` / `wal_flush_deferred` by hand).
    fn wal_open_test_mode(name: &str, durability: Durability) -> (Shm, std::path::PathBuf) {
        let p = wal_tmp_path(name);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(wal_path(&p));
        let shm = Shm::open(&p, 4 * 1024 * 1024, 64, durability, &[7u8; 32], &mpedb_types::FilePerms::default()).unwrap();
        (shm, p)
    }

    /// Async analog of [`commit_one`]: append the record WITHOUT fdatasync
    /// (deferred), flip the meta. Advances `wal_appended`, not `wal_len`.
    fn append_one_async(shm: &Shm, txn: u64, page_id: u64, fill: u8) -> MetaSnapshot {
        shm.page_mut_unchecked(page_id).unwrap().fill(fill);
        let prev = shm.newest_meta_ungated().unwrap();
        let snap = MetaSnapshot {
            slot: prev.slot,
            txn_id: txn,
            catalog_root: page_id,
            freelist_root: 0,
            high_water: shm.data_start + 8,
        };
        shm.wal_append_async(&[page_id], &snap).unwrap();
        shm.write_meta_slot(prev.slot, &snap);
        snap
    }

    fn wal_cleanup(p: &std::path::Path) {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(wal_path(p));
    }

    /// Simulate one engine commit at the shm level: fill `page_id` with
    /// `fill`, append the record, flip the meta — exactly the §5.4 order.
    fn commit_one(shm: &Shm, txn: u64, page_id: u64, fill: u8) -> MetaSnapshot {
        shm.page_mut_unchecked(page_id).unwrap().fill(fill);
        let prev = shm.newest_meta_ungated().unwrap();
        let snap = MetaSnapshot {
            slot: prev.slot,
            txn_id: txn,
            catalog_root: page_id,
            freelist_root: 0,
            high_water: shm.data_start + 8,
        };
        shm.wal_commit(&[page_id], &snap).unwrap();
        shm.write_meta_slot(prev.slot, &snap);
        shm.durable_txn().fetch_max(txn, Ordering::AcqRel);
        snap
    }

    #[test]
    fn wal_record_roundtrip_and_offset_binding() {
        let page = vec![0xABu8; PAGE_SIZE];
        let pages: Vec<(u64, &[u8])> = vec![(17, &page), (99, &page)];
        let rec = encode_wal_record(4096, 42, &pages, 7, 8, 9);
        assert_eq!(rec.len(), WAL_RECORD_FIXED + 2 * WAL_PAGE_ENTRY);

        let d = decode_wal_record(&rec, 4096, 1024).expect("valid record must decode");
        assert_eq!(d.txn_id, 42);
        assert_eq!(d.len as usize, rec.len());
        assert_eq!(d.pages.len(), 2);
        assert_eq!(d.pages[0].0, 17);
        assert_eq!(d.pages[1].0, 99);
        // a 0xAB-filled page is not a valid node header, so it stores FULL and
        // reconstructs byte-identically
        let mut got = vec![0u8; PAGE_SIZE];
        for (_, img) in &d.pages {
            img.write_into(&mut got);
            assert_eq!(got, page);
        }
        assert_eq!((d.catalog_root, d.freelist_root, d.high_water), (7, 8, 9));

        // offset binding: the identical bytes are INVALID at any other offset
        assert!(decode_wal_record(&rec, 0, 1024).is_none());
        assert!(decode_wal_record(&rec, 4095, 1024).is_none());
        // page id beyond the file is invalid even with a good checksum
        assert!(decode_wal_record(&rec, 4096, 90).is_none());
        // any single flipped byte invalidates the record
        for probe in [0usize, 5, 13, 20, rec.len() / 2, rec.len() - 1] {
            let mut bad = rec.clone();
            bad[probe] ^= 0xFF;
            assert!(
                decode_wal_record(&bad, 4096, 1024).is_none(),
                "flip at byte {probe} must invalidate"
            );
        }
        // truncation at EVERY offset yields no record
        for cut in 0..rec.len() {
            assert!(
                decode_wal_record(&rec[..cut], 4096, 1024).is_none(),
                "truncation to {cut} bytes must not decode"
            );
        }
    }

    #[test]
    fn wal_lean_record_elides_unread_middle() {
        // Hand-build a valid LEAF node (btree layout): header + one slot + one
        // small cell packed at the page end, with the free middle scribbled
        // 0xFF to prove it is neither stored nor reconstructed.
        let mut page = vec![0xFFu8; PAGE_SIZE];
        let cell = [0u8, 0, 1]; // arbitrary bytes; content is not traversed here
        let cell_start = PAGE_SIZE - cell.len();
        page[cell_start..].copy_from_slice(&cell);
        page[0] = 2; // KIND_LEAF
        page[1] = 0;
        page[2..4].copy_from_slice(&1u16.to_le_bytes()); // nkeys = 1
        page[4..6].copy_from_slice(&(cell_start as u16).to_le_bytes());
        page[6..8].copy_from_slice(&0u16.to_le_bytes());
        page[8..16].copy_from_slice(&0u64.to_le_bytes());
        page[16..18].copy_from_slice(&(cell_start as u16).to_le_bytes()); // slot 0

        let pages: Vec<(u64, &[u8])> = vec![(50, &page)];
        let rec = encode_wal_record(0, 1, &pages, 0, 0, 0);
        // the lean record is a tiny fraction of a full-page record
        assert!(
            rec.len() < WAL_RECORD_FIXED + WAL_PAGE_ENTRY / 8,
            "lean record ({}) should be far smaller than a full page",
            rec.len()
        );

        let d = decode_wal_record(&rec, 0, 1024).expect("valid lean record");
        let mut got = vec![0u8; PAGE_SIZE];
        d.pages[0].1.write_into(&mut got);
        // the two USED regions come back byte-for-byte...
        assert_eq!(&got[..18], &page[..18], "header + slot array preserved");
        assert_eq!(&got[cell_start..], &page[cell_start..], "cells preserved");
        // ...and the elided middle is zero, NOT the 0xFF that was in memory —
        // a real byte change that btree::used_span proves is never observed.
        assert!(
            got[18..cell_start].iter().all(|&b| b == 0),
            "elided middle must be zero-filled on replay"
        );
    }

    #[test]
    fn wal_commit_appends_and_recovery_replays() {
        let (shm, p) = wal_open_test("commit");
        assert_eq!(shm.wal_len().load(Ordering::Acquire), 0);
        let pg = shm.data_start;
        commit_one(&shm, 1, pg, 0x11);
        let end1 = shm.wal_len().load(Ordering::Acquire);
        assert_eq!(end1 as usize, WAL_RECORD_FIXED + WAL_PAGE_ENTRY);
        commit_one(&shm, 2, pg + 1, 0x22);
        let end2 = shm.wal_len().load(Ordering::Acquire);

        // simulate a reboot whose writeback lost everything volatile: stale
        // lock area (wal_len/wal_ckpt from format time), regressed metas,
        // garbled data pages
        shm.wal_len().store(0, Ordering::Release);
        shm.wal_ckpt().store(0, Ordering::Release);
        let genesis = MetaSnapshot {
            slot: 0,
            txn_id: 0,
            catalog_root: 0,
            freelist_root: 0,
            high_water: shm.data_start,
        };
        shm.write_meta_slot(META_PAGE_A, &genesis);
        shm.write_meta_slot(META_PAGE_B, &genesis);
        shm.page_mut_unchecked(pg).unwrap().fill(0xEE);
        shm.page_mut_unchecked(pg + 1).unwrap().fill(0xEE);

        let end = shm.wal_recover().unwrap();
        assert_eq!(end, end2, "recovery must reach the full valid prefix");
        assert_eq!(shm.wal_len().load(Ordering::Acquire), end2);
        assert_eq!(shm.wal_ckpt().load(Ordering::Acquire), end2);
        let m = shm.newest_meta_ungated().unwrap();
        assert_eq!((m.txn_id, m.catalog_root), (2, pg + 1));
        assert!(shm.page(pg).unwrap().iter().all(|&b| b == 0x11));
        assert!(shm.page(pg + 1).unwrap().iter().all(|&b| b == 0x22));

        // replay is idempotent: running recovery again changes nothing
        let end_again = shm.wal_recover().unwrap();
        assert_eq!(end_again, end2);
        assert_eq!(shm.newest_meta_ungated().unwrap(), m);
        wal_cleanup(&p);
    }

    #[test]
    fn wal_torn_tail_truncation_at_every_offset() {
        let (shm, p) = wal_open_test("torn");
        let pg = shm.data_start;
        commit_one(&shm, 1, pg, 0x11);
        let end1 = shm.wal_len().load(Ordering::Acquire);
        let m2 = commit_one(&shm, 2, pg, 0x22);
        let end2 = shm.wal_len().load(Ordering::Acquire);
        commit_one(&shm, 3, pg, 0x33);
        let end3 = shm.wal_len().load(Ordering::Acquire);

        let wal_f = OpenOptions::new().write(true).open(wal_path(&p)).unwrap();
        // walk the truncation point DOWN through every byte of records 3 and
        // 2: each cut discards the torn record entirely and recovery lands on
        // the longest valid prefix
        for cut in (end1..end3).rev() {
            wal_f.set_len(cut).unwrap();
            // stale lock area, as after a reboot that wrote nothing back
            shm.wal_len().store(0, Ordering::Release);
            shm.wal_ckpt().store(0, Ordering::Release);
            let end = shm.wal_recover().unwrap();
            let (want_end, want_txn, want_fill) = if cut >= end2 {
                (end2, 2, 0x22)
            } else {
                (end1, 1, 0x11)
            };
            assert_eq!(end, want_end, "cut at {cut}");
            let m = shm.newest_meta_ungated().unwrap();
            assert_eq!(m.txn_id, want_txn, "cut at {cut}");
            assert!(
                shm.page(pg).unwrap().iter().all(|&b| b == want_fill),
                "cut at {cut}: page must match the surviving prefix"
            );
        }
        let _ = m2;
        wal_cleanup(&p);
    }

    #[test]
    fn wal_checkpoint_threshold_and_scan_from_ckpt() {
        let (shm, p) = wal_open_test("ckpt");
        let pg = shm.data_start;
        commit_one(&shm, 1, pg, 0x11);
        let end1 = shm.wal_len().load(Ordering::Acquire);

        // below threshold: no checkpoint
        shm.wal_checkpoint_if(u64::MAX).unwrap();
        assert_eq!(shm.wal_ckpt().load(Ordering::Acquire), 0);
        // crossing the threshold checkpoints: ckpt = len, main file synced
        shm.wal_checkpoint_if(1).unwrap();
        assert_eq!(shm.wal_ckpt().load(Ordering::Acquire), end1);
        assert_eq!(shm.wal_len().load(Ordering::Acquire), end1);

        // post-checkpoint commits land beyond ckpt and are recoverable from
        // a lock area whose wal_len writeback was lost (ckpt IS durable — it
        // was msynced by the checkpoint before any reclaim)
        commit_one(&shm, 2, pg + 1, 0x22);
        let end2 = shm.wal_len().load(Ordering::Acquire);
        shm.wal_len().store(end1, Ordering::Release); // stale: pre-commit-2
        shm.page_mut_unchecked(pg + 1).unwrap().fill(0xEE);
        let end = shm.wal_recover().unwrap();
        assert_eq!(end, end2);
        assert_eq!(shm.newest_meta_ungated().unwrap().txn_id, 2);
        assert!(shm.page(pg + 1).unwrap().iter().all(|&b| b == 0x22));

        // page 'pg' was NOT replayed (its record is below ckpt) — the
        // checkpoint's full-mapping msync is what guarantees it: still intact
        assert!(shm.page(pg).unwrap().iter().all(|&b| b == 0x11));
        wal_cleanup(&p);
    }

    #[test]
    fn wal_recovery_rejects_missing_durable_bytes() {
        let (shm, p) = wal_open_test("missing");
        let pg = shm.data_start;
        commit_one(&shm, 1, pg, 0x11);
        commit_one(&shm, 2, pg, 0x22);
        let end2 = shm.wal_len().load(Ordering::Acquire);
        // the lock area says end2 bytes were durable, but the file was
        // truncated below that (deleted/replaced wal): acknowledged commits
        // are gone and recovery must refuse, not silently regress
        let wal_f = OpenOptions::new().write(true).open(wal_path(&p)).unwrap();
        wal_f.set_len(end2 - 1).unwrap();
        assert!(matches!(shm.wal_recover(), Err(Error::Corrupt(_))));
        wal_cleanup(&p);
    }

    #[test]
    fn wal_orphan_record_beyond_wal_len() {
        // a writer that died between fdatasync and the wal_len advance leaves
        // a complete, durable record beyond wal_len that was never
        // acknowledged
        let (shm, p) = wal_open_test("orphan");
        let pg = shm.data_start;
        commit_one(&shm, 1, pg, 0x11);
        let end1 = shm.wal_len().load(Ordering::Acquire);

        let img = vec![0x22u8; PAGE_SIZE];
        let orphan = encode_wal_record(end1, 2, &[(pg, &img)], pg, 0, shm.data_start + 8);
        let wal_f = OpenOptions::new().write(true).open(wal_path(&p)).unwrap();
        wal_f.write_all_at(&orphan, end1).unwrap();

        // reboot path: recovery may adopt the orphan (complete record ⇒
        // either outcome of the in-flight commit is legal; adopting keeps
        // the scan rule simple)
        shm.wal_ckpt().store(0, Ordering::Release);
        let end = shm.wal_recover().unwrap();
        assert_eq!(end, end1 + orphan.len() as u64);
        assert_eq!(shm.newest_meta_ungated().unwrap().txn_id, 2);

        // successor path: a live successor trusts wal_len and appends OVER
        // torn/orphan bytes; the overwritten offset re-binds the checksum
        shm.wal_len().store(end1, Ordering::Release);
        shm.wal_ckpt().store(0, Ordering::Release);
        let snap = commit_one(&shm, 2, pg + 1, 0x33);
        assert_eq!(snap.txn_id, 2);
        shm.wal_ckpt().store(0, Ordering::Release);
        shm.wal_len().store(0, Ordering::Release);
        let end = shm.wal_recover().unwrap();
        let m = shm.newest_meta_ungated().unwrap();
        assert_eq!((m.txn_id, m.catalog_root), (2, pg + 1), "successor's record wins");
        assert!(end >= end1);
        wal_cleanup(&p);
    }

    #[test]
    fn wal_scan_stops_on_txn_gap() {
        let (shm, p) = wal_open_test("txngap");
        let pg = shm.data_start;
        commit_one(&shm, 1, pg, 0x11);
        let end1 = shm.wal_len().load(Ordering::Acquire);
        // hand-append a checksum-valid record with a NON-consecutive txn id:
        // the scan must treat it as the end of the valid prefix
        let img = vec![0x99u8; PAGE_SIZE];
        let stray = encode_wal_record(end1, 5, &[(pg, &img)], pg, 0, shm.data_start + 8);
        let wal_f = OpenOptions::new().write(true).open(wal_path(&p)).unwrap();
        wal_f.write_all_at(&stray, end1).unwrap();
        shm.wal_len().store(0, Ordering::Release);
        shm.wal_ckpt().store(0, Ordering::Release);
        let end = shm.wal_recover().unwrap();
        assert_eq!(end, end1, "gap in txn chain ends the prefix");
        assert_eq!(shm.newest_meta_ungated().unwrap().txn_id, 1);
        assert!(shm.page(pg).unwrap().iter().all(|&b| b == 0x11));
        wal_cleanup(&p);
    }

    #[test]
    fn wal_lock_area_file_offsets_are_stable() {
        // the exported file offsets are tooling ABI (power-loss simulator
        // pokes cold files); pin them against the accessor locations
        let (shm, p) = wal_open_test("offsets");
        shm.wal_len().store(0xAABB_CCDD, Ordering::Release);
        shm.wal_ckpt().store(0x1122_3344, Ordering::Release);
        shm.msync_page(LOCK_PAGE).unwrap();
        let raw = std::fs::read(&p).unwrap();
        let at = |off: u64| {
            u64::from_le_bytes(raw[off as usize..off as usize + 8].try_into().unwrap())
        };
        assert_eq!(at(WAL_LEN_FILE_OFFSET), 0xAABB_CCDD);
        assert_eq!(at(WAL_CKPT_FILE_OFFSET), 0x1122_3344);
        // boot id offset: what the file holds at that offset equals this
        // process's boot id (we formatted the file)
        assert_eq!(
            &raw[BOOT_ID_FILE_OFFSET as usize..BOOT_ID_FILE_OFFSET as usize + 16],
            &boot_id().unwrap()
        );
        shm.wal_len().store(0, Ordering::Release);
        shm.wal_ckpt().store(0, Ordering::Release);
        wal_cleanup(&p);
    }

    #[test]
    fn wal_format_truncates_debris_and_reopen_recovers() {
        // a wal file left over from a previous incarnation must never be
        // replayed into a freshly formatted database
        let p = wal_tmp_path("debris");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(wal_path(&p));
        {
            let (shm, _) = {
                let shm =
                    Shm::open(&p, 4 * 1024 * 1024, 64, Durability::Wal, &[7u8; 32], &mpedb_types::FilePerms::default()).unwrap();
                (shm, ())
            };
            commit_one(&shm, 1, shm.data_start, 0x44);
            assert!(shm.wal_len().load(Ordering::Acquire) > 0);
        }
        // destroy the main file (dead creator debris) but leave the wal
        std::fs::write(&p, b"").unwrap();
        let shm = Shm::open(&p, 4 * 1024 * 1024, 64, Durability::Wal, &[7u8; 32], &mpedb_types::FilePerms::default()).unwrap();
        assert_eq!(shm.newest_meta().unwrap().txn_id, 0, "fresh db, no replayed debris");
        assert_eq!(shm.wal_len().load(Ordering::Acquire), 0);
        assert_eq!(
            std::fs::metadata(wal_path(&p)).unwrap().len(),
            WAL_GROW_CHUNK,
            "format resets the log to one preallocated chunk"
        );
        wal_cleanup(&p);
    }

    // ---------------- async (deferred-fsync WAL) contract, §5.4.2 ----------

    #[test]
    fn async_append_is_visible_before_it_is_durable() {
        // The defining contract of the async class: a commit is VISIBLE (its
        // meta is flipped, readers see it) the instant it is APPENDED, which is
        // BEFORE its bytes are fdatasync-durable. The durable watermark
        // (wal_len) lags behind the append cursor (wal_appended) until a
        // deferred flush runs.
        let (shm, p) = wal_open_test_mode("async-visible", Durability::Async);
        let pg = shm.data_start;
        assert_eq!(shm.wal_len().load(Ordering::Acquire), 0);
        assert_eq!(shm.wal_appended().load(Ordering::Acquire), 0);

        let snap = append_one_async(&shm, 1, pg, 0x11);
        // visible: newest meta reflects the appended commit
        assert_eq!(shm.newest_meta().unwrap().txn_id, 1);
        assert_eq!(shm.newest_meta().unwrap().catalog_root, snap.catalog_root);
        // durable frontier has NOT moved — no per-commit fdatasync
        let appended = shm.wal_appended().load(Ordering::Acquire);
        assert!(appended > 0);
        assert_eq!(shm.wal_len().load(Ordering::Acquire), 0, "not yet fdatasync'd");

        // a second commit, still un-flushed
        append_one_async(&shm, 2, pg + 1, 0x22);
        let appended2 = shm.wal_appended().load(Ordering::Acquire);
        assert!(appended2 > appended);
        assert_eq!(shm.wal_len().load(Ordering::Acquire), 0);

        // the deferred flush publishes everything appended so far as durable
        let newly = shm.wal_flush_deferred().unwrap();
        assert_eq!(newly, appended2);
        assert_eq!(shm.wal_len().load(Ordering::Acquire), appended2);
        // idempotent: nothing new to flush
        assert_eq!(shm.wal_flush_deferred().unwrap(), 0);
        wal_cleanup(&p);
    }

    #[test]
    fn async_recovers_flushed_prefix_across_reboot() {
        // A power loss AFTER a deferred flush must recover everything flushed —
        // recovery replays the log exactly like wal mode (same records).
        let (shm, p) = wal_open_test_mode("async-recover", Durability::Async);
        let pg = shm.data_start;
        append_one_async(&shm, 1, pg, 0x11);
        append_one_async(&shm, 2, pg + 1, 0x22);
        shm.wal_flush_deferred().unwrap();
        let durable = shm.wal_len().load(Ordering::Acquire);
        assert_eq!(durable, shm.wal_appended().load(Ordering::Acquire));

        // reboot that lost all volatile state: stale lock area, regressed metas
        shm.wal_ckpt().store(0, Ordering::Release);
        shm.wal_len().store(durable, Ordering::Release);
        shm.wal_appended().store(0, Ordering::Release);
        let genesis = MetaSnapshot {
            slot: 0,
            txn_id: 0,
            catalog_root: 0,
            freelist_root: 0,
            high_water: shm.data_start,
        };
        shm.write_meta_slot(META_PAGE_A, &genesis);
        shm.write_meta_slot(META_PAGE_B, &genesis);
        shm.page_mut_unchecked(pg).unwrap().fill(0xEE);
        shm.page_mut_unchecked(pg + 1).unwrap().fill(0xEE);

        let end = shm.wal_recover().unwrap();
        assert_eq!(end, durable);
        assert_eq!(shm.newest_meta_ungated().unwrap().txn_id, 2);
        assert!(shm.page(pg).unwrap().iter().all(|&b| b == 0x11));
        assert!(shm.page(pg + 1).unwrap().iter().all(|&b| b == 0x22));
        // the append cursor is re-seated at the recovered prefix end
        assert_eq!(shm.wal_appended().load(Ordering::Acquire), durable);
        wal_cleanup(&p);
    }

    #[test]
    fn async_unflushed_tail_is_a_clean_torn_tail() {
        // The loss window: commits appended but not yet flushed vanish AS WHOLE
        // RECORDS on power loss (never partially applied) — crash-consistent.
        let (shm, p) = wal_open_test_mode("async-torn", Durability::Async);
        let pg = shm.data_start;
        append_one_async(&shm, 1, pg, 0x11);
        shm.wal_flush_deferred().unwrap();
        let durable = shm.wal_len().load(Ordering::Acquire);

        // commit 2 appended but NOT flushed → in the loss window
        append_one_async(&shm, 2, pg + 1, 0x22);
        assert!(shm.wal_appended().load(Ordering::Acquire) > durable);

        // power loss: only the fdatasync'd prefix [0,durable) is guaranteed on
        // disk. Simulate the worst case — the un-flushed tail did not survive.
        let wal_f = OpenOptions::new().write(true).open(wal_path(&p)).unwrap();
        wal_f.set_len(durable).unwrap();
        drop(wal_f);
        shm.wal_ckpt().store(0, Ordering::Release);
        shm.wal_len().store(durable, Ordering::Release);
        shm.page_mut_unchecked(pg + 1).unwrap().fill(0xEE);

        let end = shm.wal_recover().unwrap();
        assert_eq!(end, durable, "recovery lands on the flushed prefix");
        assert_eq!(shm.newest_meta_ungated().unwrap().txn_id, 1, "commit 2 lost cleanly");
        assert!(shm.page(pg).unwrap().iter().all(|&b| b == 0x11));
        wal_cleanup(&p);
    }

    #[test]
    fn async_checkpoint_reclaims_to_append_cursor() {
        // async checkpoint targets the APPEND cursor: the full-mapping msync
        // makes every appended commit durable in the main file, so wal_ckpt
        // AND wal_len jump to wal_appended and the log below is reclaimed.
        let (shm, p) = wal_open_test_mode("async-ckpt", Durability::Async);
        let pg = shm.data_start;
        append_one_async(&shm, 1, pg, 0x11);
        append_one_async(&shm, 2, pg + 1, 0x22);
        let appended = shm.wal_appended().load(Ordering::Acquire);
        assert_eq!(shm.wal_len().load(Ordering::Acquire), 0, "nothing fdatasync'd yet");

        shm.wal_checkpoint_if(1).unwrap();
        assert_eq!(shm.wal_ckpt().load(Ordering::Acquire), appended);
        assert_eq!(
            shm.wal_len().load(Ordering::Acquire),
            appended,
            "checkpoint's full msync makes the appended prefix durable"
        );

        // a post-checkpoint commit is still recoverable from the durable ckpt
        append_one_async(&shm, 3, pg + 2, 0x33);
        shm.wal_flush_deferred().unwrap();
        let end_len = shm.wal_appended().load(Ordering::Acquire);
        shm.wal_len().store(appended, Ordering::Release); // stale writeback
        shm.page_mut_unchecked(pg + 2).unwrap().fill(0xEE);
        let end = shm.wal_recover().unwrap();
        assert_eq!(end, end_len);
        assert_eq!(shm.newest_meta_ungated().unwrap().txn_id, 3);
        assert!(shm.page(pg + 2).unwrap().iter().all(|&b| b == 0x33));
        wal_cleanup(&p);
    }
}
