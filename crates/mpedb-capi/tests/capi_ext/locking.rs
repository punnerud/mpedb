use super::*;
use super::helpers::*;

/// A SIBLING connection on the same thread that needs the writer lock while
/// this one holds it gets sqlite's BUSY ("database is locked"), not an
/// "internal error (bug in mpedb)" — and the busy-timeout retry clears it.
#[test]
fn sibling_connection_write_is_busy_not_internal() {
    unsafe {
        let path = "/tmp/mpedb-capi-busy-sibling.mpedb";
        let _ = std::fs::remove_file(path);
        let name = cs(path);
        let (mut a, mut b): (*mut Sqlite3, *mut Sqlite3) = (ptr::null_mut(), ptr::null_mut());
        assert_eq!(sqlite3_open(name.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(sqlite3_open(name.as_ptr(), &mut b), SQLITE_OK);
        assert_eq!(exec(a, "create table t (a INTEGER PRIMARY KEY, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(a, "insert into t(b) values('x')"), SQLITE_OK);
        // b: BEGIN needs the writer lock a holds -> BUSY, sqlite's message
        assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY);
        let msg = CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy().into_owned();
        assert_eq!(msg, "database is locked");
        // commit on a, then b proceeds
        assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
        assert_eq!(exec(b, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(b, "insert into t(b) values('y')"), SQLITE_OK);
        assert_eq!(exec(b, "COMMIT"), SQLITE_OK);
        sqlite3_close(a);
        sqlite3_close(b);
        let _ = std::fs::remove_file(path);
    }
}


/// #109 (compat gap E1): `sqlite3_busy_timeout` bounds the ENGINE's
/// writer-lock wait. A contended write answers SQLITE_BUSY *at the deadline*
/// — with elapsed-time evidence — and timeout 0 answers immediately; it
/// never blocks forever. Exercises both the interactive path (BEGIN →
/// `Database::begin`) and autocommit DML (`run_write_plan`'s bounded direct
/// path). Same-thread sibling connections contend on the same single writer
/// lock the cross-process case uses (the two-process arm lives in
/// `crates/mpedb/tests/busy_timeout.rs` and the CPython MultiprocessTests).
#[test]
fn busy_timeout_bounds_writer_lock_wait() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    let path = format!("/tmp/mpedb-capi-busy-deadline-{}.mpedb", std::process::id());
    let _ = std::fs::remove_file(&path);
    unsafe {
        // Seed the file + table from the main thread.
        let name = cs(&path);
        let mut seed: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(name.as_ptr(), &mut seed), SQLITE_OK);
        assert_eq!(exec(seed, "create table t (a INTEGER PRIMARY KEY, b TEXT)"), SQLITE_OK);
        sqlite3_close(seed);
    }

    // Holder THREAD with its own connection (the ERRORCHECK mutex is
    // per-thread-owned, so this contends exactly like a second process):
    // BEGIN + INSERT, signal, hold ~1.5 s, COMMIT.
    let held = Arc::new(AtomicBool::new(false));
    let holder = {
        let (path, held) = (path.clone(), held.clone());
        std::thread::spawn(move || unsafe {
            let name = cs(&path);
            let mut a: *mut Sqlite3 = ptr::null_mut();
            assert_eq!(sqlite3_open(name.as_ptr(), &mut a), SQLITE_OK);
            assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
            assert_eq!(exec(a, "insert into t(b) values('x')"), SQLITE_OK);
            held.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
            sqlite3_close(a);
        })
    };
    let t0 = Instant::now();
    while !held.load(std::sync::atomic::Ordering::Acquire) {
        assert!(t0.elapsed() < Duration::from_secs(10), "holder never signalled");
        std::thread::sleep(Duration::from_millis(2));
    }

    unsafe {
        let name = cs(&path);
        let mut b: *mut Sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(name.as_ptr(), &mut b), SQLITE_OK);

        // timeout 0 (sqlite's default, the connection's initial state):
        // one immediate attempt, immediate BUSY.
        let t0 = Instant::now();
        assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY);
        let dt = t0.elapsed();
        assert!(dt < Duration::from_millis(300), "timeout 0 was not immediate: {dt:?}");
        let msg = CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy().into_owned();
        assert_eq!(msg, "database is locked");

        // timeout 200 ms: BUSY only after the timeout has genuinely elapsed.
        assert_eq!(sqlite3_busy_timeout(b, 200), SQLITE_OK);
        let t0 = Instant::now();
        assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY);
        let dt = t0.elapsed();
        assert!(dt >= Duration::from_millis(200), "BUSY before the timeout: {dt:?}");
        assert!(dt < Duration::from_millis(1400), "waited far past the timeout: {dt:?}");
        let msg = CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy().into_owned();
        assert_eq!(msg, "database is locked");

        // Autocommit DML takes the same bounded wait.
        let t0 = Instant::now();
        assert_eq!(exec(b, "insert into t(b) values('y')"), SQLITE_BUSY);
        let dt = t0.elapsed();
        assert!(dt >= Duration::from_millis(200), "BUSY before the timeout: {dt:?}");
        assert!(dt < Duration::from_millis(1400), "waited far past the timeout: {dt:?}");

        // A timeout LONGER than the holder's remaining grip: the bounded
        // poll must ACQUIRE once the holder commits — the wait clears
        // instead of running to its deadline.
        assert_eq!(sqlite3_busy_timeout(b, 5000), SQLITE_OK);
        let t0 = Instant::now();
        assert_eq!(exec(b, "BEGIN"), SQLITE_OK);
        let dt = t0.elapsed();
        assert!(dt < Duration::from_millis(4000), "acquire should beat the deadline: {dt:?}");
        assert_eq!(exec(b, "insert into t(b) values('z')"), SQLITE_OK);
        assert_eq!(exec(b, "COMMIT"), SQLITE_OK);
        sqlite3_close(b);
    }
    holder.join().unwrap();
    let _ = std::fs::remove_file(&path);
}


