//! N real PROCESSES inserting into one database file, for as long as you say.
//!
//! The benchmark suite's `contended-writes` cell is `std::thread::scope` — four
//! THREADS in one process. That measures lock contention, and it is not the
//! claim the README wants to make. "Many processes writing one file" needs many
//! processes, so this forks.
//!
//! Usage: mp_writes <dir> <procs> <secs> <durability>

use mpedb::{params, Config, Database, ExecResult, Value};

fn cfg(path: &std::path::Path, dur: &str) -> Config {
    Config::from_toml_str(&format!(
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
    let dur = a.get(4).cloned().unwrap_or_else(|| "none".into());

    let path = dir.join(format!("mpw-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    drop(Database::open_with_config(cfg(&path, &dur)).unwrap());

    let t0 = std::time::Instant::now();
    let mut pids = Vec::new();
    for k in 0..nproc {
        // SAFETY: forked from a single-threaded parent; the child only opens
        // the database and leaves via _exit.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let db = Database::open_with_config(cfg(&path, &dur)).unwrap();
            let ins = db
                .prepare("INSERT INTO t (id, email, age) VALUES ($1, $2, $3)")
                .unwrap();
            let base = (k as i64) * 10_000_000;
            let mut n = 0i64;
            let c0 = std::time::Instant::now();
            while c0.elapsed().as_secs_f64() < secs {
                // No retry loop, no busy handling: there is nothing to handle.
                db.execute(&ins, &params![base + n, format!("u{}@x", base + n), n % 90])
                    .unwrap();
                n += 1;
            }
            unsafe { libc::_exit(0) };
        }
        pids.push(pid);
    }
    for p in pids {
        let mut st = 0i32;
        unsafe { libc::waitpid(p, &mut st, 0) };
    }
    let el = t0.elapsed().as_secs_f64();

    let db = Database::open_with_config(cfg(&path, &dur)).unwrap();
    let rows = match db.query("SELECT count(*) FROM t", &[]).unwrap() {
        ExecResult::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            _ => 0,
        },
        _ => 0,
    };
    db.verify().expect("verify after N processes wrote it");
    let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let get = |k: &str| -> u64 {
        st.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    println!(
        "{rows} {el:.3} {:.0} parent_hwm_kib={} parent_anon_kib={}",
        rows as f64 / el,
        get("VmHWM:"),
        get("RssAnon:")
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
}
