//! `mpedb stress --dir D --workers N --secs S --mode bank|unique|mixed`
//!
//! Multi-process stress test (DESIGN.md §10.2). The parent writes a config +
//! database under `--dir`, seeds it, respawns itself N times as the hidden
//! `stress-child` subcommand, then runs mode-specific invariant checks plus
//! `Database::verify()` (page accounting).
//!
//! Modes:
//! - `bank`: transfer txns between 100 accounts (writers) while readers
//!   full-scan snapshots and assert the sum is conserved — any deviation is
//!   an MVCC bug and the child exits 3.
//! - `unique`: children race INSERTs of the same 500-email set with distinct
//!   ids; Unique/PrimaryKey violations are the EXPECTED outcome of losing a
//!   race, anything else fails. The parent re-verifies uniqueness by index
//!   probe and recounts rows against the children's success totals.
//! - `mixed`: random INSERT/UPDATE/DELETE/SELECT over a constrained key
//!   space; correctness = no unexpected errors + final verify.
//!
//! **Capacity is not correctness.** A child that fills the file exits
//! `EXIT_CAPACITY` (4) and is counted and reported apart from one that hit an
//! unexpected error (1) or an invariant violation (3). This is not tidiness:
//! bug #37 was reported here for ten days as "8 child(ren) failed" — which reads
//! like a torn snapshot — and nobody looked, because the message said the wrong
//! thing. `--size_mb` exists for the same reason; #37's "doubling the file
//! doubles the survivable time" table had to be produced by editing the source.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mpedb::{params, Database, Error, ExecResult, Value};

use crate::args;
use crate::util::{runtime, usage, CliResult, Failure, Rng, Watchdog};

const BANK_ACCOUNTS: i64 = 100;
const BANK_TOTAL: i64 = 100_000;
const UNIQUE_EMAILS: i64 = 500;
const MIXED_KEYSPACE: u64 = 1000;
const INCR_KEYSPACE: i64 = 64;

const BANK_TOML: &str = r#"[[table]]
name = "accounts"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "balance"
  type = "int64"
  nullable = false
"#;

const UNIQUE_TOML: &str = r#"[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true
"#;

const MIXED_TOML: &str = r#"[[table]]
name = "items"
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
  type = "text"
  nullable = false
"#;

// `incr`: the autocommit conservation invariant for the optimistic path.
// Bank uses interactive sessions (which bypass the autocommit optimistic
// route), so it cannot exercise optimistic writers; `incr` does — every
// success is a single autocommit `UPDATE ctr SET v = v + 1 WHERE id = $1`, and
// the committed sum of v must equal the total successful increments across all
// children. A lost update (an unsound footprint check) breaks conservation.
const INCR_TOML: &str = r#"[[table]]
name = "ctr"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "v"
  type = "int64"
  nullable = false
"#;

// ------------------------------------------------------------------- parent

