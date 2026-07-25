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
    match std::env::var_os("MPEDB_TEST_DIR") {
        Some(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                panic!("MPEDB_TEST_DIR={} is unusable: {e}", dir.display());
            }
            dir
        }
        _ if std::path::Path::new("/dev/shm").is_dir() => PathBuf::from("/dev/shm"),
        _ => std::env::temp_dir(),
    }
}

/// [`scratch_base`] as a `String`, for the sites that build a path with
/// `format!`.
#[allow(dead_code)]
pub fn scratch_base_str() -> String {
    scratch_base().display().to_string()
}
