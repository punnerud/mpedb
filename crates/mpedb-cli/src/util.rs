//! Shared plumbing: the CLI failure type (usage vs runtime), positional
//! parameter parsing, a deterministic-ish xorshift RNG (no rand dependency),
//! a hang watchdog for the multi-process tests, and config-file helpers.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mpedb::Value;

/// A command failure. `Usage` exits 2 (bad invocation), `Runtime` exits 1.
#[derive(Debug)]
pub enum Failure {
    Usage(String),
    Runtime(String),
}

pub type CliResult = Result<(), Failure>;

impl From<mpedb::Error> for Failure {
    fn from(e: mpedb::Error) -> Failure {
        Failure::Runtime(e.to_string())
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Failure {
        Failure::Runtime(format!("i/o error: {e}"))
    }
}

pub fn usage<T>(msg: impl Into<String>) -> Result<T, Failure> {
    Err(Failure::Usage(msg.into()))
}

pub fn runtime<T>(msg: impl Into<String>) -> Result<T, Failure> {
    Err(Failure::Runtime(msg.into()))
}

// ------------------------------------------------------------- param parsing

/// Parse one CLI positional parameter:
/// `null` → Null, `true`/`false` → Bool, integer → Int, float → Float,
/// `0x…` (even-length hex) → Blob, ISO-8601 timestamp → Timestamp,
/// anything else → Text.
pub fn parse_param(s: &str) -> Value {
    if s.eq_ignore_ascii_case("null") {
        return Value::Null;
    }
    if s.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if s.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::Float(f);
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        if hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("checked hex"))
                .collect();
            return Value::Blob(bytes);
        }
    }
    if let Some(us) = parse_timestamp(s) {
        return Value::Timestamp(us);
    }
    Value::Text(s.to_owned())
}

pub fn parse_params(args: &[String]) -> Vec<Value> {
    args.iter().map(|a| parse_param(a)).collect()
}

/// An ISO-8601-like timestamp literal → microseconds since the Unix epoch
/// (the engine's `Value::Timestamp` convention): `YYYY-MM-DDTHH:MM:SS`,
/// optionally `.d{1..6}` fractional seconds and an offset (`Z`/`z` or
/// `±HH:MM`; absent = UTC). Hand-rolled — fixed formats only, no date crate.
///
/// Returns `None` for anything else, including near-misses (bad month, Feb 30,
/// trailing junk): those fall through to `Value::Text`, and the engine's rigid
/// type check reports "text, statement requires timestamp" instead of the CLI
/// silently binding a wrong instant.
fn parse_timestamp(s: &str) -> Option<i64> {
    fn num(b: &[u8], at: usize, n: usize) -> Option<i64> {
        if b.len() < at + n {
            return None;
        }
        let mut v = 0i64;
        for &c in &b[at..at + n] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + i64::from(c - b'0');
        }
        Some(v)
    }
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (num(b, 0, 4)?, num(b, 5, 2)?, num(b, 8, 2)?);
    let (h, mi, se) = (num(b, 11, 2)?, num(b, 14, 2)?, num(b, 17, 2)?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + i64::from(leap),
        _ => return None,
    };
    if d < 1 || d > days_in_month || h > 23 || mi > 59 || se > 59 {
        return None;
    }

    // Optional fractional seconds, 1–6 digits, scaled to microseconds.
    let mut i = 19;
    let mut micros = 0i64;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let ndig = i - start;
        if ndig == 0 || ndig > 6 {
            return None;
        }
        micros = num(b, start, ndig)?;
        for _ in ndig..6 {
            micros *= 10;
        }
    }

    // Optional offset; the instant stored is always UTC.
    let off_secs: i64 = if i == b.len() || ((b[i] == b'Z' || b[i] == b'z') && i + 1 == b.len()) {
        0
    } else if (b[i] == b'+' || b[i] == b'-') && b.len() == i + 6 && b[i + 3] == b':' {
        let oh = num(b, i + 1, 2)?;
        let om = num(b, i + 4, 2)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let secs = oh * 3600 + om * 60;
        if b[i] == b'-' {
            -secs
        } else {
            secs
        }
    } else {
        return None;
    };

    // Civil date → days since 1970-01-01 (proleptic Gregorian; Howard
    // Hinnant's days_from_civil). y is 0..=9999 here, so i64 never overflows.
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy / 400; // yy >= 0 always (4-digit year)
    let yoe = yy - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some((days * 86_400 + h * 3600 + mi * 60 + se - off_secs) * 1_000_000 + micros)
}

// ---------------------------------------------------------------------- rng

/// xorshift64* seeded through the std hasher — deterministic for a given seed
/// tuple (DefaultHasher uses fixed keys), no external rand crate.
pub struct Rng(u64);

impl Rng {
    pub fn seeded(parts: &[u64]) -> Rng {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        parts.hash(&mut h);
        let mut s = h.finish();
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

/// `len` bytes drawn from `rng` — deterministic blob content for the crash and
/// powerloss harnesses. Seed the RNG from values any process can reconstruct
/// (row id, stored write-generation) and verification becomes
/// recompute-and-compare: that matters because page accounting can never see
/// CONTENT corruption — a torn or cross-wired overflow chain whose pages are
/// all individually valid passes `verify()` and only the byte compare fails.
pub fn fill_bytes(rng: &mut Rng, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        out.extend_from_slice(&rng.next().to_le_bytes());
    }
    out.truncate(len);
    out
}

// ----------------------------------------------------------------- watchdog

/// Aborts the whole process (exit 1, loud message) if not disarmed within
/// `secs`. A wedged writer lock in the stress/crash tests must fail the run,
/// never hang it.
pub struct Watchdog {
    disarmed: Arc<AtomicBool>,
}

impl Watchdog {
    pub fn arm(secs: u64, what: &str) -> Watchdog {
        let disarmed = Arc::new(AtomicBool::new(false));
        let flag = disarmed.clone();
        let what = what.to_owned();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < deadline {
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !flag.load(Ordering::Relaxed) {
                eprintln!(
                    "WATCHDOG: {what} did not finish within {secs}s — \
                     wedged lock or hung child; failing the run"
                );
                std::process::exit(1);
            }
        });
        Watchdog { disarmed }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.disarmed.store(true, Ordering::Relaxed);
    }
}

