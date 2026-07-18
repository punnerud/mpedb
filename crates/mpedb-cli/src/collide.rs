//! `mpedb collide --dir D [--writers N] [--total T] [--drop-rate R] [--jitter-us J]
//!   [--keyspace K] [--detached-pct P] [--durability M]`
//!
//! An adversarial writer-collision harness. It fires `--total` (default 50,000)
//! random writes across `--writers` processes at a small shared keyspace, with:
//!
//! - **random per-write jitter** (`--jitter-us`) so interleavings vary run to run;
//! - **dropped packets** — a `--drop-rate`% fraction of writers arm a kill thread
//!   and `SIGKILL` themselves at a random instant, modelling a submitter that
//!   dies mid-commit / abandons a posted intent (the intent-ring incarnation
//!   safety this stresses, design/DESIGN.md §5.3);
//! - **data mutation** — UPDATEs rewrite (val, owner, seq, chk) atomically, so
//!   write-write races on the same key are the norm;
//! - **the SDK/hash path** — every write goes through a content-hashed plan
//!   (`execute(hash, …)`); `--detached-pct`% of writers instead carry a
//!   round-tripped detached plan (`{hash, blob, sql}` encoded→decoded, as an SDK
//!   would ship it) and `execute_detached`;
//! - **collisions in varying orders** — PK collisions (shared id), UNIQUE
//!   collisions (distinct id, shared tag), delete/re-insert churn.
//!
//! Every written row carries `chk = mix(id, val, owner, seq)`. The parent then
//! reattaches, asserts `begin_write` returns promptly (robust-mutex EOWNERDEAD
//! recovery, no wedge), runs the page-accounting verifier, and scans EVERY row
//! checking the checksum + UNIQUE index agreement — so any torn, mixed, or
//! fabricated row from the chaos is caught.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mpedb::{params, Database, Error, ExecResult, Value};
use mpedb_core::Engine;

use crate::args;
use crate::util::{runtime, usage, write_config_concurrency, CliResult, Failure, Rng, Watchdog};

const COLLIDE_TOML: &str = r#"[[table]]
name = "cells"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "val"
  type = "int64"
  nullable = false

  [[table.column]]
  name = "owner"
  type = "int64"
  nullable = false

  [[table.column]]
  name = "seq"
  type = "int64"
  nullable = false

  [[table.column]]
  name = "tag"
  type = "text"
  nullable = true
  unique = true

  [[table.column]]
  name = "chk"
  type = "int64"
  nullable = false
"#;

/// Self-consistency checksum baked into every row. A torn write (some columns
/// from an old write, some from a new one) fails this; a fabricated row would
/// have to compute it, which only a genuine writer does.
fn chk_of(id: i64, val: i64, owner: i64, seq: i64) -> i64 {
    id.wrapping_mul(-0x61c8_8647)
        .wrapping_add(val.wrapping_mul(-0x7a14_3589))
        .wrapping_add(owner.wrapping_mul(-0x3d4d_51c3))
        .wrapping_add(seq.wrapping_mul(0x27d4_eb2f))
}

const UNIQ_ID_BASE: i64 = 1_000_000_000;

