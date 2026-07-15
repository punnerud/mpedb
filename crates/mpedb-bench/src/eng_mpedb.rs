//! mpedb adapter: in-process, all threads share ONE `Database` handle
//! (mpedb's intended multi-thread shape; multi-process attach is the other).
//!
//! Hot path is `execute(hash, params)` — plans prepared once per reset.
//! Durability comes from the file's config: `none` (no msync ever) or
//! `commit` (msync before ack; the intent-ring group commit engages only
//! here, DESIGN.md §5.3-5.4).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mpedb::{params, Config, Database, ExecResult, PlanHash};

use crate::engines::{age_for, email_for, Conn, Engine};
use crate::util::{err, BResult};

const SIZE_MB: u64 = 1024;

/// TRIPWIRE for a race this benchmark found and the engine has since fixed —
/// **this counter should always read 0**.
///
/// The race (durability=commit only): a reader that loads the `durable_txn`
/// gate and is then descheduled while TWO durable commits land (one per
/// double-buffer slot) finds both checksum-VALID slots gated (`txn_id > gate`)
/// and gets a spurious `Error::Corrupt("no valid meta page ...")`. The database
/// is not corrupt; re-reading succeeds.
///
/// `mpedb-core::shm::newest_meta` now reloads the monotone gate and retries,
/// which closes it. **Verified by experiment, not by reading the code** (2026-07-15,
/// this box, 3 readers + 1 durable writer on 2 cores, same `--only mpedb` flags
/// both arms):
///
/// ```text
///   newest_meta's retry loop disabled → 3 spurious retries observed
///   newest_meta as shipped            → 0
/// ```
///
/// So the retry below is no longer a workaround; it is what keeps a regression
/// in `newest_meta` from being reported as engine corruption. **If this counter
/// is ever non-zero, that retry has regressed — do not "fix" it here.** Retry
/// time is counted in the measured latency, and genuine corruption still fails
/// after the bound.
pub static SPURIOUS_CORRUPT_RETRIES: AtomicU64 = AtomicU64::new(0);
const CORRUPT_RETRY_BOUND: u32 = 100;

pub fn spurious_corrupt_retries() -> u64 {
    SPURIOUS_CORRUPT_RETRIES.load(Ordering::Relaxed)
}

pub struct MpedbEngine {
    /// Directory holding the .mpedb file (tmpfs or disk).
    dir: PathBuf,
    durability: &'static str,
    state: Option<State>,
}

struct State {
    db: Arc<Database>,
    ins: PlanHash,
    sel: PlanHash,
    upd: PlanHash,
}

impl MpedbEngine {
    /// `durability` is `"none"` or `"commit"` (written into the file config).
    pub fn new(dir: PathBuf, durability: &'static str) -> BResult<MpedbEngine> {
        std::fs::create_dir_all(&dir)?;
        Ok(MpedbEngine {
            dir,
            durability,
            state: None,
        })
    }

    fn db_path(&self) -> PathBuf {
        self.dir.join("bench.mpedb")
    }
}

impl Engine for MpedbEngine {
    fn reset_and_seed(&mut self, rows: i64) -> BResult<()> {
        // Drop the old handle (unmap) before deleting the file.
        self.state = None;
        let path = self.db_path();
        let _ = std::fs::remove_file(&path);

        let toml = format!(
            r#"
[database]
path = "{}"
size_mb = {SIZE_MB}
max_readers = 64
durability = "{}"

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true

  [[table.column]]
  name = "age"
  type = "int64"
"#,
            path.display(),
            self.durability
        );
        let db = Arc::new(Database::open_with_config(Config::from_toml_str(&toml)?)?);

        // Prepare BEFORE opening the write session (facade locking rule).
        let ins = db.prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")?;
        let sel = db.prepare("SELECT age FROM users WHERE id = $1")?;
        let upd = db.prepare("UPDATE users SET age = $1 WHERE id = $2")?;

        // Seed in one write transaction (unmeasured setup).
        let mut session = db.begin()?;
        for id in 0..rows {
            session.execute(&ins, &params![id, email_for(id), age_for(id)])?;
        }
        session.commit()?;

        self.state = Some(State { db, ins, sel, upd });
        Ok(())
    }

    fn conn(&self) -> BResult<Box<dyn Conn>> {
        let Some(s) = &self.state else {
            return err("mpedb engine not seeded");
        };
        Ok(Box::new(MpedbConn {
            db: s.db.clone(),
            ins: s.ins,
            sel: s.sel,
            upd: s.upd,
        }))
    }
}

struct MpedbConn {
    db: Arc<Database>,
    ins: PlanHash,
    sel: PlanHash,
    upd: PlanHash,
}

impl Conn for MpedbConn {
    fn insert(&mut self, id: i64, email: &str, age: i64) -> BResult<()> {
        self.db.execute(&self.ins, &params![id, email, age])?;
        Ok(())
    }

    fn select(&mut self, id: i64) -> BResult<bool> {
        let mut tries = 0u32;
        loop {
            match self.db.execute(&self.sel, &params![id]) {
                Ok(ExecResult::Rows { rows, .. }) => return Ok(!rows.is_empty()),
                Ok(other) => return err(format!("select returned {other:?}")),
                // See SPURIOUS_CORRUPT_RETRIES: reader vs two racing durable
                // commits; retry re-reads the gate. Not real corruption.
                Err(mpedb::Error::Corrupt(msg))
                    if msg.contains("no valid meta page") && tries < CORRUPT_RETRY_BOUND =>
                {
                    tries += 1;
                    SPURIOUS_CORRUPT_RETRIES.fetch_add(1, Ordering::Relaxed);
                    std::hint::spin_loop();
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn update(&mut self, id: i64, age: i64) -> BResult<()> {
        self.db.execute(&self.upd, &params![age, id])?;
        Ok(())
    }

    fn insert_batch(&mut self, base_id: i64, n: i64) -> BResult<()> {
        // One WriteSession = one commit = one fdatasync (wal) for all n rows.
        let mut s = self.db.begin()?;
        for i in 0..n {
            let id = base_id + i;
            s.execute(&self.ins, &params![id, email_for(id), age_for(id)])?;
        }
        s.commit()?;
        Ok(())
    }
}
