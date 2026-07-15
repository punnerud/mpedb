//! **N real PROCESSES writing one file** — mpedb vs SQLite, both native Rust.
//!
//! The suite's `contended-writes` cell is `std::thread::scope`: four THREADS in
//! one process. That measures lock contention inside a process, and it is not
//! the claim mpedb actually makes. "Many processes writing one file, no server"
//! needs many processes, so this forks.
//!
//! Both arms are native and do the same work — the first version of this
//! measured Rust-mpedb against a PYTHON sqlite3 script, which is not a
//! comparison, it is a handicap.
//!
//! SQLite gets its best case: WAL (the only journal mode where a reader may run
//! beside the writer) and a 60 s `busy_timeout`. The timeout is not a courtesy —
//! without it every loser of a write race gets `SQLITE_BUSY` and the run dies.
//! That asymmetry IS the result: mpedb's arm has no retry path because there is
//! nothing to retry.
//!
//! Usage: mp_writes <dir> <procs> <secs> <engine: mpedb|sqlite> [durability]
//!   durability: none|commit|wal for mpedb; mapped to OFF|FULL|NORMAL for sqlite

use std::path::Path;
use std::time::{Duration, Instant};

/// Peak memory of the calling process, in KiB: (VmHWM, RssAnon).
///
/// Two numbers because one would lie. **VmHWM** counts every resident page,
/// including the database file mpedb has mmapped — pages that, for SQLite, live
/// in the OS page cache and are charged to nobody. Comparing those directly
/// would say mpedb uses hundreds of MB more, when what it really says is that
/// mpedb's data is visible in its RSS and SQLite's is hiding in the kernel.
///
/// **RssAnon** is the honest column: heap and stack, the memory the engine
/// actually asked for. That is comparable.
fn peak_mem_kib() -> (u64, u64) {
    let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let get = |k: &str| -> u64 {
        st.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    (get("VmHWM:"), get("RssAnon:"))
}

/// Children report their peak memory through a file the parent sums: peak RSS
/// is per-process, and `getrusage(RUSAGE_CHILDREN)` reports the MAX child, not
/// the total — which for "what does running N writers cost" is the wrong number.
fn report_mem(dir: &Path, tag: &str) {
    let (hwm, anon) = peak_mem_kib();
    let _ = std::fs::write(
        dir.join(format!("mem-{tag}-{}.txt", std::process::id())),
        format!("{hwm} {anon}"),
    );
}

fn collect_mem(dir: &Path, tag: &str) -> (u64, u64) {
    let (mut hwm, mut anon) = (0u64, 0u64);
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if !n.starts_with(&format!("mem-{tag}-")) {
                continue;
            }
            if let Ok(s) = std::fs::read_to_string(e.path()) {
                let mut it = s.split_whitespace();
                hwm += it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                anon += it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            }
            let _ = std::fs::remove_file(e.path());
        }
    }
    (hwm, anon)
}

fn mpedb_cfg(path: &Path, dur: &str) -> mpedb::Config {
    mpedb::Config::from_toml_str(&format!(
        r#"
[database]
path = "{}"
size_mb = 256
max_readers = 64
durability = "{dur}"

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"

  [[table.column]]
  name = "age"
  type = "int64"
"#,
        path.display()
    ))
    .unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&a[1]);
    let nproc: usize = a[2].parse().unwrap();
    let secs: f64 = a[3].parse().unwrap();
    let engine = a[4].clone();
    let dur = a.get(5).cloned().unwrap_or_else(|| "none".into());
    std::fs::create_dir_all(&dir).unwrap();

    let _ = collect_mem(&dir, &engine); // clear any stale reports
    let (rows, el, _busy) = match engine.as_str() {
        "mpedb" => run_mpedb(&dir, nproc, secs, &dur),
        "sqlite" => run_sqlite(&dir, nproc, secs, &dur),
        other => panic!("unknown engine {other}"),
    };
    let (hwm, anon) = collect_mem(&dir, &engine);
    // rows/s, then peak memory summed across the writer processes.
    println!(
        "{rows} {el:.3} {:.0} hwm_kib={hwm} anon_kib={anon}",
        rows as f64 / el
    );
}

