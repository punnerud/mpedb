//! Shared-memory wal operations; ordering and file layout are unchanged.
use super::*;

impl Shm {
    // ---------- write-ahead log (durability = wal) ----------

    pub(super) fn wal_file(&self) -> Result<&WalFile> {
        self.wal
            .as_ref()
            .ok_or_else(|| Error::Internal("wal operation on a non-wal database".into()))
    }

    /// Open (creating if absent) the companion `<path>-wal` file. Called on
    /// every attach path of a wal-mode database, before format/recovery.
    pub(super) fn attach_wal(&mut self, db_path: &Path) -> Result<()> {
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
    pub(super) fn wal_reset_for_format(&self) -> Result<()> {
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
    pub(super) fn wal_ensure_alloc(&self, need: u64) -> Result<()> {
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
        // Reuse a thread-local encode buffer: writer lock is exclusive per
        // process, so there is no concurrent wal_commit on this thread, and
        // avoiding a fresh multi-page allocation per durable ack is free
        // correctness-wise (buffer is only live until write_all_at returns).
        thread_local! {
            static ENC: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        ENC.with(|cell| {
            let mut buf = cell.borrow_mut();
            // Build page list without a second heap Vec of slices: encode
            // header with count, then append each image from the mmap.
            buf.clear();
            buf.reserve(WAL_RECORD_FIXED + dirty_sorted.len() * 256);
            buf.extend_from_slice(&WAL_MAGIC.to_le_bytes());
            buf.extend_from_slice(&snap.txn_id.to_le_bytes());
            buf.extend_from_slice(&(dirty_sorted.len() as u32).to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes()); // rec_len placeholder
            for &id in dirty_sorted {
                let img = self.page(id)?;
                append_wal_page_img(&mut buf, id, img);
            }
            buf.extend_from_slice(&snap.catalog_root.to_le_bytes());
            buf.extend_from_slice(&snap.freelist_root.to_le_bytes());
            buf.extend_from_slice(&snap.high_water.to_le_bytes());
            buf.extend_from_slice(&snap.extent_map_root.to_le_bytes());
            let rec_len = (buf.len() + 8) as u32;
            buf[16..20].copy_from_slice(&rec_len.to_le_bytes());
            let sum = wal_checksum(off, &buf);
            buf.extend_from_slice(&sum.to_le_bytes());
            self.wal_ensure_alloc(off + buf.len() as u64)?;
            wal.file.write_all_at(&buf, off)?;
            if crate::os::fdatasync(wal.file.as_raw_fd()) != 0 {
                return Err(io_err("fdatasync(wal)"));
            }
            self.wal_len().store(off + buf.len() as u64, Ordering::Release);
            Ok(())
        })
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
        // Same thread-local encode buffer as `wal_commit` (writer lock exclusive).
        thread_local! {
            static ENC: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        ENC.with(|cell| {
            let mut buf = cell.borrow_mut();
            buf.clear();
            buf.reserve(WAL_RECORD_FIXED + dirty_sorted.len() * 256);
            buf.extend_from_slice(&WAL_MAGIC.to_le_bytes());
            buf.extend_from_slice(&snap.txn_id.to_le_bytes());
            buf.extend_from_slice(&(dirty_sorted.len() as u32).to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            for &id in dirty_sorted {
                let img = self.page(id)?;
                append_wal_page_img(&mut buf, id, img);
            }
            buf.extend_from_slice(&snap.catalog_root.to_le_bytes());
            buf.extend_from_slice(&snap.freelist_root.to_le_bytes());
            buf.extend_from_slice(&snap.high_water.to_le_bytes());
            buf.extend_from_slice(&snap.extent_map_root.to_le_bytes());
            let rec_len = (buf.len() + 8) as u32;
            buf[16..20].copy_from_slice(&rec_len.to_le_bytes());
            let sum = wal_checksum(off, &buf);
            buf.extend_from_slice(&sum.to_le_bytes());
            self.wal_ensure_alloc(off + buf.len() as u64)?;
            wal.file.write_all_at(&buf, off)?;
            self.wal_appended()
                .store(off + buf.len() as u64, Ordering::Release);
            Ok(())
        })
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
    /// 1. Make every page image in the log prefix durable in the main file
    ///    (plus both meta slots). On Linux `msync(MS_SYNC)` is dirty-tag
    ///    driven, so a whole-mapping call is fine and cheap. On Darwin
    ///    `msync` costs the RANGE WIDTH, so a 1 GiB whole-file call costs
    ///    milliseconds even when only a few dozen pages are dirty — measured
    ///    M3 Pro: 2.4 ms for 1 GiB vs ~0.3 ms for 4 MiB. There we msync only
    ///    the page ids that appear in WAL records `[ckpt, target)` (plus
    ///    meta A/B): those are exactly the pages COW commits dirtied since
    ///    the last checkpoint, so the main file then contains every commit
    ///    the log could recover. `MPEDB_WAL_CKPT_FULL=1` restores the
    ///    whole-mapping arm on every platform for A/B.
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
        // 1. main file catches up to the log
        if wal_ckpt_full_msync() || cfg!(target_os = "linux") {
            self.msync_range(0, self.len)?;
        } else if self.wal_msync_logged_pages(ckpt, target)? {
            // Both meta slots may have been published by post-ckpt commits
            // (write_meta_slot stores into the mmap without an msync in wal
            // mode). Recovery does not trust metas, but a post-checkpoint
            // crash that loses only the meta pages would force a WAL replay
            // we just declared redundant — msync them explicitly.
            self.msync_page(META_PAGE_A)?;
            self.msync_page(META_PAGE_B)?;
        } else {
            // The scan did not reach `target`, so the pages it collected are a
            // PREFIX of what this checkpoint is about to declare durable —
            // and `wal_ckpt` is advanced to `target` unconditionally below,
            // after which the records that could have recovered the rest are
            // punched away. Bookkeeping outrunning the data it claims is the
            // one way a checkpoint loses an acknowledged commit, so the
            // anomaly path does what §5.4 says a checkpoint does: msync the
            // WHOLE mapping. It can only ever flush more, never less, and it
            // is the same call the Linux arm makes on every checkpoint.
            self.msync_range(0, self.len)?;
        }
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

    /// msync every data page image in WAL records `[from, to)`. Used by the
    /// Darwin checkpoint arm so a multi-hundred-MiB mapping does not pay for
    /// a full-file `msync` when only the logged pages changed.
    ///
    /// Returns whether the scan actually REACHED `to`. It is a selective flush
    /// driven by reading the log back, and every malformed-record path below
    /// stops the walk — so a caller that advanced the checkpoint watermark
    /// regardless would declare durable a range whose tail was never flushed,
    /// and then reclaim the records that could have recovered it. §5.4 states
    /// the invariant the recovery path leans on as a FULL-MAPPING msync; this
    /// arm may only stand in for it when it covered the whole range, and says
    /// so here rather than leaving the caller to assume it.
    #[must_use = "false means the range was not covered — the caller must msync the whole mapping"]
    pub(super) fn wal_msync_logged_pages(&self, from: u64, to: u64) -> Result<bool> {
        if to <= from {
            return Ok(true);
        }
        let wal = self.wal_file()?;
        let mut off = from;
        let mut header = [0u8; WAL_HDR_LEN];
        let mut buf: Vec<u8> = Vec::new();
        let mut ids: Vec<u64> = Vec::new();
        while off < to {
            if read_full_at(&wal.file, &mut header, off)? < header.len() {
                break;
            }
            let n_pages = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            let rec_len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
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
                break;
            }
            let Some(rec) = decode_wal_record(&buf, off, self.page_count) else {
                break;
            };
            for (id, _) in &rec.pages {
                if *id >= self.data_start {
                    ids.push(*id);
                }
            }
            off += rec.len;
            if off > to {
                break;
            }
        }
        ids.sort_unstable();
        ids.dedup();
        // Coalesce contiguous page ids into range msyncs (Darwin still pays
        // range width, so tight runs matter; gaps skip clean pages).
        let mut i = 0;
        while i < ids.len() {
            let start = ids[i];
            let mut end = start;
            while i + 1 < ids.len() && ids[i + 1] == end + 1 {
                i += 1;
                end = ids[i];
            }
            self.msync_range(
                start as usize * PAGE_SIZE,
                (end - start + 1) as usize * PAGE_SIZE,
            )?;
            i += 1;
        }
        // `>=`, not `==`: a record may straddle `to`, and the loop flushes it
        // whole. Overshooting the range is coverage, not a gap.
        Ok(off >= to)
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
                extent_map_root: rec.extent_map_root,
                // WAL records carry no gen; 0 after replay is SAFE — the
                // signal is compared for INEQUALITY (any mismatch reloads
                // from the catalog), and recovery re-attaches everyone.
                schema_gen: 0,
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


}
