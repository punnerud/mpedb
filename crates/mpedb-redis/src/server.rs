//! One thread per connection, blocking reads. The engine groups commits
//! across those threads when durability is `commit` or `wal`; an async
//! runtime would not change that, and it would be a dependency the rest of
//! the process does not have.
//!
//! The data directory is held with an exclusive lock for the life of the
//! process. The type check and the write are two statements, and a second
//! process running them at the same time could land a string and a hash on
//! the same key.

use crate::cmd::dispatch;
use crate::config::ServeConfig;
use crate::resp::{self, CommandReader, ProtocolError, Resp};
use crate::store::BlobStore;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct State {
    pub store: BlobStore,
    pub config: ServeConfig,
    pub inflight: AtomicUsize,
    pub stop: AtomicBool,
    _lock: std::fs::File,
}

impl State {
    pub fn open(config: ServeConfig) -> Result<Arc<Self>, String> {
        let lock = lock_data_dir(&config.store.dir)?;
        let store = BlobStore::open(&config.store).map_err(|e| e.to_string())?;
        Ok(Arc::new(State {
            store,
            config,
            inflight: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            _lock: lock,
        }))
    }
}

/// Bind `config.listen` and serve until the process is killed or [`State::stop`]
/// is set and one more connection arrives to wake `accept`.
pub fn serve(config: ServeConfig) -> Result<(), String> {
    let listen = config.listen;
    let listener = TcpListener::bind(listen).map_err(|e| format!("bind {listen}: {e}"))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("local address: {e}"))?;
    let state = State::open(config)?;
    eprintln!(
        "mpedb-redis: listening on {bound}, {} shard(s), durability {}, data {}",
        state.store.shards(),
        state.config.store.durability,
        state.store.dir().display()
    );
    if matches!(state.config.store.durability.as_str(), "commit" | "wal") {
        eprintln!("mpedb-redis: group commit is on for writes up to 1 MiB of parameters");
    }
    run(listener, state)
}

pub fn run(listener: TcpListener, state: Arc<State>) -> Result<(), String> {
    spawn_reaper(&state);
    loop {
        // EMFILE and ECONNABORTED are a full table or one bad peer. Neither
        // is a reason to drop every connection that is already open.
        let sock = match listener.accept() {
            Ok((sock, _)) => sock,
            Err(e) => {
                eprintln!("mpedb-redis: accept: {e}");
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if state.stop.load(Ordering::Relaxed) {
            break;
        }
        if !acquire(&state) {
            let _ = write_resp(&sock, &Resp::err("ERR max connections"));
            continue;
        }
        let state = Arc::clone(&state);
        thread::spawn(move || {
            let _release = Release(&state.inflight);
            if let Err(e) = connection(sock, &state) {
                if state.stop.load(Ordering::Relaxed) {
                    return;
                }
                eprintln!("mpedb-redis: connection: {e}");
            }
        });
    }
    Ok(())
}

fn spawn_reaper(state: &Arc<State>) {
    if state.config.cleanup_interval.is_zero() {
        return;
    }
    let state = Arc::clone(state);
    thread::spawn(move || {
        while !state.stop.load(Ordering::Relaxed) {
            thread::sleep(state.config.cleanup_interval);
            if state.stop.load(Ordering::Relaxed) {
                break;
            }
            // A full chunk means more is waiting. Stopping there would cap
            // the reaper at one chunk per interval while clients expire
            // faster, and the file would fill with dead rows.
            let chunk = state.config.cleanup_chunk;
            while !state.stop.load(Ordering::Relaxed) {
                match state.store.reap(now_ms(), chunk) {
                    Ok(n) if n >= chunk as u64 => continue,
                    Ok(_) => break,
                    Err(e) => {
                        eprintln!("mpedb-redis: cleanup: {e}");
                        break;
                    }
                }
            }
        }
    });
}

fn connection(mut sock: TcpStream, state: &State) -> Result<(), String> {
    sock.set_nodelay(true)
        .map_err(|e| format!("nodelay: {e}"))?;
    // No read timeout. Redis ships `timeout 0`, and a pooled client that
    // sat idle must find its socket open. A reader that stops taking a
    // reply is different: it holds a thread mid-command, so writes give up.
    sock.set_write_timeout(Some(WRITE_STALL))
        .map_err(|e| format!("write timeout: {e}"))?;
    let mut reader = CommandReader::new(state.config.store.max_value_bytes);
    let mut out = Vec::new();
    let mut tmp = vec![0u8; 64 * 1024];
    loop {
        loop {
            let args = match reader.next_command() {
                Ok(Some(args)) => args,
                Ok(None) => break,
                Err(ProtocolError(message)) => {
                    // Say why before closing. A client that only sees a
                    // reset cannot tell an oversized value from a dead
                    // server.
                    resp::encode(
                        &Resp::err(format!("ERR Protocol error: {message}")),
                        &mut out,
                    );
                    let _ = sock.write_all(&out);
                    return Err(message);
                }
            };
            let outcome = dispatch(&state.store, &state.config.cluster, &args, now_ms());
            resp::encode(&outcome.reply, &mut out);
            if outcome.close {
                return sock.write_all(&out).map_err(|e| format!("write: {e}"));
            }
            // A pipeline is answered in one write, but a client that sends
            // without reading must not grow this without bound.
            if out.len() >= FLUSH_AT {
                flush(&mut sock, &mut out)?;
            }
        }
        flush(&mut sock, &mut out)?;
        let n = sock.read(&mut tmp).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        reader.feed(&tmp[..n]);
    }
}

/// How long one reply may wait on a reader that has stopped reading.
const WRITE_STALL: Duration = Duration::from_secs(60);
/// Replies of a pipeline are coalesced up to this many bytes.
const FLUSH_AT: usize = 256 * 1024;

fn flush(sock: &mut TcpStream, out: &mut Vec<u8>) -> Result<(), String> {
    if out.is_empty() {
        return Ok(());
    }
    sock.write_all(out).map_err(|e| format!("write: {e}"))?;
    out.clear();
    if out.capacity() > FLUSH_AT * 2 {
        *out = Vec::new();
    }
    Ok(())
}

fn write_resp(mut sock: &TcpStream, value: &Resp) -> Result<(), String> {
    let mut bytes = Vec::new();
    resp::encode(value, &mut bytes);
    sock.write_all(&bytes).map_err(|e| format!("write: {e}"))?;
    Ok(())
}

fn acquire(state: &State) -> bool {
    let max = state.config.max_connections;
    loop {
        let n = state.inflight.load(Ordering::Relaxed);
        if n >= max {
            return false;
        }
        if state
            .inflight
            .compare_exchange(n, n + 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
    }
}

struct Release<'a>(&'a AtomicUsize);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn lock_data_dir(dir: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("server.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    try_lock(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn try_lock(file: &std::fs::File) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` owns the descriptor for as long as the lock is held,
    // and `flock` only borrows it. LOCK_NB returns immediately so a second
    // process gets an error instead of waiting out a dead one. The lock
    // dies with the process, which is what a crash has to do.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Err("another mpedb-redis process holds this data directory".into())
    } else {
        Err(format!("lock data directory: {err}"))
    }
}

#[cfg(not(unix))]
fn try_lock(_file: &std::fs::File) -> Result<(), String> {
    Ok(())
}
