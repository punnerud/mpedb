//! Turso adapter (tursodatabase/turso — the Rust SQLite rewrite), via the
//! `turso` crate's async API driven by a per-worker current-thread tokio
//! runtime (`block_on`): the harness is synchronous and one runtime per
//! worker keeps connections thread-local, mirroring the SQLite adapter's
//! one-connection-per-thread shape.
//!
//! Honesty notes for the report:
//! - Turso is WAL-only by design (their COMPAT.md: rollback modes "Not
//!   Needed"); the none-class cell is "tmpfs + default sync" — if PRAGMA
//!   synchronous is a no-op there, the cell is at worst UNDER-reporting
//!   turso (extra syncs on tmpfs cost little).
//! - Concurrent writers: Turso returns Busy for a second writer (their
//!   documented, deliberate gap — no blocking `busy_timeout` exists). The
//!   adapter retries with a yielding backoff up to ~60 s so the cell
//!   measures throughput-under-retry, the closest analog of SQLite's
//!   busy_timeout arbitration. Without it the cell would just error.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::engines::{age_for, email_for, Conn, Engine};
use crate::util::{BResult, BoxErr};

/// The `turso` crate has no version API; keep this in step with Cargo.toml.
/// Durability facts verified against this version's source (turso_core 0.7.0):
/// default `SyncMode::Full` (lib.rs), `PRAGMA fullfsync` exists but is
/// Apple-only and defaults OFF like SQLite's (io/mod.rs `FileSyncType`).
pub const TURSO_VERSION: &str = "0.7.0";

#[derive(Clone, Copy)]
pub enum TursoMode {
    /// tmpfs file, default settings (WAL) — none-class medium.
    NoneClass,
    /// disk file, default settings (WAL, durable per turso's defaults).
    CommitClass,
}

pub struct TursoEngine {
    dir: PathBuf,
    mode: TursoMode,
}

impl TursoEngine {
    pub fn new(dir: PathBuf, mode: TursoMode) -> BResult<TursoEngine> {
        std::fs::create_dir_all(&dir)?;
        Ok(TursoEngine { dir, mode })
    }

    fn db_path(&self) -> PathBuf {
        self.dir.join("bench.turso")
    }
}

/// Checkpoint cadence, in write ops. SQLite's default WAL autocheckpoint
/// (1000 pages) is what its adapter runs with; turso 0.7 has NO autocheckpoint
/// (the pragma does not exist), and without one its WAL grew ~1.9 GB inside a
/// single 3 s disk cell — measured filling the dev box to ENOSPC. The manual
/// TRUNCATE checkpoint below is the closest honest analog; its cost is
/// included in the measured time, exactly as SQLite's autocheckpoint cost is.
const CHECKPOINT_EVERY: u64 = 1000;

struct TursoConn {
    rt: tokio::runtime::Runtime,
    conn: turso::Connection,
    writes: u64,
    in_txn: bool,
}

fn rt() -> BResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| BoxErr::from(format!("tokio: {e}")))
}

fn open_conn(path: &Path, mode: TursoMode) -> BResult<TursoConn> {
    let rt = rt()?;
    let conn = rt.block_on(async {
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .map_err(|e| BoxErr::from(format!("turso open: {e}")))?;
        db.connect().map_err(|e| BoxErr::from(format!("turso connect: {e}")))
    })?;
    let mut c = TursoConn { rt, conn, writes: 0, in_txn: false };
    // Same honesty rule as the SQLite adapter: on Apple, `fsync()` does not
    // flush the drive's write cache, and turso's `PRAGMA fullfsync` (Apple-only
    // pragma) defaults OFF — without this, macOS commit-class numbers would be
    // 20-165x too good. Turso's default sync mode is already Full (one fsync
    // per commit), so this only upgrades WHICH fsync is issued.
    if cfg!(target_os = "macos") && matches!(mode, TursoMode::CommitClass) {
        c.exec_retry("PRAGMA fullfsync = 1", ())?;
    }
    Ok(c)
}

/// Retry-on-busy with yielding backoff — the busy_timeout analog. Turso has
/// no blocking arbitration; a second concurrent writer sees Busy immediately.
fn is_busy(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("busy") || m.contains("locked")
}