#[derive(Default)]
struct Totals {
    ops: u64,
    ok: u64,
    conflicts: u64,
}

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["dir", "workers", "secs", "mode", "durability", "concurrency", "size_mb", "extent-kb"],
        &[],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let workers = p.require_u64("workers")?;
    let secs = p.require_u64("secs")?;
    let mode = p.require("mode")?.to_owned();
    if !matches!(mode.as_str(), "bank" | "unique" | "mixed" | "incr") {
        return usage("--mode must be bank, unique, mixed or incr");
    }
    let durability = p.value("durability").unwrap_or("none").to_owned();
    if !matches!(durability.as_str(), "none" | "commit" | "async" | "wal") {
        return usage("--durability must be none, commit, async or wal");
    }
    let concurrency = p.value("concurrency").unwrap_or("serial").to_owned();
    if !matches!(concurrency.as_str(), "serial" | "optimistic") {
        return usage("--concurrency must be serial or optimistic");
    }
    if workers == 0 || secs == 0 {
        return usage("--workers and --secs must be >= 1");
    }
    // Was hardcoded to 64. It needs to be a knob: the capacity path cannot be
    // exercised without one, and #37's "doubling the file exactly doubles the
    // survivable time" table had to be produced by editing this line.
    let size_mb = match p.value("size_mb") {
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| Failure::Usage("--size_mb must be a number".into()))?,
        None => 64,
    };
    if size_mb == 0 {
        return usage("--size_mb must be >= 1");
    }

    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let cfg = dir.join("config.toml");
    let dbf = dir.join("stress.mpedb");
    let _ = std::fs::remove_file(&dbf); // a stale db may have another schema

    let tables = match mode.as_str() {
        "bank" => BANK_TOML,
        "unique" => UNIQUE_TOML,
        "incr" => INCR_TOML,
        _ => MIXED_TOML,
    };
    let extent_kb = match p.u64_or("extent-kb", 0)? {
        0 => None,
        kb => Some(kb),
    };
    crate::util::write_config_concurrency(&cfg, &dbf, size_mb, tables, &durability, &concurrency, extent_kb)?;

    let db = Database::open(&cfg)?;
    seed(&db, &mode)?;

    // Children get secs to run + generous slack before we call it a wedge.
    let _wd = Watchdog::arm(secs + 60, "stress run");

    let exe = std::env::current_exe()?;
    let start = Instant::now();
    let mut children = Vec::new();
    for k in 0..workers {
        let child = Command::new(&exe)
            .arg("stress-child")
            .arg("--dir")
            .arg(&dir)
            .args(["--mode", &mode, "--secs", &secs.to_string(), "--id", &k.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        children.push(child);
    }

    let mut totals = Totals::default();
    let mut failures = 0u64;
    let mut out_of_space = 0u64;
    for (k, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if let Some(stats) = line.strip_prefix("STATS ") {
                let (kind, ops, ok, conflicts) = parse_stats(stats);
                totals.ops += ops;
                totals.ok += ok;
                totals.conflicts += conflicts;
                println!("child {k} ({kind}): ops={ops} ok={ok} expected-conflicts={conflicts}");
            }
        }
        match out.status.code() {
            Some(0) => {}
            // Capacity, not correctness. Counted and reported apart so this can
            // never again read like an invariant violation (see EXIT_CAPACITY).
            Some(c) if c == EXIT_CAPACITY => out_of_space += 1,
            _ => {
                failures += 1;
                eprintln!("child {k} FAILED: {}", out.status);
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();

    let check = final_check(&db, &mode, &totals);
    println!(
        "stress {mode}: workers={workers} secs={secs} ops={} ok={} expected-conflicts={} \
         throughput={:.0} ops/s",
        totals.ops,
        totals.ok,
        totals.conflicts,
        totals.ops as f64 / elapsed
    );
    match &check {
        Ok(()) => println!("verify: ok"),
        Err(Failure::Runtime(m)) | Err(Failure::Usage(m)) => eprintln!("VERIFY FAILED: {m}"),
    }
    // Report capacity BEFORE correctness: a full file explains a short run, and
    // saying so plainly is the whole point of separating the two.
    if out_of_space > 0 {
        let pages = size_mb * 1024 * 1024 / 4096;
        let geom = match db.leak_counters() {
            Ok((txn, hw, _bound, ents)) => format!(
                "high_water={hw}/{pages} pages ({size_mb} MB), freelist={ents} entries, txn={txn}"
            ),
            Err(e) => format!("(could not read geometry: {e})"),
        };
        // State the facts and the discriminator; do NOT conclude which case it
        // is. The harness cannot know, and "pages are not being reclaimed" is
        // wrong the moment --size_mb is small enough that the writers' own page
        // pools do not fit — which an earlier draft of this message asserted.
        eprintln!(
            "OUT OF SPACE: {out_of_space}/{workers} child(ren) hit DbFull. CAPACITY, not \
             correctness — `verify` above is the correctness check.\n  {geom}\n  \
             Which one it is: if this workload's live set is BOUNDED and the file still \
             fills, pages are not being reclaimed and that is an engine bug (#37's exact \
             signature — see crates/mpedb-core/tests/high_water_leak.rs, and instrument \
             with --features leakstat + examples/leak_probe). If the live set grows, or \
             --size_mb is small enough that {workers} writers' page pools alone do not fit, \
             it is the workload."
        );
    }
    check?;
    if failures > 0 {
        return runtime(format!("{failures} child(ren) failed"));
    }
    if out_of_space > 0 {
        return runtime(format!(
            "{out_of_space} child(ren) ran out of space (exit {EXIT_CAPACITY}); \
             raise --size_mb (now {size_mb}), or fix the reclamation bug this is pointing at"
        ));
    }
    Ok(())
}

fn seed(db: &Database, mode: &str) -> CliResult {
    match mode {
        "bank" => {
            // Prepare BEFORE opening the session (facade locking rule).
            let ins = db.prepare("INSERT INTO accounts (id, balance) VALUES ($1, $2)")?;
            let mut s = db.begin()?;
            for i in 0..BANK_ACCOUNTS {
                s.execute(&ins, &params![i, BANK_TOTAL / BANK_ACCOUNTS])?;
            }
            s.commit()?;
        }
        "mixed" => {
            let ins = db.prepare("INSERT INTO items (id, a, b) VALUES ($1, $2, $3)")?;
            let mut s = db.begin()?;
            for i in (0..MIXED_KEYSPACE as i64).step_by(2) {
                s.execute(&ins, &params![i, i, "seed"])?;
            }
            s.commit()?;
        }
        "incr" => {
            let ins = db.prepare("INSERT INTO ctr (id, v) VALUES ($1, 0)")?;
            let mut s = db.begin()?;
            for i in 0..INCR_KEYSPACE {
                s.execute(&ins, &params![i])?;
            }
            s.commit()?;
        }
        _ => {} // unique: children create all rows
    }
    Ok(())
}

fn final_check(db: &Database, mode: &str, totals: &Totals) -> CliResult {
    match mode {
        "bank" => {
            let ExecResult::Rows { rows, .. } = db.query("SELECT balance FROM accounts", &[])?
            else {
                return runtime("bank check: expected rows");
            };
            if rows.len() as i64 != BANK_ACCOUNTS {
                return runtime(format!("bank check: {} accounts, want {BANK_ACCOUNTS}", rows.len()));
            }
            let mut sum = 0i64;
            for row in &rows {
                sum += int(&row[0])?;
            }
            if sum != BANK_TOTAL {
                return runtime(format!(
                    "BANK SUM VIOLATION: committed sum {sum} != {BANK_TOTAL} — lost/duplicated money"
                ));
            }
        }
        "unique" => {
            let ExecResult::Rows { rows, .. } = db.query("SELECT id, email FROM users", &[])?
            else {
                return runtime("unique check: expected rows");
            };
            let mut seen = std::collections::HashSet::new();
            for row in &rows {
                let id = int(&row[0])?;
                let Value::Text(email) = &row[1] else {
                    return runtime("unique check: email not text");
                };
                if !seen.insert(email.clone()) {
                    return runtime(format!("UNIQUE VIOLATION: email {email} appears twice"));
                }
                let idx: i64 = email
                    .strip_prefix("email")
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Failure::Runtime(format!("unexpected email {email}")))?;
                if id % 1_000_000 != idx {
                    return runtime(format!("unique check: row (id={id}, {email}) inconsistent"));
                }
            }
            // Index probes must agree with the scan, one row per email max.
            for i in 0..UNIQUE_EMAILS {
                let email = format!("email{i}");
                let ExecResult::Rows { rows, .. } =
                    db.query("SELECT id FROM users WHERE email = $1", &params![email.clone()])?
                else {
                    return runtime("unique check: expected rows");
                };
                let want = u64::from(seen.contains(&email));
                if rows.len() as u64 != want {
                    return runtime(format!(
                        "unique check: index probe for {email} found {} rows, scan says {want}",
                        rows.len()
                    ));
                }
            }
            if seen.len() as u64 != totals.ok {
                return runtime(format!(
                    "unique check: {} rows on disk but children reported {} successful inserts",
                    seen.len(),
                    totals.ok
                ));
            }
        }
        "incr" => {
            // Conservation: every successful autocommit increment must be
            // reflected exactly once in the committed sum of v. This is the
            // acid test for optimistic footprint soundness (a lost update from
            // a missed WriteConflict would make sum < ok).
            let ExecResult::Rows { rows, .. } = db.query("SELECT v FROM ctr", &[])? else {
                return runtime("incr check: expected rows");
            };
            if rows.len() as i64 != INCR_KEYSPACE {
                return runtime(format!("incr check: {} rows, want {INCR_KEYSPACE}", rows.len()));
            }
            let mut sum = 0i64;
            for row in &rows {
                sum += int(&row[0])?;
            }
            if sum as u64 != totals.ok {
                return runtime(format!(
                    "INCR CONSERVATION VIOLATION: committed sum {sum} != {} successful \
                     increments — a lost update (unsound optimistic footprint check)",
                    totals.ok
                ));
            }
        }
        _ => {}
    }
    db.verify()?;
    Ok(())
}

fn parse_stats(s: &str) -> (String, u64, u64, u64) {
    let mut kind = String::from("?");
    let (mut ops, mut ok, mut conflicts) = (0, 0, 0);
    for tok in s.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            match k {
                "kind" => kind = v.to_owned(),
                "ops" => ops = v.parse().unwrap_or(0),
                "ok" => ok = v.parse().unwrap_or(0),
                "conflicts" => conflicts = v.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    (kind, ops, ok, conflicts)
}

fn int(v: &Value) -> Result<i64, Failure> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(Failure::Runtime(format!("expected int, got {other}"))),
    }
}

// -------------------------------------------------------------------- child

/// Exit code for "the file filled up". Its own code because DbFull is a
/// CAPACITY outcome, not a correctness one, and every other error path here
/// means the engine did something it should not have.
///
/// This distinction is not bookkeeping. Bug #37 — an unbounded high-water leak
/// on a working set that did not grow — was reported by this harness for ten
/// days as "8 child(ren) failed", which reads exactly like a torn snapshot or a
/// double free. Nobody looked, because the message said the wrong thing.
pub const EXIT_CAPACITY: i32 = 4;

/// A child that ran out of space: say so precisely, and exit `EXIT_CAPACITY` so
/// the parent can tell this apart from an engine bug. Shared with the crash and
/// powerloss children, whose blob modes can plausibly fill a file mid-wave.
pub fn exit_db_full(kind: &str, e: &mpedb::Error) -> ! {
    eprintln!(
        "{kind} child: OUT OF SPACE ({e}) — capacity, not a correctness failure.\n           If the workload's live set is BOUNDED, the file filling is an engine bug \
         (that is #37's signature: see crates/mpedb-core/tests/high_water_leak.rs). \
         If it is unbounded, the workload outgrew --size_mb."
    );
    std::process::exit(EXIT_CAPACITY);
}

/// Hidden subcommand. Exit codes: 0 ok, 1 unexpected error (via Failure),
/// 3 invariant violation observed (MVCC bug — the evidence the parent wants),
/// 4 out of space (capacity — see `EXIT_CAPACITY`).
pub fn run_child(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["dir", "mode", "secs", "id"], &[])?;
    let dir = PathBuf::from(p.require("dir")?);
    let mode = p.require("mode")?.to_owned();
    let secs = p.require_u64("secs")?;
    let id = p.require_u64("id")?;

    let db = Database::open(&dir.join("config.toml"))?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut rng = Rng::seeded(&[id, secs, u64::from(std::process::id())]);

    match mode.as_str() {
        "bank" if id % 2 == 0 => bank_writer(&db, deadline, &mut rng),
        "bank" => bank_reader(&db, deadline),
        "unique" => unique_child(&db, deadline, id),
        "mixed" => mixed_child(&db, deadline, &mut rng),
        "incr" => incr_child(&db, deadline, &mut rng),
        other => usage(format!("stress-child: bad mode {other}")),
    }
}

