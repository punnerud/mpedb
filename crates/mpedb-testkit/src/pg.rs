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

/// Locate the PostgreSQL server binaries (`initdb`/`pg_ctl`).
///
/// This was a hardcoded `/usr/lib/postgresql/16/bin` — Debian's layout. The
/// cost of that showed up the moment a second platform existed: PostgreSQL was
/// installed on the M3 Mac and the harness still reported it "not found",
/// because Homebrew puts it under /opt/homebrew. The cells failed honestly, so
/// nothing was ever WRONG — the engine was simply, silently, unmeasurable on
/// that machine, which is worse than it sounds when the whole point is a
/// three-engine comparison.
///
/// Order: `MPEDB_PG_BIN` (an explicit answer always wins), then the usual
/// install roots, then `$PATH`. Returns the directory, not the binary.
fn pg_bin() -> String {
    // Resolved once; "" means "not found", which the callers surface as an
    // honest engine-unavailable rather than a panic.
    static DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        pg_bin_dir()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
    .clone()
}

fn pg_bin_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Ok(p) = std::env::var("MPEDB_PG_BIN") {
        let p = PathBuf::from(p);
        if p.join("initdb").is_file() {
            return Some(p);
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    // Debian/Ubuntu keep versioned trees off $PATH; newest first.
    for v in ["18", "17", "16", "15", "14"] {
        cands.push(PathBuf::from(format!("/usr/lib/postgresql/{v}/bin")));
        // Homebrew: Apple Silicon, then Intel.
        cands.push(PathBuf::from(format!("/opt/homebrew/opt/postgresql@{v}/bin")));
        cands.push(PathBuf::from(format!("/usr/local/opt/postgresql@{v}/bin")));
    }
    cands.push(PathBuf::from("/usr/bin"));
    cands.push(PathBuf::from("/usr/local/bin"));
    cands.push(PathBuf::from("/opt/homebrew/bin"));
    for c in cands {
        if c.join("initdb").is_file() {
            return Some(c);
        }
    }
    // Last resort: whatever $PATH says.
    std::process::Command::new("initdb")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|_| {
            std::process::Command::new("sh")
                .args(["-c", "command -v initdb"])
                .output()
                .ok()
        })
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

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
        if pg_bin().is_empty() || !Path::new(&pg_bin()).join("initdb").exists() {
            return Err(Failure(format!(
                "{PG_UNAVAILABLE}initdb not found (looked in the usual Debian/Homebrew roots and $PATH; set MPEDB_PG_BIN to point at it)"
            )));
        }
        let dir = TempDir::new("pg")?;
        let datadir = dir.path().join("data");
        let sockdir = dir.path().join("s");
        std::fs::create_dir_all(&sockdir)?;

        let mut initdb = Command::new(format!("{}/initdb", pg_bin()));
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
        let mut start = Command::new(format!("{}/pg_ctl", pg_bin()));
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
            let _ = Command::new(format!("{}/pg_ctl", pg_bin()))
                .arg("-D")
                .arg(&self.datadir)
                .args(["-m", "immediate", "-w", "-t", "30", "stop"])
                .output();
            self.running = false;
        }
        // self.dir (TempDir) removes datadir + sockdir after this.
    }
}
