//! `threads` event loops, one by default, each with its own poller and its
//! own share of the connections. The accept thread deals new sockets out
//! in turn.
//!
//! A thread per connection was the first shape, and it was the slow part: a
//! small `SET` is about 10 µs in the engine, and waking a thread to read
//! one request and another to lead its commit cost several times that. A
//! loop reads every connection that is ready, runs what they sent as one
//! round, and writes the replies. The `SET`s of a round are one
//! transaction, whichever connections they came from, so eight clients
//! that each wait for a reply still share commits. Measured 2026-11-21,
//! eight connections, 64-byte values, no pipeline, threads against one
//! loop: `SET` 15 908 against 22 787 /s under `async`, 23 768 against
//! 39 722 /s under `none`, and `GET` 36 466 against 51 793 /s.
//!
//! A round blocks its thread for the length of its commit. Under `wal` and
//! `commit` that is a flush, and what arrives meanwhile is the next round.
//!
//! The data directory is held with an exclusive lock for the life of the
//! process. The type check and the write are two statements, and a second
//! process running them at the same time could land a string and a hash on
//! the same key.

use crate::cmd::{dispatch_round, Pipeline};
use crate::config::ServeConfig;
use crate::resp::{self, CommandReader, ProtocolError, Resp};
use crate::store::BlobStore;
use mio::net::TcpStream as Sock;
use mio::{Events, Interest, Poll, Token, Waker};
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    if !state.config.write_behind.is_zero() {
        eprintln!(
            "mpedb-redis: write_behind_ms = {}: a plain SET is answered from memory, and a \
             killed process loses the writes of that window",
            state.config.write_behind.as_millis()
        );
    }
    run(listener, state)
}

pub fn run(listener: TcpListener, state: Arc<State>) -> Result<(), String> {
    spawn_reaper(&state);
    spawn_drainer(&state);
    let mut loops = Vec::new();
    for _ in 0..state.config.threads.max(1) {
        let poll = Poll::new().map_err(|e| format!("poller: {e}"))?;
        let waker = Waker::new(poll.registry(), WAKE).map_err(|e| format!("waker: {e}"))?;
        let (tx, rx) = channel();
        let state = Arc::clone(&state);
        let handle = thread::spawn(move || EventLoop::new(state, poll, rx).run());
        loops.push((tx, waker, handle));
    }
    let mut turn = 0usize;
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
        let seat = Seat(Arc::clone(&state));
        let (tx, waker, _) = &loops[turn % loops.len()];
        turn += 1;
        if tx.send((sock, seat)).is_err() || waker.wake().is_err() {
            return Err("an event loop is gone".into());
        }
    }
    for (tx, waker, handle) in loops {
        drop(tx);
        let _ = waker.wake();
        let _ = handle.join();
    }
    state
        .store
        .drain()
        .map_err(|e| format!("held writes: {e}"))?;
    Ok(())
}

/// Set by SIGTERM and SIGINT while `write_behind` is on.
static TERMINATED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_terminate(_: libc::c_int) {
    TERMINATED.store(true, Ordering::SeqCst);
}

