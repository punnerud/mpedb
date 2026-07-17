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
//!     or partial row is detectable,
//!  4. replays a sample of index point-probes on `a` (which is `indexed =
//!     true` in both modes) against the full scan's own filter: `verify()`
//!     checks page accounting and can NEVER catch table↔index divergence —
//!     the two trees are each internally consistent — so the probe-vs-scan
//!     comparison (the stress `unique` mode's pattern) is the only check
//!     that can.
//!
//! Table: `rows(id int64 pk, a int64 indexed, b int64, check_sum int64)`.
//!
//! `--blob-kb N` (default 0 = off) mixes ~20% blob ops into the writer loop:
//! the schema gains nullable `data blob` + `blob_seq int64` columns and
//! children write N-KiB values whose bytes derive deterministically from
//! (id, blob_seq) via xorshift — any process can recompute the expected
//! content. The parent (after every wave) and the reader children byte-compare
//! each surviving blob against that recomputation: a torn chain, a
//! cross-wired chain (another row's bytes), or an old/new page mix (another
//! generation's bytes) all fail loudly where page accounting sees nothing.
//! Suggested N: 64; above 256 a single blob write can dominate the 5-60 ms
//! kill window and starve the small-txn/index code paths this harness also
//! exists for.
//!
//! HONESTY, `--durability commit|wal`: blob parameters exceed the intent
//! ring's `RING_PARAMS_CAP` (824 B), so blob ops always take the direct
//! writer-lock fallback. `--blob-kb` exercises overflow chains under SIGKILL
//! (and WAL replay), NOT "the ring with blobs" — only the small ops still
//! ride the ring.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mpedb::{params, Database, Error, ExecResult, Value};
use mpedb_core::Engine;

use crate::args;
use crate::stress::{exit_db_full, EXIT_CAPACITY};
use crate::util::{
    fill_bytes, runtime, usage, write_config_concurrency, CliResult, Failure, Rng, Watchdog,
};

const KEYSPACE: u64 = 200;
const WAVE_TIMEOUT_SECS: u64 = 30;

/// `a` is `indexed = true` (a non-unique secondary index, tree 1 — 0 is the
/// PK tree): every update rewrites its index entry, so each wave exercises
/// index maintenance under SIGKILL, and the parent's probe-vs-scan check is
/// what verifies the two trees still agree.
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
  indexed = true

  [[table.column]]
  name = "b"
  type = "int64"
  nullable = false

  [[table.column]]
  name = "check_sum"
  type = "int64"
  nullable = false
"#;

/// [`CRASH_TOML`] + the blob columns (`--blob-kb` > 0). `data`/`blob_seq` are
/// written by ONE statement, so a committed row has both set or both NULL, and
/// the expected bytes are `crash_blob(id, blob_seq, kb)` — recomputable by any
/// process. Appended after `check_sum` so columns 0..=3 keep their positions,
/// and neither is indexed, so `a` stays index 1 in both modes.
const CRASH_BLOB_TOML: &str = r#"[[table]]
name = "rows"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "a"
  type = "int64"
  nullable = false
  indexed = true

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

/// The N-KiB blob every process expects in row `id` at write-generation
/// `seq`: pure xorshift from (id, seq, kb). Torn chain → garbage bytes;
/// cross-wired chain → another id's bytes; old/new page mix → another
/// generation's bytes. All three fail the compare; none fail `verify()`.
fn crash_blob(id: i64, seq: i64, kb: u64) -> Vec<u8> {
    let mut rng = Rng::seeded(&[id as u64, seq as u64, kb]);
    fill_bytes(&mut rng, (kb * 1024) as usize)
}

/// Check the `(data, blob_seq)` pair of one row. `Ok(true)` = a blob is
/// present and byte-identical to the recomputation, `Ok(false)` = both NULL.
fn check_blob_cols(id: i64, data: &Value, seq: &Value, kb: u64) -> Result<bool, String> {
    match (data, seq) {
        (Value::Null, Value::Null) => Ok(false),
        (Value::Blob(got), Value::Int(s)) => {
            let want = crash_blob(id, *s, kb);
            if *got == want {
                Ok(true)
            } else {
                let diff = got.iter().zip(&want).position(|(a, b)| a != b);
                Err(format!(
                    "id={id} blob_seq={s}: blob content mismatch (len {} vs expected {}, \
                     first differing byte at {diff:?}) — torn or cross-wired overflow chain",
                    got.len(),
                    want.len()
                ))
            }
        }
        _ => Err(format!(
            "id={id}: data/blob_seq inconsistent (one NULL, one set) — they are \
             written by a single statement and must change together"
        )),
    }
}