/// Autocommit increment writer (conservation invariant). Every `ok` is exactly
/// one committed `v = v + 1`; the parent checks the committed sum equals the
/// total `ok` across children. `WriteConflict` is retried transparently inside
/// the engine, so it never surfaces here — a surfaced error is a real bug.
fn incr_child(db: &Database, deadline: Instant, rng: &mut Rng) -> CliResult {
    let upd = db.prepare("UPDATE ctr SET v = v + 1 WHERE id = $1")?;
    let mut ops = 0u64;
    let mut ok = 0u64;
    while Instant::now() < deadline {
        let key = rng.below(INCR_KEYSPACE as u64) as i64;
        ops += 1;
        match db.execute(&upd, &params![key]) {
            Ok(ExecResult::Affected(1)) => ok += 1,
            Ok(other) => return runtime(format!("incr child: unexpected {other:?}")),
            Err(e) if matches!(e, mpedb::Error::DbFull) => exit_db_full("incr", &e),
                Err(e) => return runtime(format!("incr child: unexpected error: {e}")),
        }
    }
    println!("STATS kind=incr ops={ops} ok={ok} conflicts=0");
    Ok(())
}

fn bank_writer(db: &Database, deadline: Instant, rng: &mut Rng) -> CliResult {
    let sel = db.prepare("SELECT balance FROM accounts WHERE id = $1")?;
    let upd = db.prepare("UPDATE accounts SET balance = $1 WHERE id = $2")?;
    let mut ops = 0u64;
    while Instant::now() < deadline {
        let a = rng.below(BANK_ACCOUNTS as u64) as i64;
        let b = (a + 1 + rng.below(BANK_ACCOUNTS as u64 - 1) as i64) % BANK_ACCOUNTS;
        let amount = 1 + rng.below(50) as i64;
        let mut s = db.begin()?;
        let bal_a = one_int(s.execute(&sel, &params![a])?)?;
        let bal_b = one_int(s.execute(&sel, &params![b])?)?;
        s.execute(&upd, &params![bal_a - amount, a])?;
        s.execute(&upd, &params![bal_b + amount, b])?;
        s.commit()?;
        ops += 1;
    }
    println!("STATS kind=bank-writer ops={ops} ok={ops} conflicts=0");
    Ok(())
}

