//! `mpedb powerloss --dir D [--rounds N] [--workers W] [--durability wal|async]`
//! — WAL-class power-loss simulation (DESIGN.md §5.4). `wal` (durable-on-ack)
//! and `async` (deferred fsync, §5.4.2) share the same recovery machinery and
//! the same crash-consistency invariant: a torn tail truncates whole records,
//! so any surviving prefix conserves the workload invariants. `async` simply
//! has a larger legal loss window (un-flushed commits), which the random-offset
//! truncation below models exactly. Default: wal.
//!
//! Each round: format+seed a WAL-class database, snapshot the main file (S0),
//! run a multi-process bank/rows workload, SIGKILL every worker at a random
//! instant, then reconstruct a worst-case power-loss disk image:
//!
//! 1. the main file is rolled back to S0 (as if the kernel wrote back
//!    nothing after the snapshot — legal, because the checkpoint threshold is
//!    raised so no full-mapping msync runs during the round),
//! 2. the WAL is truncated at a RANDOM byte offset within the worker tail
//!    (simulating a torn append; the offset floor is S0's durable `wal_len`,
//!    below which bytes had already been fdatasync'd and survive real power
//!    loss),
//! 3. one byte of the stored boot id is flipped, which is exactly what a
//!    reboot looks like to `post_attach` — the next open takes the init
//!    flock and runs WAL recovery before anything else.
//!
//! The reopened database must recover to SOME committed prefix, exactly:
//! - bank: all 100 accounts present, balances summing to precisely 100 000
//!   (every transfer is one commit record — a torn batch must vanish
//!   entirely, so any surviving prefix conserves the sum);
//! - rows: every row satisfies a + b = 0 and check_sum = id (the autocommit
//!   workers ride the intent ring, so this checks batch-record atomicity);
//! - page accounting verifies.
//!
//! Repeats `--rounds` times (default 20).

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use mpedb::{params, Database, Error, ExecResult, Value};
use mpedb_core::shm::{wal_path, BOOT_ID_FILE_OFFSET, WAL_LEN_FILE_OFFSET};

use crate::args;
use crate::util::{runtime, usage, write_config_durable, CliResult, Failure, Rng, Watchdog};

const BANK_ACCOUNTS: i64 = 100;
const BANK_TOTAL: i64 = 100_000;
const ROWS_KEYSPACE: u64 = 200;
const ROUND_TIMEOUT_SECS: u64 = 120;

const POWERLOSS_TOML: &str = r#"[[table]]
name = "accounts"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "balance"
  type = "int64"
  nullable = false

[[table]]
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

fn read_u64_at(path: &Path, offset: u64) -> Result<u64, Failure> {
    let f = std::fs::File::open(path)?;
    let mut buf = [0u8; 8];
    f.read_exact_at(&mut buf, offset)?;
    Ok(u64::from_le_bytes(buf))
}

