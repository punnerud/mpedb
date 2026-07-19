//! mpedb-testkit — an SQLite-inspired correctness battery for mpedb.
//!
//! Three pieces, mirroring the reusable parts of SQLite's public test
//! methodology (see README.md for what is reused and what cannot be):
//!
//! 1. [`run_slt_file`] — a runner for the classic **sqllogictest** file
//!    format (`statement ok`, `statement error`, `query <types> [sort]`,
//!    expected results after `----`), extended with a `# schema:` header
//!    because mpedb has no `CREATE TABLE` — schemas come from TOML config.
//! 2. A curated corpus of `.test` files under `tests/slt/` — executable
//!    documentation of mpedb's SQL semantics.
//! 3. [`diff`] — a randomized differential tester that runs the same
//!    generated program against mpedb, the BUNDLED sqlite (rusqlite
//!    `bundled`, pinned in Cargo.toml; STRICT tables) and — in three-way
//!    mode — a throwaway PostgreSQL 16 cluster ([`pg::PgCluster`]),
//!    comparing SELECT outputs and per-statement success across all engines.
//!
//! Randomness is a seeded xorshift (the workspace convention —
//! deterministic, reproducible failures); sqlite is in-process via the
//! pinned bundled build, and psql is driven as a batch subprocess.

pub mod diff;
pub mod pg;
pub mod slt;

pub use slt::{run_slt_file, SltStats};

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A test-harness failure: either the harness could not do its job (I/O,
/// malformed .test file) or — the interesting case — the
/// engine under test produced something other than the expected result.
/// The message is self-contained: file/line/SQL plus expected-vs-got.
#[derive(Debug)]
pub struct Failure(pub String);

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Failure {}

impl Failure {
    pub fn new(msg: impl Into<String>) -> Failure {
        Failure(msg.into())
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Failure {
        Failure(format!("i/o error: {e}"))
    }
}

pub type Result<T, E = Failure> = std::result::Result<T, E>;

// ---------------------------------------------------------------- xorshift

/// Deterministic xorshift64* RNG (workspace convention: no `rand` dep).
/// Same-seed runs generate identical programs, so every reported failure is
/// reproducible from its seed alone.
pub struct Xorshift(u64);

impl Xorshift {
    pub fn new(seed: u64) -> Xorshift {
        // Never allow the all-zero state (xorshift's fixed point).
        Xorshift(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `lo..=hi`.
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        lo + (self.below((hi - lo + 1) as u64) as i64)
    }

    /// True with probability `num`/`den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

// ---------------------------------------------------------------- temp dirs

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// A per-test scratch directory under /dev/shm (mpedb's natural habitat),
/// removed on drop. Database files inside always use the `.mpedb` extension.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Result<TempDir> {
        let base = if Path::new("/dev/shm").is_dir() {
            PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let path = base.join(format!(
            "mpedb-testkit-{prefix}-{}-{}",
            std::process::id(),
            UNIQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path)?;
        Ok(TempDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path for a database file inside this directory (`.mpedb` extension).
    pub fn db_path(&self, name: &str) -> PathBuf {
        self.path.join(format!("{name}.mpedb"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xorshift_is_deterministic_and_covers_range() {
        let mut a = Xorshift::new(42);
        let mut b = Xorshift::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut r = Xorshift::new(7);
        let mut seen = [false; 10];
        for _ in 0..1000 {
            seen[r.below(10) as usize] = true;
            let v = r.range_i64(-3, 3);
            assert!((-3..=3).contains(&v));
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn temp_dir_cleans_up() {
        let p;
        {
            let d = TempDir::new("unit").unwrap();
            p = d.path().to_path_buf();
            assert!(p.is_dir());
            assert!(d.db_path("x").to_string_lossy().ends_with("x.mpedb"));
        }
        assert!(!p.exists());
    }
}
