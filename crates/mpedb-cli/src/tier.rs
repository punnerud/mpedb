//! `mpedb tier` — cold-data tiering v1 (#78, design/DESIGN-SYNC-TIERING.md).
//!
//! `tier drain <hot> <cold> --table T --where PRED [param ...]` moves every
//! matching row from the hot database to the cold `.mpedb` with the
//! copy-commit-verify-then-delete protocol (see `mpedb::tier`): SIGKILL at
//! any instant loses nothing — the worst case is identical duplicates in
//! both files, and re-running the SAME drain reconciles them. A missing
//! `<cold>` ending in `.mpedb` is created carrying exactly the hot table's
//! definition. Read-back is `ATTACH '<cold>' AS cold; SELECT ... UNION ALL
//! SELECT ... FROM cold.<T>` — the #51 cross-file read path.
//!
//! `tier crash --dir D --waves W` is the SIGKILL evidence harness (the
//! crash-subcommand convention: multi-process/crash behavior is tested here,
//! not in unit tests). Each wave seeds a deterministic dataset, spawns a
//! `tier-crash-child` that ping-pongs the SAME drain hot→cold→hot forever
//! with a 5..60 ms self-SIGKILL armed BEFORE the databases open, then:
//!  1. asserts prompt writer-lock recovery + page accounting on BOTH files,
//!  2. asserts the union invariant on the killed state: every seeded row is
//!     in hot, in cold, or in both — bit-identical to the recomputation,
//!     never missing, never divergent under one PK;
//!  3. runs the drain to completion (the reconcile) and asserts the final
//!     split is EXACT: cold = all matching rows, hot = all others, no row in
//!     both, and the ATTACH union row-identical to the seed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mpedb::{Database, Durability, ExecResult, Value};
use mpedb_core::Engine;

use crate::args;
use crate::util::{
    fill_bytes, open_target, parse_params, runtime, usage, CliResult, Failure, Rng, Watchdog,
};

pub fn run(argv: &[String]) -> CliResult {
    match argv.split_first() {
        Some((sub, rest)) if sub == "drain" => drain(rest),
        Some((sub, rest)) if sub == "crash" => crash_parent(rest),
        _ => usage(
            "tier needs a subcommand: drain <hot> <cold> --table T --where PRED [param ...] \
             [--batch N] [--size-mb M] [--durability D] | crash --dir D --waves W",
        ),
    }
}

// -------------------------------------------------------------------- drain

fn drain(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["table", "where", "batch", "size-mb", "durability"], &[])?;
    let [hot_path, cold_path, params @ ..] = &p.positional[..] else {
        return usage("tier drain needs <hot> <cold> [param ...]");
    };
    let table = p.require("table")?;
    let predicate = p.require("where")?;
    let batch = p.u64_or("batch", 1000)?.max(1) as usize;
    let params = parse_params(params);

    let hot = open_target(hot_path)?;
    let cold_p = Path::new(cold_path);
    let cold = if !cold_p.exists() && cold_p.extension().is_some_and(|e| e == "mpedb") {
        // Fresh cold file: seed it with the hot table's exact definition.
        // Default size = the hot file's (an upper bound on what can drain
        // into it); default durability = commit, so the handoff is power-loss
        // durable by default, not only SIGKILL-safe.
        let size = match p.value("size-mb") {
            Some(s) => {
                let mb: u64 = s
                    .parse()
                    .map_err(|_| Failure::Usage("--size-mb must be an integer".into()))?;
                mb << 20
            }
            None => std::fs::metadata(hot.path())?.len().max(16 << 20),
        };
        let durability = match p.value("durability").unwrap_or("commit") {
            "none" => Durability::None,
            "commit" => Durability::Commit,
            "async" => Durability::Async,
            "wal" => Durability::Wal,
            other => return usage(format!("--durability none|commit|async|wal, got `{other}`")),
        };
        eprintln!(
            "tier: creating {} ({} MiB, durability={:?}) with table `{table}`",
            cold_p.display(),
            size >> 20,
            durability
        );
        hot.tier_create_cold(cold_p, table, size, durability)?
    } else {
        // Existing file or a config.toml. (--size-mb/--durability only apply
        // to creation; an existing cold's geometry is file-authoritative.)
        open_target(cold_path)?
    };
    // A busy hot/cold writer should wait, not fail the drain instantly.
    hot.set_busy_timeout(Some(Duration::from_secs(30)));
    cold.set_busy_timeout(Some(Duration::from_secs(30)));

    let report = hot.tier_drain(&cold, table, predicate, &params, batch)?;
    println!(
        "tier drain: moved={} reconciled={} batches={} (cold verified before every hot reclaim)",
        report.moved, report.reconciled, report.batches
    );
    if report.reconciled > 0 {
        println!(
            "tier drain: {} row(s) were already landed in cold — an earlier drain's \
             crash window, now reconciled",
            report.reconciled
        );
    }
    Ok(())
}

