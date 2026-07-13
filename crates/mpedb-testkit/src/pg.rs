//! Throwaway PostgreSQL 16 cluster for the differential tester — the same
//! recipe as `mpedb-bench`'s `eng_pg.rs` (`initdb --auth=trust --locale=C`
//! into a scratch dir, `pg_ctl start` with a private unix socket dir, no
//! TCP), minus the honesty constraints a benchmark needs: this cluster only
//! answers correctness questions, so it runs with `--no-sync` / `fsync=off`
//! for speed. `--locale=C` still matters — text must collate bytewise,
//! matching mpedb's memcmp ordering and sqlite's BINARY collation.
//!
//! [`PgCluster`] is a guard: `Drop` ALWAYS stops the server (`pg_ctl -m
//! immediate`) and then the [`TempDir`] field removes the data and socket
//! dirs, even when a test panics.
//!
//! The port is fixed but meaningless for isolation: `listen_addresses` is
//! empty, so 54331 only names the socket file *inside this cluster's own
//! socket dir* — concurrent clusters (parallel test binaries) get distinct
//! temp dirs and never clash.

use crate::{Failure, Result, TempDir};
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = "/usr/lib/postgresql/16/bin";

/// Socket-file port (see module docs: not a shared resource).
pub const PORT: u16 = 54331;

/// Prefix of every "the environment cannot run PostgreSQL" failure, so
/// callers can fail soft (skip loudly) instead of reporting a false
/// divergence. Anything *without* this prefix is a real harness error.
pub const PG_UNAVAILABLE: &str = "postgres unavailable: ";

pub struct PgCluster {
    /// Declared first so `Drop for PgCluster` (server stop) runs before the
    /// directory removal in `Drop for TempDir` — order is load-bearing.
    dir: TempDir,
    datadir: PathBuf,
    sockdir: PathBuf,
    running: bool,
}

fn run_cmd(mut cmd: Command, what: &str) -> Result<()> {
    let out = cmd
        .output()
        .map_err(|e| Failure(format!("{PG_UNAVAILABLE}failed to spawn {what}: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        // A spawnable-but-failing initdb/pg_ctl is still an environment
        // problem (permissions, resources), not an engine divergence.
        Err(Failure(format!(
            "{PG_UNAVAILABLE}{what} failed (status {}):\n{}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

impl PgCluster {
    /// initdb + start a fresh single-user cluster under a temp dir (on
    /// /dev/shm when available — both fast and short, and unix socket paths
    /// have a 107-byte limit).
    pub fn start() -> Result<PgCluster> {
        if !Path::new(BIN).join("initdb").exists() {
            return Err(Failure(format!(
                "{PG_UNAVAILABLE}{BIN}/initdb not found"
            )));
        }
        let dir = TempDir::new("pg")?;
        let datadir = dir.path().join("data");
        let sockdir = dir.path().join("s");
        std::fs::create_dir_all(&sockdir)?;

        let mut initdb = Command::new(format!("{BIN}/initdb"));
        initdb
            .arg("-D")
            .arg(&datadir)
            .args(["--auth=trust", "-U", "diff", "-E", "UTF8"])
            .args(["--locale=C", "--no-instructions", "--no-sync"]);
        run_cmd(initdb, "initdb")?;

        let mut cluster = PgCluster {
            dir,
            datadir,
            sockdir,
            running: false,
        };
        let opts = format!(
            "-c port={PORT} -c unix_socket_directories={} -c listen_addresses= \
             -c fsync=off -c synchronous_commit=off \
             -c shared_buffers=64MB -c max_connections=8",
            cluster.sockdir.display()
        );
        let mut start = Command::new(format!("{BIN}/pg_ctl"));
        start
            .arg("-D")
            .arg(&cluster.datadir)
            .arg("-l")
            .arg(cluster.dir.path().join("server.log"))
            .args(["-w", "-t", "60", "-o", &opts, "start"]);
        run_cmd(start, "pg_ctl start")?;
        cluster.running = true;
        Ok(cluster)
    }

    /// A ready-to-run `psql` command against this cluster: no psqlrc, quiet
    /// (no command tags), unaligned tuples-only output with `|` field
    /// separator and the literal `NULL` for SQL NULL — the exact row shape
    /// [`crate::diff`] parses for sqlite3.
    pub fn psql(&self) -> Command {
        let mut cmd = Command::new("psql");
        cmd.args(["-X", "-q", "-A", "-t", "-F", "|", "-P", "null=NULL"])
            .arg("-h")
            .arg(&self.sockdir)
            .args(["-p", &PORT.to_string(), "-U", "diff", "-d", "postgres"]);
        cmd
    }
}

impl Drop for PgCluster {
    fn drop(&mut self) {
        if self.running {
            let _ = Command::new(format!("{BIN}/pg_ctl"))
                .arg("-D")
                .arg(&self.datadir)
                .args(["-m", "immediate", "-w", "-t", "30", "stop"])
                .output();
            self.running = false;
        }
        // self.dir (TempDir) removes datadir + sockdir after this.
    }
}
