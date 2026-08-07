//! `mpedb-pg` — serve one PostgreSQL connection, then exit.
//!
//! # The three ways to run it, and why the first is the default
//!
//! ```text
//! mpedb-pg serve --inherited-fd <db>      # inetd / systemd Accept=yes
//! mpedb-pg serve --listen-fd <db>         # systemd Accept=no, one child per connection
//! mpedb-pg serve --unix <path> <db>       # development
//! ```
//!
//! **`--inherited-fd` is the point of the whole crate.** The connection is
//! already on stdin when the process starts; the process serves it and exits.
//! There is no listening socket, no accept loop and no resident process — the
//! OS is the doorbell, which is DESIGN-SERVICE §1 Model A applied to a wire
//! protocol instead of to a queue.
//!
//! `--listen-fd` and `--unix` exist because socket activation is not always
//! available (a container without systemd, a developer's laptop). They are the
//! same session code behind an accept loop.
//!
//! # Deployment
//!
//! `deploy/` carries the systemd units and the nginx `stream` block. TLS
//! terminates in nginx: a process spawned per connection has no business
//! holding a certificate, and `SSLRequest` is answered `N` so every client
//! falls back cleanly.

use mpedb::Database;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::path::PathBuf;

/// The port number libpq bakes into a unix socket's NAME. Nothing listens on it
/// — it is part of the filename convention, not a TCP port.
const DEFAULT_PORT: u16 = 5432;

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(m) => {
            eprintln!("mpedb-pg: {m}");
            1
        }
    };
    std::process::exit(code);
}

enum Mode {
    /// The connection is already on fd 0.
    InheritedFd,
    /// A listening socket is on fd 3 (systemd's `LISTEN_FDS` convention).
    ListenFd,
    /// Bind and listen on a unix socket path.
    Unix(PathBuf),
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let verb = args.next().unwrap_or_default();
    if verb != "serve" {
        return Err(usage());
    }
    let mut mode = None;
    let mut db_path: Option<PathBuf> = None;
    let mut require_password = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--inherited-fd" => mode = Some(Mode::InheritedFd),
            "--listen-fd" => mode = Some(Mode::ListenFd),
            "--unix" => {
                let p = args.next().ok_or("--unix needs a path")?;
                mode = Some(Mode::Unix(PathBuf::from(p)));
            }
            "--require-password" => require_password = true,
            "--help" | "-h" => {
                println!("{}", usage());
                return Ok(());
            }
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            other => db_path = Some(PathBuf::from(other)),
        }
    }
    let db_path = db_path.ok_or_else(usage)?;
    let mode = mode.unwrap_or(Mode::InheritedFd);

    match mode {
        Mode::InheritedFd => {
            // SAFETY: fd 0 is the connected socket by contract with inetd /
            // systemd `Accept=yes`. Taking ownership is correct: this process
            // serves exactly this connection and then exits.
            let sock = unsafe { std::os::unix::net::UnixStream::from_raw_fd(0) };
            serve_one(sock, &db_path, require_password)
        }
        Mode::ListenFd => {
            // systemd `Accept=no` passes listening sockets starting at fd 3.
            // SAFETY: same contract; the listener is ours for the process's
            // lifetime.
            let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
            accept_loop(listener, &db_path, require_password)
        }
        Mode::Unix(path) => {
            // libpq does NOT take a socket PATH — it takes a DIRECTORY and
            // appends `.s.PGSQL.<port>` itself. So `--unix /run/mpedb` has to
            // mean "put the socket libpq will look for in that directory", or
            // every psql invocation fails with "No such file or directory" while
            // the socket sits there plainly visible under the name the user
            // chose. A path that is not an existing directory is taken
            // literally, which keeps the escape hatch for anything that does
            // not speak libpq's convention.
            let path = if path.is_dir() {
                path.join(format!(".s.PGSQL.{DEFAULT_PORT}"))
            } else {
                path
            };
            // A stale socket file from a killed predecessor would make bind
            // fail with EADDRINUSE, which reads as "already running" when it is
            // not. Removing it is what every unix-socket server does.
            let _ = std::fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path)
                .map_err(|e| format!("bind {}: {e}", path.display()))?;
            accept_loop(listener, &db_path, require_password)
        }
    }
}

fn accept_loop(
    listener: std::os::unix::net::UnixListener,
    db_path: &std::path::Path,
    require_password: bool,
) -> Result<(), String> {
    for conn in listener.incoming() {
        let conn = conn.map_err(|e| format!("accept: {e}"))?;
        // Serialised on purpose. mpedb's readers are lock-free and its writers
        // take one lock, so concurrency here would buy throughput only for
        // read-heavy loads — and the model this crate is FOR is one process per
        // connection, where the OS already provides the parallelism. A thread
        // pool would be a second, worse answer to a question systemd already
        // answers.
        if let Err(e) = serve_one(conn, db_path, require_password) {
            eprintln!("mpedb-pg: connection: {e}");
        }
    }
    Ok(())
}

fn serve_one<S: Read + Write>(
    io: S,
    db_path: &std::path::Path,
    require_password: bool,
) -> Result<(), String> {
    let db = open(db_path)?;
    mpedb_pg::serve(
        io,
        mpedb_pg::Options {
            db,
            server_version: mpedb_pg::server_version(),
            require_password,
        },
    )
    .map_err(|e| e.to_string())
}

/// Open the user database, config file or bare file.
fn open(path: &std::path::Path) -> Result<Database, String> {
    let db = if path.extension().is_some_and(|e| e == "toml") {
        Database::open(path)
    } else {
        Database::open_from_file(path)
    }
    .map_err(|e| format!("open {}: {e}", path.display()))?;
    Ok(db)
}

fn usage() -> String {
    "usage: mpedb-pg serve [--inherited-fd | --listen-fd | --unix <path>] \
     [--require-password] <db.mpedb | config.toml>"
        .to_string()
}