// ------------------------------------------------------------ crash harness

const CRASH_ROWS: i64 = 400;
const CRASH_PRED: &str = "grp < 5";
const WAVE_TIMEOUT_SECS: u64 = 30;

const TIER_TOML: &str = r#"[[table]]
name = "rows"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "grp"
  type = "int64"
  nullable = false
  indexed = true

  [[table.column]]
  name = "payload"
  type = "blob"
  nullable = false
"#;

/// Row `id`'s full expected content — pure xorshift, recomputable by any
/// process, so "no row lost, none altered" is recompute-and-compare.
fn seed_row(id: i64) -> Vec<Value> {
    let mut rng = Rng::seeded(&[0x71E4, id as u64]);
    let len = 16 + (rng.below(48)) as usize;
    Vec::from([
        Value::Int(id),
        Value::Int(id % 10),
        Value::Blob(fill_bytes(&mut rng, len)),
    ])
}

fn crash_parent(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["dir", "waves", "batch"], &[])?;
    let dir = PathBuf::from(p.require("dir")?);
    let waves = p.require_u64("waves")?;
    if waves == 0 {
        return usage("--waves must be >= 1");
    }
    let batch = p.u64_or("batch", 7)?.max(1);

    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let cfg = dir.join("config.toml");
    let hot_f = dir.join("tier-hot.mpedb");
    let cold_f = dir.join("tier-cold.mpedb");
    let exe = std::env::current_exe()?;

    let mut total_killed = 0u64;
    let mut recoveries = 0u64;
    for wave in 0..waves {
        let _wd = Watchdog::arm(WAVE_TIMEOUT_SECS, &format!("tier crash wave {wave}"));
        // Fresh pair each wave, so kill offsets walk the whole protocol from
        // a known state. The parent is never killed; it creates both files
        // (a torn CREATE is engine-init territory, covered by `crash`).
        let _ = std::fs::remove_file(&hot_f);
        let _ = std::fs::remove_file(&cold_f);
        crate::util::write_config_durable(&cfg, &hot_f, 64, TIER_TOML, "none", None)?;
        {
            let hot = Database::open(&cfg)?;
            let ins = hot.prepare("INSERT INTO rows (id, grp, payload) VALUES ($1, $2, $3)")?;
            let mut s = hot.begin()?;
            for id in 0..CRASH_ROWS {
                s.execute(&ins, &seed_row(id))?;
            }
            s.commit()?;
            hot.tier_create_cold(&cold_f, "rows", 64 << 20, Durability::None)?;
        }

        // The child ping-pongs the drain until its own SIGKILL lands.
        let status = Command::new(&exe)
            .arg("tier-crash-child")
            .arg("--dir")
            .arg(&dir)
            .args(["--wave", &wave.to_string(), "--batch", &batch.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?
            .wait()?;
        use std::os::unix::process::ExitStatusExt;
        if status.signal() == Some(libc::SIGKILL) {
            total_killed += 1;
        } else {
            return runtime(format!(
                "wave {wave}: tier-crash-child did not die by SIGKILL: {status}"
            ));
        }

        // 1. Prompt recovery + page accounting on BOTH files.
        let t0 = Instant::now();
        for f in [&hot_f, &cold_f] {
            let eng = Engine::open_from_file(f)?;
            let w = eng.begin_write()?;
            if w.recovered {
                recoveries += 1;
            }
            w.abort();
            eng.verify_page_accounting()?;
        }
        let lock_wait = t0.elapsed();

        // 2. Union invariant on the KILLED state: every seeded row in hot,
        // cold, or both; content bit-identical; nothing extra.
        let hot = Database::open_from_file(&hot_f)?;
        let cold = Database::open_from_file(&cold_f)?;
        let (hot_rows, cold_rows) = (all_rows(&hot)?, all_rows(&cold)?);
        let mut dup = 0u64;
        for id in 0..CRASH_ROWS {
            let want = seed_row(id);
            let in_hot = check_row(&hot_rows, id, &want, "hot", wave)?;
            let in_cold = check_row(&cold_rows, id, &want, "cold", wave)?;
            if !in_hot && !in_cold {
                return runtime(format!(
                    "TIER INVARIANT VIOLATION wave {wave}: row id={id} is in NEITHER \
                     file — the drain lost a row"
                ));
            }
            dup += u64::from(in_hot && in_cold);
        }
        let extra = hot_rows.len() + cold_rows.len() - dup as usize;
        if extra != CRASH_ROWS as usize {
            return runtime(format!(
                "TIER INVARIANT VIOLATION wave {wave}: {extra} distinct ids, seeded {CRASH_ROWS}"
            ));
        }

        // 3. Reconcile: the SAME drain to completion, then the split is exact.
        hot.set_busy_timeout(Some(Duration::from_secs(10)));
        let report = hot.tier_drain(&cold, "rows", CRASH_PRED, &[], batch as usize)?;
        let (hot_rows, cold_rows) = (all_rows(&hot)?, all_rows(&cold)?);
        for id in 0..CRASH_ROWS {
            let want = seed_row(id);
            let in_hot = check_row(&hot_rows, id, &want, "hot", wave)?;
            let in_cold = check_row(&cold_rows, id, &want, "cold", wave)?;
            let should_be_cold = id % 10 < 5;
            if in_hot == should_be_cold || in_cold != should_be_cold {
                return runtime(format!(
                    "TIER RECONCILE VIOLATION wave {wave}: id={id} (grp {}) in_hot={in_hot} \
                     in_cold={in_cold} after the final drain",
                    id % 10
                ));
            }
        }
        // Read-back: the ATTACH union must be row-identical to the seed.
        drop(cold);
        hot.query(&format!("ATTACH DATABASE '{}' AS cold", cold_f.display()), &[])?;
        let union = match hot.query(
            "SELECT id, grp, payload FROM rows UNION ALL \
             SELECT id, grp, payload FROM cold.rows",
            &[],
        )? {
            ExecResult::Rows { rows, .. } => rows,
            other => return runtime(format!("attach union: unexpected {other:?}")),
        };
        if union.len() != CRASH_ROWS as usize {
            return runtime(format!(
                "TIER READ-BACK VIOLATION wave {wave}: ATTACH union has {} rows, \
                 seeded {CRASH_ROWS}",
                union.len()
            ));
        }
        for row in &union {
            let Value::Int(id) = row[0] else {
                return runtime(format!("attach union: non-int id {:?}", row[0]));
            };
            if row != &seed_row(id) {
                return runtime(format!(
                    "TIER READ-BACK VIOLATION wave {wave}: id={id} diverges through ATTACH"
                ));
            }
        }
        println!(
            "wave {wave}: killed=1 lock_wait={}us duplicates-at-kill={dup} \
             reconcile: moved={} reconciled={} — union invariant held, final split exact",
            lock_wait.as_micros(),
            report.moved,
            report.reconciled
        );
    }
    println!(
        "tier crash: waves={waves} killed={total_killed} EOWNERDEAD recoveries={recoveries} \
         — no row lost, no divergence, every reconcile converged"
    );
    Ok(())
}

/// All rows of `rows` keyed by id, via a full scan.
fn all_rows(db: &Database) -> Result<std::collections::BTreeMap<i64, Vec<Value>>, Failure> {
    let out = match db.query("SELECT id, grp, payload FROM rows", &[])? {
        ExecResult::Rows { rows, .. } => rows,
        other => return runtime(format!("scan: unexpected {other:?}")),
    };
    let mut map = std::collections::BTreeMap::new();
    for row in out {
        let Value::Int(id) = row[0] else {
            return runtime(format!("scan: non-int id {:?}", row[0]));
        };
        if map.insert(id, row).is_some() {
            return runtime(format!("scan: duplicate id {id} within ONE file"));
        }
    }
    Ok(map)
}

/// Is seeded row `id` present in `rows`? If present it must be bit-identical
/// to the recomputation — a divergent copy is a violation, not a presence.
fn check_row(
    rows: &std::collections::BTreeMap<i64, Vec<Value>>,
    id: i64,
    want: &[Value],
    side: &str,
    wave: u64,
) -> Result<bool, Failure> {
    match rows.get(&id) {
        None => Ok(false),
        Some(got) if got == want => Ok(true),
        Some(got) => runtime(format!(
            "TIER INVARIANT VIOLATION wave {wave}: {side} row id={id} diverges from \
             the seeded content ({} cols vs {})",
            got.len(),
            want.len()
        )),
    }
}

// -------------------------------------------------------------------- child

/// Hidden subcommand: arm the kill BEFORE opening anything, then ping-pong
/// the SAME predicate hot→cold and cold→hot forever. Every instant of the
/// copy-commit-verify-delete protocol is therefore a possible kill point, in
/// both directions, including the windows between batches and between whole
/// drains. Any voluntary exit is a failure (the parent checks for SIGKILL).
pub fn run_crash_child(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["dir", "wave", "batch"], &[])?;
    let dir = PathBuf::from(p.require("dir")?);
    let wave = p.require_u64("wave")?;
    let batch = p.u64_or("batch", 7)?.max(1) as usize;

    let mut rng = Rng::seeded(&[0x71E4C4A5, wave]);
    let kill_ms = 5 + rng.below(56);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(kill_ms));
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    });

    let hot = Database::open(&dir.join("config.toml"))?;
    let cold = Database::open_from_file(&dir.join("tier-cold.mpedb"))?;
    loop {
        hot.tier_drain(&cold, "rows", CRASH_PRED, &[], batch)?;
        cold.tier_drain(&hot, "rows", CRASH_PRED, &[], batch)?;
    }
}
