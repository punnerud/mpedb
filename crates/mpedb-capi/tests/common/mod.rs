//! Shared test helpers for the shim's integration tests.
//!
//! This crate is its OWN workspace and cannot depend on `mpedb-testkit`: the
//! testkit pulls in `mpedb` and with it the bundled sqlite, which this crate
//! must never co-link — it EXPORTS the `sqlite3_*` symbols itself. So the
//! scratch-directory rule lives here as a second copy, deliberately, and has
//! to be kept in step with `mpedb_testkit::scratch_base`.

use std::path::PathBuf;

/// Where these tests put their scratch files. Honors `MPEDB_TEST_DIR` so a
/// run can be moved off the 3.8 GB `/dev/shm` tmpfs onto a real volume;
/// otherwise `/dev/shm` (fast, and mpedb's natural habitat), and
/// `std::env::temp_dir()` where there is none — macOS has no `/dev/shm`, so
/// that fallback is load-bearing.
///
/// Note what this does NOT cover: the shim's own mapping of a named
/// `mode=memory` database to a backing file (`src/lib.rs`) is production
/// behavior, not test scratch, and stays on `/dev/shm` regardless of this
/// variable. A test that opens `:memory:` therefore still writes there.
#[allow(dead_code)] // not every test binary that includes this uses it
pub fn scratch_base() -> PathBuf {
    let base = match std::env::var_os("MPEDB_TEST_DIR") {
        Some(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                panic!("MPEDB_TEST_DIR={} is unusable: {e}", dir.display());
            }
            dir
        }
        _ if std::path::Path::new("/dev/shm").is_dir() => PathBuf::from("/dev/shm"),
        _ => std::env::temp_dir(),
    };
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| sweep_dead_scratch(&base));
    base
}

/// The twin of `mpedb_testkit::sweep_dead_scratch`, here for the reason the
/// module header gives: this crate cannot depend on the testkit, so the rule
/// is copied rather than shared, and the two must be kept in step.
///
/// `Drop` guards are the first line and catch a panicking test. This is the
/// second, for what no destructor can reach: a test binary killed by SIGKILL,
/// by a harness timeout, or by the OOM killer. All three happened on this
/// project, and a sidecar is ~4x its source — `/dev/shm` hit 96 % three times
/// in one day, once taking the session that was filling it.
///
/// Only provably dead files: the name must look like ours AND carry a PID that
/// no longer exists. `/dev/shm` is shared with other software (PostgreSQL puts
/// its own files there), and a test that names its files without a PID cannot
/// be shown to be stale, so neither is touched.
#[allow(dead_code)]
fn sweep_dead_scratch(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        let ours = |n: &str| {
            n.starts_with("mpedb-")
                || n.ends_with(".mpedb")
                || n.ends_with(".mpedb.src")
                || n.ends_with(".mpedb.lock")
                || n.ends_with(".overlay.mpedb")
                // A write-ahead log beside a scratch database: the suffix is
                // `.mpedb-wal`, so neither of the tests above sees it.
                || (n.ends_with("-wal") && n.contains(".mpedb"))
        };
        let gone = |n: &str| {
            let b = n.as_bytes();
            let (mut i, mut saw) = (0usize, false);
            while i < b.len() {
                if !b[i].is_ascii_digit() {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i - start >= 4 {
                    if let Ok(pid) = n[start..i].parse::<i32>() {
                        saw = true;
                        // ESRCH: no such process. EPERM: exists, someone
                        // else's — alive either way, so keep the file.
                        if unsafe { libc::kill(pid, 0) } == 0
                            || std::io::Error::last_os_error().raw_os_error()
                                == Some(libc::EPERM)
                        {
                            return false;
                        }
                    }
                }
            }
            saw
        };
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !ours(&n) || !gone(&n) {
                continue;
            }
            // Directories too — see the testkit twin.
            let _ = if e.file_type().is_ok_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(e.path())
            } else {
                std::fs::remove_file(e.path())
            };
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// [`scratch_base`] as a `String`, for the sites that build a path with
/// `format!`.
#[allow(dead_code)]
pub fn scratch_base_str() -> String {
    scratch_base().display().to_string()
}
