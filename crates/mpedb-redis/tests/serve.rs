//! One real socket. The command tests cover the replies; this one checks
//! that a client can pipeline two commands and that a second process cannot
//! take the same data directory.

use mpedb_redis::{now_ms, run, Cluster, ServeConfig, State};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn dir() -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = mpedb_testkit::scratch_base().join(format!(
        "mpedb-redis-serve-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn config(path: std::path::PathBuf, listen: std::net::SocketAddr) -> ServeConfig {
    let mut cfg = ServeConfig::single(listen, path, 1, "none".into());
    cfg.cleanup_interval = Duration::ZERO;
    cfg.store.size_mb = 8;
    cfg.store.max_value_bytes = 1024 * 1024;
    cfg
}

#[test]
fn a_client_can_pipeline_set_and_get() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let state = State::open(config(dir(), addr)).unwrap();
    let stop = Arc::clone(&state);
    let server = thread::spawn(move || run(listener, state).unwrap());

    let mut sock = TcpStream::connect(addr).unwrap();
    sock.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nhello\r\n*2\r\n$3\r\nGET\r\n$1\r\nk\r\n")
        .unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 64];
    let mut got = Vec::new();
    while got.len() < b"+OK\r\n$5\r\nhello\r\n".len() {
        let n = sock.read(&mut buf).unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, b"+OK\r\n$5\r\nhello\r\n");
    drop(sock);

    let err = State::open(config(stop.store.dir().to_path_buf(), addr));
    let message = err.err().expect("a second open must be refused");
    assert!(message.contains("another mpedb-redis"), "{message}");

    stop.stop.store(true, Ordering::Relaxed);
    let _ = TcpStream::connect(addr);
    server.join().unwrap();
    let _ = now_ms();
    let _ = Cluster::single("127.0.0.1:1");
}