fn run_mpedb(dir: &Path, nproc: usize, secs: f64, dur: &str) -> (i64, f64, u64) {
    use mpedb::{params, Database, ExecResult, Value};
    let path = dir.join(format!("mpw-{}.mpedb", std::process::id()));
    for s in ["", "-wal"] {
        let _ = std::fs::remove_file(format!("{}{s}", path.display()));
    }
    drop(Database::open_with_config(mpedb_cfg(&path, dur)).unwrap());

    let t0 = Instant::now();
    let mut pids = Vec::new();
    for k in 0..nproc {
        // SAFETY: forked from a single-threaded parent; the child only opens the
        // database and leaves via _exit.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let db = Database::open_with_config(mpedb_cfg(&path, dur)).unwrap();
            let ins = db
                .prepare("INSERT INTO t (id, email, age) VALUES ($1, $2, $3)")
                .unwrap();
            let base = (k as i64) * 10_000_000;
            let mut n = 0i64;
            let c0 = Instant::now();
            while c0.elapsed().as_secs_f64() < secs {
                // No retry loop: there is nothing to retry.
                db.execute(&ins, &params![base + n, format!("u{}@x", base + n), n % 90])
                    .unwrap();
                n += 1;
            }
            report_mem(dir, "mpedb");
            unsafe { libc::_exit(0) };
        }
        pids.push(pid);
    }
    for p in pids {
        let mut st = 0i32;
        unsafe { libc::waitpid(p, &mut st, 0) };
    }
    let el = t0.elapsed().as_secs_f64();
    let db = Database::open_with_config(mpedb_cfg(&path, dur)).unwrap();
    let rows = match db.query("SELECT count(*) FROM t", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            _ => 0,
        },
        _ => 0,
    };
    db.verify().expect("verify after N processes wrote it");
    for s in ["", "-wal"] {
        let _ = std::fs::remove_file(format!("{}{s}", path.display()));
    }
    (rows, el, 0)
}

fn run_sqlite(dir: &Path, nproc: usize, secs: f64, dur: &str) -> (i64, f64, u64) {
    use rusqlite::Connection;
    let path = dir.join(format!("mpw-{}.db", std::process::id()));
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{s}", path.display()));
    }
    let sync = match dur {
        "none" => "OFF",
        "wal" | "async" => "NORMAL",
        _ => "FULL",
    };
    {
        let c = Connection::open(&path).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT, age INTEGER) STRICT;",
        )
        .unwrap();
    }

    let t0 = Instant::now();
    let mut pids = Vec::new();
    for k in 0..nproc {
        // SAFETY: as above.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let c = Connection::open(&path).unwrap();
            c.busy_timeout(Duration::from_secs(60)).unwrap();
            c.pragma_update(None, "journal_mode", "WAL").unwrap();
            c.pragma_update(None, "synchronous", sync).unwrap();
            let base = (k as i64) * 10_000_000;
            let mut n = 0i64;
            let c0 = Instant::now();
            while c0.elapsed().as_secs_f64() < secs {
                let mut st = c
                    .prepare_cached("INSERT INTO t (id,email,age) VALUES (?1,?2,?3)")
                    .unwrap();
                // The retry path mpedb's arm does not have.
                match st.execute(rusqlite::params![base + n, format!("u{}@x", base + n), n % 90]) {
                    Ok(_) => n += 1,
                    Err(_) => continue,
                }
            }
            report_mem(dir, "sqlite");
            unsafe { libc::_exit(0) };
        }
        pids.push(pid);
    }
    for p in pids {
        let mut st = 0i32;
        unsafe { libc::waitpid(p, &mut st, 0) };
    }
    let el = t0.elapsed().as_secs_f64();
    let c = Connection::open(&path).unwrap();
    let rows: i64 = c
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    drop(c);
    for s in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{s}", path.display()));
    }
    (rows, el, 0)
}
