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
//! - rows also carries a nullable 8-16 KiB blob (`data` + its `blob_seq`
//!   write-generation, both set by ONE statement): ~20% of the autocommit
//!   worker's ops rewrite it, so the torn-tail truncation sweep lands inside
//!   multi-page records — overflow chains through the WAL SPLIT encoding.
//!   Blob params exceed the intent ring's 824 B cap, so those commits take
//!   the direct writer-lock path (the small updates still ride the ring).
//!   After recovery every surviving blob is recomputed from (id, blob_seq)
//!   and byte-compared — page accounting cannot see content corruption in a
//!   half-replayed chain;
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
use crate::stress::{exit_db_full, EXIT_CAPACITY};
use crate::util::{
    fill_bytes, runtime, usage, write_config_durable, CliResult, Failure, Rng, Watchdog,
};

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

  [[table.column]]
  name = "data"
  type = "blob"
  nullable = true

  [[table.column]]
  name = "blob_seq"
  type = "int64"
  nullable = true
"#;

/// The 8-16 KiB blob every process expects in row `id` at write-generation
/// `seq`: length AND content derive from (id, seq) via xorshift, so recovery
/// verification recomputes the exact bytes. 8-16 KiB is a 3-5 page overflow
/// chain — the multi-page WAL records the torn-tail sweep is aimed at.
fn powerloss_blob(id: i64, seq: i64) -> Vec<u8> {
    let mut rng = Rng::seeded(&[id as u64, seq as u64]);
    let len = (8 + rng.below(9)) * 1024;
    fill_bytes(&mut rng, len as usize)
}

fn read_u64_at(path: &Path, offset: u64) -> Result<u64, Failure> {
    let f = std::fs::File::open(path)?;
    let mut buf = [0u8; 8];
    f.read_exact_at(&mut buf, offset)?;
    Ok(u64::from_le_bytes(buf))
}

