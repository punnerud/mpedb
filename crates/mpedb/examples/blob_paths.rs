//! What does it actually cost to get a large blob INTO the file? Same bytes,
//! same medium, varying one thing at a time — so a design question gets a number
//! instead of an argument.
//!
//! Measured on this box (ext4, 64 MiB, page cache evicted between the layout and
//! the measurement — see `fresh_dst`):
//!
//! ```text
//!   layout      cold    warm      what it is
//!   sparse       855   16256      ftruncate only, no blocks
//!   fallocate    933   16344      blocks reserved, extents UNWRITTEN  <- mpedb
//!   prezeroed    357   13766      fallocate + actually write zeros
//!   chain        963      —       fallocate + a per-page header (the real format)
//!   cfr         1638      —       copy_file_range, in-kernel
//! ```
//!
//! Four things worth keeping:
//!
//! 1. **The cost is page faults, not the copy.** Warm is 17x cold, same memcpy.
//!    A MAP_SHARED page must be faulted in before it can be written even when
//!    the write overwrites every byte of it; `write(2)` owes no such fault, which
//!    is most of why the raw baseline looks so far ahead. A long-lived process
//!    recycling pages through the freelist pays this once per page, not per blob
//!    (see `examples/blob_warm`).
//! 2. **`fallocate` with UNWRITTEN extents is the right layout, and pre-zeroing
//!    is a 2.6x regression.** An unwritten extent tells the kernel "this is
//!    zeros", so a fault can zero-fill for free. Write real zeros over it and the
//!    extent becomes "written" — now a fault must READ 4 KiB off the platter,
//!    because the kernel no longer knows the content is trivial. `shm.rs` gets
//!    this right today.
//! 3. **The per-page header costs ~3%** (chain 963 vs memcpy 933 — inside noise).
//!    A contiguous headerless extent format buys nothing on its own.
//! 4. **`copy_file_range` wins by 75%** by not faulting the destination at all.
//!    Reflink (`FICLONERANGE`) would be ~O(metadata), but ext4 rejects it and
//!    macOS has no range-clone into an existing file. Both need a file-backed
//!    blob API that does not exist: our params are `Value::Blob(Vec<u8>)`, already
//!    in userspace, with no fd to clone from.
//!
//! ⚠ The eviction in `fresh_dst` is load-bearing. Without it the prezeroed arm
//! measures its own freshly-written page cache and reports **+88% instead of
//! -62%** — the number flips sign. Every arm gets the same treatment so this
//! compares LAYOUT, not cache state.
//!
//! Usage: `blob_paths <dir> [mib]`

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

#[cfg(target_os = "linux")]
const PAGE: usize = 4096;
#[cfg(target_os = "linux")]
const HDR: usize = 16; // mpedb's overflow page header (btree.rs)

#[cfg(target_os = "linux")]
fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

// copy_file_range / loff_t / fadvise are Linux-only; the numbers above are
// Linux numbers. On other platforms this example is a no-op rather than a
// build break for `cargo test --workspace`.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("blob_paths measures Linux-specific I/O paths; nothing to do here");
}

