//! Known issue **3b**: why does one `fdatasync` cost ~1.8 flush-units in the
//! embedded log engines and 1.0 in PostgreSQL?
//!
//! The recorded hypothesis: PostgreSQL never changes a WAL segment's size —
//! segments are zero-filled once and then *recycled* — while an engine that
//! grows its log makes every `fdatasync` also commit a filesystem journal
//! transaction (an i_size update, or an unwritten->written extent
//! conversion). On ext4 a journal commit is its OWN device cache flush, so a
//! size-changing append+fdatasync costs **two** barriers where a recycled one
//! costs one. That would explain a ~1.8x with no engine-level cause at all.
//!
//! This probe puts the four log-file layouts side by side, all arms
//! **interleaved inside one loop** so host drift cancels, all on the same
//! filesystem, each appending the same record at the same rate:
//!
//! | arm | what it models |
//! |---|---|
//! | `grow-sparse` | pwrite past EOF, no preallocation — i_size changes on every append (SQLite's WAL as it extends) |
//! | `fallocate-unwritten` | `fallocate` in 4 MiB chunks, appends land in UNWRITTEN extents — every fdatasync journals a conversion |
//! | `fallocate-prezero` | `fallocate` + write zeros over the chunk — **what mpedb's `wal_ensure_alloc` does today** |
//! | `recycled` | whole file fallocated, zero-filled and fsynced ONCE up front, then appended into — PostgreSQL's recycled segment |
//! | `recycled-fsync` | identical, but `fsync` instead of `fdatasync` — SQLite's WAL sync (`os_unix.c` only takes the `fdatasync` branch for a DATAONLY sync, and `wal.c` never asks for one) |
//!
//! The last arm is the control for the OTHER way to buy a second barrier:
//! `fsync` must persist inode metadata (mtime alone is enough), and on ext4
//! that is a journal transaction whether or not anything about the layout
//! changed.
//!
//! The kernel's own device-flush counters are read around each arm, so the
//! result is not just "slower" but "issues N barriers per fdatasync".

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Instant;

use crate::util::{
    block_device_name, block_device_of, err, flush_stat, median, stats_from, BResult, FlushStat,
};

const CHUNK: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    GrowSparse,
    FallocateUnwritten,
    FallocatePrezero,
    Recycled,
    RecycledFsync,
}

impl Layout {
    fn label(self) -> &'static str {
        match self {
            Layout::GrowSparse => "grow-sparse",
            Layout::FallocateUnwritten => "fallocate-unwritten",
            Layout::FallocatePrezero => "fallocate-prezero",
            Layout::Recycled => "recycled",
            Layout::RecycledFsync => "recycled-fsync",
        }
    }
}

struct Log {
    layout: Layout,
    file: File,
    off: u64,
    alloc: u64,
}

fn preallocate(f: &File, off: u64, len: u64) -> BResult<()> {
    // Linux: real fallocate. Apple has no fallocate(2); reserve with fcntl
    // F_PREALLOCATE when available, else write zeros (still a valid arm for
    // the layout probe — the point is "space exists before append").
    #[cfg(target_os = "linux")]
    {
        let rc = unsafe { libc::fallocate(f.as_raw_fd(), 0, off as i64, len as i64) };
        if rc != 0 {
            return err(format!("fallocate: {}", std::io::Error::last_os_error()));
        }
        // Tail of the function on Linux — the `not(linux)` block below is
        // cfg'd out here, so this block IS the last expression.
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = f;
        // Grow via zero-fill; macOS fcntl F_PREALLOCATE is optional and
        // filesystem-dependent — zeros are portable and match the prezero arm.
        write_zeros(f, off, off + len)
    }
}

fn write_zeros(f: &File, from: u64, to: u64) -> BResult<()> {
    let zeros = vec![0u8; 1 << 20];
    let mut off = from;
    while off < to {
        let n = ((to - off) as usize).min(zeros.len());
        f.write_all_at(&zeros[..n], off)?;
        off += n as u64;
    }
    Ok(())
}

