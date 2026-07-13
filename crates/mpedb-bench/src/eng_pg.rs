//! PostgreSQL 16 adapter: a THROWAWAY single-user cluster (initdb + pg_ctl
//! as the current user; the system cluster is not touched), reached over a
//! unix socket via the `postgres` crate. One client per worker thread.
//!
//! Modes matched by durability class:
//! - none-class:  data dir on /dev/shm, `fsync=off, synchronous_commit=off`.
//!   ASYMMETRY, stated plainly: PostgreSQL has no true none-mode — it always
//!   writes WAL; we only stop waiting for it. It also keeps its full
//!   client/server architecture: every op pays IPC + protocol round-trip.
//! - commit-class: data dir on disk, `fsync=on, synchronous_commit=on`
//!   (durable on ack, like mpedb commit).
//!
//! `initdb` runs WITHOUT --no-sync and the server keeps default
//! full_page_writes etc. — durability is matched honestly, not tuned away.
//! `--locale=C` so the unique email index compares bytewise, exactly like
//! SQLite's BINARY collation and mpedb's memcmp key encoding.
//!
//! `PgServer` is a guard: Drop stops the server and deletes both the data
//! dir and socket dir even when the benchmark panics.

use std::path::{Path, PathBuf};
use std::process::Command;

use postgres::{Client, NoTls, Statement};

use crate::engines::{age_for, email_for, Conn, Engine};
use crate::util::{err, BResult};

const BIN: &str = "/usr/lib/postgresql/16/bin";
const PORT: u16 = 54329;

pub struct PgServer {
    datadir: PathBuf,
    sockdir: PathBuf,
    running: bool,
}