/// Same-THREAD sibling contention is an unwinnable wait (the lock's owner is
/// the caller's own thread — it cannot release while we poll), so a busy
/// timeout is deliberately NOT burned down: immediate BUSY, sqlite's message.
#[test]
// glibc-only: Apple's libpthread answers EBUSY (not EDEADLK) for an
// owner's errorcheck trylock (rdar://16261552), so on macOS the reentry
// fold is dead and a same-thread sibling waits out its full deadline —
// bounded, just not immediate.
#[cfg_attr(target_os = "macos", ignore = "EBUSY-on-relock: no immediate fold on macOS")]
fn busy_timeout_same_thread_sibling_is_immediate() {
    use std::time::{Duration, Instant};
    unsafe {
        let path = format!("/tmp/mpedb-capi-busy-sibling-timeout-{}.mpedb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let name = cs(&path);
        let (mut a, mut b): (*mut Sqlite3, *mut Sqlite3) = (ptr::null_mut(), ptr::null_mut());
        assert_eq!(sqlite3_open(name.as_ptr(), &mut a), SQLITE_OK);
        assert_eq!(sqlite3_open(name.as_ptr(), &mut b), SQLITE_OK);
        assert_eq!(exec(a, "create table t (a INTEGER PRIMARY KEY, b TEXT)"), SQLITE_OK);
        assert_eq!(exec(a, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(a, "insert into t(b) values('x')"), SQLITE_OK);

        assert_eq!(sqlite3_busy_timeout(b, 2000), SQLITE_OK);
        let t0 = Instant::now();
        assert_eq!(exec(b, "BEGIN"), SQLITE_BUSY);
        let dt = t0.elapsed();
        assert!(dt < Duration::from_millis(500), "same-thread BUSY should not wait: {dt:?}");
        let msg = CStr::from_ptr(sqlite3_errmsg(b)).to_string_lossy().into_owned();
        assert_eq!(msg, "database is locked");

        assert_eq!(exec(a, "COMMIT"), SQLITE_OK);
        assert_eq!(exec(b, "BEGIN"), SQLITE_OK);
        assert_eq!(exec(b, "insert into t(b) values('y')"), SQLITE_OK);
        assert_eq!(exec(b, "COMMIT"), SQLITE_OK);
        sqlite3_close(a);
        sqlite3_close(b);
        let _ = std::fs::remove_file(&path);
    }
}
