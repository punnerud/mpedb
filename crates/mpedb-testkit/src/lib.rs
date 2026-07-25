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

/// Where this test run puts its scratch — the ONE place that decides, for
/// every crate's tests.
///
/// Default is `/dev/shm`: mpedb's natural habitat, and fast. But it is a
/// tmpfs sized from RAM (3.8 GB on the dev box), and the suite can exhaust
/// it — debris from an earlier run, or one heavy test, and the next
/// `fallocate` returns `StorageFull` from inside an unrelated test while the
/// real disk sits empty. That failure reads as an engine bug and points at
/// the wrong place entirely.
///
/// `MPEDB_TEST_DIR` moves a run onto a real volume:
///
/// ```text
/// MPEDB_TEST_DIR=/mnt/xfs/mpedb-scratch cargo test --workspace
/// ```
///
/// It is also the honest place to measure durability modes, since tmpfs has
/// no platter to flush to. macOS has no `/dev/shm` at all, so the temp-dir
/// fallback is load-bearing there (#66) — do not drop it.
///
/// A named directory that does not exist is created. One that cannot be used
/// PANICS by name rather than silently falling back: a run that quietly
/// ignores where you told it to write is worse than one that stops.
pub fn scratch_base() -> PathBuf {
    match std::env::var_os("MPEDB_TEST_DIR") {
        Some(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                panic!("MPEDB_TEST_DIR={} is unusable: {e}", dir.display());
            }
            dir
        }
        _ if Path::new("/dev/shm").is_dir() => PathBuf::from("/dev/shm"),
        _ => std::env::temp_dir(),
    }
}

/// [`scratch_base`] as a `String`, for the call sites that interpolate it
/// into a path with `format!`.
pub fn scratch_base_str() -> String {
    scratch_base().display().to_string()
}

/// Scratch on a REAL filesystem — for tests that measure memory.
///
/// `/dev/shm` is a tmpfs, so a database file living there IS resident
/// memory: a test asserting a memory bound would be measuring its own
/// storage and would fail, or pass, for reasons that have nothing to do with
/// the engine. Two tests need this (`agg_stream_mem`, `prune_width_mem`,
/// both asserting the #123 §5.1 bound); they used to carry hand-rolled
/// `/mnt/...` preferences with no stated reason, which is how one of them
/// silently lost it in a sweep.
///
/// `MPEDB_TEST_DIR` wins when set — an explicit answer beats a guess, and
/// the caller may know the volume is real. Otherwise the machine's mounted
/// scratch volumes if they exist, then the platform temp dir. Never falls
/// back to `/dev/shm`: that would defeat the whole point silently. On a box
/// whose temp dir is itself a tmpfs the measurement is still polluted —
/// point `MPEDB_TEST_DIR` at real storage there.
pub fn scratch_base_real_disk() -> PathBuf {
    if let Some(dir) = std::env::var_os("MPEDB_TEST_DIR") {
        if !dir.is_empty() {
            return scratch_base(); // same creation + loud-refusal handling
        }
    }
    for candidate in ["/mnt/xfs/mpedb-scratch", "/mnt/ext4/mpedb-scratch"] {
        let p = Path::new(candidate);
        if p.is_dir() || std::fs::create_dir_all(p).is_ok() {
            return PathBuf::from(candidate);
        }
    }
    std::env::temp_dir()
}

/// A per-test scratch directory, removed on drop. Database files inside
/// always use the `.mpedb` extension.
///
/// Default base is `/dev/shm` — mpedb's natural habitat, and fast. But it is
/// a tmpfs sized from RAM (3.8 GB on the dev box), which the suite can
/// exhaust: a run that leaves debris behind, or one heavy test, and the next
/// `fallocate` fails with `StorageFull` deep inside an unrelated test while
/// the real disk sits empty. `MPEDB_TEST_DIR` moves the whole suite onto a
/// real volume for those runs — also the honest place to measure durability
/// modes, since tmpfs has no platter to flush to.
///
/// ```text
/// MPEDB_TEST_DIR=/mnt/xfs/mpedb-scratch cargo test --workspace
/// ```
///
/// A path that does not exist is created; if that fails, the value is
/// refused loudly rather than silently falling back — a test run that
/// quietly ignores where you told it to write is worse than one that stops.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Result<TempDir> {
        let base = scratch_base();
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
