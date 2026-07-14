//! Test-only throwaway PostgreSQL harness (compiled under `cfg(test)` only):
//! `initdb --auth=trust` + `pg_ctl start` on a private unix socket, owned by the
//! current user, torn down on drop. Modelled on `mpedb-bench`'s PgServer.
//!
//! Integration tests that use this are `#[ignore]`d (they need PostgreSQL
//! installed and take seconds), so `cargo test --workspace` stays fast and
//! PG-free. Run them with `cargo test -p mpedb-mirror -- --ignored`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use postgres::{Client, NoTls};

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
static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct ThrowawayPg {
    datadir: PathBuf,
    sockdir: PathBuf,
    port: u16,
    running: bool,
}

impl ThrowawayPg {
    /// Spin up a fresh cluster. Panics with a clear message if the PG binaries
    /// are missing (the caller is an `#[ignore]`d test, so that only surfaces
    /// when explicitly run).
    pub fn start() -> ThrowawayPg {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        // /dev/shm: roomy tmpfs and a short path (the unix socket has a 107-byte
        // limit), and it keeps the ~40 MB datadirs off the small root fs.
        let shm = std::path::Path::new("/dev/shm");
        let base = if shm.is_dir() {
            shm.join(format!("mpgm-{pid}-{n}"))
        } else {
            std::env::temp_dir().join(format!("mpgm-{pid}-{n}"))
        };
        let datadir = base.join("d");
        let sockdir = base.join("s");
        // TCP is disabled; the port only names the socket file, so a fixed value
        // is fine because sockdirs are unique per instance.
        let port = 54331;
        std::fs::create_dir_all(&sockdir).unwrap();
        std::fs::create_dir_all(&datadir).unwrap();

        let ok = Command::new(format!("{}/initdb", pg_bin()))
            .arg("-D")
            .arg(&datadir)
            .args(["--auth=trust", "-U", "mirror", "-E", "UTF8", "--locale=C", "--no-instructions"])
            .output()
            .expect("run initdb")
            .status
            .success();
        assert!(
            ok,
            "initdb failed (looked in the usual Debian/Homebrew roots and $PATH; \
             resolved to {:?}. Set MPEDB_PG_BIN if it lives elsewhere)",
            pg_bin()
        );

        let opts = format!(
            "-c port={port} -c unix_socket_directories={} -c listen_addresses= \
             -c fsync=off -c synchronous_commit=off -c max_connections=16 \
             -c wal_level=logical",
            sockdir.display()
        );
        let ok = Command::new(format!("{}/pg_ctl", pg_bin()))
            .arg("-D")
            .arg(&datadir)
            .arg("-l")
            .arg(datadir.join("server.log"))
            .args(["-w", "-t", "30", "-o", &opts, "start"])
            .output()
            .expect("run pg_ctl")
            .status
            .success();
        assert!(ok, "pg_ctl start failed");

        ThrowawayPg {
            datadir,
            sockdir,
            port,
            running: true,
        }
    }

    pub fn conn_str(&self) -> String {
        format!(
            "host={} port={} user=mirror dbname=postgres",
            self.sockdir.display(),
            self.port
        )
    }

    pub fn client(&self) -> Client {
        Client::connect(&self.conn_str(), NoTls).expect("connect to throwaway pg")
    }
}

impl Drop for ThrowawayPg {
    fn drop(&mut self) {
        if self.running {
            let _ = Command::new(format!("{}/pg_ctl", pg_bin()))
                .arg("-D")
                .arg(&self.datadir)
                .args(["-m", "immediate", "-w", "-t", "20", "stop"])
                .output();
        }
        if let Some(base) = self.datadir.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}
