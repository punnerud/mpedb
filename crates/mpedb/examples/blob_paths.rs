//! What does it actually cost to get a large blob INTO the file? Four paths,
//! same bytes, same medium — so the prize for a format change is a number, not
//! an argument.
//!
//! 1. **chain** — what mpedb does today: memcpy the payload into 4 KiB pages
//!    through a 16-byte per-page header, i.e. in `PAGE-HDR`-sized chunks with a
//!    chain pointer written per page.
//! 2. **memcpy** — one contiguous memcpy into the same mapping. The floor a
//!    headerless extent format reaches on ANY filesystem, no syscall trick.
//! 3. **cfr** — `copy_file_range(2)`: in-kernel copy, source is an fd. Works on
//!    ext4 (a real copy, no userspace bounce); on btrfs/XFS the kernel may turn
//!    it into a reflink.
//! 4. **ficlone** — `FICLONERANGE`: pure extent sharing, ~O(metadata). ext4
//!    rejects it (EOPNOTSUPP); btrfs/XFS do it near-instantly.
//!
//! Measured here (ext4, 64 MiB): chain 803, memcpy 815, cfr 1346 MiB/s. So the
//! header chain costs **3%** — a contiguous format buys nothing by itself — and
//! what copy_file_range actually buys is not skipping headers but not FAULTING
//! the destination through the mapping.
//!
//! Caveat before anyone quotes the table: mpedb itself measures 1037 MiB/s on
//! 16 MiB blobs — FASTER than this file's `chain` simulation, most likely
//! because the real file is fallocate'd and its pages are already resident. The
//! true gap needs an in-engine A/B, not this. And raw `std::fs` write() beats
//! cfr (2211 vs 1346) because a write() from a hot userspace buffer is ONE
//! page-cache touch where cfr is two.
//!
//! Usage: `blob_paths <dir> [mib]`

use std::os::fd::AsRawFd;

const PAGE: usize = 4096;
const HDR: usize = 16; // mpedb's overflow page header (btree.rs)

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

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

    // A destination file the size of the payload, mmap'd MAP_SHARED — mpedb's
    // arrangement, which is what rules O_DIRECT out and makes the clone-into-a
    // -live-mapping question load-bearing.
    let fresh_dst = || {
        let _ = std::fs::remove_file(&dst_path);
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dst_path)
            .unwrap();
        f.set_len(n as u64).unwrap();
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

    // ---- 1. chain: header every (PAGE-HDR) bytes, as the overflow chain does
    {
        let f = fresh_dst();
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
        let t0 = std::time::Instant::now();
        let cap = PAGE - HDR;
        let mut off = 0usize;
        let mut page = 0usize;
        while off < n && (page + 1) * PAGE <= n {
            let take = cap.min(n - off);
            unsafe {
                let p = (map as *mut u8).add(page * PAGE);
                // the header: kind + payload len + next page id
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
        report("chain", d, "today: memcpy through a per-page header");
    }

    // ---- 2. memcpy: one contiguous copy into the mapping
    {
        let f = fresh_dst();
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
        let t0 = std::time::Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), map as *mut u8, n) };
        let d = t0.elapsed();
        unsafe { libc::munmap(map, n) };
        report("memcpy", d, "contiguous extent, no syscall trick, ANY fs");
    }

    // ---- 2b. memcpy into a mapping whose pages are ALREADY faulted in.
    //
    // This is the one that decides whether the gap mpedb shows on large blobs is
    // real or a harness artefact. A MAP_SHARED page must be faulted in before it
    // can be written, even when the write overwrites every byte of it; `write(2)`
    // owes no such fault. If the cold and warm numbers differ, the cold cost is
    // one-time-per-page — and a long-lived process that recycles pages through
    // the freelist pays it once, not per blob. If they match, the fault theory is
    // dead and the cost is somewhere else.
    {
        let f = fresh_dst();
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
        // touch every page first — the faults happen HERE, not in the timed part
        unsafe { std::ptr::write_bytes(map as *mut u8, 1u8, n) };
        let t0 = std::time::Instant::now();
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), map as *mut u8, n) };
        let d = t0.elapsed();
        unsafe { libc::munmap(map, n) };
        report("memcpy2", d, "same, but pages already faulted (2nd write)");
    }

    // ---- 3. copy_file_range: in-kernel, source is an fd
    {
        let src = std::fs::File::open(&src_path).unwrap();
        let dst = fresh_dst();
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
        let dst = fresh_dst();
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
