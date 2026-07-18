//! See the module declaration in `lib.rs` for why this exists.

use std::ops::Deref;
use std::path::PathBuf;

use crate::Database;

/// Base directory for scratch databases: `/dev/shm` (mpedb's natural tmpfs
/// habitat, and fast) when it exists, else the platform temp dir. The fallback
/// is what keeps these tests portable to macOS, where `/dev/shm` does not exist
/// (#66). On Linux the choice is unchanged.
pub(crate) fn scratch_dir() -> PathBuf {
    if std::path::Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    }
}

/// Full scratch path for `name` under [`scratch_dir`], as a `String` — these
/// call sites splice it straight into a TOML `path = "..."`.
pub(crate) fn scratch_path(name: impl std::fmt::Display) -> String {
    scratch_dir()
        .join(name.to_string())
        .to_string_lossy()
        .into_owned()
}

/// Anything test-owned, plus the files it must take with it when it dies.
///
/// Generic over the handle rather than tied to `Database`, because the leak was
/// never specific to one type: a `ShardSet` fans out into `<path>.shard0..k` and
/// a `Workspace` owns one file per member, and both leaked exactly as hard.
/// Fixing only `Database` left `/dev/shm` filling anyway — which is how the
/// first attempt at this got caught, one commit later.
pub(crate) struct Owned<T> {
    inner: T,
    files: Vec<PathBuf>,
}

impl<T> Owned<T> {
    pub fn new(inner: T, files: Vec<PathBuf>) -> Owned<T> {
        Owned { inner, files }
    }
}

impl<T> Deref for Owned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        for p in &self.files {
            let _ = std::fs::remove_file(p);
            // The WAL sidecar is part of the database. Leaving it behind also
            // leaves the next run to open a database beside a foreign log.
            let _ = std::fs::remove_file(format!("{}-wal", p.display()));
        }
    }
}

/// A `Database` that removes its file on drop.
pub(crate) type TestDb = Owned<Database>;

impl TestDb {
    pub fn new_db(db: Database) -> TestDb {
        let p = db.path().to_path_buf();
        Owned::new(db, vec![p])
    }
}
