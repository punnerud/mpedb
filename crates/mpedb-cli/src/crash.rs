//! `mpedb crash --dir D --waves W --children C` — SIGKILL crash injection.
//!
//! The core promise under test: a process SIGKILLed at ANY instant (attach,
//! plan publication, mid-commit, holding the writer lock, holding a reader
//! pin) corrupts nothing and wedges nothing.
//!
//! Each wave spawns C `crash-child` processes doing continuous small write
//! transactions (a couple of them scan-read instead). Every child arms a
//! thread that sleeps 5..60 ms — armed BEFORE attach, so kills also land in
//! the attach window — then SIGKILLs its own process; children always die
//! mid-work by design. The parent then:
//!  1. opens the file fresh and asserts `begin_write` succeeds promptly
//!     (robust-mutex EOWNERDEAD recovery, no wedge — a watchdog turns a hang
//!     into a loud failure),
//!  2. runs the page-accounting verifier,
//!  3. scans the table and checks the per-row checksum invariant: children
//!     only ever write rows with `b = -a` and `check_sum = id`, so any torn
//!     or partial row is detectable.
//!
//! Table: `rows(id int64 pk, a int64, b int64, check_sum int64)`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mpedb::{params, Database, Error, ExecResult, Value};
use mpedb_core::Engine;

use crate::args;
use crate::util::{runtime, usage, write_config_concurrency, CliResult, Failure, Rng, Watchdog};

const KEYSPACE: u64 = 200;
const WAVE_TIMEOUT_SECS: u64 = 30;

const CRASH_TOML: &str = r#"[[table]]
name = "rows"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "a"
  type = "int64"
  nullable = false

  [[table.column]]
  name = "b"
  type = "int64"
  nullable = false

  [[table.column]]
  name = "check_sum"
  type = "int64"
  nullable = false
"#;

// ------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["dir", "waves", "children", "durability", "concurrency"],
        &["size_mb"],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let waves = p.require_u64("waves")?;
    let n_children = p.require_u64("children")?;
    if waves == 0 || n_children == 0 {
        return usage("--waves and --children must be >= 1");
    }
    let concurrency = p.value("concurrency").unwrap_or("serial").to_owned();
    if !matches!(concurrency.as_str(), "serial" | "optimistic") {
        return usage("--concurrency must be serial or optimistic");
    }

    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let cfg = dir.join("config.toml");
    let dbf = dir.join("crash.mpedb");
    let _ = std::fs::remove_file(&dbf);
    let durability = p.value("durability").unwrap_or("none").to_owned();
    if !matches!(durability.as_str(), "none" | "commit" | "async" | "wal") {
        return usage("--durability must be none, commit, async or wal");
    }
    // Fast machines (e.g. M3) can churn enough COW pages inside one 5-60ms kill
    // window to exhaust a small DB before SIGKILL lands (DbFull, not a lock bug).
    // Allow --size_mb to grow the file so recovery is observed, not masked.
    let size_mb = match p.value("size_mb") {
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| Failure::Usage("--size_mb must be an integer".into()))?,
        None => 64,
    };
    write_config_concurrency(&cfg, &dbf, size_mb, CRASH_TOML, &durability, &concurrency)?;

    // Create + seed so children (and readers) always find the full keyspace.
    {
        let db = Database::open(&cfg)?;
        let ins = db.prepare("INSERT INTO rows (id, a, b, check_sum) VALUES ($1, $2, $3, $4)")?;
        let mut s = db.begin()?;
        for i in 0..KEYSPACE as i64 {
            s.execute(&ins, &params![i, 0i64, 0i64, i])?;
        }
        s.commit()?;
    } // handle dropped: the parent holds nothing across waves

    let exe = std::env::current_exe()?;
    let mut recoveries = 0u64;
    let mut total_killed = 0u64;

    for wave in 0..waves {
        let _wd = Watchdog::arm(WAVE_TIMEOUT_SECS, &format!("crash wave {wave}"));

        let mut children = Vec::new();
        for k in 0..n_children {
            let child = Command::new(&exe)
                .arg("crash-child")
                .arg("--dir")
                .arg(&dir)
                .args(["--id", &k.to_string(), "--wave", &wave.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?;
            children.push(child);
        }

        let mut killed = 0u64;
        let mut unexpected = 0u64;
        for (k, mut child) in children.into_iter().enumerate() {
            let status = child.wait()?;
            use std::os::unix::process::ExitStatusExt;
            if status.signal() == Some(libc::SIGKILL) {
                killed += 1;
            } else {
                // Children never exit voluntarily; anything but SIGKILL means
                // a child hit an unexpected error (exit 1/3/4) first.
                unexpected += 1;
                eprintln!("wave {wave}: child {k} did not die by SIGKILL: {status}");
            }
        }
        total_killed += killed;

        // --- recovery: fresh handle, prompt writer lock, verify, invariants.
        let t0 = Instant::now();
        let eng = Engine::open_from_file(&dbf)?;
        let w = eng.begin_write()?;
        let recovered = w.recovered;
        w.abort();
        let lock_wait = t0.elapsed();
        if recovered {
            recoveries += 1;
        }

        eng.verify_page_accounting()?;

        let r = eng.begin_read()?;
        let mut rows = 0u64;
        let mut cursor = r.scan(0, None, None)?;
        while let Some(row) = cursor.next()? {
            let (id, a, b, cs) = (int(&row[0])?, int(&row[1])?, int(&row[2])?, int(&row[3])?);
            if a + b != 0 || cs != id {
                return runtime(format!(
                    "CRASH INVARIANT VIOLATION in wave {wave}: \
                     row id={id} a={a} b={b} check_sum={cs} (want a+b=0, check_sum=id) — torn write"
                ));
            }
            rows += 1;
        }
        r.finish()?;

        println!(
            "wave {wave}: children={n_children} killed={killed} \
             eowner_recovery={recovered} lock_wait={}us rows={rows} verify=ok",
            lock_wait.as_micros()
        );
        if unexpected > 0 {
            return runtime(format!(
                "wave {wave}: {unexpected} child(ren) exited abnormally (not SIGKILL)"
            ));
        }
    }

    println!(
        "crash: waves={waves} children/wave={n_children} killed={total_killed} \
         parent-observed EOWNERDEAD recoveries={recoveries} — all invariants held"
    );
    Ok(())
}

fn int(v: &Value) -> Result<i64, Failure> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(Failure::Runtime(format!("expected int, got {other}"))),
    }
}

