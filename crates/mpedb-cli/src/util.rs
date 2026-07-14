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
/// `0x…` (even-length hex) → Blob, anything else → Text.
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
    Value::Text(s.to_owned())
}

pub fn parse_params(args: &[String]) -> Vec<Value> {
    args.iter().map(|a| parse_param(a)).collect()
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