// ------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &[
            "dir",
            "writers",
            "total",
            "drop-rate",
            "jitter-us",
            "keyspace",
            "detached-pct",
            "durability",
            "concurrency",
        ],
        &[],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let writers = p.u64_or("writers", 32)?.max(1);
    let total = p.u64_or("total", 50_000)?.max(writers);
    let drop_rate = p.u64_or("drop-rate", 15)?.min(90);
    let jitter_us = p.u64_or("jitter-us", 50)?;
    let keyspace = p.u64_or("keyspace", 500)?.max(1);
    let detached_pct = p.u64_or("detached-pct", 25)?.min(100);
    let durability = p.value("durability").unwrap_or("wal").to_owned();
    if !matches!(durability.as_str(), "none" | "commit" | "async" | "wal") {
        return usage("--durability must be none, commit, async or wal");
    }
    let concurrency = p.value("concurrency").unwrap_or("serial").to_owned();
    if !matches!(concurrency.as_str(), "serial" | "optimistic") {
        return usage("--concurrency must be serial or optimistic");
    }

    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let cfg = dir.join("config.toml");
    let dbf = dir.join("collide.mpedb");
    let _ = std::fs::remove_file(&dbf);
    write_config_concurrency(&cfg, &dbf, 128, COLLIDE_TOML, &durability, &concurrency, None)?;

    // Seed the PK keyspace so UPDATEs have live targets and PK inserts collide.
    {
        let db = Database::open(&cfg)?;
        let ins = db.prepare(
            "INSERT INTO cells (id, val, owner, seq, tag, chk) VALUES ($1, $2, $3, $4, NULL, $5)",
        )?;
        let mut s = db.begin()?;
        for id in 0..keyspace as i64 {
            s.execute(&ins, &params![id, 0i64, -1i64, 0i64, chk_of(id, 0, -1, 0)])?;
        }
        s.commit()?;
    }

    let per_writer = total / writers;
    // The whole fleet should finish well inside this; the watchdog turns a wedge
    // (a failure to recover the writer lock, say) into a loud abort.
    let _wd = Watchdog::arm(120, "collide run");

    let exe = std::env::current_exe()?;
    let mut seed_rng = Rng::seeded(&[writers, total, drop_rate, jitter_us, keyspace]);
    let start = Instant::now();
    let mut children = Vec::new();
    let mut planned_kills = 0u64;
    for k in 0..writers {
        // A drop-rate fraction of writers are killed at a random instant.
        let kill_ms = if seed_rng.below(100) < drop_rate {
            planned_kills += 1;
            2 + seed_rng.below(250) // 2..252 ms into the run
        } else {
            0
        };
        let detached = seed_rng.below(100) < detached_pct;
        let child = Command::new(&exe)
            .arg("collide-child")
            .arg("--dir")
            .arg(&dir)
            .args([
                "--id",
                &k.to_string(),
                "--writes",
                &per_writer.to_string(),
                "--keyspace",
                &keyspace.to_string(),
                "--jitter-us",
                &jitter_us.to_string(),
                "--kill-ms",
                &kill_ms.to_string(),
                "--detached",
                if detached { "1" } else { "0" },
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        children.push(child);
    }

    let (mut committed, mut pk_coll, mut uniq_coll, mut dels) = (0u64, 0u64, 0u64, 0u64);
    let (mut killed, mut clean, mut failures) = (0u64, 0u64, 0u64);
    use std::os::unix::process::ExitStatusExt;
    for (k, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output()?;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(stats) = line.strip_prefix("STATS ") {
                let s = parse_kv(stats);
                committed += s.get("committed").copied().unwrap_or(0);
                pk_coll += s.get("pk_coll").copied().unwrap_or(0);
                uniq_coll += s.get("uniq_coll").copied().unwrap_or(0);
                dels += s.get("dels").copied().unwrap_or(0);
            }
        }
        if out.status.success() {
            clean += 1;
        } else if out.status.signal() == Some(libc::SIGKILL) {
            killed += 1; // a "dropped packet" — expected for the kill cohort
        } else {
            failures += 1;
            eprintln!("collide: writer {k} exited abnormally: {}", out.status);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();

    // ---- recovery + invariant verification on a fresh handle ----------------
    // 1) Robust-mutex recovery + page accounting via the config-free engine.
    let t0 = Instant::now();
    let (recovered, lock_wait, rows, tags) = {
        let eng = Engine::open_from_file(&dbf)?;
        let w = eng.begin_write()?;
        let recovered = w.recovered;
        w.abort();
        let lock_wait = t0.elapsed();

        eng.verify_page_accounting()?;

        // 2) Every surviving row must satisfy its baked checksum.
        let r = eng.begin_read()?;
        let mut rows = 0u64;
        let mut tags = std::collections::HashSet::new();
        let mut cur = r.scan(0, None, None)?;
        while let Some(row) = cur.next()? {
            let id = int(&row[0])?;
            let val = int(&row[1])?;
            let owner = int(&row[2])?;
            let seq = int(&row[3])?;
            let chk = int(&row[5])?;
            if chk != chk_of(id, val, owner, seq) {
                return runtime(format!(
                    "COLLIDE INVARIANT VIOLATION: row id={id} val={val} owner={owner} seq={seq} \
                     chk={chk} != {} — torn/mixed write survived",
                    chk_of(id, val, owner, seq)
                ));
            }
            if let Value::Text(t) = &row[4] {
                if !tags.insert(t.clone()) {
                    return runtime(format!("UNIQUE VIOLATION: tag {t} present twice"));
                }
            }
            rows += 1;
        }
        r.finish()?;
        (recovered, lock_wait, rows, tags)
    };

    // 3) The UNIQUE index must agree with the scan for every tag (via the facade
    //    — the engine has no SQL). Exactly one row per tag through the index.
    let db = Database::open(&cfg)?;
    for t in &tags {
        let ExecResult::Rows { rows: probe, .. } =
            db.query("SELECT id FROM cells WHERE tag = $1", &params![t.clone()])?
        else {
            return runtime("collide: index probe expected rows");
        };
        if probe.len() != 1 {
            return runtime(format!(
                "UNIQUE index disagreement: tag {t} probes {} rows, scan says 1",
                probe.len()
            ));
        }
    }

    println!(
        "collide: writers={writers} total={total} committed(survivors)={committed} \
         pk_collisions={pk_coll} unique_collisions={uniq_coll} deletes={dels}",
    );
    println!(
        "  writers: clean={clean} dropped(SIGKILL)={killed} (planned {planned_kills}) \
         abnormal={failures}",
    );
    println!(
        "  recovery: eowner_recovery={recovered} lock_wait={}us  final_rows={rows} unique_tags={} \
         verify=ok  ({:.0} attempted writes/s over {:.2}s)",
        lock_wait.as_micros(),
        tags.len(),
        total as f64 / elapsed,
        elapsed,
    );
    if failures > 0 {
        return runtime(format!("{failures} writer(s) exited abnormally (a real bug)"));
    }
    println!("  ALL INVARIANTS HELD (checksum, uniqueness, page accounting, no wedge)");
    Ok(())
}

// -------------------------------------------------------------------- child

pub fn run_child(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["dir", "id", "writes", "keyspace", "jitter-us", "kill-ms", "detached"],
        &[],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let id = p.require_u64("id")?;
    let writes = p.require_u64("writes")?;
    let keyspace = p.require_u64("keyspace")?;
    let jitter_us = p.require_u64("jitter-us")?;
    let kill_ms = p.require_u64("kill-ms")?;
    let detached = p.require("detached")? == "1";

    // "Dropped packet": die at a random instant mid-run, armed before attach so
    // the kill can also land in the attach / prepare / commit windows.
    if kill_ms > 0 {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(kill_ms));
            unsafe {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
        });
    }

    let db = Database::open(&dir.join("config.toml"))?;
    let mut rng = Rng::seeded(&[id, writes, u64::from(std::process::id())]);

    // The SDK/hash path: content-hashed plans, executed parse-free by hash.
    let insert_sql =
        "INSERT INTO cells (id, val, owner, seq, tag, chk) VALUES ($1, $2, $3, $4, $5, $6)";
    let update_sql = "UPDATE cells SET val = $1, owner = $2, seq = $3, chk = $4 WHERE id = $5";
    let delete_sql = "DELETE FROM cells WHERE id = $1";

    // Detached writers carry the plan the way an SDK would: a `{hash, blob, sql}`
    // bundle, round-tripped through the wire encoding, then executed by hash.
    let detached_plans = if detached {
        let mk = |sql: &str| -> Result<mpedb::DetachedPlan, Failure> {
            let d = db.prepare_detached(sql)?;
            // Simulate shipping it over the wire and receiving it back.
            let wire = d.encode();
            Ok(mpedb::DetachedPlan::decode(&wire)?)
        };
        Some((mk(insert_sql)?, mk(update_sql)?, mk(delete_sql)?))
    } else {
        None
    };
    let hash_plans = if detached {
        None
    } else {
        Some((db.prepare(insert_sql)?, db.prepare(update_sql)?, db.prepare(delete_sql)?))
    };

    let owner = id as i64;
    let (mut committed, mut pk_coll, mut uniq_coll, mut dels) = (0u64, 0u64, 0u64, 0u64);

    for seq in 0..writes as i64 {
        if jitter_us > 0 {
            let j = rng.below(jitter_us + 1);
            if j > 0 {
                std::thread::sleep(Duration::from_micros(j));
            }
        }
        let key = rng.below(keyspace) as i64;
        let val = rng.next() as i64;
        // Op mix: PK-insert (shared id), update (write-write), delete (churn),
        // unique-insert (distinct id, shared tag) — collisions in varying order.
        let roll = rng.below(100);
        let params_and_kind: (Kind, [Value; 6]) = if roll < 30 {
            // PK-collision insert (tag NULL avoids the UNIQUE dimension here).
            (
                Kind::Insert,
                [
                    Value::Int(key),
                    Value::Int(val),
                    Value::Int(owner),
                    Value::Int(seq),
                    Value::Null,
                    Value::Int(chk_of(key, val, owner, seq)),
                ],
            )
        } else if roll < 55 {
            // Data mutation (write-write) on a shared key.
            (
                Kind::Update,
                [
                    Value::Int(val),
                    Value::Int(owner),
                    Value::Int(seq),
                    Value::Int(chk_of(key, val, owner, seq)),
                    Value::Int(key),
                    Value::Null, // unused slot
                ],
            )
        } else if roll < 70 {
            (Kind::Delete, [Value::Int(key), Value::Null, Value::Null, Value::Null, Value::Null, Value::Null])
        } else {
            // UNIQUE-collision insert: unique id, shared tag `t<key>`.
            let uid = owner.wrapping_mul(UNIQ_ID_BASE).wrapping_add(seq + 1);
            (
                Kind::Insert,
                [
                    Value::Int(uid),
                    Value::Int(val),
                    Value::Int(owner),
                    Value::Int(seq),
                    Value::Text(format!("t{key}")),
                    Value::Int(chk_of(uid, val, owner, seq)),
                ],
            )
        };

        let (kind, pv) = params_and_kind;
        let res = match (&hash_plans, &detached_plans, kind) {
            (Some((ins, _, _)), _, Kind::Insert) => db.execute(ins, &pv),
            (Some((_, upd, _)), _, Kind::Update) => db.execute(upd, &pv[..5]),
            (Some((_, _, del)), _, Kind::Delete) => db.execute(del, &pv[..1]),
            (_, Some((ins, _, _)), Kind::Insert) => db.execute_detached(ins, &pv),
            (_, Some((_, upd, _)), Kind::Update) => db.execute_detached(upd, &pv[..5]),
            (_, Some((_, _, del)), Kind::Delete) => db.execute_detached(del, &pv[..1]),
            _ => unreachable!(),
        };
        match res {
            Ok(_) => {
                committed += 1;
                if kind == Kind::Delete {
                    dels += 1;
                }
            }
            Err(Error::PrimaryKeyViolation { .. }) => pk_coll += 1, // lost a PK race
            Err(Error::UniqueViolation { .. }) => uniq_coll += 1,   // lost a tag race
            Err(Error::WriteConflict) => {} // optimistic retry exhausted — retryable
            Err(e) => return runtime(format!("collide writer {id}: unexpected error: {e}")),
        }
    }

    println!(
        "STATS id={id} committed={committed} pk_coll={pk_coll} uniq_coll={uniq_coll} dels={dels}",
    );
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Insert,
    Update,
    Delete,
}

fn parse_kv(s: &str) -> std::collections::HashMap<String, u64> {
    let mut m = std::collections::HashMap::new();
    for tok in s.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            if let Ok(n) = v.parse::<u64>() {
                m.insert(k.to_owned(), n);
            }
        }
    }
    m
}

fn int(v: &Value) -> Result<i64, Failure> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(Failure::Runtime(format!("expected int, got {other}"))),
    }
}