fn run_cmd(mut cmd: Command, what: &str) -> BResult<String> {
    let out = cmd
        .output()
        .map_err(|e| format!("failed to spawn {what}: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        err(format!(
            "{what} failed (status {}):\n{}\n{}",
            out.status,
            stdout,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

impl PgServer {
    /// initdb + start. `datadir` must not pre-exist; `sockdir` should be a
    /// SHORT path (unix socket 107-byte limit). `durable` selects
    /// fsync/synchronous_commit on or off.
    pub fn start(datadir: PathBuf, sockdir: PathBuf, durable: bool) -> BResult<PgServer> {
        let (fsync, sync_commit) = if durable { ("on", "on") } else { ("off", "off") };
        Self::start_general(datadir, sockdir, fsync, sync_commit)
    }

    /// Like [`start`] but with explicit `fsync` / `synchronous_commit` GUCs.
    /// Used for the deferred class: `fsync=on, synchronous_commit=off` — WAL is
    /// still written and fsync'd by the WAL writer, commits just do not WAIT
    /// for it (the analog of sqlite `synchronous=NORMAL` and mpedb `async`).
    pub fn start_general(
        datadir: PathBuf,
        sockdir: PathBuf,
        fsync: &str,
        sync_commit: &str,
    ) -> BResult<PgServer> {
        std::fs::create_dir_all(&sockdir)?;
        std::fs::create_dir_all(datadir.parent().unwrap_or(Path::new("/")))?;

        let mut initdb = Command::new(format!("{BIN}/initdb"));
        initdb
            .arg("-D")
            .arg(&datadir)
            .args(["--auth=trust", "-U", "bench", "-E", "UTF8"])
            .args(["--locale=C", "--no-instructions"]);
        // NOT passing --no-sync: initial durability is matched honestly.
        run_cmd(initdb, "initdb")?;

        let mut guard = PgServer {
            datadir: datadir.clone(),
            sockdir: sockdir.clone(),
            running: false,
        };

        let opts = format!(
            "-c port={PORT} -c unix_socket_directories={} -c listen_addresses= \
             -c fsync={fsync} -c synchronous_commit={sync_commit} \
             -c shared_buffers=256MB -c max_connections=32",
            sockdir.display()
        );
        let mut start = Command::new(format!("{BIN}/pg_ctl"));
        start
            .arg("-D")
            .arg(&datadir)
            .arg("-l")
            .arg(datadir.join("server.log"))
            .args(["-w", "-t", "60", "-o", &opts, "start"]);
        run_cmd(start, "pg_ctl start")?;
        guard.running = true;
        Ok(guard)
    }

    pub fn conn_str(&self) -> String {
        format!(
            "host={} port={PORT} user=bench dbname=postgres",
            self.sockdir.display()
        )
    }
}

impl Drop for PgServer {
    fn drop(&mut self) {
        if self.running {
            // `-m immediate`: fastest teardown; the data dir is deleted next.
            let _ = Command::new(format!("{BIN}/pg_ctl"))
                .arg("-D")
                .arg(&self.datadir)
                .args(["-m", "immediate", "-w", "-t", "30", "stop"])
                .output();
            self.running = false;
        }
        let _ = std::fs::remove_dir_all(&self.datadir);
        let _ = std::fs::remove_dir_all(&self.sockdir);
    }
}

pub struct PgEngine {
    /// Admin client for reset/seed. Declared before `server` so it drops
    /// (closes its socket) before the guard stops the server.
    admin: Client,
    server: PgServer,
}

impl PgEngine {
    pub fn new(server: PgServer) -> BResult<PgEngine> {
        let mut admin = Client::connect(&server.conn_str(), NoTls)?;
        // Connectivity sanity check before any cell runs.
        let _: String = admin.query_one("SHOW server_version", &[])?.get(0);
        Ok(PgEngine { admin, server })
    }
}

impl Engine for PgEngine {
    fn reset_and_seed(&mut self, rows: i64) -> BResult<()> {
        self.admin.batch_execute(
            "DROP TABLE IF EXISTS users;
             CREATE TABLE users (
                id    bigint PRIMARY KEY,
                email text UNIQUE NOT NULL,
                age   bigint
             );",
        )?;
        // COPY: batched, unmeasured setup.
        let mut w = self.admin.copy_in("COPY users (id, email, age) FROM STDIN")?;
        let mut buf = String::with_capacity(1 << 16);
        for id in 0..rows {
            use std::fmt::Write as _;
            let _ = writeln!(buf, "{id}\t{}\t{}", email_for(id), age_for(id));
            if buf.len() >= (1 << 16) - 64 {
                std::io::Write::write_all(&mut w, buf.as_bytes())?;
                buf.clear();
            }
        }
        std::io::Write::write_all(&mut w, buf.as_bytes())?;
        w.finish()?;
        Ok(())
    }

    fn conn(&self) -> BResult<Box<dyn Conn>> {
        let mut client = Client::connect(&self.server.conn_str(), NoTls)?;
        let ins = client.prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")?;
        let sel = client.prepare("SELECT age FROM users WHERE id = $1")?;
        let upd = client.prepare("UPDATE users SET age = $1 WHERE id = $2")?;
        Ok(Box::new(PgConn {
            client,
            ins,
            sel,
            upd,
        }))
    }
}

struct PgConn {
    client: Client,
    ins: Statement,
    sel: Statement,
    upd: Statement,
}

impl Conn for PgConn {
    fn insert(&mut self, id: i64, email: &str, age: i64) -> BResult<()> {
        self.client.execute(&self.ins, &[&id, &email, &age])?;
        Ok(())
    }

    fn select(&mut self, id: i64) -> BResult<bool> {
        Ok(self.client.query_opt(&self.sel, &[&id])?.is_some())
    }

    fn update(&mut self, id: i64, age: i64) -> BResult<()> {
        self.client.execute(&self.upd, &[&age, &id])?;
        Ok(())
    }

    fn insert_batch(&mut self, base_id: i64, n: i64) -> BResult<()> {
        let mut tx = self.client.transaction()?;
        for i in 0..n {
            let id = base_id + i;
            tx.execute(&self.ins, &[&id, &email_for(id), &age_for(id)])?;
        }
        tx.commit()?; // one WAL flush for the whole batch
        Ok(())
    }
}
