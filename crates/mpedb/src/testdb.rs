//! See the module declaration in `lib.rs` for why this exists.

use std::ops::Deref;
use std::path::PathBuf;

use crate::Database;

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
