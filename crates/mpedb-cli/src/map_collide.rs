//! `mpedb map-collide` — SIGKILL crash fuzz for the rRETL table-SET map
//! sync (design/DESIGN-RRETL.md §13). N source writers and N target
//! writers churn OPPOSITE sides of a live map inside one `.mpedb` while a
//! syncer process loops `rretl map sync` and the parent SIGKILLs it on a
//! tight cadence — kills land mid-classification, mid-push, mid-commit.
//! Named CONFLICTs are EXPECTED under churn (both sides moving between two
//! syncs is exactly what the writers produce) and the syncer swallows
//! them: a conflicted sync aborts WHOLE, so the next round sees the same
//! world. Anything else the syncer hits is a real bug and fails the run.
//!
//! The final drain is the contract. Quiesce the writers, resolve standing
//! conflicts deterministically (source wins: target row := forward(source
//! row), the row's state entry dropped so adopt-on-agree re-arbitrates),
//! then hard-assert the steady state: one more sync moves NOTHING (the
//! echo guard), `rretl_map_check` is CLEAN (no pending, no conflicts, no
//! diverged rows, no orphaned state), `rretl_fsck` is empty, and the row
//! sets correspond 1:1. Divergence after any number of kills means a row
//! was lost, duplicated or half-applied — the run fails with the evidence.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mpedb::spellfn::SpellLang;
use mpedb::{Database, ExecResult};
use mpedb_types::{Error, Value};

use crate::args;
use crate::mirror_collide::{check_bad_exits, kill_loop, reap};
use crate::util::{open_target, runtime, usage, CliResult, Failure, Rng, Watchdog};

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

/// One map, three mapped columns, all three backward-capable classes the
/// syncer must survive under fire: a guarded BIJECTIVE pair, a RESIDUAL
/// pair (the live-rex path), and an identity copy.
const MAP_TOML: &str = r#"
[map]
name = "m"

[[map.table]]
source = "src"
target = "ext"
  [[map.table.column]]
  source = "v"
  target = "vneg"
  pair = "neg"
  [[map.table.column]]
  source = "q"
  target = "qabs"
  pair = "mag"
  [[map.table.column]]
  source = "w"
  target = "wc"
"#;

fn setup(dir: &Path, secs: u64, keyspace: u64, stream: bool) -> Result<(String, PathBuf, u128), Failure> {
    std::fs::create_dir_all(dir)?;
    let dir = dir.canonicalize()?;
    let db_path = dir.join("map.mpedb");
    let _ = std::fs::remove_file(&db_path);
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "[database]\npath = \"{}\"\nsize_mb = 64\nmax_readers = 32\n\
             durability = \"none\"\n",
            db_path.display()
        ),
    )?;
    let cfg = cfg.to_str().unwrap().to_string();

    let db = open_target(&cfg)?;
    db.query(
        "CREATE TABLE src (id INTEGER PRIMARY KEY, v ANY, q ANY, w ANY)",
        &[],
    )?;
    // The tests' standing pairs: neg guards its domain (fractions, zero and
    // huge magnitudes are deterministic refusals — the writers respect it),
    // mag ⇄ sgn is the residual pair whose rex is computed LIVE at sync.
    let def = |src: &str| db.create_function(SpellLang::Python, src).map(|_| ());
    def("def neg(x):\n    if x % 1 != 0:\n        return 1 // 0\n    if x == 0:\n        return 1 // 0\n    if x > 4000000000:\n        return 1 // 0\n    if x < 0 - 4000000000:\n        return 1 // 0\n    return 0 - x\n")?;
    db.create_lens("neg", "neg", "neg", mpedb::lens::LensClass::Bijective)?;
    def("def mag(x):\n    if x < 0:\n        return 0 - x\n    return x\n")?;
    def("def sgn(x):\n    if x < 0:\n        return 1\n    return 0\n")?;
    def("def unmag(y, s):\n    if s == 1:\n        return 0 - y\n    return y\n")?;
    db.create_residual_lens("mag", "mag", "sgn", "unmag", mpedb::ColumnType::Int64)?;
    db.rretl_map_define(&if stream {
        MAP_TOML.replace("[map]", "[map]\nstream = true")
    } else {
        MAP_TOML.to_string()
    })?;

    let mut rng = Rng::seeded(&[0x6d61_7063, keyspace, secs]);
    let ins = db.prepare("INSERT INTO src (id, v, q, w) VALUES ($1, $2, $3, $4)")?;
    for id in 0..keyspace / 2 {
        db.execute(
            &ins,
            &[
                Value::Int(id as i64),
                Value::Int(legal_v(&mut rng)),
                Value::Int(1 + rng.below(1_000_000) as i64),
                Value::Int(rng.below(1_000_000) as i64),
            ],
        )?;
    }
    db.rretl_map_sync("m")?;
    // Do the daemon's FIRST run here too: it creates the cursor/run tables,
    // and that DDL invalidates every other process's prepared plans. Real
    // deployments take that hit once, at whatever moment the cron line
    // first fires; the harness takes it before the writers exist so the
    // fuzz measures the sync path and not one DDL bump.
    db.rretl_map_run("m", &mpedb::rretl_map_run::RunOptions::default())?;
    Ok((cfg, std::env::current_exe()?, now_ms() + secs as u128 * 1000))
}