// ------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["dir", "rounds", "workers", "durability", "extent-kb"], &[])?;
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
        // 64 MB (was 32): the blob workload churns 8-16 KiB overflow chains,
        // and DbFull mid-round would abort workers before the kill.
        let extent_kb = match p.u64_or("extent-kb", 0)? {
            0 => None,
            kb => Some(kb),
        };
        // Extents raise the size: in `async` the durable-frontier gate (§6)
        // holds freed runs unreusable until the flusher has flushed the
        // freeing record, so reuse latency is flusher-shaped and the file
        // must absorb churn-rate × flush-interval of un-reclaimable runs.
        // Bounded working set, deferred reclaim — the documented price of
        // the gate, not a leak.
        let size_mb = if extent_kb.is_some() { 256 } else { 64 };
        write_config_durable(&cfg, &dbf, size_mb, POWERLOSS_TOML, &durability, extent_kb)?;
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

        // 3. workload, then SIGKILL every worker at a random instant.
        // With extents on, workers log every payload range whose msync
        // RETURNED — the loss model must not erase what real power loss
        // would keep (DESIGN-BLOBEXTENT §4: the range-msyncs ARE the
        // pre-record durability; rolling them back with S0 fabricates a
        // failure mode real hardware does not have).
        let sync_log = dir.join("extent-sync.log");
        let _ = std::fs::remove_file(&sync_log);
        let mut children = Vec::new();
        for k in 0..workers {
            let child = Command::new(&exe)
                .arg("powerloss-child")
                .arg("--dir")
                .arg(&dir)
                .args(["--id", &k.to_string()])
                .env("MPEDB_EXTENT_SYNC_LOG", &sync_log)
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
        let mut out_of_space = 0u64;
        for mut child in children {
            let status = child.wait()?;
            use std::os::unix::process::ExitStatusExt;
            if status.signal() != Some(libc::SIGKILL) {
                if status.code() == Some(EXIT_CAPACITY) {
                    out_of_space += 1; // capacity, not correctness (#38)
                } else {
                    unexpected += 1;
                    eprintln!("round {round}: worker did not die by SIGKILL: {status}");
                }
            }
        }
        if unexpected > 0 {
            return runtime(format!(
                "round {round}: {unexpected} worker(s) hit an error before the kill"
            ));
        }
        if out_of_space > 0 {
            // Capacity, not correctness (#38). With extents in `async` this
            // is EXPECTED under churn: the durable-frontier gate (§6) holds
            // freed runs unreusable until the flusher passes, so a worker can
            // outrun the file. Its commits are valid; the round's cut +
            // recovery + verify still test everything that was written.
            eprintln!(
                "round {round}: note: {out_of_space} worker(s) exited on DbFull \
                 (capacity — expected under async extent churn)"
            );
        }

        // The cut floor is the durable watermark AT THE KILL — every byte
        // below the last fdatasync survives real power loss, in both modes
        // (`wal` advances it per commit, `async` per flusher pass). The old
        // floor (S0's start-of-round wal_len) modeled losses real hardware
        // cannot produce; that was harmless while the WAL carried everything
        // (page images replay at any prefix) and became visible the day
        // extents put durable bytes OUTSIDE the log: a cut between a durable
        // free-record and the reuse it licensed is exactly the impossible
        // state the durable-frontier gate exists to rule out on hardware.
        let floor = read_u64_at(&dbf, WAL_LEN_FILE_OFFSET)?;

        // 4. reconstruct the power-loss disk image
        // Extent ranges whose msync returned survive real power loss: carry
        // their CURRENT bytes across the S0 rollback. (A range synced more
        // than once carries its last synced bytes — extents are immutable
        // once published, and an unpublished overwrite of a reused run is
        // itself durable by the time its record exists, so last-wins is the
        // faithful model.)
        let preserved: Vec<(u64, u64)> = match std::fs::read(&sync_log) {
            Ok(bytes) => bytes
                .chunks_exact(12)
                .map(|ch| {
                    (
                        u64::from_le_bytes(ch[0..8].try_into().unwrap()),
                        u64::from(u32::from_le_bytes(ch[8..12].try_into().unwrap())),
                    )
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let kept: Vec<(u64, Vec<u8>)> = if preserved.is_empty() {
            Vec::new()
        } else {
            let cur = std::fs::OpenOptions::new().read(true).open(&dbf)?;
            let mut kept = Vec::with_capacity(preserved.len());
            for &(start, npages) in &preserved {
                let mut buf = vec![0u8; (npages * 4096) as usize];
                cur.read_exact_at(&mut buf, start * 4096)?;
                kept.push((start * 4096, buf));
            }
            kept
        };
        std::fs::copy(&s0, &dbf)?; // stale main file
        if !kept.is_empty() {
            let dst = std::fs::OpenOptions::new().write(true).open(&dbf)?;
            for (off, buf) in &kept {
                dst.write_all_at(buf, *off)?;
            }
        }
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
    // rows: per-row invariant written by the ring-riding autocommit workers,
    // plus the blob content check for the direct-path multi-page records
    let ExecResult::Rows { rows, .. } =
        db.query("SELECT id, a, b, check_sum, data, blob_seq FROM rows", &[])?
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
        // Blob writes set (data, blob_seq) in one statement, so a recovered
        // row has both or neither, and the bytes must be exactly the (id,
        // blob_seq) recomputation — a half-replayed overflow chain passes
        // page accounting but cannot pass this compare.
        match (&row[4], &row[5]) {
            (Value::Null, Value::Null) => {}
            (Value::Blob(got), Value::Int(seq)) => {
                let want = powerloss_blob(id, *seq);
                if *got != want {
                    let diff = got.iter().zip(&want).position(|(x, y)| x != y);
                    return runtime(format!(
                        "round {round}: BLOB CONTENT VIOLATION after recovery: id={id} \
                         blob_seq={seq} (len {} vs expected {}, first differing byte at \
                         {diff:?}) — a torn overflow chain survived replay",
                        got.len(),
                        want.len()
                    ));
                }
            }
            _ => {
                return runtime(format!(
                    "round {round}: data/blob_seq inconsistent after recovery for id={id} \
                     — they are written by one statement and must change together"
                ))
            }
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
            let step = || -> Result<(), Error> {
                let mut s = db.begin()?;
                let bal_a = one_int_e(s.execute(&sel, &params![a])?)?;
                let bal_b = one_int_e(s.execute(&sel, &params![b])?)?;
                s.execute(&upd, &params![bal_a - amount, a])?;
                s.execute(&upd, &params![bal_b + amount, b])?;
                s.commit()
            };
            match step() {
                Ok(()) => {}
                // Capacity, not correctness (#38): the bank rows are tiny,
                // but their PAGE allocations hit the same full file when
                // extent churn outruns the async flusher's reclaim.
                Err(e) if matches!(e, Error::DbFull) => exit_db_full("powerloss", &e),
                Err(e) => return runtime(format!("powerloss child: unexpected error: {e}")),
            }
        }
    } else {
        let upd = db.prepare("UPDATE rows SET a = $1, b = $2 WHERE id = $3")?;
        let upd_blob = db.prepare("UPDATE rows SET data = $1, blob_seq = $2 WHERE id = $3")?;
        loop {
            let key = rng.below(ROWS_KEYSPACE) as i64;
            // ~20% blob rewrites: 8-16 KiB, multi-page — the records the
            // torn-tail sweep is aimed at. Their params exceed the ring cap,
            // so these commits take the direct writer-lock path; the small
            // updates below still ride the intent ring.
            let res = if rng.below(5) == 0 {
                let seq = rng.below(1 << 30) as i64;
                db.execute(&upd_blob, &params![powerloss_blob(key, seq), seq, key])
            } else {
                let x = rng.below(1_000_000) as i64;
                db.execute(&upd, &params![x, -x, key])
            };
            match res {
                Ok(_) => {}
                Err(Error::PrimaryKeyViolation { .. }) => {}
                // Capacity, not correctness (#38): its own exit code.
                Err(e) if matches!(e, Error::DbFull) => exit_db_full("powerloss", &e),
                Err(e) => return runtime(format!("powerloss child: unexpected error: {e}")),
            }
        }
    }
}

fn one_int_e(res: ExecResult) -> Result<i64, Error> {
    match res {
        ExecResult::Rows { mut rows, .. } if rows.len() == 1 => match rows.pop().unwrap().pop() {
            Some(Value::Int(v)) => Ok(v),
            other => Err(Error::Internal(format!("expected int, got {other:?}"))),
        },
        other => Err(Error::Internal(format!("expected one row, got {other:?}"))),
    }
}

