//! SQLite adapter via rusqlite (bundled SQLite 3.45.0 — the system ships
//! only libsqlite3.so.0 with no dev symlink/header, so linking the system
//! library fails; verified before choosing `bundled`).
//!
//! STRICT table, prepared statements (`prepare_cached`), one connection per
//! thread (WAL allows concurrent readers; writers serialize internally,
//! arbitrated by `busy_timeout`).
//!
//! Modes matched by durability class:
//! - none-class:  tmpfs file + `synchronous=OFF, journal_mode=MEMORY`
//!   (no fsync guarantees; an app crash mid-write can also corrupt the db —
//!   strictly WEAKER than mpedb none, which stays process-crash-safe).
//! - commit-class: disk file + `synchronous=FULL, journal_mode=WAL`
//!   (durable on ack, like mpedb commit).

use std::path::PathBuf;
use std::time::Duration;

use rusqlite::Connection;

use crate::engines::{age_for, email_for, Conn, Engine};
use crate::util::{BResult, BoxErr};

#[derive(Clone, Copy)]
// The `*Class` suffix is intentional — each variant names a durability class.
#[allow(clippy::enum_variant_names)]
pub enum SqliteMode {
    /// synchronous=OFF, journal_mode=MEMORY (none-class; tmpfs)
    NoneClass,
    /// synchronous=FULL, journal_mode=WAL (durable-on-ack; disk)
    CommitClass,
    /// synchronous=NORMAL, journal_mode=WAL (crash-consistent-deferred; disk):
    /// fsync only at checkpoint, not per commit — the analog of mpedb `async`
    /// and PostgreSQL `synchronous_commit=off`.
    NormalClass,
}

pub struct SqliteEngine {
    dir: PathBuf,
    mode: SqliteMode,
}

impl SqliteEngine {
    pub fn new(dir: PathBuf, mode: SqliteMode) -> BResult<SqliteEngine> {
        std::fs::create_dir_all(&dir)?;
        Ok(SqliteEngine { dir, mode })
    }

    fn db_path(&self) -> PathBuf {
        self.dir.join("bench.sqlite3")
    }

    fn open(&self) -> BResult<Connection> {
        let conn = Connection::open(self.db_path()).map_err(BoxErr::from)?;
        conn.busy_timeout(Duration::from_secs(60))?;
        match self.mode {
            SqliteMode::NoneClass => {
                conn.pragma_update(None, "journal_mode", "MEMORY")?;
                conn.pragma_update(None, "synchronous", "OFF")?;
            }
            SqliteMode::CommitClass => {
                conn.pragma_update(None, "journal_mode", "WAL")?;
                conn.pragma_update(None, "synchronous", "FULL")?;
            }
            SqliteMode::NormalClass => {
                conn.pragma_update(None, "journal_mode", "WAL")?;
                conn.pragma_update(None, "synchronous", "NORMAL")?;
            }
        }
        Ok(conn)
    }
}

impl Engine for SqliteEngine {
    fn reset_and_seed(&mut self, rows: i64) -> BResult<()> {
        let path = self.db_path();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
        let mut conn = self.open()?;
        conn.execute_batch(
            "CREATE TABLE users (
                id    INTEGER PRIMARY KEY,
                email TEXT NOT NULL UNIQUE,
                age   INTEGER
             ) STRICT;",
        )?;
        let tx = conn.transaction()?;
        {
            let mut ins =
                tx.prepare("INSERT INTO users (id, email, age) VALUES (?1, ?2, ?3)")?;
            for id in 0..rows {
                ins.execute((id, email_for(id), age_for(id)))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn conn(&self) -> BResult<Box<dyn Conn>> {
        Ok(Box::new(SqliteConn { conn: self.open()? }))
    }
}

struct SqliteConn {
    conn: Connection,
}

impl Conn for SqliteConn {
    fn insert(&mut self, id: i64, email: &str, age: i64) -> BResult<()> {
        self.conn
            .prepare_cached("INSERT INTO users (id, email, age) VALUES (?1, ?2, ?3)")?
            .execute((id, email, age))?;
        Ok(())
    }

    fn select(&mut self, id: i64) -> BResult<bool> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT age FROM users WHERE id = ?1")?;
        let mut rows = stmt.query([id])?;
        Ok(rows.next()?.is_some())
    }

    fn update(&mut self, id: i64, age: i64) -> BResult<()> {
        self.conn
            .prepare_cached("UPDATE users SET age = ?1 WHERE id = ?2")?
            .execute((age, id))?;
        Ok(())
    }

    fn insert_batch(&mut self, base_id: i64, n: i64) -> BResult<()> {
        let tx = self.conn.transaction()?;
        {
            let mut ins =
                tx.prepare_cached("INSERT INTO users (id, email, age) VALUES (?1, ?2, ?3)")?;
            for i in 0..n {
                let id = base_id + i;
                ins.execute((id, email_for(id), age_for(id)))?;
            }
        }
        tx.commit()?; // one fsync (WAL) for the whole batch
        Ok(())
    }
}
