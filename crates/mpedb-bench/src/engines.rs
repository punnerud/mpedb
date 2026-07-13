//! The engine abstraction every benchmarked system implements.
//!
//! Identical logical schema everywhere:
//! `users(id int64/bigint PK, email text UNIQUE NOT NULL, age int64/bigint)`
//! (STRICT in SQLite). All hot-path operations go through prepared
//! statements / precompiled plans in every engine.

use crate::util::BResult;

/// One worker's handle. `Send` so worker threads can own one each:
/// - mpedb: a clone of the shared `Arc<Database>` (threads share ONE handle);
/// - SQLite: a dedicated `Connection` per thread;
/// - PostgreSQL: a dedicated client (socket) per thread.
pub trait Conn: Send {
    fn insert(&mut self, id: i64, email: &str, age: i64) -> BResult<()>;
    /// Point PK lookup; returns whether the row was found.
    fn select(&mut self, id: i64) -> BResult<bool>;
    fn update(&mut self, id: i64, age: i64) -> BResult<()>;

    /// Insert `n` sequential rows (ids `base_id..base_id+n`) in ONE durable
    /// commit — a transaction / WriteSession — so a single fsync amortizes
    /// across the whole batch. Default: an autocommit loop (n separate
    /// commits); engines with a cheaper batch path override it.
    fn insert_batch(&mut self, base_id: i64, n: i64) -> BResult<()> {
        for i in 0..n {
            let id = base_id + i;
            self.insert(id, &email_for(id), age_for(id))?;
        }
        Ok(())
    }
}

/// A running engine instance in one medium/durability configuration.
pub trait Engine {
    /// Drop all data, recreate the `users` table, and seed rows with
    /// ids `0..rows` in one batched transaction (unmeasured setup), so every
    /// workload cell starts from an identical table.
    fn reset_and_seed(&mut self, rows: i64) -> BResult<()>;

    /// Open a handle for one worker thread.
    fn conn(&self) -> BResult<Box<dyn Conn>>;
}

/// Deterministic unique email for a row id (unique because ids are unique).
pub fn email_for(id: i64) -> String {
    format!("u{id}@example.com")
}

pub fn age_for(id: i64) -> i64 {
    id % 100
}
