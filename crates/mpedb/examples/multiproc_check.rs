//! Multi-process crash safety, for a platform where it is not a formality.
//!
//! The single-process version of this proves little: threads share a memory
//! model the compiler already reasoned about. The claim mpedb actually makes is
//! that SEPARATE PROCESSES map the same file, and any of them may be SIGKILLed
//! between any two instructions without corrupting it. On x86-64 a missing
//! fence usually hides — the hardware is TSO. On ARM it does not.
//!
//! Children write in a loop; the parent kills one at an arbitrary instant, then
//! reopens and verifies. `verify()` walks the page accounting, so a torn commit
//! or a leaked page is a failure, not a shrug.
//!
//! Usage: multiproc_check <dir> [children] [seconds] [kills]

use mpedb::{params, Config, Database, ExecResult, Value};

fn config(path: &std::path::Path) -> Config {
    Config::from_toml_str(&format!(
        r#"
[database]
path = "{}"
size_mb = 48
max_readers = 32
durability = "none"

[[table]]
name = "t"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "seq"
  type = "int64"

  [[table.column]]
  name = "mirror"
  type = "int64"
"#,
        path.display()
    ))
    .unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(a.get(1).cloned().unwrap_or_else(|| "/dev/shm".into()));
    let nkids: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let secs: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
    let nkills: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(3);

    println!("arch: {} ({}-bit usize), {nkids} children, {nkills} kill waves",
             std::env::consts::ARCH, usize::BITS);

    let path = dir.join(format!("mpc-{}.mpedb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let db = Database::open_with_config(config(&path)).unwrap();
        for id in 0..64i64 {
            db.query("INSERT INTO t (id, seq, mirror) VALUES ($1, 0, 0)", &params![id])
                .unwrap();
        }
    }

    let mut torn_total = 0u64;
    for wave in 0..nkills {
        let mut kids = Vec::new();
        for k in 0..nkids {
            // SAFETY: fork in a single-threaded parent; each child only opens
            // the database and exits via _exit, touching no inherited locks.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                child(&path, k as i64, secs);
                unsafe { libc::_exit(0) };
            }
            kids.push(pid);
        }

        // Kill one mid-flight. The point is the INSTANT: no cooperation, no
        // unwinding, no chance to release the writer lock it may be holding.
        std::thread::sleep(std::time::Duration::from_millis(300 + (wave as u64) * 137 % 400));
        let victim = kids[wave % kids.len()];
        unsafe { libc::kill(victim, libc::SIGKILL) };

        for pid in kids {
            let mut st = 0i32;
            unsafe { libc::waitpid(pid, &mut st, 0) };
        }

        // Reopen: this is where a dead writer's lock has to be recovered
        // (EOWNERDEAD / the flock path), and where a half-written meta would
        // surface.
        let db = Database::open_with_config(config(&path)).unwrap();
        // Every row was written seq==mirror in one transaction. A reader that
        // sees them differ saw a commit that half-happened.
        if let Ok(ExecResult::Rows { rows, .. }) = db.query("SELECT seq, mirror FROM t", &[]) {
            for r in &rows {
                if let [Value::Int(s), Value::Int(m)] = r[..] {
                    if s != m {
                        torn_total += 1;
                    }
                }
            }
        }
        db.verify()
            .unwrap_or_else(|e| panic!("wave {wave}: verify failed after SIGKILL: {e}"));
        println!("  wave {wave}: killed pid {victim} mid-write, reopened, verify OK");
    }

    println!("torn rows after {nkills} kill waves: {torn_total}");
    let _ = std::fs::remove_file(&path);
    assert_eq!(torn_total, 0, "a SIGKILL left a half-applied commit visible");
    println!("PASS");
}

fn child(path: &std::path::Path, k: i64, secs: u64) {
    let Ok(db) = Database::open_with_config(config(path)) else {
        return;
    };
    let t0 = std::time::Instant::now();
    let mut n = k * 1_000_000;
    while t0.elapsed().as_secs() < secs {
        n += 1;
        let id = n % 64;
        // seq and mirror move together or not at all.
        let _ = db.query(
            "UPDATE t SET seq = $1, mirror = $1 WHERE id = $2",
            &params![n, id],
        );
        let _ = db.query("SELECT seq, mirror FROM t WHERE id = $1", &params![id]);
    }
}
