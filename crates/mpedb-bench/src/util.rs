//! Shared plumbing: error alias, deterministic RNG (no rand dep, mirroring
//! the workspace convention), latency statistics, and machine introspection.

use std::path::Path;
use std::time::Duration;

pub type BoxErr = Box<dyn std::error::Error + Send + Sync>;
pub type BResult<T> = Result<T, BoxErr>;

pub fn err<T>(msg: impl Into<String>) -> BResult<T> {
    Err(msg.into().into())
}

/// `--only` filter: empty means all; otherwise comma-separated substrings
/// (`mpedb,sqlite` matches either). Used by the primary matrix and the
/// durable-on-ack control group so mpedb↔sqlite can interleave without PG.
pub fn only_matches(key: &str, only: &Option<String>) -> bool {
    only.as_ref().is_none_or(|f| {
        f.split(',')
            .map(str::trim)
            .any(|part| !part.is_empty() && key.contains(part))
    })
}

// ---------------------------------------------------------------------- rng

/// xorshift64* — deterministic for a given seed, no external crate.
pub struct Rng(u64);

impl Rng {
    pub fn seeded(parts: &[u64]) -> Rng {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for &p in parts {
            s = (s ^ p).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            s ^= s >> 31;
        }
        if s == 0 {
            s = 0x9E37_79B9_7F4A_7C15;
        }
        Rng(s)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// -------------------------------------------------------------- latency stats

/// One measured series: op count, wall time, and latency percentiles (µs).
#[derive(Debug, Clone)]
pub struct LatStats {
    pub ops: u64,
    pub elapsed_s: f64,
    pub p50_us: u64,
    pub p99_us: u64,
}

impl LatStats {
    pub fn ops_per_s(&self) -> f64 {
        if self.elapsed_s > 0.0 {
            self.ops as f64 / self.elapsed_s
        } else {
            0.0
        }
    }
}

/// Consume a latency series (µs per op) and a wall time into `LatStats`.
pub fn stats_from(mut lat_us: Vec<u32>, elapsed: Duration) -> LatStats {
    lat_us.sort_unstable();
    let pct = |q: f64| -> u64 {
        if lat_us.is_empty() {
            0
        } else {
            u64::from(lat_us[((lat_us.len() - 1) as f64 * q) as usize])
        }
    };
    LatStats {
        ops: lat_us.len() as u64,
        elapsed_s: elapsed.as_secs_f64(),
        p50_us: pct(0.50),
        p99_us: pct(0.99),
    }
}

// ------------------------------------------------------------- machine info

/// Filesystem type of the mount containing `path` (via /proc/mounts, longest
/// mount-point prefix). "?" when undeterminable.
pub fn fs_type(path: &Path) -> String {
    let canon = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => path.to_path_buf(),
    };
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return "?".into();
    };
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut it = line.split_whitespace();
        let (Some(_dev), Some(mp), Some(ty)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if canon.starts_with(mp) && best.as_ref().is_none_or(|(len, _)| mp.len() >= *len) {
            best = Some((mp.len(), ty.to_string()));
        }
    }
    best.map_or_else(|| "?".into(), |(_, ty)| ty)
}

// ------------------------------------------------------- device flush counters

/// Kernel-counted **device cache flush requests** for one block device, from
/// the last two fields of `/proc/diskstats` (present since Linux 5.5).
///
/// This is the instrument that turns "engine A's durable commit is 1.8x
/// engine B's" into a decomposition: how many barriers the commit costs, and
/// how long each one takes. An `fdatasync` that also has to commit a
/// filesystem journal transaction (an i_size change, an unwritten->written
/// extent conversion) issues TWO device flushes, not one — and no
/// engine-level timer can see the difference.
#[derive(Clone, Copy, Debug, Default)]
pub struct FlushStat {
    /// flush requests completed
    pub ios: u64,
    /// milliseconds spent flushing
    pub ticks_ms: u64,
}

impl FlushStat {
    pub fn since(self, before: FlushStat) -> FlushStat {
        FlushStat {
            ios: self.ios.saturating_sub(before.ios),
            ticks_ms: self.ticks_ms.saturating_sub(before.ticks_ms),
        }
    }
}

/// `major:minor` of the block device holding `path`, or `None` where that
/// cannot be determined (non-Linux, or a path on a virtual filesystem).
#[cfg(target_os = "linux")]
pub fn block_device_of(path: &Path) -> Option<(u32, u32)> {
    use std::os::linux::fs::MetadataExt;
    let dev = std::fs::metadata(path).ok()?.st_dev();
    let (maj, min) = (libc::major(dev), libc::minor(dev));
    (maj != 0).then_some((maj, min))
}

