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

pub mod corpus_baseline;
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
/// Remove scratch files left by test processes that are no longer running.
///
/// Called once per process from [`scratch_base`], because `Drop` cannot be the
/// whole answer: a test binary killed by the OOM killer, by SIGKILL, or by a
/// harness timeout never runs one. On this project that is not hypothetical —
/// `/dev/shm` reached 96 % three times in one day, 312 files and 3.8 GB, and
/// the first time it did the OOM killer took the session that was writing
/// them. A sidecar is ~4x its source, so a handful of abandoned imports fills
/// a tmpfs.
///
/// **Only provably dead files.** A name must both look like ours — an
/// `.mpedb` family suffix, or an `mpedb-` prefix — and carry a PID that no
/// longer exists. Anything else is left alone, including another engine's
/// files (`/dev/shm` is shared: PostgreSQL keeps its own there) and any test
/// that names its files without a PID, where staleness cannot be proven.
///
/// Parallel test binaries are the reason the check is `kill(pid, 0)` and not a
/// timestamp: two suites running at once must not collect each other's live
/// databases, and an mtime says nothing about whether a writer is still there.
///
/// And it runs ONCE, at the first `scratch_base` of the process — so a file
/// created later in the run is never a candidate. That is what makes this safe
/// rather than merely careful: there is no window in which a test's own
/// scratch can be swept out from under it.
#[cfg(unix)]
fn sweep_dead_scratch(dir: &Path) {
    fn looks_like_ours(name: &str) -> bool {
        name.starts_with("mpedb-")
            || name.ends_with(".mpedb")
            || name.ends_with(".mpedb.src")
            || name.ends_with(".mpedb.lock")
            || name.ends_with(".overlay.mpedb")
            || (name.ends_with("-wal") && name.contains(".mpedb"))
    }
    fn owner_is_gone(name: &str) -> bool {
        // Every PID-looking run of digits must be dead; a name with none is
        // not attributable and so is never swept.
        let mut saw = false;
        let bytes = name.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if !bytes[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start >= 4 {
                let Ok(pid) = name[start..i].parse::<i32>() else { continue };
                saw = true;
                // ESRCH means no such process. EPERM means it exists and is
                // someone else's — alive either way, so keep the file.
                if unsafe { libc::kill(pid, 0) } == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                {
                    return false;
                }
            }
        }
        saw
    }

    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !looks_like_ours(&name) || !owner_is_gone(&name) {
            continue;
        }
        // Directories too: several suites give a killed run a whole scratch
        // directory, and one held 16 MB. Same standard as a file — the name
        // is ours and the PID in it is gone — because a recursive delete
        // deserves no weaker a test, not a stronger one. A directory with no
        // PID (the fixed-name ones several suites share) fails `owner_is_gone`
        // above and is never reached.
        let removed = if e.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(e.path())
        } else {
            std::fs::remove_file(e.path())
        };
        // Best effort throughout: something another process is racing us to
        // delete is not a problem worth reporting from a test helper.
        let _ = removed;
    }
}

#[cfg(not(unix))]
fn sweep_dead_scratch(_dir: &Path) {}

pub fn scratch_base() -> PathBuf {
    let base = match std::env::var_os("MPEDB_TEST_DIR") {
        Some(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                panic!("MPEDB_TEST_DIR={} is unusable: {e}", dir.display());
            }
            dir
        }
        _ if Path::new("/dev/shm").is_dir() => PathBuf::from("/dev/shm"),
        _ => std::env::temp_dir(),
    };
    // Once per process, not per call: `scratch_base` is used from ~200 test
    // files and often several times in one test.
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| sweep_dead_scratch(&base));
    PathBuf::from(windows_safe(&base, true))
}

/// [`scratch_base`] as a `String`, for the call sites that interpolate it
/// into a path with `format!("{dir}/name.mpedb")` — so no trailing separator,
/// because those call sites write the `/` themselves.
pub fn scratch_base_str() -> String {
    windows_safe(&scratch_base(), false)
}

/// Spell a scratch directory so that paths built from it survive being
/// interpolated into a TOML `path = "..."` line. Identity on Unix.
///
/// Around 200 test files build config text by hand, and a backslash in that
/// string is a TOML escape: `C:\Users\…` makes `\U` an invalid unicode escape
/// and `…\mpedb-x` makes `\m` an unknown one, so the config is a *parse error*
/// whose message says nothing about paths (#159). Escaping at all 244
/// interpolation sites is one fix; spelling the base so no backslash is ever
/// produced is the other, and it needs no edit to any of them.
///
/// Two facts make it work, both verified on Windows rather than assumed:
/// `std::path` accepts `/` as a separator there, and `PathBuf::push` skips
/// adding one when the base already ends in a separator. So a base of
/// `C:/Users/x/Temp/` survives `.join("db.mpedb")` with its spelling intact,
/// while the same base *without* the trailing slash comes back as
/// `C:/Users/x/Temp\db.mpedb` — one backslash, and the config no longer parses.
/// Hence `trailing`: the `PathBuf` form keeps the separator, the `String` form
/// drops it because its callers add one.
///
/// This is not a substitute for [`toml_path`]. A path that comes from
/// somewhere else — a user, a fixture, `current_exe()` — must still be escaped.
fn windows_safe(p: &Path, trailing: bool) -> String {
    let mut s = p.display().to_string();
    if cfg!(windows) {
        s = s.replace('\\', "/");
    }
    let has = s.ends_with('/') || (cfg!(windows) && s.ends_with('\\'));
    match (trailing, has) {
        (true, false) => {
            s.push('/');
            s
        }
        (false, true) => {
            s.truncate(s.len() - 1);
            s
        }
        _ => s,
    }
}

/// A scratch path, escaped for a TOML `path = "..."` line.
///
/// On Unix this is the identity, which is why every test that hand-builds
/// config text got away without it. On Windows `C:\Users\...` makes `\U` a
/// TOML unicode escape, so the unescaped path is a *parse error* — the config
/// never reaches the engine and the failure looks nothing like a path problem.
/// #159 found it in four production call sites first (each with a different
/// wrong answer, one of them lossy); the test tree has the same shape.
pub fn toml_path(p: impl AsRef<Path>) -> String {
    mpedb_types::toml_escape(&p.as_ref().display().to_string())
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
    ///
    /// Spelled with forward slashes on Windows, for the same reason
    /// [`scratch_base`] is: this lands inside a TOML `path = "..."` at nearly
    /// every call site, and `PathBuf::join` inserts a BACKSLASH. `scratch_base`
    /// protects the base; joining onto it undoes that protection, which is why
    /// guarding only the base was not enough (#159, found on the fourth
    /// instance).
    ///
    /// The failure is worse than a parse error, and that is the reason this is
    /// fixed HERE and not at the call sites. `\b`, `\t`, `\n`, `\r`, `\f`
    /// are all VALID TOML escapes — so `dir.join("bug.mpedb")` parses happily
    /// into a string containing a literal backspace, and the caller opens a
    /// path nobody named. It surfaced as `InvalidFilename` from the OS, a long
    /// way from the interpolation that caused it. A name starting with any
    /// other letter would simply have been wrong in silence.
    pub fn db_path(&self, name: &str) -> PathBuf {
        PathBuf::from(windows_safe(&self.path.join(format!("{name}.mpedb")), false))
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