// -------------------------------------------------------------------- child

/// Hidden subcommand: attach and hammer small write txns until the kill
/// thread SIGKILLs us (5..60 ms, armed before attach). Exit 3 = a reader
/// observed an invariant violation; any voluntary exit is reported by the
/// parent as a failure.
pub fn run_child(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["dir", "id", "wave"], &[])?;
    let dir = PathBuf::from(p.require("dir")?);
    let id = p.require_u64("id")?;
    let wave = p.require_u64("wave")?;

    // Deterministic-ish per (wave, child): the kill spread is what walks the
    // kill point across attach / prepare / commit windows over the waves.
    let mut rng = Rng::seeded(&[wave, id]);
    let kill_ms = 5 + rng.below(56);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(kill_ms));
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    });

    let db = Database::open(&dir.join("config.toml"))?;
    if id % 4 == 3 {
        reader_loop(&db)
    } else {
        writer_loop(&db, &mut rng)
    }
}

fn reader_loop(db: &Database) -> CliResult {
    let sel = db.prepare("SELECT id, a, b, check_sum FROM rows")?;
    loop {
        let rows = match db.execute(&sel, &[]) {
            Ok(ExecResult::Rows { rows, .. }) => rows,
            Ok(other) => return runtime(format!("crash reader: unexpected {other:?}")),
            Err(Error::SnapshotEvicted) => continue,
            Err(e) => return Err(e.into()),
        };
        for row in &rows {
            let (id, a, b, cs) = (int(&row[0])?, int(&row[1])?, int(&row[2])?, int(&row[3])?);
            if a + b != 0 || cs != id {
                eprintln!(
                    "CRASH READER INVARIANT VIOLATION: id={id} a={a} b={b} check_sum={cs}"
                );
                std::process::exit(3);
            }
        }
    }
}

fn writer_loop(db: &Database, rng: &mut Rng) -> CliResult {
    // Prepared BEFORE any session (facade locking rule); prepare itself opens
    // short write txns, so kills land in registry publication too.
    let upd = db.prepare("UPDATE rows SET a = $1, b = $2 WHERE id = $3")?;
    let ins = db.prepare("INSERT INTO rows (id, a, b, check_sum) VALUES ($1, $2, $3, $4)")?;
    let del = db.prepare("DELETE FROM rows WHERE id = $1")?;
    loop {
        let key = rng.below(KEYSPACE) as i64;
        let a = rng.below(1_000_000) as i64;
        let outcome = match rng.below(10) {
            // Mostly small autocommit updates: fast txns, dense commit windows.
            0..=5 => db.execute(&upd, &params![a, -a, key]).map(|_| ()),
            // Two-row session txn: a wider window with the writer lock held.
            6..=7 => {
                let key2 = rng.below(KEYSPACE) as i64;
                let a2 = rng.below(1_000_000) as i64;
                let mut s = db.begin()?;
                s.execute(&upd, &params![a, -a, key])?;
                s.execute(&upd, &params![a2, -a2, key2])?;
                s.commit().map(|_| ())
            }
            // Churn: delete + reinsert exercises freelist reuse mid-kill.
            8 => db.execute(&del, &params![key]).map(|_| ()),
            _ => db.execute(&ins, &params![key, a, -a, key]).map(|_| ()),
        };
        match outcome {
            Ok(()) => {}
            // Losing an insert race on a live key is expected.
            Err(Error::PrimaryKeyViolation { .. }) => {}
            Err(e) => return runtime(format!("crash writer: unexpected error: {e}")),
        }
    }
}