fn bank_reader(db: &Database, deadline: Instant) -> CliResult {
    let sel = db.prepare("SELECT balance FROM accounts")?;
    let mut ops = 0u64;
    let mut evicted = 0u64;
    while Instant::now() < deadline {
        let rows = match db.execute(&sel, &[]) {
            Ok(ExecResult::Rows { rows, .. }) => rows,
            Ok(other) => return runtime(format!("bank reader: unexpected {other:?}")),
            Err(Error::SnapshotEvicted) => {
                evicted += 1;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let mut sum = 0i64;
        for row in &rows {
            sum += int(&row[0])?;
        }
        if rows.len() as i64 != BANK_ACCOUNTS || sum != BANK_TOTAL {
            eprintln!(
                "BANK INVARIANT VIOLATION: snapshot has {} accounts summing {sum} \
                 (want {BANK_ACCOUNTS} accounts, sum {BANK_TOTAL}) — torn MVCC snapshot",
                rows.len()
            );
            std::process::exit(3);
        }
        ops += 1;
    }
    println!("STATS kind=bank-reader ops={ops} ok={ops} conflicts={evicted}");
    Ok(())
}

fn unique_child(db: &Database, deadline: Instant, id: u64) -> CliResult {
    let ins = db.prepare("INSERT INTO users (id, email) VALUES ($1, $2)")?;
    let (mut ops, mut ok, mut conflicts) = (0u64, 0u64, 0u64);
    'outer: loop {
        for i in 0..UNIQUE_EMAILS {
            if Instant::now() >= deadline {
                break 'outer;
            }
            let row_id = id as i64 * 1_000_000 + i;
            ops += 1;
            match db.execute(&ins, &params![row_id, format!("email{i}")]) {
                Ok(r) => {
                    ok += 1;
                    if std::env::var("MPEDB_DEBUG_UNIQUE").is_ok() {
                        mpedb_core::ring::ring_debug_pub(format!(
                            "OKLOG child={id} i={i} row_id={row_id} result={r:?}"
                        ));
                    }
                }
                Err(Error::UniqueViolation { .. }) | Err(Error::PrimaryKeyViolation { .. }) => {
                    conflicts += 1; // lost the race — the expected outcome
                }
                Err(e) if matches!(e, mpedb::Error::DbFull) => exit_db_full("unique", &e),
                Err(e) => return runtime(format!("unique child: unexpected error: {e}")),
            }
        }
    }
    println!("STATS kind=unique ops={ops} ok={ok} conflicts={conflicts}");
    Ok(())
}

fn mixed_child(db: &Database, deadline: Instant, rng: &mut Rng) -> CliResult {
    let ins = db.prepare("INSERT INTO items (id, a, b) VALUES ($1, $2, $3)")?;
    let upd = db.prepare("UPDATE items SET a = $1 WHERE id = $2")?;
    let del = db.prepare("DELETE FROM items WHERE id = $1")?;
    let sel = db.prepare("SELECT id, a, b FROM items WHERE id = $1")?;
    let (mut ops, mut ok, mut conflicts) = (0u64, 0u64, 0u64);
    while Instant::now() < deadline {
        let key = rng.below(MIXED_KEYSPACE) as i64;
        ops += 1;
        let res = match rng.below(10) {
            0..=2 => db.execute(&ins, &params![key, key * 7, "mixed"]),
            3..=5 => db.execute(&upd, &params![rng.below(1 << 20) as i64, key]),
            6..=8 => db.execute(&sel, &params![key]),
            _ => db.execute(&del, &params![key]),
        };
        match res {
            Ok(_) => ok += 1,
            Err(Error::PrimaryKeyViolation { .. }) => conflicts += 1, // insert on live key
            Err(e) if matches!(e, mpedb::Error::DbFull) => exit_db_full("mixed", &e),
                Err(e) => return runtime(format!("mixed child: unexpected error: {e}")),
        }
    }
    println!("STATS kind=mixed ops={ops} ok={ok} conflicts={conflicts}");
    Ok(())
}

fn one_int(res: ExecResult) -> Result<i64, Failure> {
    match res {
        ExecResult::Rows { rows, .. } if rows.len() == 1 => int(&rows[0][0]),
        other => Err(Failure::Runtime(format!(
            "expected exactly one row, got {other:?}"
        ))),
    }
}
