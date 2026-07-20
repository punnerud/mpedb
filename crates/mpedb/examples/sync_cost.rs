//! What does a durability barrier actually cost, and does its SHAPE matter?
//!
//! This is the probe behind #111 (BENCHMARKS.md, "The one cell PostgreSQL
//! wins"). Two questions the commit path's design hangs on, each answered by
//! measurement rather than by reading a man page:
//!
//! 1. **Is `msync` of a mapped range more expensive than `pwrite` +
//!    `fdatasync` of a small record?** — i.e. is `durability = commit`'s
//!    mmap-based barrier structurally worse than `wal`'s log append? (This is
//!    the difference between mpedb `commit` and how PostgreSQL and SQLite do
//!    it: `xlog.c` contains zero `msync` calls.)
//!
//! 2. **Does the NUMBER of msync calls matter, and does their WIDTH?** — the
//!    commit path used to issue one msync per contiguous run of dirty COW
//!    pages. #111 replaced that with one msync over the whole dirty span, which
//!    is only sound as a performance change if a wide range is not itself
//!    expensive.
//!
//! Measured on this box (Linux, AMD EPYC-Milan 2c, p50 µs, arms INTERLEAVED
//! inside one loop so host drift cancels; the box was not idle, so read the
//! ratios, not the absolutes):
//!
//! ```text
//!   arm                                             ext4      xfs
//!   A msync(2 meta pages of a 64 MiB map)          1,847    2,480
//!   B pwrite(200 B) + fdatasync                    1,887    2,554   <- A == B
//!   C 8 scattered 1-page msyncs + meta msync      15,280   15,846   <- 6.7x A
//!   D 1 msync(8 contiguous pages) + meta msync     2,276    4,449
//!   E pwrite(32 KiB) + fdatasync                   2,181    3,019
//!   F 1 msync over the SPAN of 8 scattered + meta  5,358    7,870   <- the fix
//!
//!   span width, 1 GiB mapping, 8 dirty pages spread over the span:
//!       4 MiB    4,453   10,246
//!      64 MiB    2,940    5,524
//!     512 MiB    2,963    5,583
//!   1,023 MiB    2,990    5,497   <- 256x wider, no cost
//! ```
//!
//! Three conclusions, all load-bearing elsewhere:
//!
//! - **A ≈ B.** msync and fdatasync cost the same thing, because both are one
//!   device cache flush. `wal` is not cheaper per flush; it is cheaper because
//!   it issues ONE where `commit` issues two.
//! - **C is the bug #111 fixed.** On Linux `msync(MS_SYNC)` *is*
//!   `vfs_fsync_range`, so every call ends in a filesystem-log commit plus a
//!   `blkdev_issue_flush`. Eight of them cost 6.7x one.
//! - **Width is free.** Flat from 64 MiB to 1 GiB on both filesystems —
//!   writeback is driven by the page cache's DIRTY tag, so an msync range walks
//!   dirty pages, not pages. (The 4 MiB arm being *slowest* is the tell that
//!   this is not a range scan at all.) That is what makes the span msync sound.
//!
//! Usage: `cargo run --release -p mpedb --example sync_cost -- <dir> [iters]`
//! Writes two scratch files in `<dir>` and removes them. Run on an idle box.

#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;

#[cfg(target_os = "linux")]
const PAGE: usize = 4096;
#[cfg(target_os = "linux")]
const MAP_LEN: usize = 64 * 1024 * 1024;
#[cfg(target_os = "linux")]
const SPAN_LEN: usize = 1024 * 1024 * 1024;

#[cfg(target_os = "linux")]
fn map_file(path: &str, len: usize) -> (std::fs::File, *mut u8) {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    // fallocate, not ftruncate: unwritten extents, which is what shm.rs does.
    unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, len as i64) };
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        )
    };
    assert!(p != libc::MAP_FAILED, "mmap failed");
    let p = p.cast::<u8>();
    // Fault every page in and flush once, so we measure the barrier and not
    // first-touch faults or extent conversion.
    for off in (0..len).step_by(PAGE) {
        unsafe { *p.add(off) = 1 };
    }
    unsafe { libc::msync(p.cast(), len, libc::MS_SYNC) };
    (f, p)
}

#[inline]
#[cfg(target_os = "linux")]
fn msync_at(map: *mut u8, page: usize, npages: usize) {
    let rc = unsafe {
        libc::msync(
            map.add(page * PAGE).cast(),
            npages * PAGE,
            libc::MS_SYNC,
        )
    };
    assert_eq!(rc, 0, "msync failed");
}

#[inline]
#[cfg(target_os = "linux")]
fn dirty(map: *mut u8, page: usize, tag: u8) {
    unsafe { *map.add(page * PAGE + 32) = tag };
}