// ------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["dir", "rounds", "workers", "durability"], &[])?;
    let dir = PathBuf::from(p.require("dir")?);
    let rounds = p.u64_or("rounds", 20)?;
    let workers = p.u64_or("workers", 6)?;
    // WAL-class only: `wal` (durable-on-ack) or `async` (deferred fsync). Both
    // recover to a crash-consistent prefix after a torn tail — the invariant
    // checked below is identical; `async` merely has a larger legal loss
    // window (commits appended but not yet flushed), which a truncated tail
    // models exactly.
    let durability = p.value("durability").unwrap_or("wal").to_owned();
    if !matches!(durability.as_str(), "wal" | "async") {
        return usage("--durability must be wal or async (the WAL-class modes)");
    }
    if rounds == 0 || workers == 0 {
        return usage("--rounds and --workers must be >= 1");
    }

    // The round's simulated stale main file is the post-seed snapshot; that
    // is a legal power-loss disk image ONLY if no checkpoint (full-mapping
    // MS_SYNC) runs during the round — after a checkpoint the real disk would
    // hold at least the checkpointed state. Raise the threshold out of reach;
    // children inherit the parent's environment.
    std::env::set_var("MPEDB_WAL_CKPT_BYTES", u64::MAX.to_string());

    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let cfg = dir.join("config.toml");
    let dbf = dir.join("powerloss.mpedb");
    let s0 = dir.join("powerloss.s0");
    let exe = std::env::current_exe()?;

    let mut total_truncated = 0u64;
    for round in 0..rounds {
        let _wd = Watchdog::arm(ROUND_TIMEOUT_SECS, &format!("powerloss round {round}"));
        let mut rng = Rng::seeded(&[round, u64::from(std::process::id())]);

        // 1. fresh wal-mode database, seeded
        let _ = std::fs::remove_file(&dbf);
        let _ = std::fs::remove_file(wal_path(&dbf));
        write_config_durable(&cfg, &dbf, 32, POWERLOSS_TOML, &durability)?;
        {
            let db = Database::open(&cfg)?;
            let ins_a = db.prepare("INSERT INTO accounts (id, balance) VALUES ($1, $2)")?;
            let ins_r =
                db.prepare("INSERT INTO rows (id, a, b, check_sum) VALUES ($1, $2, $3, $4)")?;
            let mut s = db.begin()?;
            for i in 0..BANK_ACCOUNTS {
                s.execute(&ins_a, &params![i, BANK_TOTAL / BANK_ACCOUNTS])?;
            }
            for i in 0..ROWS_KEYSPACE as i64 {
                s.execute(&ins_r, &params![i, 0i64, 0i64, i])?;
            }
            s.commit()?;
        } // handle dropped: nothing attached while we snapshot

        // 2. S0 = the main file as of "the last writeback before power loss"
        std::fs::copy(&dbf, &s0)?;
        // bytes below S0's wal_len were fdatasync'd and survive power loss
        let floor = read_u64_at(&s0, WAL_LEN_FILE_OFFSET)?;

        // 3. workload, then SIGKILL every worker at a random instant
        let mut children = Vec::new();
        for k in 0..workers {
            let child = Command::new(&exe)
                .arg("powerloss-child")
                .arg("--dir")
                .arg(&dir)
                .args(["--id", &k.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?;
            children.push(child);
        }
        std::thread::sleep(Duration::from_millis(100 + rng.below(500)));
        for child in &mut children {
            let _ = child.kill(); // SIGKILL
        }
        let mut unexpected = 0u64;
        for mut child in children {
            let status = child.wait()?;
            use std::os::unix::process::ExitStatusExt;
            if status.signal() != Some(libc::SIGKILL) {
                unexpected += 1;
                eprintln!("round {round}: worker did not die by SIGKILL: {status}");
            }
        }
        if unexpected > 0 {
            return runtime(format!(
                "round {round}: {unexpected} worker(s) hit an error before the kill"
            ));
        }

        // 4. reconstruct the power-loss disk image
        std::fs::copy(&s0, &dbf)?; // stale main file
        let walf = std::fs::OpenOptions::new()
            .write(true)
            .open(wal_path(&dbf))?;
        let wal_size = walf.metadata()?.len();
        let cut = floor + rng.below(wal_size - floor + 1); // ∈ [floor, size]
        walf.set_len(cut)?; // torn tail
        drop(walf);
        total_truncated += (wal_size - cut > 0) as u64;
        {
            // a different boot id is what makes post_attach run recovery
            let f = std::fs::OpenOptions::new().write(true).read(true).open(&dbf)?;
            let mut b = [0u8; 1];
            f.read_exact_at(&mut b, BOOT_ID_FILE_OFFSET)?;
            b[0] ^= 0xFF;
            f.write_all_at(&b, BOOT_ID_FILE_OFFSET)?;
        }

        // 5. reopen (runs WAL recovery under the init flock) and verify
        let db = Database::open(&cfg).map_err(|e| {
            Failure::Runtime(format!("round {round}: recovery failed to open: {e}"))
        })?;
        verify_round(&db, round)?;
        db.verify()
            .map_err(|e| Failure::Runtime(format!("round {round}: page accounting: {e}")))?;
        println!(
            "round {round}: workers={workers} wal_size={wal_size} cut={cut} \
             (floor={floor}) verify=ok"
        );
    }
    println!(
        "powerloss[{durability}]: rounds={rounds} workers/round={workers} \
         rounds-with-truncated-tail={total_truncated} — all invariants held"
    );
    let _ = std::fs::remove_file(&s0);
    Ok(())
}

fn verify_round(db: &Database, round: u64) -> CliResult {
    // bank: the sum is conserved by EVERY committed prefix (each transfer is
    // one commit record); a torn batch must be entirely absent
    let ExecResult::Rows { rows, .. } = db.query("SELECT balance FROM accounts", &[])? else {
        return runtime("powerloss: expected rows");
    };
    if rows.len() as i64 != BANK_ACCOUNTS {
        return runtime(format!(
            "round {round}: {} accounts after recovery, want {BANK_ACCOUNTS}",
            rows.len()
        ));
    }
    let mut sum = 0i64;
    for row in &rows {
        sum += int(&row[0])?;
    }
    if sum != BANK_TOTAL {
        return runtime(format!(
            "round {round}: BANK SUM VIOLATION after recovery: {sum} != {BANK_TOTAL} — \
             a commit was applied partially"
        ));
    }
    // rows: per-row invariant written by the ring-riding autocommit workers
    let ExecResult::Rows { rows, .. } = db.query("SELECT id, a, b, check_sum FROM rows", &[])?
    else {
        return runtime("powerloss: expected rows");
    };
    if rows.len() as u64 != ROWS_KEYSPACE {
        return runtime(format!(
            "round {round}: {} rows after recovery, want {ROWS_KEYSPACE}",
            rows.len()
        ));
    }
    for row in &rows {
        let (id, a, b, cs) = (int(&row[0])?, int(&row[1])?, int(&row[2])?, int(&row[3])?);
        if a + b != 0 || cs != id {
            return runtime(format!(
                "round {round}: ROW INVARIANT VIOLATION after recovery: \
                 id={id} a={a} b={b} check_sum={cs} — torn record application"
            ));
        }
    }
    Ok(())
}

fn int(v: &Value) -> Result<i64, Failure> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(Failure::Runtime(format!("expected int, got {other}"))),
    }
}

