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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::engines::{age_for, email_for, Conn, Engine};
use crate::util::{BResult, BoxErr};

#[derive(Clone, Copy)]
pub enum TursoMode {
    /// tmpfs file, default settings (WAL) — none-class medium.
    NoneClass,
    /// disk file, default settings (WAL, durable per turso's defaults).
    CommitClass,
}

pub struct TursoEngine {
    dir: PathBuf,
    #[allow(dead_code)]
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

struct TursoConn {
    rt: tokio::runtime::Runtime,
    conn: turso::Connection,
}

fn rt() -> BResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| BoxErr::from(format!("tokio: {e}")))
}

fn open_conn(path: &PathBuf) -> BResult<TursoConn> {
    let rt = rt()?;
    let conn = rt.block_on(async {
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .map_err(|e| BoxErr::from(format!("turso open: {e}")))?;
        db.connect().map_err(|e| BoxErr::from(format!("turso connect: {e}")))
    })?;
    Ok(TursoConn { rt, conn })
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
}

impl Conn for TursoConn {
    fn insert(&mut self, id: i64, email: &str, age: i64) -> BResult<()> {
        self.exec_retry(
            "INSERT INTO users (id, email, age) VALUES (?, ?, ?)",
            (id, email.to_string(), age),
        )
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
        self.exec_retry("UPDATE users SET age = ? WHERE id = ?", (age, id))
    }

    fn insert_batch(&mut self, base_id: i64, n: i64) -> BResult<()> {
        self.exec_retry("BEGIN", ())?;
        for i in 0..n {
            let id = base_id + i;
            if let Err(e) = self.insert(id, &email_for(id), age_for(id)) {
                let _ = self.exec_retry("ROLLBACK", ());
                return Err(e);
            }
        }
        self.exec_retry("COMMIT", ())
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
        let mut c = open_conn(&path)?;
        c.exec_retry(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE NOT NULL, age INTEGER)",
            (),
        )?;
        c.exec_retry("BEGIN", ())?;
        for id in 0..rows {
            c.insert(id, &email_for(id), age_for(id))?;
        }
        c.exec_retry("COMMIT", ())?;
        Ok(())
    }

    fn conn(&self) -> BResult<Box<dyn Conn>> {
        Ok(Box::new(open_conn(&self.db_path())?))
    }
}