/// Writes what `write_behind` holds, every interval.
///
/// While it runs, SIGTERM and SIGINT do not kill the process at once: this
/// thread sees them within one interval, writes what is held and exits.
/// SIGKILL and a crash still lose the window.
fn spawn_drainer(state: &Arc<State>) {
    if state.config.write_behind.is_zero() {
        return;
    }
    #[cfg(unix)]
    // SAFETY: the handler only stores to an atomic, which is
    // async-signal-safe, and it is installed before any thread reads it.
    unsafe {
        let handler = on_terminate as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
    let state = Arc::clone(state);
    thread::spawn(move || loop {
        thread::sleep(state.config.write_behind);
        let terminated = TERMINATED.load(Ordering::SeqCst);
        if let Err(e) = state.store.drain() {
            eprintln!("mpedb-redis: held writes: {e}");
            if terminated {
                std::process::exit(1);
            }
        } else if terminated {
            std::process::exit(0);
        }
        if state.stop.load(Ordering::Relaxed) {
            break;
        }
    });
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

const WAKE: Token = Token(usize::MAX);

struct Conn {
    sock: Sock,
    _seat: Seat,
    reader: CommandReader,
    /// Parsed and not yet run: what a round left behind when a reply
    /// budget stopped it.
    parsed: VecDeque<Vec<Vec<u8>>>,
    out: Vec<u8>,
    /// Bytes of `out` the socket has taken.
    sent: usize,
    /// Sticky: set by an event, cleared when a read or a write says
    /// `WouldBlock`. The poller reports edges, so a flag that was cleared
    /// any earlier would wait for an edge that has already passed.
    readable: bool,
    writable: bool,
    /// The reader may hold complete commands that were not taken yet.
    backlog: bool,
    /// On the list for the next round.
    queued: bool,
    /// The peer sent FIN. What it sent before that is still answered.
    eof: bool,
    /// The frame boundary is lost. Nothing more is read or parsed, and the
    /// connection closes with this message once what came before it has
    /// been answered.
    fault: Option<String>,
    /// Close once `out` is written.
    closing: bool,
    /// When the peer last refused bytes with some still owed.
    stalled: Option<Instant>,
}

impl Conn {
    /// Write what is owed. `Ok` with bytes still owed means the socket is
    /// full and a writable event will follow.
    fn flush(&mut self) -> io::Result<()> {
        while self.sent < self.out.len() {
            match self.sock.write(&self.out[self.sent..]) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.sent += n;
                    self.stalled = None;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.writable = false;
                    self.stalled.get_or_insert_with(Instant::now);
                    return Ok(());
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.out.clear();
        self.sent = 0;
        self.stalled = None;
        if self.out.capacity() > FLUSH_AT * 2 {
            self.out = Vec::new();
        }
        Ok(())
    }

    fn owed(&self) -> usize {
        self.out.len() - self.sent
    }

    /// Read until the socket is empty or `READ_ROUND` bytes are in.
    fn fill(&mut self, tmp: &mut [u8]) -> io::Result<()> {
        let mut got = 0usize;
        while got < READ_ROUND {
            match self.sock.read(tmp) {
                Ok(0) => {
                    self.eof = true;
                    self.readable = false;
                    return Ok(());
                }
                Ok(n) => {
                    self.reader.feed(&tmp[..n]);
                    got += n;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.readable = false;
                    return Ok(());
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

struct EventLoop {
    state: Arc<State>,
    poll: Poll,
    inbox: Receiver<(TcpStream, Seat)>,
    conns: Vec<Option<Conn>>,
    free: Vec<usize>,
    /// Connections with something to do in the next round.
    active: Vec<usize>,
    tmp: Vec<u8>,
}

impl EventLoop {
    fn new(state: Arc<State>, poll: Poll, inbox: Receiver<(TcpStream, Seat)>) -> Self {
        EventLoop {
            state,
            poll,
            inbox,
            conns: Vec::new(),
            free: Vec::new(),
            active: Vec::new(),
            tmp: vec![0u8; 64 * 1024],
        }
    }

    fn run(mut self) {
        let mut events = Events::with_capacity(256);
        let mut swept = Instant::now();
        loop {
            // With work left over, look for new events without waiting.
            let wait = if self.active.is_empty() {
                SWEEP
            } else {
                Duration::ZERO
            };
            if let Err(e) = self.poll.poll(&mut events, Some(wait)) {
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                eprintln!("mpedb-redis: poll: {e}");
                return;
            }
            if self.state.stop.load(Ordering::Relaxed) {
                return;
            }
            for event in events.iter() {
                if event.token() == WAKE {
                    self.adopt();
                    continue;
                }
                let i = event.token().0;
                let Some(conn) = self.conns.get_mut(i).and_then(Option::as_mut) else {
                    continue;
                };
                // An error or a hangup is found by the read that follows.
                if event.is_readable() || event.is_read_closed() || event.is_error() {
                    conn.readable = true;
                }
                if event.is_writable() || event.is_write_closed() {
                    conn.writable = true;
                }
                if !std::mem::replace(&mut conn.queued, true) {
                    self.active.push(i);
                }
            }
            self.round();
            if swept.elapsed() >= SWEEP {
                swept = Instant::now();
                self.sweep();
            }
        }
    }

    fn adopt(&mut self) {
        while let Ok((sock, seat)) = self.inbox.try_recv() {
            let i = self.free.pop().unwrap_or_else(|| {
                self.conns.push(None);
                self.conns.len() - 1
            });
            let made = sock
                .set_nonblocking(true)
                .and_then(|()| sock.set_nodelay(true))
                .map(|()| Sock::from_std(sock))
                .and_then(|mut sock| {
                    self.poll.registry().register(
                        &mut sock,
                        Token(i),
                        Interest::READABLE | Interest::WRITABLE,
                    )?;
                    Ok(sock)
                });
            match made {
                Ok(sock) => {
                    self.conns[i] = Some(Conn {
                        sock,
                        _seat: seat,
                        reader: CommandReader::new(self.state.config.store.max_value_bytes),
                        parsed: VecDeque::new(),
                        out: Vec::new(),
                        sent: 0,
                        // Bytes that arrived before the registration raise
                        // no edge, so the first round reads unasked.
                        readable: true,
                        writable: true,
                        backlog: false,
                        queued: true,
                        eof: false,
                        fault: None,
                        closing: false,
                        stalled: None,
                    });
                    self.active.push(i);
                }
                Err(e) => {
                    eprintln!("mpedb-redis: connection: {e}");
                    self.free.push(i);
                }
            }
        }
    }

    fn close(&mut self, i: usize, why: Option<String>) {
        if let Some(mut conn) = self.conns[i].take() {
            let _ = self.poll.registry().deregister(&mut conn.sock);
            self.free.push(i);
            if let Some(why) = why {
                eprintln!("mpedb-redis: connection: {why}");
            }
        }
    }

    /// One pass over the connections that have something to do: write what
    /// is owed, read, run the commands of all of them together, write.
    fn round(&mut self) {
        let work = std::mem::take(&mut self.active);
        let mut owners = Vec::new();
        let mut pipes = Vec::new();
        let (mut commands, mut bytes) = (0usize, 0usize);
        for &i in &work {
            let Some(conn) = self.conns[i].as_mut() else {
                continue;
            };
            conn.queued = false;
            if let Err(e) = conn.flush() {
                self.close(i, Some(format!("write: {e}")));
                continue;
            }
            // A client that sends without reading is not read from, or its
            // replies would grow without bound. The writable event that
            // ends this puts it back on the list.
            if conn.closing {
                if conn.owed() == 0 {
                    let why = conn.fault.take();
                    self.close(i, why);
                }
                continue;
            }
            if conn.owed() >= FLUSH_AT {
                continue;
            }
            let open = conn.fault.is_none();
            if open && conn.readable && !conn.backlog && conn.parsed.is_empty() {
                if let Err(e) = conn.fill(&mut self.tmp) {
                    self.close(i, Some(format!("read: {e}")));
                    continue;
                }
            }
            // The commands before a protocol error are still run and
            // answered.
            let mut cmds: Vec<Vec<Vec<u8>>> = conn.parsed.drain(..).collect();
            let mut mine = 0usize;
            conn.backlog = open;
            while open
                && cmds.len() < PIPELINE_COMMANDS
                && mine < PIPELINE_BYTES
                && commands + cmds.len() < ROUND_COMMANDS
                && bytes + mine < ROUND_BYTES
            {
                match conn.reader.next_command() {
                    Ok(Some(args)) => {
                        mine += args.iter().map(Vec::len).sum::<usize>();
                        cmds.push(args);
                    }
                    Ok(None) => {
                        conn.backlog = false;
                        break;
                    }
                    Err(ProtocolError(message)) => {
                        conn.backlog = false;
                        conn.fault = Some(message);
                        break;
                    }
                }
            }
            commands += cmds.len();
            bytes += mine;
            owners.push(i);
            pipes.push(Pipeline::new(cmds));
        }
        if pipes.iter().any(|pipe| !pipe.cmds.is_empty()) {
            dispatch_round(
                &self.state.store,
                &self.state.config.cluster,
                &mut pipes,
                now_ms(),
            );
        }
        for (i, mut pipe) in owners.into_iter().zip(pipes) {
            let Some(conn) = self.conns[i].as_mut() else {
                continue;
            };
            let ran = pipe.replies.len();
            for outcome in &pipe.replies {
                resp::encode(&outcome.reply, &mut conn.out);
                conn.closing |= outcome.close;
            }
            if !conn.closing {
                // What the reply budget held back goes first next time.
                conn.parsed.extend(pipe.cmds.drain(ran..));
                // Say why before closing. A client that only sees a reset
                // cannot tell an oversized value from a dead server.
                if let (Some(message), true) = (&conn.fault, conn.parsed.is_empty()) {
                    resp::encode(
                        &Resp::err(format!("ERR Protocol error: {message}")),
                        &mut conn.out,
                    );
                    conn.closing = true;
                }
                if conn.eof && !conn.backlog && conn.parsed.is_empty() {
                    conn.closing = true;
                }
            }
            if let Err(e) = conn.flush() {
                self.close(i, Some(format!("write: {e}")));
                continue;
            }
            if conn.closing {
                if conn.owed() == 0 {
                    let why = conn.fault.take();
                    self.close(i, why);
                }
                continue;
            }
            let more = conn.backlog || conn.readable || !conn.parsed.is_empty();
            if more && conn.owed() < FLUSH_AT && !std::mem::replace(&mut conn.queued, true) {
                self.active.push(i);
            }
        }
    }

    /// Drop the connections whose peer has taken nothing for `WRITE_STALL`.
    fn sweep(&mut self) {
        for i in 0..self.conns.len() {
            let stuck = self.conns[i]
                .as_ref()
                .and_then(|conn| conn.stalled)
                .is_some_and(|since| since.elapsed() >= WRITE_STALL);
            if stuck {
                self.close(i, Some("the peer stopped reading".into()));
            }
        }
    }
}

/// How long a reply may wait on a reader that has stopped reading.
const WRITE_STALL: Duration = Duration::from_secs(60);
/// How often an idle loop wakes to look for stalled peers and for `stop`.
const SWEEP: Duration = Duration::from_secs(1);
/// One connection's share of a round: at most this many commands, and no
/// further command once their arguments pass this many bytes.
const PIPELINE_COMMANDS: usize = 64;
const PIPELINE_BYTES: usize = 4 * 1024 * 1024;
/// A whole round. Its `SET`s are one transaction, so these bound how long
/// it holds the writer lock. What is over stays for the next round.
const ROUND_COMMANDS: usize = 512;
const ROUND_BYTES: usize = 8 * 1024 * 1024;
/// Bytes read from one connection per round.
const READ_ROUND: usize = 1024 * 1024;
/// A connection that owes this many reply bytes is not read from.
const FLUSH_AT: usize = 256 * 1024;

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

/// One connection's place under `max_connections`, given back on drop.
struct Seat(Arc<State>);

impl Drop for Seat {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::Relaxed);
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