#[cfg(target_os = "linux")]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(a.get(1).cloned().unwrap_or("/tmp/bp".into()));
    let mib: usize = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(16);
    std::fs::create_dir_all(&dir).unwrap();
    let n = mib * 1024 * 1024;

    // incompressible-ish, so nothing gets a free ride from a compressing fs
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let payload: Vec<u8> = (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();

    let src_path = dir.join("src.bin");
    std::fs::write(&src_path, &payload).unwrap();
    let dst_path = dir.join("dst.bin");

    // How the destination file's blocks are laid out. This is not a detail: it
    // is most of what the cold numbers below measure, and the first version of
    // this file got it wrong — it used `set_len` (ftruncate), which leaves a
    // SPARSE file with no blocks at all, so every first touch had to allocate
    // one. mpedb does not do that; `shm.rs` calls `fallocate` (unwritten
    // extents: blocks reserved, no data written) precisely so ENOSPC surfaces at
    // create instead of as SIGBUS mid-commit.
    #[derive(Clone, Copy, PartialEq)]
    enum Layout {
        /// ftruncate only — no blocks. NOT what mpedb does; here to show the cost.
        Sparse,
        /// fallocate — blocks reserved, extents "unwritten". **What mpedb does.**
        Fallocated,
        /// fallocate + actually write zeros over it, as `shm::prezero` does for
        /// the WAL (but not for the data file).
        Prezeroed,
    }
    let fresh_dst = |layout: Layout| {
        let _ = std::fs::remove_file(&dst_path);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dst_path)
            .unwrap();
        f.set_len(n as u64).unwrap();
        if layout != Layout::Sparse {
            let rc = unsafe { libc::fallocate(f.as_raw_fd(), 0, 0, n as i64) };
            assert_eq!(rc, 0, "fallocate: {}", std::io::Error::last_os_error());
        }
        if layout == Layout::Prezeroed {
            use std::os::unix::fs::FileExt;
            let zeros = vec![0u8; 1 << 20];
            let mut off = 0usize;
            while off < n {
                let take = (n - off).min(zeros.len());
                f.write_all_at(&zeros[..take], off as u64).unwrap();
                off += take;
            }
        }
        // EVICT. Without this the prezero arm is a lie: it has just written the
        // whole file, so its pages sit hot and dirty in the page cache and the
        // "cold" number measures a warm mapping. fsync to make them clean (fadvise
        // only drops clean pages), then DONTNEED to drop them. Every arm gets the
        // same treatment so the comparison is of LAYOUT, not of cache state.
        f.sync_all().unwrap();
        let rc = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, n as i64, libc::POSIX_FADV_DONTNEED) };
        assert_eq!(rc, 0, "posix_fadvise(DONTNEED)");
        f
    };

    println!("{mib} MiB payload, {}", dir.display());
    println!("{:<9} {:>9} {:>11}  note", "path", "ms", "MiB/s");

    let report = |name: &str, d: std::time::Duration, note: &str| {
        println!(
            "{:<9} {:>9.2} {:>11.1}  {}",
            name,
            ms(d),
            mib as f64 / d.as_secs_f64(),
            note
        );
    };

    // ---- memcpy into each layout, cold and warm.
    //
    // Cold = first touch of every page, which is what mpedb-bench's blob cells
    // see (fresh file per cell). Warm = the pages are already faulted in, which
    // is what a long-lived process sees once the freelist starts recycling.
    let mmap_of = |f: &std::fs::File| {
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                n,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        assert_ne!(map, libc::MAP_FAILED, "mmap");
        map
    };
    for (layout, name) in [
        (Layout::Sparse, "sparse"),
        (Layout::Fallocated, "fallocate"),
        (Layout::Prezeroed, "prezeroed"),
    ] {
        let f = fresh_dst(layout);
        let map = mmap_of(&f);
        let t0 = std::time::Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), map as *mut u8, n) };
        let cold = t0.elapsed();
        // second pass over the SAME mapping: every page is now faulted in
        let t1 = std::time::Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), map as *mut u8, n) };
        let warm = t1.elapsed();
        unsafe { libc::munmap(map, n) };
        println!(
            "{:<9} {:>9.2} {:>11.1}  cold 1st touch{}",
            name,
            ms(cold),
            mib as f64 / cold.as_secs_f64(),
            if layout == Layout::Fallocated { "   <- what mpedb does" } else { "" }
        );
        println!(
            "{:<9} {:>9.2} {:>11.1}  warm (pages already faulted)",
            "",
            ms(warm),
            mib as f64 / warm.as_secs_f64()
        );
    }

    // ---- chain: the same copy, but through a per-page header, on the layout
    // mpedb actually uses. Isolates the overflow format from everything else.
    {
        let f = fresh_dst(Layout::Fallocated);
        let map = mmap_of(&f);
        let t0 = std::time::Instant::now();
        let cap = PAGE - HDR;
        let mut off = 0usize;
        let mut page = 0usize;
        while off < n && (page + 1) * PAGE <= n {
            let take = cap.min(n - off);
            unsafe {
                let p = (map as *mut u8).add(page * PAGE);
                std::ptr::write_bytes(p, 3u8, 1);
                (p.add(6) as *mut u16).write_unaligned(take as u16);
                (p.add(8) as *mut u64).write_unaligned((page + 1) as u64);
                std::ptr::copy_nonoverlapping(payload.as_ptr().add(off), p.add(HDR), take);
            }
            off += take;
            page += 1;
        }
        let d = t0.elapsed();
        unsafe { libc::munmap(map, n) };
        report("chain", d, "cold, fallocate + per-page header (the real format)");
    }

    // ---- 3. copy_file_range: in-kernel, source is an fd
    {
        let src = std::fs::File::open(&src_path).unwrap();
        let dst = fresh_dst(Layout::Fallocated);
        let t0 = std::time::Instant::now();
        let mut off_in: libc::loff_t = 0;
        let mut off_out: libc::loff_t = 0;
        let mut left = n;
        let mut err = None;
        while left > 0 {
            let rc = unsafe {
                libc::copy_file_range(
                    src.as_raw_fd(),
                    &mut off_in,
                    dst.as_raw_fd(),
                    &mut off_out,
                    left,
                    0,
                )
            };
            if rc < 0 {
                err = Some(std::io::Error::last_os_error());
                break;
            }
            if rc == 0 {
                break;
            }
            left -= rc as usize;
        }
        let d = t0.elapsed();
        match err {
            None => report("cfr", d, "copy_file_range: in-kernel copy (ext4)"),
            Some(e) => {
                println!("{:<9} {:>9} {:>11}  copy_file_range failed: {e}", "cfr", "-", "-")
            }
        }
    }

    // ---- 4. FICLONERANGE: extent sharing, metadata only
    {
        let src = std::fs::File::open(&src_path).unwrap();
        let dst = fresh_dst(Layout::Fallocated);
        #[repr(C)]
        struct CloneRange {
            src_fd: i64,
            src_offset: u64,
            src_length: u64,
            dest_offset: u64,
        }
        const FICLONERANGE: libc::c_ulong = 0x4020_940D;
        let arg = CloneRange {
            src_fd: src.as_raw_fd() as i64,
            src_offset: 0,
            src_length: n as u64,
            dest_offset: 0,
        };
        let t0 = std::time::Instant::now();
        let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONERANGE, &arg) };
        let d = t0.elapsed();
        if rc == 0 {
            report("ficlone", d, "FICLONERANGE: extents shared, no data copied");
        } else {
            let e = std::io::Error::last_os_error();
            println!(
                "{:<9} {:>9} {:>11}  FICLONERANGE unsupported here: {e}",
                "ficlone", "-", "-"
            );
            println!("            (ext4 has no reflink; btrfs/XFS(reflink=1) would be ~O(metadata))");
        }
    }

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&dst_path);
}