// ------------------------------------------------------------ config helper

/// Write a config TOML pointing at `db_path` with the given `[[table]]`
/// blocks and durability mode.
pub fn write_config_durable(
    cfg_path: &Path,
    db_path: &Path,
    size_mb: u64,
    tables_toml: &str,
    durability: &str,
) -> CliResult {
    write_config_concurrency(cfg_path, db_path, size_mb, tables_toml, durability, "serial")
}

/// Like [`write_config_durable`] but also pins the write-path `concurrency`
/// mode (`serial`|`optimistic`) — used by stress/crash to exercise the
/// Phase-3 optimistic path.
pub fn write_config_concurrency(
    cfg_path: &Path,
    db_path: &Path,
    size_mb: u64,
    tables_toml: &str,
    durability: &str,
    concurrency: &str,
) -> CliResult {
    let text = format!(
        "[database]\npath = \"{}\"\nsize_mb = {size_mb}\ndurability = \"{durability}\"\n\
         concurrency = \"{concurrency}\"\n\n{tables_toml}",
        db_path.display()
    );
    std::fs::write(cfg_path, text)?;
    Ok(())
}

/// Prefer /dev/shm for scratch databases (the intended medium), else the
/// system temp dir.
pub fn shm_or_temp() -> PathBuf {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        shm.to_path_buf()
    } else {
        std::env::temp_dir()
    }
}

/// Open a database given either a `config.toml` or a `.mpedb` file directly.
///
/// `Database::open` needs a config, but a mirror `.mpedb` is deliberately
/// config-free: schema and geometry are file-authoritative, so there is no TOML
/// to point at and hand-writing one only risks a config-drift hard-error. That
/// left a real hole — you could `mirror switch --to mpedb` and then have no way
/// to write to the mirror from the CLI at all, which is most of the point of
/// taking authority. Dispatching on the extension closes it without changing
/// any existing invocation.
pub fn open_target(path: &str) -> Result<mpedb::Database, Failure> {
    let p = Path::new(path);
    if p.extension().is_some_and(|e| e == "toml") {
        Ok(mpedb::Database::open(p)?)
    } else {
        Ok(mpedb::Database::open_from_file(p)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_literals_parse_to_epoch_micros() {
        // Reference values computed with Python's datetime (UTC).
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("1970-01-01T00:00:01"), Some(1_000_000));
        assert_eq!(
            parse_timestamp("2026-07-16T12:00:00Z"),
            Some(1_784_203_200_000_000)
        );
        assert_eq!(
            parse_timestamp("2001-02-03T04:05:06.000007Z"),
            Some(981_173_106_000_007)
        );
        // fraction is padded to micros; lowercase t/z accepted
        assert_eq!(parse_timestamp("1970-01-01t00:00:00.5z"), Some(500_000));
        // offsets shift back to UTC
        assert_eq!(parse_timestamp("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(
            parse_timestamp("2026-07-16T12:00:00+02:00"),
            Some(1_784_196_000_000_000)
        );
        assert_eq!(
            parse_timestamp("1969-12-31T22:59:59.5-01:00"),
            Some(-500_000)
        );
        // leap day exists in 2000
        assert_eq!(
            parse_timestamp("2000-02-29T00:00:00Z"),
            Some(951_782_400_000_000)
        );
    }

    #[test]
    fn near_misses_are_rejected() {
        for bad in [
            "2026-13-01T00:00:00Z", // month 13
            "2026-02-29T00:00:00Z", // 2026 is not a leap year
            "2026-07-00T00:00:00Z", // day 0
            "2026-07-16T24:00:00Z", // hour 24
            "2026-07-16T12:00:60Z", // leap second not supported
            "2026-07-16T12:00:00Zx",       // trailing junk
            "2026-07-16T12:00:00.Z",       // empty fraction
            "2026-07-16T12:00:00.1234567", // 7 fraction digits
            "2026-07-16T12:00:00+0200",    // offset without colon
            "2026-07-16T12:00:00+24:00",   // offset hour 24
            "2026-07-16",                  // date only
            "not-a-date",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
            assert_eq!(parse_param(bad), Value::Text(bad.to_owned()), "{bad}");
        }
    }

    #[test]
    fn parse_param_still_prefers_the_older_forms() {
        assert_eq!(parse_param("null"), Value::Null);
        assert_eq!(parse_param("42"), Value::Int(42));
        assert_eq!(parse_param("4.5"), Value::Float(4.5));
        assert_eq!(parse_param("0xff00"), Value::Blob(vec![0xff, 0x00]));
        assert_eq!(
            parse_param("2026-07-16T12:00:00Z"),
            Value::Timestamp(1_784_203_200_000_000)
        );
        assert_eq!(parse_param("hello"), Value::Text("hello".into()));
    }
}