// ------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["dir", "waves", "children", "durability", "concurrency", "size_mb", "blob-kb", "extent-kb"],
        &[],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let waves = p.require_u64("waves")?;
    let n_children = p.require_u64("children")?;
    if waves == 0 || n_children == 0 {
        return usage("--waves and --children must be >= 1");
    }
    let blob_kb = p.u64_or("blob-kb", 0)?;
    // DESIGN-BLOBEXTENT §13.3: with a threshold the blob ops become extent
    // ops, so the SIGKILL waves exercise the run allocator + map + reclaim.
    // The children read it from the CONFIG file, not from argv.
    let extent_kb = match p.u64_or("extent-kb", 0)? {
        0 => None,
        kb => Some(kb),
    };
    if blob_kb > 256 {
        eprintln!(
            "crash: note: --blob-kb {blob_kb} > 256 — a single blob write can dominate \
             the 5-60ms kill window and starve the small-txn/index code paths"
        );
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
    // Blob mode raises the default: every blob write COWs kb/4 fresh pages, so
    // blob churn eats a 64 MB file fast, and DbFull capacity-exits (reported
    // apart, per #38) must not drown the correctness signal.
    let size_mb = match p.value("size_mb") {
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| Failure::Usage("--size_mb must be an integer".into()))?,
        None if blob_kb > 0 => 256,
        None => 64,
    };
    let tables = if blob_kb > 0 { CRASH_BLOB_TOML } else { CRASH_TOML };
    write_config_concurrency(&cfg, &dbf, size_mb, tables, &durability, &concurrency, extent_kb)?;

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
    let mut total_capacity = 0u64;

    for wave in 0..waves {
        let _wd = Watchdog::arm(WAVE_TIMEOUT_SECS, &format!("crash wave {wave}"));

        let mut children = Vec::new();
        for k in 0..n_children {
            let child = Command::new(&exe)
                .arg("crash-child")
                .arg("--dir")
                .arg(&dir)
                .args(["--id", &k.to_string(), "--wave", &wave.to_string()])
                .args(["--blob-kb", &blob_kb.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?;
            children.push(child);
        }

        let mut killed = 0u64;
        let mut capacity = 0u64;
        let mut unexpected = 0u64;
        for (k, mut child) in children.into_iter().enumerate() {
            let status = child.wait()?;
            use std::os::unix::process::ExitStatusExt;
            if status.signal() == Some(libc::SIGKILL) {
                killed += 1;
            } else if status.code() == Some(EXIT_CAPACITY) {
                // Capacity, not correctness (#38): the child filled the file
                // before its kill landed. Counted apart and reported at the
                // end; the wave's invariant checks below still run and mean
                // exactly what they always mean.
                capacity += 1;
            } else {
                // Children never exit voluntarily; anything but SIGKILL (or a
                // capacity exit) means a child hit an unexpected error first.
                unexpected += 1;
                eprintln!("wave {wave}: child {k} did not die by SIGKILL: {status}");
            }
        }
        total_killed += killed;
        total_capacity += capacity;

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
        let mut blobs = 0u64;
        let mut by_a: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        let mut cursor = r.scan(0, None, None)?;
        while let Some(row) = cursor.next()? {
            let (id, a, b, cs) = (int(&row[0])?, int(&row[1])?, int(&row[2])?, int(&row[3])?);
            if a + b != 0 || cs != id {
                return runtime(format!(
                    "CRASH INVARIANT VIOLATION in wave {wave}: \
                     row id={id} a={a} b={b} check_sum={cs} (want a+b=0, check_sum=id) — torn write"
                ));
            }
            by_a.entry(a).or_default().push(id); // PK scan ⇒ ids arrive ascending
            if blob_kb > 0 {
                match check_blob_cols(id, &row[4], &row[5], blob_kb) {
                    Ok(present) => blobs += u64::from(present),
                    Err(msg) => {
                        return runtime(format!(
                            "CRASH BLOB INVARIANT VIOLATION in wave {wave}: {msg}"
                        ))
                    }
                }
            }
            rows += 1;
        }
        drop(cursor);

        // Table↔index agreement (stress `unique`'s probe-vs-scan pattern):
        // for a sample of `a` values — up to 16 present, plus two that were
        // never written — the index point-probe must return exactly the ids
        // the full scan's own filter says. `verify_page_accounting` can never
        // see this divergence; this comparison is the only thing that can.
        // Same ReadTxn as the scan, so both sides see one snapshot.
        let step = (by_a.len() / 16).max(1);
        let probes: Vec<i64> = by_a
            .keys()
            .copied()
            .step_by(step)
            .take(16)
            .chain([-1, 1_000_007]) // a ∈ 0..1_000_000 — these must probe empty
            .collect();
        for v in probes {
            let got: Vec<i64> = r
                .scan_by_index(0, 1, &Value::Int(v))? // index 1 = column `a`
                .iter()
                .map(|row| int(&row[0]))
                .collect::<Result<_, _>>()?;
            let want = by_a.get(&v).map(Vec::as_slice).unwrap_or(&[]);
            if got != want {
                return runtime(format!(
                    "INDEX DIVERGENCE in wave {wave}: scan_by_index(a={v}) returned ids \
                     {got:?} but the full-scan filter says {want:?} — the row tree and \
                     the secondary index disagree"
                ));
            }
        }
        r.finish()?;

        let blob_note = if blob_kb > 0 { format!(" blobs={blobs}") } else { String::new() };
        let cap_note = if capacity > 0 { format!(" capacity-exits={capacity}") } else { String::new() };
        println!(
            "wave {wave}: children={n_children} killed={killed}{cap_note} \
             eowner_recovery={recovered} lock_wait={}us rows={rows}{blob_note} \
             index-probe=ok verify=ok",
            lock_wait.as_micros()
        );
        if unexpected > 0 {
            return runtime(format!(
                "wave {wave}: {unexpected} child(ren) exited abnormally (not SIGKILL)"
            ));
        }
    }

    // Capacity apart from correctness (#38): the waves above all verified, but
    // a child that fills the file stops exercising kills — say which it was.
    if total_capacity > 0 {
        eprintln!(
            "OUT OF SPACE: {total_capacity} child-run(s) hit DbFull before their kill \
             landed. CAPACITY, not correctness — every wave above still verified \
             (killed={total_killed}, EOWNERDEAD recoveries={recoveries}). Raise \
             --size_mb (now {size_mb}); with --blob-kb {blob_kb} every blob write \
             COWs ~{} fresh pages.",
            (blob_kb * 1024).div_ceil(4096).max(1)
        );
        return runtime(format!(
            "{total_capacity} child(ren) ran out of space (exit {EXIT_CAPACITY}); \
             raise --size_mb (now {size_mb})"
        ));
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
    let p = args::parse(argv, &["dir", "id", "wave", "blob-kb"], &[])?;
    let dir = PathBuf::from(p.require("dir")?);
    let id = p.require_u64("id")?;
    let wave = p.require_u64("wave")?;
    let blob_kb = p.u64_or("blob-kb", 0)?;

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
        reader_loop(&db, blob_kb)
    } else {
        writer_loop(&db, &mut rng, blob_kb)
    }
}

fn reader_loop(db: &Database, blob_kb: u64) -> CliResult {
    let sel = db.prepare(if blob_kb > 0 {
        "SELECT id, a, b, check_sum, data, blob_seq FROM rows"
    } else {
        "SELECT id, a, b, check_sum FROM rows"
    })?;
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
            // Blob content is snapshot-stable too: whatever generation this
            // snapshot sees, its bytes must match that generation exactly.
            if blob_kb > 0 {
                if let Err(msg) = check_blob_cols(id, &row[4], &row[5], blob_kb) {
                    eprintln!("CRASH READER BLOB INVARIANT VIOLATION: {msg}");
                    std::process::exit(3);
                }
            }
        }
    }
}

