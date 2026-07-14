//! See the module declaration in `lib.rs` for why this exists.

use std::ops::Deref;
use std::path::PathBuf;

use crate::Database;

/// A `Database` plus ownership of its file(s): dropping it removes them,
/// whether the test passed, failed, or panicked.
pub(crate) struct TestDb {
    db: Database,
}

impl TestDb {
    pub fn new(db: Database) -> TestDb {
        TestDb { db }
    }
}

impl Deref for TestDb {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let main = self.db.path().to_path_buf();
        // The WAL sidecar is part of the database; leaving it behind would also
        // leave the next run to open a database with a foreign log next to it.
        let wal = PathBuf::from(format!("{}-wal", main.display()));
        let _ = std::fs::remove_file(&main);
        let _ = std::fs::remove_file(&wal);
    }
}
