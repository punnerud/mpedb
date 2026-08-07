//! Raw whole-database backup and restore.
//!
//! The alternative was already there and is why this exists: dumping a database
//! as SQL means one `SELECT *` per table, a literal per value, a hex expansion
//! per blob byte, and a full parse on the way back. For a wasm build — where
//! the whole arena is already in memory and there is no filesystem to
//! `VACUUM INTO` — that is a lot of work to move bytes that are already
//! contiguous.
//!
//! # Why this needs no lock
//!
//! A backup runs under a plain [`crate::engine::ReadTxn`] and does not block
//! writers. That falls out of two invariants the engine already maintains, and
//! it is worth naming both because the whole design rests on them:
//!
//! 1. **Committed pages are immutable.** `page_mut` is only reachable for pages
//!    the current write transaction allocated (COW discipline), so no page this
//!    snapshot can see is rewritten under it.
//! 2. **A pinned reader holds the reuse floor.** Pages freed by a commit become
//!    reusable only when that commit is at or below the oldest-pinned bound, so
//!    while the backup's reader is pinned, nothing it can see is handed out
//!    again.
//!
//! Together: every page below the snapshot's `high_water` is frozen for the
//! duration, and pages a concurrent writer allocates ABOVE it are not part of
//! this snapshot and are correctly not copied. Taking the writer lock instead
//! would have been simpler to argue and would have stalled every writer for the
//! length of a copy — for no gain.
//!
//! # What is NOT copied
//!
//! Pages 0..[`Shm::data_start`] — the two meta pages, the lock area, the reader
//! table, the intent ring, the notification region and the committed-footprint
//! ring. Those are PROCESS state: pids, generation words, robust-mutex
//! ownership. Copying them would restore another machine's live processes as
//! though they were still attached. The header carries the meta's logical
//! contents (the roots) instead, and `restore` builds the rest fresh.
//!
//! That is also why `max_readers` is in the header and is checked. The reader
//! table's width decides `data_start`, so restoring pages into an arena with a
//! different `max_readers` would land every one of them at the wrong page id —
//! a silently corrupt database rather than a failed restore.

use crate::shm::{MetaSnapshot, Shm};
use mpedb_types::PAGE_SIZE;
use mpedb_types::{Error, Result};

/// `"MPEDBBAK"` — deliberately distinct from the arena's own `MPEDB1\0\0`, so
/// a tool handed either can tell which it has and say so.
pub const MAGIC: [u8; 8] = *b"MPEDBBAK";

/// Bumped when the header layout or the body's meaning changes. A reader
/// refuses a version above its own by name rather than misreading it.
pub const FORMAT: u32 = 1;

/// Header size in bytes; the body starts here.
const HEADER: usize = 96;

// Header field offsets (all little-endian).
const H_MAGIC: usize = 0; // [u8; 8]
const H_FORMAT: usize = 8; // u32
const H_PAGE_SIZE: usize = 12; // u32
const H_MAX_READERS: usize = 16; // u32
const H_PAD: usize = 20; // u32, zero
const H_TXN_ID: usize = 24; // u64
const H_CATALOG_ROOT: usize = 32; // u64
const H_FREELIST_ROOT: usize = 40; // u64
const H_HIGH_WATER: usize = 48; // u64
const H_EXTENT_MAP_ROOT: usize = 56; // u64
const H_SCHEMA_GEN: usize = 64; // u64
const H_FIRST_PAGE: usize = 72; // u64
const H_N_PAGES: usize = 80; // u64
const H_CHECKSUM: usize = 88; // u64 — xxh3_64 over the BODY

/// What a backup says about itself, without reading its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupInfo {
    pub format: u32,
    pub page_size: u32,
    pub max_readers: u32,
    pub txn_id: u64,
    pub high_water: u64,
    pub first_page: u64,
    pub n_pages: u64,
    pub schema_gen: u64,
}

impl BackupInfo {
    /// Total size of the backup this header describes.
    pub fn total_len(&self) -> u64 {
        HEADER as u64 + self.n_pages * self.page_size as u64
    }
}

fn w32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn w64(buf: &mut [u8], at: usize, v: u64) {
    buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn r32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("bounds checked by caller"))
}

fn r64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("bounds checked by caller"))
}