// -------------------------------------------------------------------- child

/// Hidden subcommand: hammer the database until SIGKILLed by the parent.
/// Even ids run session bank transfers (multi-statement commits through the
/// direct path); odd ids run autocommit row updates (the intent-ring path).
pub fn run_child(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["dir", "id"], &[])?;
    let dir = PathBuf::from(p.require("dir")?);
    let id = p.require_u64("id")?;
    let db = Database::open(&dir.join("config.toml"))?;
    let mut rng = Rng::seeded(&[id, u64::from(std::process::id())]);

    if id % 2 == 0 {
        let sel = db.prepare("SELECT balance FROM accounts WHERE id = $1")?;
        let upd = db.prepare("UPDATE accounts SET balance = $1 WHERE id = $2")?;
        loop {
            let a = rng.below(BANK_ACCOUNTS as u64) as i64;
            let b = (a + 1 + rng.below(BANK_ACCOUNTS as u64 - 1) as i64) % BANK_ACCOUNTS;
            let amount = 1 + rng.below(50) as i64;
            let mut s = db.begin()?;
            let bal_a = one_int(s.execute(&sel, &params![a])?)?;
            let bal_b = one_int(s.execute(&sel, &params![b])?)?;
            s.execute(&upd, &params![bal_a - amount, a])?;
            s.execute(&upd, &params![bal_b + amount, b])?;
            s.commit()?;
        }
    } else {
        let upd = db.prepare("UPDATE rows SET a = $1, b = $2 WHERE id = $3")?;
        loop {
            let key = rng.below(ROWS_KEYSPACE) as i64;
            let x = rng.below(1_000_000) as i64;
            match db.execute(&upd, &params![x, -x, key]) {
                Ok(_) => {}
                Err(Error::PrimaryKeyViolation { .. }) => {}
                Err(e) => return runtime(format!("powerloss child: unexpected error: {e}")),
            }
        }
    }
}

fn one_int(res: ExecResult) -> Result<i64, Failure> {
    match res {
        ExecResult::Rows { rows, .. } if rows.len() == 1 => int(&rows[0][0]),
        other => Err(Failure::Runtime(format!(
            "expected exactly one row, got {other:?}"
        ))),
    }
}
