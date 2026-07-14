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

const BIN: &str = "/usr/lib/postgresql/16/bin";
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
        // short paths (unix socket has a 107-byte limit)
        let base = std::env::temp_dir().join(format!("mpgm-{pid}-{n}"));
        let datadir = base.join("d");
        let sockdir = base.join("s");
        // TCP is disabled; the port only names the socket file, so a fixed value
        // is fine because sockdirs are unique per instance.
        let port = 54331;
        std::fs::create_dir_all(&sockdir).unwrap();
        std::fs::create_dir_all(&datadir).unwrap();

        let ok = Command::new(format!("{BIN}/initdb"))
            .arg("-D")
            .arg(&datadir)
            .args(["--auth=trust", "-U", "mirror", "-E", "UTF8", "--locale=C", "--no-instructions"])
            .output()
            .expect("run initdb")
            .status
            .success();
        assert!(ok, "initdb failed (is PostgreSQL 16 installed at {BIN}?)");

        let opts = format!(
            "-c port={port} -c unix_socket_directories={} -c listen_addresses= \
             -c fsync=off -c synchronous_commit=off -c max_connections=16 \
             -c wal_level=logical",
            sockdir.display()
        );
        let ok = Command::new(format!("{BIN}/pg_ctl"))
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
            let _ = Command::new(format!("{BIN}/pg_ctl"))
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
