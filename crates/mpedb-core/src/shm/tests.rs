    use super::*;
    use std::sync::atomic::Ordering;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = crate::scratch_dir().join("mpedb-shm-tests");
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

    /// POSIX mode bits: Unix-only by construction, not by neglect.
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
            extent_map_root: 0,
            schema_gen: 0,
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
            extent_map_root: 0,
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

    /// An in-memory database must touch the filesystem NOWHERE — and in
    /// particular must not drop a `:memory:.wlock` sidecar into the process's
    /// current directory. It did, on every non-Linux platform, until #159:
    /// harmless-looking litter on macOS, and fatal on Windows, where `:` is
    /// not a legal filename character so the create fails and `:memory:`
    /// cannot open at all. Wine caught it by leaving the file in the repo.
    ///
    /// It sweeps the litter FIRST. Asserting on a bare `exists()` made a file
    /// dropped by one pre-fix run fail every run after it, on a machine where
    /// the fix was in — a test that cannot tell "the bug is back" from "the bug
    /// was here once" (the M3 reported exactly that, from a file dated the day
    /// before the fix). Sweeping makes the assertion about THIS open.
    #[test]
    fn an_in_memory_database_leaves_no_sidecar() {
        const LITTER: [&str; 2] = [":memory:.wlock", ":memory:"];
        let zero = [0u8; 32];
        let dir = std::env::current_dir().unwrap();
        for litter in LITTER {
            let _ = std::fs::remove_file(dir.join(litter));
        }
        let shm = Shm::open_memory(8 << 20, 8, Durability::None, &zero).unwrap();
        let _ = shm.claim_and_pin().unwrap();
        for litter in LITTER {
            assert!(
                !dir.join(litter).exists(),
                "in-memory open created {litter} in {}",
                dir.display()
            );
        }
    }

    #[test]
    fn private_memory_pin_table() {
        let zero = [0u8; 32];
        let shm = Shm::open_memory(8 << 20, 8, Durability::None, &zero).unwrap();
        assert!(shm.is_private);
        let (idx, word, meta) = shm.claim_and_pin().unwrap();
        assert_eq!(word, 0);
        assert_eq!(meta.txn_id, 0);
        assert!(shm.slot_still_owned(idx, word));
        assert_eq!(shm.compute_oldest_pinned(5), 0);
        assert!(shm.release_slot(idx, word));
        assert!(!shm.slot_still_owned(idx, word));
        assert!(!shm.release_slot(idx, word), "double release must fail");
        assert_eq!(shm.compute_oldest_pinned(5), 5);

        let (i1, w1, _) = shm.claim_and_pin().unwrap();
        let (i2, w2, _) = shm.claim_and_pin().unwrap();
        assert_ne!(i1, i2);
        assert_eq!(shm.compute_oldest_pinned(9), 0);
        assert!(shm.release_slot(i1, w1));
        assert!(shm.release_slot(i2, w2));
        assert_eq!(shm.compute_oldest_pinned(9), 9);
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
        let dir = mpedb_testkit::scratch_base().join("mpedb-wal-tests");
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
            extent_map_root: 0,
            schema_gen: 0,
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
        // The shared test dir is deliberately LEFT standing. Removing it here
        // ("remove_dir only succeeds on an empty directory") looked safe, but
        // raced: a STARTING test's create_dir_all sees the dir and returns, a
        // finishing test's remove_dir then catches it still empty — the
        // starter has not created its file yet — and the starter's Shm::open
        // lands in a deleted directory. CI hit that exact NotFound. An empty
        // dir left in the scratch base costs nothing.
    }

    /// Simulate one engine commit at the shm level: fill `page_id` with
    /// `fill`, append the record, flip the meta — exactly the §5.4 order.
    fn commit_one(shm: &Shm, txn: u64, page_id: u64, fill: u8) -> MetaSnapshot {
        shm.page_mut_unchecked(page_id).unwrap().fill(fill);
        let prev = shm.newest_meta_ungated().unwrap();
        let snap = MetaSnapshot {
            slot: prev.slot,
            extent_map_root: 0,
            schema_gen: 0,
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
        let rec = encode_wal_record(4096, 42, &pages, 7, 8, 9, 0);
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
        let rec = encode_wal_record(0, 1, &pages, 0, 0, 0, 0);
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
    fn wal_lean_record_elides_overflow_tail() {
        // An OVERFLOW page stores `payload` bytes right after the 16-byte
        // header and its used span ends there — the SPLIT encoding must keep
        // header + chain pointer + payload byte-for-byte and zero-fill the
        // elided tail on replay. The KIND_OVERFLOW arm of btree::used_span
        // was live in every durable blob write and, until this test (#58),
        // exercised by nothing: no test had ever pushed an overflow page
        // through WAL encode/replay.
        let mut page = vec![0xFFu8; PAGE_SIZE];
        page[0] = 3; // KIND_OVERFLOW
        page[1] = 0;
        page[2..4].copy_from_slice(&0u16.to_le_bytes());
        page[4..6].copy_from_slice(&0u16.to_le_bytes());
        let payload = 100usize;
        page[6..8].copy_from_slice(&(payload as u16).to_le_bytes());
        // The next-page chain pointer lives in the header and MUST survive —
        // losing it on replay would sever the chain silently.
        page[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        for i in 0..payload {
            page[16 + i] = (i as u8).wrapping_mul(31).wrapping_add(7);
        }
        // bytes 16+payload.. stay 0xFF: a tail no replay may resurrect

        let pages: Vec<(u64, &[u8])> = vec![(50, &page)];
        let rec = encode_wal_record(0, 1, &pages, 0, 0, 0, 0);
        assert!(
            rec.len() < WAL_RECORD_FIXED + WAL_PAGE_ENTRY / 8,
            "a 100-byte overflow chunk should encode lean, got {}",
            rec.len()
        );

        let d = decode_wal_record(&rec, 0, 1024).expect("valid lean overflow record");
        let mut got = vec![0u8; PAGE_SIZE];
        d.pages[0].1.write_into(&mut got);
        assert_eq!(
            &got[..16 + payload],
            &page[..16 + payload],
            "header + chain pointer + payload must replay byte-for-byte"
        );
        assert!(
            got[16 + payload..].iter().all(|&b| b == 0),
            "elided overflow tail must be zero-filled on replay"
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
            extent_map_root: 0,
            schema_gen: 0,
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

    /// The selective Darwin checkpoint flush must SAY when it fell short.
    ///
    /// It reads the log back to decide which pages to msync, so a malformed
    /// record stops the walk — and the caller advances `wal_ckpt` to `target`
    /// regardless, then punches away the records below it. Declaring a range
    /// durable that was only flushed up to its first bad record is the one way
    /// a checkpoint loses an acknowledged commit, and it is the shape SQLite's
    /// 2025 WAL-reset bug had (bookkeeping ahead of the data). The answer is
    /// not to make the scan cleverer: it is to make it report, so the caller
    /// can fall back to the whole-mapping msync §5.4 specifies.
    ///
    /// Exercised directly rather than through `wal_checkpoint_if`, because the
    /// selective arm is `cfg!(target_os = "linux")`-gated OFF here — the
    /// coverage decision is the part that must hold on every platform.
    #[test]
    fn a_selective_checkpoint_flush_reports_a_range_it_could_not_cover() {
        let (shm, p) = wal_open_test("ckptcover");
        let pg = shm.data_start;
        commit_one(&shm, 1, pg, 0x11);
        commit_one(&shm, 2, pg + 1, 0x22);
        let end = shm.wal_len().load(Ordering::Acquire);

        // A clean log: the scan reaches the end and says so.
        assert!(shm.wal_msync_logged_pages(0, end).unwrap());
        // An empty range is covered vacuously — not a failure to report.
        assert!(shm.wal_msync_logged_pages(end, end).unwrap());
        assert!(shm.wal_msync_logged_pages(end, 0).unwrap());

        // Corrupt the FIRST record's magic. The walk now stops at offset 0,
        // so the second commit's page is never flushed — and the caller must
        // not be told the range is durable.
        {
            // Seek + write rather than `FileExt::write_all_at`: the positional
            // form is `std::os::unix` on unix and `seek_write` on Windows, and
            // this test does not need either. It broke the Windows lib-test
            // build — the library compiled, so only `cargo test` saw it.
            use std::io::{Seek, SeekFrom, Write};
            let mut wal = std::fs::OpenOptions::new()
                .write(true)
                .open(wal_path(&p))
                .unwrap();
            wal.seek(SeekFrom::Start(0)).unwrap();
            wal.write_all(b"XXXX").unwrap();
        }
        assert!(
            !shm.wal_msync_logged_pages(0, end).unwrap(),
            "a scan that stopped at the first record must not claim the range"
        );
        // …and a range that starts AFTER the damage is still coverable, so the
        // report is about what was walked, not a blanket refusal once a file
        // has any bad byte in it.
        let second = {
            let wal = std::fs::File::open(wal_path(&p)).unwrap();
            let mut hdr = [0u8; WAL_HDR_LEN];
            // The corrupted first record's length field is untouched.
            read_full_at(&wal, &mut hdr, 0).unwrap();
            u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as u64
        };
        assert!(second > 0 && second < end);
        assert!(shm.wal_msync_logged_pages(second, end).unwrap());

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
        let orphan = encode_wal_record(end1, 2, &[(pg, &img)], pg, 0, shm.data_start + 8, 0);
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
        let stray = encode_wal_record(end1, 5, &[(pg, &img)], pg, 0, shm.data_start + 8, 0);
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
        // Boot-id offset: what the file holds there must name THIS boot (we
        // formatted the file). Deliberately the predicate and not byte
        // equality — on Windows the stamp carries a live uptime, so two calls
        // to `boot_id()` differ within one boot and equality would be asking
        // the wrong question (crate::os::boot_id_matches).
        let stamped: [u8; 16] = raw
            [BOOT_ID_FILE_OFFSET as usize..BOOT_ID_FILE_OFFSET as usize + 16]
            .try_into()
            .unwrap();
        assert!(boot_id_matches(&stamped).unwrap());
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
            extent_map_root: 0,
            schema_gen: 0,
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