fn writer_loop(db: &Database, rng: &mut Rng, blob_kb: u64) -> CliResult {
    // Prepared BEFORE any session (facade locking rule); prepare itself opens
    // short write txns, so kills land in registry publication too.
    let upd = db.prepare("UPDATE rows SET a = $1, b = $2 WHERE id = $3")?;
    let ins = db.prepare("INSERT INTO rows (id, a, b, check_sum) VALUES ($1, $2, $3, $4)")?;
    let del = db.prepare("DELETE FROM rows WHERE id = $1")?;
    // Blob statements (--blob-kb > 0). Their params blow RING_PARAMS_CAP, so
    // under durability commit|wal these take the direct writer-lock fallback,
    // never the ring — see the module doc.
    let blob_stmts = if blob_kb > 0 {
        Some((
            db.prepare("UPDATE rows SET data = $1, blob_seq = $2 WHERE id = $3")?,
            db.prepare("UPDATE rows SET data = NULL, blob_seq = NULL WHERE id = $1")?,
            db.prepare(
                "INSERT INTO rows (id, a, b, check_sum, data, blob_seq) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )?,
        ))
    } else {
        None
    };
    loop {
        let key = rng.below(KEYSPACE) as i64;
        let a = rng.below(1_000_000) as i64;
        let op = rng.below(10);
        let outcome: Result<(), Error> = match (&blob_stmts, op) {
            // ~20% blob ops in blob mode: build / free / rewrite overflow
            // chains right inside the kill window.
            (Some((wr, _, _)), 8) => {
                let seq = rng.below(1 << 30) as i64;
                db.execute(wr, &params![crash_blob(key, seq, blob_kb), seq, key]).map(|_| ())
            }
            (Some((_, clr, insb)), 9) => {
                if rng.below(2) == 0 {
                    // Clear: frees the chain, rewrites the row inline.
                    db.execute(clr, &params![key]).map(|_| ())
                } else {
                    // Fresh row born WITH a chain (usually loses the PK race —
                    // that path is expected; after a delete it lands).
                    let seq = rng.below(1 << 30) as i64;
                    db.execute(
                        insb,
                        &params![key, a, -a, key, crash_blob(key, seq, blob_kb), seq],
                    )
                    .map(|_| ())
                }
            }
            // Mostly small autocommit updates: fast txns, dense commit windows.
            // (In blob mode 0..=3, i.e. 40%; without blobs 0..=5 as always.)
            (Some(_), 0..=3) | (None, 0..=5) => {
                db.execute(&upd, &params![a, -a, key]).map(|_| ())
            }
            // Two-row session txn: a wider window with the writer lock held.
            (Some(_), 4..=5) | (None, 6..=7) => {
                let key2 = rng.below(KEYSPACE) as i64;
                let a2 = rng.below(1_000_000) as i64;
                (|| -> Result<(), Error> {
                    let mut s = db.begin()?;
                    s.execute(&upd, &params![a, -a, key])?;
                    s.execute(&upd, &params![a2, -a2, key2])?;
                    s.commit().map(|_| ())
                })()
            }
            // Churn: delete + reinsert exercises freelist reuse mid-kill (and,
            // in blob mode, frees whole overflow chains).
            (Some(_), 6) | (None, 8) => db.execute(&del, &params![key]).map(|_| ()),
            _ => db.execute(&ins, &params![key, a, -a, key]).map(|_| ()),
        };
        match outcome {
            Ok(()) => {}
            // Losing an insert race on a live key is expected.
            Err(Error::PrimaryKeyViolation { .. }) => {}
            // Capacity, not correctness (#38): exit apart so the parent can say so.
            Err(e) if matches!(e, Error::DbFull) => exit_db_full("crash writer", &e),
            Err(e) => return runtime(format!("crash writer: unexpected error: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blob verifier must fail on anything but the exact recomputation —
    /// a checker that cannot fail proves nothing about the waves it blesses.
    #[test]
    fn blob_check_catches_every_corruption_shape() {
        let ok = crash_blob(7, 42, 4);
        assert_eq!(ok.len(), 4096);
        assert!(check_blob_cols(7, &Value::Blob(ok.clone()), &Value::Int(42), 4).unwrap());
        assert!(!check_blob_cols(7, &Value::Null, &Value::Null, 4).unwrap());
        // torn chain: one flipped byte
        let mut torn = ok;
        torn[1234] ^= 1;
        assert!(check_blob_cols(7, &Value::Blob(torn), &Value::Int(42), 4).is_err());
        // cross-wired chain: another row's bytes
        let other_row = crash_blob(8, 42, 4);
        assert!(check_blob_cols(7, &Value::Blob(other_row), &Value::Int(42), 4).is_err());
        // old/new page mix: another write-generation's bytes
        let stale = crash_blob(7, 41, 4);
        assert!(check_blob_cols(7, &Value::Blob(stale), &Value::Int(42), 4).is_err());
        // half-applied pair: data and blob_seq are written by one statement
        assert!(check_blob_cols(7, &Value::Null, &Value::Int(42), 4).is_err());
    }
}