#[cfg(target_os = "linux")]
fn report(name: &str, mut d: Vec<u128>) {
    d.sort_unstable();
    let n = d.len();
    let mean = d.iter().sum::<u128>() as f64 / n as f64 / 1000.0;
    println!(
        "  {name:<44} p50={:>9.1}us  p90={:>9.1}us  mean={mean:>9.1}us",
        d[n / 2] as f64 / 1000.0,
        d[n * 9 / 10] as f64 / 1000.0,
    );
}

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| ".".into());
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);

    // ---- part 1: barrier shape, on a 64 MiB mapping + an append log ----
    let mpath = format!("{dir}/sync_cost-map.bin");
    let wpath = format!("{dir}/sync_cost-log.bin");
    let (_mf, map) = map_file(&mpath, MAP_LEN);
    let log = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&wpath)
        .unwrap();
    unsafe { libc::fallocate(log.as_raw_fd(), 0, 0, MAP_LEN as i64) };
    // Pre-zero like shm.rs does for the WAL: an fdatasync over an UNWRITTEN
    // extent has to convert it, which is filesystem work, not a flush.
    let zeros = vec![0u8; 1 << 20];
    for off in (0..MAP_LEN).step_by(zeros.len()) {
        unsafe {
            libc::pwrite(
                log.as_raw_fd(),
                zeros.as_ptr().cast(),
                zeros.len(),
                off as i64,
            )
        };
    }
    unsafe { libc::fdatasync(log.as_raw_fd()) };

    let small = [0xABu8; 200];
    let big = vec![0xCDu8; 8 * PAGE];
    let (mut a, mut b, mut c, mut d, mut e, mut f) = (vec![], vec![], vec![], vec![], vec![], vec![]);
    let mut log_off: i64 = 0;

    for i in 0..iters {
        let tag = i as u8;
        let now = std::time::Instant::now;

        // A: dirty one meta page, msync the meta pair
        dirty(map, i % 2, tag);
        let t = now();
        msync_at(map, 0, 2);
        a.push(t.elapsed().as_nanos());

        // B: append a small record + fdatasync (the `wal` shape)
        let t = now();
        unsafe {
            libc::pwrite(log.as_raw_fd(), small.as_ptr().cast(), small.len(), log_off);
            libc::fdatasync(log.as_raw_fd());
        }
        b.push(t.elapsed().as_nanos());
        log_off = (log_off + 256) % (32 * 1024 * 1024);

        // C: 8 SCATTERED pages, one msync each, then the meta (the old commit path)
        let base = 1024 + (i * 37) % 8000;
        for k in 0..8 {
            dirty(map, base + k * 13, tag);
        }
        let t = now();
        for k in 0..8 {
            msync_at(map, base + k * 13, 1);
        }
        msync_at(map, 0, 2);
        c.push(t.elapsed().as_nanos());

        // D: 8 CONTIGUOUS pages in one msync, then the meta (the best case)
        let base2 = 10000 + (i * 11) % 5000;
        for k in 0..8 {
            dirty(map, base2 + k, tag);
        }
        let t = now();
        msync_at(map, base2, 8);
        msync_at(map, 0, 2);
        d.push(t.elapsed().as_nanos());

        // E: append 32 KiB + fdatasync (a `wal` record carrying 8 page images)
        let t = now();
        unsafe {
            libc::pwrite(
                log.as_raw_fd(),
                big.as_ptr().cast(),
                big.len(),
                40 * 1024 * 1024 + (i as i64 % 100) * 40960,
            );
            libc::fdatasync(log.as_raw_fd());
        }
        e.push(t.elapsed().as_nanos());

        // F: 8 scattered pages under ONE msync over their span (the #111 path)
        let base3 = 1024 + (i * 53) % 6000;
        for k in 0..8 {
            dirty(map, base3 + k * 13, tag);
        }
        let t = now();
        msync_at(map, base3, 7 * 13 + 1);
        msync_at(map, 0, 2);
        f.push(t.elapsed().as_nanos());
    }

    println!("barrier shape — {iters} iterations, 64 MiB mapping, dir={dir}");
    report("A msync(2 meta pages)", a);
    report("B pwrite(200 B) + fdatasync", b);
    report("C 8 scattered 1-page msyncs + meta", c);
    report("D 1 msync(8 contiguous) + meta", d);
    report("E pwrite(32 KiB) + fdatasync", e);
    report("F 1 msync over span of 8 scattered + meta", f);
    drop(log);
    let _ = std::fs::remove_file(&mpath);
    let _ = std::fs::remove_file(&wpath);

    // ---- part 2: does span WIDTH cost anything? ----
    let spath = format!("{dir}/sync_cost-span.bin");
    let (_sf, smap) = map_file(&spath, SPAN_LEN);
    let npages = SPAN_LEN / PAGE;
    let widths = [1024usize, 16 * 1024, 128 * 1024, npages - 16];
    let mut out: Vec<Vec<u128>> = widths.iter().map(|_| Vec::new()).collect();
    let span_iters = (iters / 6).max(50);
    for i in 0..span_iters {
        for (w, res) in widths.iter().zip(out.iter_mut()) {
            let base = 16 + (i * 97) % (npages - w - 32);
            for k in 0..8 {
                dirty(smap, base + k * (w / 8), i as u8);
            }
            let t = std::time::Instant::now();
            msync_at(smap, base, *w);
            res.push(t.elapsed().as_nanos());
        }
    }
    println!("\nspan width — {span_iters} iterations, 1 GiB mapping, 8 dirty pages per arm");
    for (w, res) in widths.iter().zip(out) {
        report(&format!("span {:>6} MiB", w * PAGE / (1024 * 1024)), res);
    }
    let _ = std::fs::remove_file(&spath);
}

/// `libc::fallocate` is Linux-only, and part 2 measures a Linux-specific claim
/// (that `msync` span WIDTH is free because writeback is DIRTY-tag driven — it
/// is not, on Darwin). Stub rather than port: a probe that answers a different
/// question on another platform is worse than no probe.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sync_cost is Linux-only (fallocate + DIRTY-tag writeback semantics)");
}