fn sync(f: &File, data_only: bool) -> BResult<()> {
    let rc = if data_only {
        // fdatasync is Linux; on Apple fcntl F_FULLFSYNC is the durable path
        // and ordinary fsync is what the extents probe needs for "data-only"
        // comparison against fsync (metadata). Use fsync on both when
        // fdatasync is missing — the recycled-fsync arm still differs by
        // asking for full fsync explicitly below.
        #[cfg(target_os = "linux")]
        {
            unsafe { libc::fdatasync(f.as_raw_fd()) }
        }
        #[cfg(not(target_os = "linux"))]
        {
            unsafe { libc::fsync(f.as_raw_fd()) }
        }
    } else {
        unsafe { libc::fsync(f.as_raw_fd()) }
    };
    if rc != 0 {
        return err(format!("sync: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

impl Log {
    fn create(dir: &Path, layout: Layout, total: u64) -> BResult<Log> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.log", layout.label()));
        let _ = std::fs::remove_file(&path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        let mut alloc = 0;
        if matches!(layout, Layout::Recycled | Layout::RecycledFsync) {
            // The whole segment, written and durable BEFORE the first measured
            // append: from here on nothing about the inode ever changes.
            preallocate(&file, 0, total)?;
            write_zeros(&file, 0, total)?;
            file.sync_all()?;
            alloc = total;
        }
        Ok(Log {
            layout,
            file,
            off: 0,
            alloc,
        })
    }

    /// One append + fdatasync of `rec`, doing whatever growth this layout
    /// implies first. Returns the measured microseconds of the append+sync
    /// pair ONLY — growth is excluded, because in every real engine it is
    /// amortized over a whole chunk and the question is what a *typical*
    /// commit costs.
    fn append(&mut self, rec: &[u8]) -> BResult<Option<u64>> {
        let need = self.off + rec.len() as u64;
        let mut grew = false;
        if need > self.alloc {
            match self.layout {
                Layout::GrowSparse => {}
                Layout::Recycled | Layout::RecycledFsync => {
                    return err("recycled log ran past its segment")
                }
                Layout::FallocateUnwritten | Layout::FallocatePrezero => {
                    let target = need.max(self.alloc).div_ceil(CHUNK) * CHUNK;
                    preallocate(&self.file, self.alloc, target - self.alloc)?;
                    if self.layout == Layout::FallocatePrezero {
                        write_zeros(&self.file, self.alloc, target)?;
                    }
                    self.alloc = target;
                    grew = true;
                }
            }
        }
        let t = Instant::now();
        self.file.write_all_at(rec, self.off)?;
        sync(&self.file, self.layout != Layout::RecycledFsync)?;
        let us = t.elapsed().as_micros() as u64;
        self.off += rec.len() as u64;
        // The append that immediately follows a grow pays that chunk's whole
        // writeback; it is real but amortized 1-in-256, so it is counted in
        // the flush totals and excluded from the latency percentiles.
        Ok((!grew).then_some(us))
    }
}

/// `iters` appends of `rec_bytes` each, per arm, all arms interleaved.
pub fn run(disk_base: &Path, iters: usize, rec_bytes: usize) -> BResult<()> {
    let dev = block_device_of(disk_base);
    match dev.and_then(block_device_name) {
        Some(n) => eprintln!("[extents] {} on /dev/{n}", disk_base.display()),
        None => eprintln!("[extents] {} (no device flush counters)", disk_base.display()),
    }
    let total = (iters as u64 * rec_bytes as u64).div_ceil(CHUNK) * CHUNK + CHUNK;
    let layouts = [
        Layout::GrowSparse,
        Layout::FallocateUnwritten,
        Layout::FallocatePrezero,
        Layout::Recycled,
        Layout::RecycledFsync,
    ];
    let dir = disk_base.join("extents");
    let mut logs: Vec<Log> = layouts
        .iter()
        .map(|&l| Log::create(&dir, l, total))
        .collect::<BResult<_>>()?;

    let rec = vec![0xA5u8; rec_bytes];
    let mut lat: Vec<Vec<u32>> = vec![Vec::new(); logs.len()];
    let mut flushes: Vec<FlushStat> = vec![FlushStat::default(); logs.len()];
    let t0 = Instant::now();
    for i in 0..iters {
        for (k, log) in logs.iter_mut().enumerate() {
            let before = dev.and_then(flush_stat);
            let us = log.append(&rec)?;
            let after = dev.and_then(flush_stat);
            if let (Some(b), Some(a)) = (before, after) {
                let d = a.since(b);
                flushes[k].ios += d.ios;
                flushes[k].ticks_ms += d.ticks_ms;
            }
            if let Some(us) = us {
                lat[k].push(us.min(u64::from(u32::MAX)) as u32);
            }
        }
        if i % 200 == 0 {
            eprint!(".");
        }
    }
    eprintln!(" {:.1} s", t0.elapsed().as_secs_f64());

    println!(
        "\n=== 3b probe: append+fdatasync by log-file layout ({iters} iters x {rec_bytes} B, \
         interleaved) ===\n"
    );
    println!(
        "  {:<22} {:>8} {:>8} {:>10} {:>14} {:>12}",
        "layout", "p50 us", "p99 us", "x recycled", "flushes/append", "us/flush"
    );
    let dur = std::time::Duration::from_secs(1); // ops/s unused here
    let stats: Vec<_> = lat
        .iter()
        .map(|v| stats_from(v.clone(), dur))
        .collect();
    let base = stats
        .iter()
        .zip(layouts.iter())
        .find(|(_, l)| **l == Layout::Recycled)
        .map_or(1.0, |(s, _)| s.p50_us as f64)
        .max(1.0);
    for (k, l) in layouts.iter().enumerate() {
        let n = lat[k].len().max(1) as f64;
        println!(
            "  {:<22} {:>8} {:>8} {:>10.2} {:>14.2} {:>12.0}",
            l.label(),
            stats[k].p50_us,
            stats[k].p99_us,
            stats[k].p50_us as f64 / base,
            flushes[k].ios as f64 / n,
            if flushes[k].ios > 0 {
                flushes[k].ticks_ms as f64 * 1000.0 / flushes[k].ios as f64
            } else {
                0.0
            },
        );
    }
    let allp: Vec<f64> = lat
        .iter()
        .map(|v| median(&v.iter().map(|&x| f64::from(x)).collect::<Vec<_>>()))
        .collect();
    println!("\n  (medians, cross-checked: {allp:?})");
    Ok(())
}