/// Read a backup's header without copying its body.
///
/// Every field is bounds- and consistency-checked here, so the rest of this
/// module can index the body arithmetically. `n_pages` in particular is checked
/// against the ACTUAL length: a header claiming more pages than the file holds
/// is the shape a truncated download has, and it must not become an
/// out-of-bounds read.
pub fn read_header(bytes: &[u8]) -> Result<BackupInfo> {
    if bytes.len() < HEADER {
        return Err(Error::Corrupt("backup: shorter than its own header".into()));
    }
    if bytes[H_MAGIC..H_MAGIC + 8] != MAGIC {
        return Err(Error::Corrupt(
            "not an mpedb backup (bad magic) — an arena file starts `MPEDB1`, a SQL dump is \
             a gzip"
                .into(),
        ));
    }
    let format = r32(bytes, H_FORMAT);
    if format > FORMAT {
        return Err(Error::Corrupt(format!(
            "backup format {format}; this build reads up to {FORMAT}"
        )));
    }
    let page_size = r32(bytes, H_PAGE_SIZE);
    if page_size as usize != PAGE_SIZE {
        return Err(Error::Corrupt(format!(
            "backup page size {page_size}; this build uses {PAGE_SIZE}"
        )));
    }
    let info = BackupInfo {
        format,
        page_size,
        max_readers: r32(bytes, H_MAX_READERS),
        txn_id: r64(bytes, H_TXN_ID),
        high_water: r64(bytes, H_HIGH_WATER),
        first_page: r64(bytes, H_FIRST_PAGE),
        n_pages: r64(bytes, H_N_PAGES),
        schema_gen: r64(bytes, H_SCHEMA_GEN),
    };
    if info.max_readers == 0 {
        return Err(Error::Corrupt("backup: max_readers is zero".into()));
    }
    // `first_page + n_pages` must be exactly the high water: the body IS the
    // data region, and a header that disagrees with itself cannot be trusted to
    // place pages.
    if info.first_page.checked_add(info.n_pages) != Some(info.high_water) {
        return Err(Error::Corrupt(
            "backup: page range does not reach the recorded high water".into(),
        ));
    }
    if info.first_page != crate::shm::data_start_page(info.max_readers) {
        return Err(Error::Corrupt(
            "backup: first page does not match the geometry its own max_readers implies".into(),
        ));
    }
    if bytes.len() as u64 != info.total_len() {
        return Err(Error::Corrupt(format!(
            "backup: header claims {} bytes, file is {}",
            info.total_len(),
            bytes.len()
        )));
    }
    let want = r64(bytes, H_CHECKSUM);
    let got = xxhash_rust::xxh3::xxh3_64(&bytes[HEADER..]);
    if want != got {
        return Err(Error::Corrupt(
            "backup: checksum mismatch — the file is damaged or truncated".into(),
        ));
    }
    Ok(info)
}

/// Copy a consistent image of the database out.
///
/// `meta` is the reader's snapshot and `shm` the live mapping; the caller holds
/// the read transaction open across this call, which is what freezes the pages
/// (see the module docs). Passing them separately rather than taking the
/// `ReadTxn` keeps this module free of the engine's transaction types.
pub fn write_backup(shm: &Shm, meta: &MetaSnapshot) -> Result<Vec<u8>> {
    let first = shm.data_start;
    let high = meta.high_water;
    if high < first {
        return Err(Error::Corrupt(format!(
            "high water {high} is below the data region's start {first}"
        )));
    }
    let n = high - first;
    let total = HEADER + (n as usize) * PAGE_SIZE;
    let mut out = vec![0u8; total];

    out[H_MAGIC..H_MAGIC + 8].copy_from_slice(&MAGIC);
    w32(&mut out, H_FORMAT, FORMAT);
    w32(&mut out, H_PAGE_SIZE, PAGE_SIZE as u32);
    w32(&mut out, H_MAX_READERS, shm.max_readers);
    w32(&mut out, H_PAD, 0);
    w64(&mut out, H_TXN_ID, meta.txn_id);
    w64(&mut out, H_CATALOG_ROOT, meta.catalog_root);
    w64(&mut out, H_FREELIST_ROOT, meta.freelist_root);
    w64(&mut out, H_HIGH_WATER, high);
    w64(&mut out, H_EXTENT_MAP_ROOT, meta.extent_map_root);
    w64(&mut out, H_SCHEMA_GEN, meta.schema_gen);
    w64(&mut out, H_FIRST_PAGE, first);
    w64(&mut out, H_N_PAGES, n);

    for i in 0..n {
        let src = shm.page(first + i)?;
        let at = HEADER + (i as usize) * PAGE_SIZE;
        out[at..at + PAGE_SIZE].copy_from_slice(src);
    }
    let sum = xxhash_rust::xxh3::xxh3_64(&out[HEADER..]);
    w64(&mut out, H_CHECKSUM, sum);
    Ok(out)
}