#[cfg(not(target_os = "linux"))]
pub fn block_device_of(_path: &Path) -> Option<(u32, u32)> {
    None
}

/// Read the flush counters for `(major, minor)`. `None` if /proc/diskstats is
/// absent or the device is not listed.
pub fn flush_stat(dev: (u32, u32)) -> Option<FlushStat> {
    let stats = std::fs::read_to_string("/proc/diskstats").ok()?;
    for line in stats.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // major minor name + 17 counters; flush ios/ticks are the last two
        if f.len() < 20 {
            continue;
        }
        if f[0].parse::<u32>().ok() != Some(dev.0) || f[1].parse::<u32>().ok() != Some(dev.1) {
            continue;
        }
        // The flush counters are the LAST two fields (taken positionally from
        // the end, so a kernel that appends further counters cannot silently
        // shift this onto the discard columns).
        return Some(FlushStat {
            ios: f[f.len() - 2].parse().ok()?,
            ticks_ms: f[f.len() - 1].parse().ok()?,
        });
    }
    None
}

/// Device name (`sdc`) for `(major, minor)`, for the report header.
pub fn block_device_name(dev: (u32, u32)) -> Option<String> {
    let stats = std::fs::read_to_string("/proc/diskstats").ok()?;
    stats.lines().find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        (f.len() >= 3
            && f[0].parse::<u32>().ok() == Some(dev.0)
            && f[1].parse::<u32>().ok() == Some(dev.1))
        .then(|| f[2].to_string())
    })
}

/// Median of a slice (mean of the two middle values for even lengths).
pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        0.5 * (s[n / 2 - 1] + s[n / 2])
    }
}

/// Read a `sysctl -n <name>` value (macOS; there is no /proc there).
#[cfg(target_os = "macos")]
fn sysctl(name: &str) -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn cpu_model() -> String {
    #[cfg(target_os = "macos")]
    {
        // Apple Silicon reports e.g. "Apple M3 Pro" here.
        sysctl("machdep.cpu.brand_string").unwrap_or_else(|| "unknown cpu".into())
    }
    #[cfg(not(target_os = "macos"))]
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_else(|| "unknown cpu".into())
}

pub fn mem_total() -> String {
    #[cfg(target_os = "macos")]
    {
        sysctl("hw.memsize")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|b| format!("{:.1} GiB", b as f64 / (1024.0 * 1024.0 * 1024.0)))
            .unwrap_or_else(|| "unknown".into())
    }
    #[cfg(not(target_os = "macos"))]
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("MemTotal")).map(|l| {
                let kb: u64 = l
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                format!("{:.1} GiB", kb as f64 / (1024.0 * 1024.0))
            })
        })
        .unwrap_or_else(|| "unknown".into())
}

/// OS name + version, e.g. "Linux 6.8.0-134-generic" or "macOS 26.6 (Darwin
/// 25.6.0)".
///
/// Returns the OS NAME too, rather than leaving the caller to hardcode it. The
/// caller did hardcode it — the machine line read `kernel: Linux {}` — so a Mac
/// run would have reported "kernel: Linux unknown": not merely missing, but
/// WRONG, on the one line a reader uses to decide what a number means.
pub fn os_release() -> String {
    #[cfg(target_os = "macos")]
    {
        let darwin = std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        let product = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "?".into());
        format!("macOS {product} (Darwin {darwin})")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let r = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        format!("Linux {r}")
    }
}

/// A filesystem-safe slug identifying this machine, for the per-host report
/// filename: `linux-amd-epyc-milan-2c`, `macos-apple-m3-pro-11c`.
///
/// The report is a SINGLE-MACHINE document (its own first line says so) and the
/// writer used to default to one fixed `RESULTS.md`, so a second machine
/// silently deleted the first machine's numbers instead of adding its own.
/// Deriving the name from the machine makes that collision impossible by
/// accident; `--out` still overrides.
pub fn host_slug() -> String {
    let os = if cfg!(target_os = "macos") { "macos" } else { "linux" };
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    let cpu: String = cpu_model()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // collapse runs of '-' and trim, so "amd epyc-milan processor" reads well
    let mut cpu: Vec<&str> = cpu.split('-').filter(|p| !p.is_empty()).collect();
    cpu.retain(|p| !matches!(*p, "processor" | "cpu" | "r" | "with"));
    cpu.truncate(4);
    format!("{os}-{}-{cores}c", cpu.join("-"))
}

pub fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "rustc (version unavailable at run time)".into())
}

/// Today's date from the system clock, YYYY-MM-DD (UTC), no chrono dep.
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