impl TursoConn {
    fn exec_retry(&mut self, sql: &str, params: impl turso::IntoParams + Clone) -> BResult<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let r = self.rt.block_on(self.conn.execute(sql, params.clone()));
            match r {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    if is_busy(&msg) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_micros(100));
                        continue;
                    }
                    return Err(BoxErr::from(format!("turso: {msg}")));
                }
            }
        }
    }

    /// Count a write and checkpoint every CHECKPOINT_EVERY of them (never
    /// inside an open transaction — the catch-up happens after COMMIT).
    fn note_write_and_checkpoint(&mut self) -> BResult<()> {
        self.writes += 1;
        if self.in_txn || self.writes < CHECKPOINT_EVERY {
            return Ok(());
        }
        self.writes = 0;
        self.checkpoint_now()
    }

    /// The pragma returns a row (busy, log, checkpointed), so it goes through
    /// query; Busy gets the same yielding retry as writes do — a checkpointer
    /// waiting out writers is exactly what SQLite's autocheckpoint does too.
    fn checkpoint_now(&mut self) -> BResult<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let r = self.rt.block_on(async {
                let mut rows = self
                    .conn
                    .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
                    .await?;
                while rows.next().await?.is_some() {}
                Ok::<(), turso::Error>(())
            });
            match r {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    if is_busy(&msg) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_micros(100));
                        continue;
                    }
                    return Err(BoxErr::from(format!("turso checkpoint: {msg}")));
                }
            }
        }
    }
}

impl Conn for TursoConn {
    fn insert(&mut self, id: i64, email: &str, age: i64) -> BResult<()> {
        self.exec_retry(
            "INSERT INTO users (id, email, age) VALUES (?, ?, ?)",
            (id, email.to_string(), age),
        )?;
        self.note_write_and_checkpoint()
    }

    fn select(&mut self, id: i64) -> BResult<bool> {
        let found = self.rt.block_on(async {
            let mut rows = self
                .conn
                .query("SELECT email FROM users WHERE id = ?", (id,))
                .await
                .map_err(|e| BoxErr::from(format!("turso select: {e}")))?;
            Ok::<bool, BoxErr>(
                rows.next()
                    .await
                    .map_err(|e| BoxErr::from(format!("turso next: {e}")))?
                    .is_some(),
            )
        })?;
        Ok(found)
    }

    fn update(&mut self, id: i64, age: i64) -> BResult<()> {
        self.exec_retry("UPDATE users SET age = ? WHERE id = ?", (age, id))?;
        self.note_write_and_checkpoint()
    }

    fn insert_batch(&mut self, base_id: i64, n: i64) -> BResult<()> {
        self.exec_retry("BEGIN", ())?;
        self.in_txn = true;
        for i in 0..n {
            let id = base_id + i;
            if let Err(e) = self.insert(id, &email_for(id), age_for(id)) {
                let _ = self.exec_retry("ROLLBACK", ());
                self.in_txn = false;
                return Err(e);
            }
        }
        self.exec_retry("COMMIT", ())?;
        self.in_txn = false;
        // Catch up on any checkpoint threshold crossed inside the transaction.
        if self.writes >= CHECKPOINT_EVERY {
            self.writes = 0;
            return self.checkpoint_now();
        }
        Ok(())
    }
}

impl Engine for TursoEngine {
    fn reset_and_seed(&mut self, rows: i64) -> BResult<()> {
        let path = self.db_path();
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
        let mut c = open_conn(&path, self.mode)?;
        c.exec_retry(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE NOT NULL, age INTEGER)",
            (),
        )?;
        c.exec_retry("BEGIN", ())?;
        c.in_txn = true;
        for id in 0..rows {
            c.insert(id, &email_for(id), age_for(id))?;
        }
        c.exec_retry("COMMIT", ())?;
        c.in_txn = false;
        c.checkpoint_now()?;
        c.writes = 0;
        Ok(())
    }

    fn conn(&self) -> BResult<Box<dyn Conn>> {
        Ok(Box::new(open_conn(&self.db_path(), self.mode)?))
    }
}