/// Domain-legal value for the neg pair: integral, nonzero, |v| well under
/// the guard's 4e9 bound.
fn legal_v(rng: &mut Rng) -> i64 {
    let v = 1 + rng.below(1_000_000) as i64;
    if rng.below(2) == 0 {
        -v
    } else {
        v
    }
}

// ------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["dir", "writers", "secs", "kill-ms", "keyspace", "mode"],
        &["stream"],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let writers = p.u64_or("writers", 2)?.max(1);
    let secs = p.u64_or("secs", 5)?.max(1);
    let kill_ms = p.u64_or("kill-ms", 40)?.max(1);
    let keyspace = p.u64_or("keyspace", 24)?.max(2);
    // `sync` kills a ONE-TRANSACTION sync; `run` kills the daemon, whose
    // chunk commits are the newer risk — a kill between two commits must
    // leave the cursor and the rows it covers agreeing, because they are
    // written in the same transaction.
    let daemon = match p.value("mode").unwrap_or("sync") {
        "sync" => false,
        "run" => true,
        other => return usage(format!("--mode must be sync or run, got `{other}`")),
    };

    // The stream form (§15) puts the journal's triggers in the writers' path
    // and the drain in the syncer's — a different set of writes to be killed
    // between, so it gets its own fuzz.
    let stream = p.has("stream");
    let (cfg, exe, deadline) = setup(&dir, secs, keyspace, stream)?;
    let _wd = Watchdog::arm(secs + 60, "map-collide");
    let deadline_s = deadline.to_string();

    let mut writer_procs: Vec<Child> = Vec::new();
    for side in ["map-collide-awriter", "map-collide-bwriter"] {
        for id in 0..writers {
            writer_procs.push(
                Command::new(&exe)
                    .arg(side)
                    .args(["--config", &cfg])
                    .args(["--deadline", &deadline_s])
                    .args(["--keyspace", &keyspace.to_string()])
                    .args(["--id", &id.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()?,
            );
        }
    }

    let spawn = || -> std::io::Result<Child> {
        Command::new(&exe)
            .arg("map-collide-syncer")
            .args(["--config", &cfg])
            .args(["--deadline", &deadline_s])
            .args(["--daemon", if daemon { "1" } else { "0" }])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
    };
    let (kills, bad_exits) = kill_loop(deadline, kill_ms, &spawn)?;
    reap(writer_procs)?;

    // Final drain: converge, then hard-assert the steady state.
    let db = open_target(&cfg)?;
    let mut fixes = 0u64;
    let mut clean_streak = 0u32;
    while clean_streak < 2 {
        match db.rretl_map_sync("m") {
            Ok(r) => {
                if r.changed_total() == 0 {
                    clean_streak += 1;
                } else {
                    clean_streak = 0;
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("CONFLICT") {
                    return Err(e.into());
                }
                resolve_source_wins(&db, parse_conflict_key(&msg)?)?;
                fixes += 1;
                clean_streak = 0;
                if fixes > keyspace * 8 {
                    return runtime(format!(
                        "MAP-COLLIDE: conflict resolution did not converge after {fixes} \
                         source-wins fixes — an unresolvable conflict state"
                    ));
                }
            }
        }
    }

    let chk = db.rretl_map_check("m")?;
    if !chk.is_clean() {
        return runtime(format!(
            "MAP-COLLIDE DIVERGENCE after {kills} kill(s): the converged map is not \
             clean — {chk:?}"
        ));
    }
    let fsck = db.rretl_fsck()?;
    if !fsck.is_empty() {
        return runtime(format!(
            "MAP-COLLIDE after {kills} kill(s): fsck found {} problem(s): {fsck:?}",
            fsck.len()
        ));
    }
    let (n_src, n_ext) = (count(&db, "src")?, count(&db, "ext")?);
    if n_src != n_ext {
        return runtime(format!(
            "MAP-COLLIDE DIVERGENCE after {kills} kill(s): src has {n_src} row(s), \
             ext has {n_ext}"
        ));
    }
    check_bad_exits(bad_exits)?;
    // The journal must be EMPTY at the end. A leftover entry after the
    // final drain would mean a kill separated "the row was synced" from
    // "the entry was consumed" — the one thing §15.5's single transaction
    // exists to prevent.
    let left: i64 = db.rretl_map_backlog("m")?.iter().map(|(_, n)| n).sum();
    if left != 0 {
        return Err(Failure::Runtime(format!(
            "MAP-COLLIDE: {left} journal entr(ies) survived the final drain after \
             {kills} kill(s) — a kill separated the sync from the entry that named it"
        )));
    }
    println!(
        "map-collide({}{}): writers={writers}x2 secs={secs} syncer-kills={kills} \
         conflict-fixes={fixes} rows={n_src} check=clean fsck=clean journal=drained \
         — no row lost, duplicated or half-synced across kills",
        if daemon { "run" } else { "sync" },
        if stream { "+stream" } else { "" }
    );
    Ok(())
}

/// The conflict message names the row (`… row Int(5) — …`); the harness
/// key type is Int, so that is the only shape parsed here.
fn parse_conflict_key(msg: &str) -> Result<i64, Failure> {
    let tail = msg
        .split("row Int(")
        .nth(1)
        .ok_or_else(|| Failure::Runtime(format!("unparseable conflict message: {msg}")))?;
    let digits: String = tail.chars().take_while(|c| *c != ')').collect();
    digits
        .parse()
        .map_err(|_| Failure::Runtime(format!("unparseable conflict key in: {msg}")))
}

/// Source wins: target row := forward(source row) (or deleted with it),
/// and the row's state entry is dropped so the next sync re-arbitrates
/// through adopt-on-agree instead of seeing both sides moved again.
fn resolve_source_wins(db: &Database, key: i64) -> Result<(), Failure> {
    let src = match db.query("SELECT v, q, w FROM src WHERE id = $1", &[Value::Int(key)])? {
        ExecResult::Rows { rows, .. } => rows.into_iter().next(),
        other => return runtime(format!("unexpected read result: {other:?}")),
    };
    match src {
        Some(r) => {
            let (v, q) = match (&r[0], &r[1]) {
                (Value::Int(v), Value::Int(q)) => (*v, *q),
                other => return runtime(format!("non-int source values: {other:?}")),
            };
            let params = [
                Value::Int(key),
                Value::Int(-v),
                Value::Int(q.abs()),
                r[2].clone(),
            ];
            let hit = matches!(
                db.query(
                    "UPDATE ext SET vneg = $2, qabs = $3, wc = $4 WHERE id = $1",
                    &params,
                )?,
                ExecResult::Affected(n) if n > 0
            );
            if !hit {
                db.query(
                    "INSERT INTO ext (id, vneg, qabs, wc) VALUES ($1, $2, $3, $4)",
                    &params,
                )?;
            }
        }
        None => {
            db.query("DELETE FROM ext WHERE id = $1", &[Value::Int(key)])?;
        }
    }
    db.query(
        "DELETE FROM rretl_map_state WHERE map = 'm' AND k = $1",
        &[Value::Int(key)],
    )?;
    Ok(())
}

fn count(db: &Database, table: &str) -> Result<i64, Failure> {
    match db.query(&format!("SELECT count(*) FROM \"{table}\""), &[])? {
        ExecResult::Rows { rows, .. } => match rows.first().map(|r| r[0].clone()) {
            Some(Value::Int(n)) => Ok(n),
            other => runtime(format!("unexpected count: {other:?}")),
        },
        other => runtime(format!("unexpected read result: {other:?}")),
    }
}

// ------------------------------------------------------------------ children

fn parse_child(argv: &[String]) -> Result<(String, u128, u64, u64), Failure> {
    let p = args::parse(argv, &["config", "deadline", "keyspace", "id"], &[])?;
    let cfg = p.require("config")?.to_string();
    let deadline: u128 = p
        .require("deadline")?
        .parse()
        .map_err(|_| Failure::Usage("--deadline must be an integer".into()))?;
    let keyspace = p.require_u64("keyspace")?.max(2);
    let id = p.require_u64("id")?;
    Ok((cfg, deadline, keyspace, id))
}

/// Retry transient outcomes a churning writer legitimately sees: the
/// writer lock timing out under a kill storm, and insert races.
fn tolerable(e: &Error) -> bool {
    matches!(
        e,
        Error::Busy
            | Error::PrimaryKeyViolation { .. }
            | Error::UniqueViolation { .. }
            // Any DDL in the file invalidates prepared plans; re-preparing
            // is the caller's job, and the writers do it by re-running the
            // statement next iteration.
            | Error::PlanInvalidated
    )
}

/// Hidden subcommand: churn the SOURCE side with domain-legal values.
pub fn run_awriter(argv: &[String]) -> CliResult {
    let (cfg, deadline, keyspace, id) = parse_child(argv)?;
    let db = open_target(&cfg)?;
    let (upd, ins, del) = (
        "UPDATE src SET v = $2, q = $3, w = $4 WHERE id = $1",
        "INSERT INTO src (id, v, q, w) VALUES ($1, $2, $3, $4)",
        "DELETE FROM src WHERE id = $1",
    );
    let mut rng = Rng::seeded(&[0x00a1_1ce5, id, keyspace]);
    while now_ms() < deadline {
        for _ in 0..8 {
            let key = Value::Int(rng.below(keyspace) as i64);
            let r = if rng.below(6) == 0 {
                db.query(del, std::slice::from_ref(&key)).map(|_| ())
            } else {
                let params = [
                    key.clone(),
                    Value::Int(legal_v(&mut rng)),
                    Value::Int(1 + rng.below(1_000_000) as i64),
                    Value::Int(rng.below(1_000_000) as i64),
                ];
                match db.query(upd, &params) {
                    Ok(ExecResult::Affected(n)) if n > 0 => Ok(()),
                    Ok(_) => db.query(ins, &params).map(|_| ()),
                    Err(e) => Err(e),
                }
            };
            match r {
                Ok(()) => {}
                Err(e) if tolerable(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Hidden subcommand: churn the TARGET side with IN-IMAGE values only —
/// vneg gets nonzero integers (the neg pair's image), qabs gets ≥ 1 (the
/// mag pair's image, and never 0 so the live residual survives the edit),
/// wc is a free identity copy. Never inserts: creation through the
/// residual column is a designed refusal, and this harness must only feed
/// the syncer conflicts, not refusals.
pub fn run_bwriter(argv: &[String]) -> CliResult {
    let (cfg, deadline, keyspace, id) = parse_child(argv)?;
    let db = open_target(&cfg)?;
    let (upd, del) = (
        "UPDATE ext SET vneg = $2, qabs = $3, wc = $4 WHERE id = $1",
        "DELETE FROM ext WHERE id = $1",
    );
    let mut rng = Rng::seeded(&[0xb0b_cafe, id, keyspace]);
    while now_ms() < deadline {
        for _ in 0..8 {
            let key = Value::Int(rng.below(keyspace) as i64);
            let r = if rng.below(8) == 0 {
                db.query(del, std::slice::from_ref(&key)).map(|_| ())
            } else {
                db.query(
                    upd,
                    &[
                        key,
                        Value::Int(legal_v(&mut rng)),
                        Value::Int(1 + rng.below(1_000_000) as i64),
                        Value::Int(rng.below(1_000_000) as i64),
                    ],
                )
                .map(|_| ())
            };
            match r {
                Ok(()) => {}
                Err(e) if tolerable(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Hidden subcommand: loop `map sync` until the deadline (or until the
/// parent SIGKILLs it). CONFLICT under churn is the expected steady diet
/// (aborted whole — retrying is correct); Busy is the lock under a kill
/// storm. Every other error is a bug this harness exists to catch, and
/// self-exiting with it is what the parent counts as a failure.
pub fn run_syncer(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["config", "deadline", "daemon"], &[])?;
    let cfg = p.require("config")?.to_string();
    let deadline: u128 = p
        .require("deadline")?
        .parse()
        .map_err(|_| Failure::Usage("--deadline must be an integer".into()))?;
    let daemon = p.value("daemon") == Some("1");
    let db = open_target(&cfg)?;
    // The daemon swallows conflicts itself (counted, skipped), so only the
    // one-txn sync needs the CONFLICT arm here.
    let opts = mpedb::rretl_map_run::RunOptions {
        max_secs: Some(1),
        ..Default::default()
    };
    while now_ms() < deadline {
        let r = if daemon {
            db.rretl_map_run("m", &opts).map(|_| ())
        } else {
            db.rretl_map_sync("m").map(|_| ())
        };
        match r {
            Ok(()) => {}
            Err(Error::Busy) => {}
            Err(e) if e.to_string().contains("CONFLICT") => {}
            Err(e) => return Err(e.into()),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
