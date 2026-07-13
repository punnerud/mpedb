//! Shared plumbing: error alias, deterministic RNG (no rand dep, mirroring
//! the workspace convention), latency statistics, and machine introspection.

use std::path::Path;
use std::time::Duration;

pub type BoxErr = Box<dyn std::error::Error + Send + Sync>;
pub type BResult<T> = Result<T, BoxErr>;

pub fn err<T>(msg: impl Into<String>) -> BResult<T> {
    Err(msg.into().into())
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

pub fn cpu_model() -> String {
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

pub fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
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
