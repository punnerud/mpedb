//! `mpedb bench <config.toml>|--auto [--secs N]` — single-process
//! execute-by-hash throughput and latency (p50/p99 µs).
//!
//! All three plans are prepared ONCE; the measured loop is pure
//! `execute(hash, params)` — the zero-parse hot path the design optimizes.
//! With `--auto` a scratch config + database are created under /dev/shm
//! (or the temp dir) and removed afterwards; otherwise the given config must
//! define the dedicated bench table:
//! `bench(id int64 pk, val int64, pad text)`.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mpedb::{params, Database, Error, ExecResult};

use crate::args;
use crate::util::{runtime, shm_or_temp, usage, write_config_durable, CliResult, Rng};

const SEED_ROWS: i64 = 10_000;

const BENCH_TABLE_TOML: &str = r#"[[table]]
name = "bench"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "val"
  type = "int64"
  nullable = false

  [[table.column]]
  name = "pad"
  type = "text"
  nullable = false
"#;

struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn run(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["secs", "durability", "disk"], &["auto"])?;
    let secs = p.u64_or("secs", 5)?.max(1);
    let durability = p.value("durability").unwrap_or("none").to_owned();
    if !matches!(durability.as_str(), "none" | "commit" | "async" | "wal") {
        return usage("--durability must be none, commit, async or wal");
    }

    let (config_path, _guard, label) = if p.has("auto") {
        // Durable modes are meaningless on tmpfs: default the scratch db to a
        // real-disk dir via --disk (else /dev/shm, fine only for none).
        let base = match p.value("disk") {
            Some(d) => PathBuf::from(d),
            None => shm_or_temp(),
        };
        let dir = base.join(format!("mpedb-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let guard = TempDirGuard(dir.clone());
        let cfg = dir.join("config.toml");
        write_config_durable(&cfg, &dir.join("bench.mpedb"), 128, BENCH_TABLE_TOML, &durability, None)?;
        (cfg, Some(guard), format!("--auto (scratch db, durability={durability})"))
    } else {
        let [cfg] = p.positional.as_slice() else {
            return usage("bench needs <config.toml> or --auto");
        };
        (PathBuf::from(cfg), None, cfg.clone())
    };

    let db = Database::open(&config_path)?;
    check_bench_table(&db)?;

    // Prepare once — everything below runs by hash only.
    let ins = db.prepare("INSERT INTO bench (id, val, pad) VALUES ($1, $2, $3)")?;
    let sel = db.prepare("SELECT val FROM bench WHERE id = $1")?;
    let upd = db.prepare("UPDATE bench SET val = $1 WHERE id = $2")?;

    // Seed the point-read/point-update key space (idempotent across runs).
    for id in 0..SEED_ROWS {
        match db.execute(&ins, &params![id, id, "seed"]) {
            Ok(_) => {}
            Err(Error::PrimaryKeyViolation { .. }) => break, // already seeded
            Err(e) => return Err(e.into()),
        }
    }

    let per_phase = Duration::from_secs_f64((secs as f64 / 3.0).max(0.5));
    let mut rng = Rng::seeded(&[std::process::id() as u64, secs]);
    // Insert ids unique across repeated runs on the same db.
    let mut next_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64;

    println!("bench: {label}  (~{:.1}s per op)", per_phase.as_secs_f64());
    println!(
        "{:<8} {:>10} {:>12} {:>9} {:>9}",
        "op", "ops", "ops/s", "p50(us)", "p99(us)"
    );

    run_phase("insert", per_phase, &mut rng, &mut |rng| {
        next_id += 1;
        let pad = "xxxxxxxxxxxxxxxx";
        match db.execute(&ins, &params![next_id, rng.below(1 << 30) as i64, pad]) {
            Ok(_) => Ok(()),
            Err(Error::DbFull) => {
                runtime("database full during insert phase; use a larger size_mb")
            }
            Err(e) => Err(e.into()),
        }
    })?;
    run_phase("select", per_phase, &mut rng, &mut |rng| {
        let id = rng.below(SEED_ROWS as u64) as i64;
        match db.execute(&sel, &params![id])? {
            ExecResult::Rows { .. } => Ok(()),
            other => runtime(format!("select returned {other:?}")),
        }
    })?;
    run_phase("update", per_phase, &mut rng, &mut |rng| {
        let id = rng.below(SEED_ROWS as u64) as i64;
        db.execute(&upd, &params![rng.below(1 << 30) as i64, id])?;
        Ok(())
    })?;

    db.verify()?;
    Ok(())
}

fn run_phase(
    name: &str,
    per_phase: Duration,
    rng: &mut Rng,
    op: &mut dyn FnMut(&mut Rng) -> CliResult,
) -> CliResult {
    let mut lat: Vec<u64> = Vec::with_capacity(1 << 20);
    let start = Instant::now();
    let deadline = start + per_phase;
    while Instant::now() < deadline {
        let t = Instant::now();
        op(rng)?;
        lat.push(t.elapsed().as_micros() as u64);
    }
    let elapsed = start.elapsed().as_secs_f64();
    lat.sort_unstable();
    let pct = |q: f64| -> u64 {
        if lat.is_empty() {
            0
        } else {
            lat[((lat.len() - 1) as f64 * q) as usize]
        }
    };
    println!(
        "{:<8} {:>10} {:>12.0} {:>9} {:>9}",
        name,
        lat.len(),
        lat.len() as f64 / elapsed,
        pct(0.50),
        pct(0.99)
    );
    Ok(())
}

fn check_bench_table(db: &Database) -> CliResult {
    let ok = db.schema().tables.iter().any(|t| {
        t.name == "bench"
            && t.columns.len() == 3
            && t.columns[0].name == "id"
            && t.columns[1].name == "val"
            && t.columns[2].name == "pad"
    });
    if ok {
        Ok(())
    } else {
        runtime(
            "the config must define the dedicated bench table \
             `bench(id int64 pk, val int64, pad text)` — or use `mpedb bench --auto`",
        )
    }
}