/// Write a backup's pages into a freshly created arena and publish its meta.
///
/// The target must be EMPTY — a database this process just created — and must
/// have been created with the same `max_readers` and enough pages. Both are
/// checked; a mismatch is a named refusal rather than a partial write, because
/// a half-restored arena is indistinguishable from a corrupt one.
///
/// The frozen meta header (magic, format, page count, max_readers, durability,
/// SEED schema hash) is the TARGET's and is left alone. Only the flipping meta
/// — the roots, the high water, the schema generation — comes from the backup.
/// That is what makes a restore land on a file that is legitimately this
/// process's own: the seed hash still matches the config that created it, so
/// attach validation is unchanged.
pub fn apply_backup(shm: &Shm, bytes: &[u8]) -> Result<BackupInfo> {
    let info = read_header(bytes)?;
    if shm.max_readers != info.max_readers {
        return Err(Error::Config(format!(
            "restore: this database reserves {} reader slots, the backup was taken from one \
             with {} — the reader table's width decides where page {} lives, so the pages \
             would land at the wrong ids",
            shm.max_readers, info.max_readers, info.first_page
        )));
    }
    if shm.page_count < info.high_water {
        return Err(Error::Config(format!(
            "restore: the backup needs {} pages ({} MiB) and this database has {} — create \
             it with a larger size_mb",
            info.high_water,
            (info.high_water * PAGE_SIZE as u64) / (1024 * 1024),
            shm.page_count
        )));
    }
    for i in 0..info.n_pages {
        let at = HEADER + (i as usize) * PAGE_SIZE;
        let dst = shm.page_mut_unchecked(info.first_page + i)?;
        dst.copy_from_slice(&bytes[at..at + PAGE_SIZE]);
    }
    // The pages first, the meta second, and never the other way round: a meta
    // published before its pages names roots that do not exist yet, and a
    // crash there leaves a database that opens and then cannot read itself.
    // This order leaves the target as it was (empty) if we die mid-copy.
    let meta = MetaSnapshot {
        slot: 0,
        txn_id: info.txn_id,
        catalog_root: r64(bytes, H_CATALOG_ROOT),
        freelist_root: r64(bytes, H_FREELIST_ROOT),
        high_water: info.high_water,
        extent_map_root: r64(bytes, H_EXTENT_MAP_ROOT),
        schema_gen: info.schema_gen,
    };
    // Durable BEFORE the meta names them, and the barrier is the point: a
    // reordered write that lands the meta first leaves a database that opens
    // and cannot read itself.
    shm.msync_range(
        info.first_page as usize * PAGE_SIZE,
        info.n_pages as usize * PAGE_SIZE,
    )?;
    let prev = shm.newest_meta()?.slot;
    shm.write_meta_slot(prev, &meta);
    shm.msync_range(0, 2 * PAGE_SIZE)?;
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header describing a body that is not there must be refused, not
    /// indexed into. This is the shape a truncated download has.
    #[test]
    fn a_truncated_backup_is_refused_rather_than_read_out_of_bounds() {
        let mut buf = [0u8; HEADER];
        buf[H_MAGIC..H_MAGIC + 8].copy_from_slice(&MAGIC);
        w32(&mut buf, H_FORMAT, FORMAT);
        w32(&mut buf, H_PAGE_SIZE, PAGE_SIZE as u32);
        w32(&mut buf, H_MAX_READERS, 8);
        let first = crate::shm::data_start_page(8);
        w64(&mut buf, H_FIRST_PAGE, first);
        w64(&mut buf, H_N_PAGES, 1000);
        w64(&mut buf, H_HIGH_WATER, first + 1000);
        let e = read_header(&buf).unwrap_err().to_string();
        assert!(e.contains("file is"), "{e}");
    }

    #[test]
    fn every_truncation_of_a_header_errs_and_never_panics() {
        let mut buf = [0u8; HEADER];
        buf[H_MAGIC..H_MAGIC + 8].copy_from_slice(&MAGIC);
        for cut in 0..HEADER {
            assert!(read_header(&buf[..cut]).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn the_other_two_formats_are_named_rather_than_rejected_blankly() {
        // Header-length so the MAGIC check is what decides, not the length one.
        let mut arena = [0u8; HEADER];
        arena[..8].copy_from_slice(b"MPEDB1\0\0");
        let e = read_header(&arena).unwrap_err().to_string();
        assert!(e.contains("MPEDB1"), "{e}");

        let mut gz = [0u8; HEADER];
        gz[0] = 0x1f;
        gz[1] = 0x8b;
        let e = read_header(&gz).unwrap_err().to_string();
        assert!(e.contains("gzip"), "{e}");
    }

    /// The geometry check is the one that prevents SILENT corruption: pages
    /// placed at the wrong ids would restore without complaint.
    /// The geometry check is the one that prevents SILENT corruption: pages
    /// placed at the wrong ids would restore without complaint.
    ///
    /// The two widths are 8 and 200 rather than 8 and 64, and that is not
    /// arbitrary — `reader_table_pages` rounds UP to a page, so 8 slots and 64
    /// slots both fit in ONE page and produce the SAME `data_start_page`. A
    /// test written with 64 passes for the wrong reason: nothing contradicts.
    #[test]
    fn a_header_whose_first_page_contradicts_its_max_readers_is_refused() {
        assert_ne!(
            crate::shm::data_start_page(8),
            crate::shm::data_start_page(200),
            "the test needs two widths that actually differ"
        );
        let mut buf = [0u8; HEADER];
        buf[H_MAGIC..H_MAGIC + 8].copy_from_slice(&MAGIC);
        w32(&mut buf, H_FORMAT, FORMAT);
        w32(&mut buf, H_PAGE_SIZE, PAGE_SIZE as u32);
        w32(&mut buf, H_MAX_READERS, 8);
        // A first page that belongs to a DIFFERENT reader-table width.
        w64(&mut buf, H_FIRST_PAGE, crate::shm::data_start_page(200));
        w64(&mut buf, H_N_PAGES, 0);
        w64(&mut buf, H_HIGH_WATER, crate::shm::data_start_page(200));
        let e = read_header(&buf).unwrap_err().to_string();
        assert!(e.contains("geometry"), "{e}");
    }
}
