//! **Arm F — many editors on one document (#146).**
//!
//! Arm E asks what acting on a notification costs. This asks the shape the
//! question actually arrives in: *a hundred people have one Google-Docs-like
//! document open, and someone else's document must not care.*
//!
//! Three models of "a document", measured against each other and against
//! PostgreSQL doing the same three things the natural way:
//!
//! * **F-field — the document is one field.** Every editor read-modify-writes
//!   `doc.body`. Two concurrent edits are a genuine lost update, so this must
//!   serialize; the only question is what it costs and whether the engine
//!   notices. The `-noguard` control answers the second part by losing edits.
//! * **F-blocks — the document is a table of blocks.** Editor `w` owns block
//!   `w` of the same document: same table, same document, different rows. This
//!   is the model [`ordkey`](mpedb_types::ordkey) exists for, and the one that
//!   is supposed to scale.
//! * **F-move — one block, two columns.** Half the workers edit a block's
//!   `body`; the other half move that same block by rewriting its `ord`. Same
//!   row. Column granularity (#146 K1) says a move and an edit are not a
//!   conflict; a row lock says they are. PostgreSQL is here to show which.
//!
//! And the independence claim, which is the user-visible one: **F-docs** holds
//! the editors *per document* fixed at [`EDITORS_PER_DOC`] and scales the
//! number of documents. Every document is internally contended — its editors
//! all fight over its one field — so if contention were a property of the
//! *table*, adding documents would not add throughput. Per-document rates are
//! printed alongside the total, because a doubled total with one document
//! starved is a different result from a doubled total with both flat.
//!
//! ## The lost-update check
//!
//! Throughput without correctness is meaningless here: an engine that simply
//! drops concurrent edits is infinitely fast. So in `field` mode every worker
//! owns a 16-byte slot of the body and writes its action counter there, by
//! read-modify-write of the whole field. Afterwards the parent reads the field
//! back and checks every slot reached the number of actions that worker ran.
//! A slot that fell short **is** a lost edit, and the unguarded control is
//! expected to show them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::eng_pg::PgServer;
use crate::util::BResult;

/// Actions per worker. `MPEDB_F_ACTIONS` overrides.
fn actions() -> usize {
    env_u64("MPEDB_F_ACTIONS", 40) as usize
}
/// Think time between reading the document and writing it back. The window a
/// lock is held across, or not. `MPEDB_F_DELAY_MS` overrides.
fn delay_ms() -> u64 {
    env_u64("MPEDB_F_DELAY_MS", 10)
}
/// The feedback deadline the contract promises (#150). Every arm reports the
/// fraction of actions that finished inside it, because "did everyone know
/// within a second" is a fraction, not a percentile.
fn deadline_ms() -> u64 {
    env_u64("MPEDB_F_DEADLINE_MS", 1000)
}
/// Think time for the calibration arms. **Zero by default, deliberately.**
/// `F-cap` exists to measure what the ENGINE costs per edit, and a 10 ms sleep
/// inside the measured window would put the benchmark's own artefact into the
/// derived editor cap.
fn cap_delay_ms() -> u64 {
    env_u64("MPEDB_F_CAP_DELAY_MS", 0)
}
/// Actions per editor in the calibration arms. Separate from
/// [`actions`] because `F-words` runs a thousand editors: at the main arms'
/// count it would dominate the whole cell's runtime without sharpening the
/// number it exists to produce.
fn cap_actions() -> usize {
    env_u64("MPEDB_F_CAP_ACTIONS", 10) as usize
}
fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Editors to scale across. Small, because the serialized arms cost
/// `workers × actions × think` seconds by construction.
const WORKERS: [usize; 3] = [2, 4, 8];
/// `F-cap`: editors piled onto ONE block — the contention unit. The sweep is
/// what turns "50 per section" from a guess into a measured number.
const CAP_SWEEP: [usize; 6] = [2, 4, 8, 16, 32, 64];
/// `F-words`: blocks a paragraph is split into, at [`WORDS_EDITORS`] editors
/// each. If a block can be a word, capacity should be blocks × cap — this is
/// the arm that makes that claim falsifiable rather than arithmetic.
const WORDS_BLOCKS: [usize; 3] = [1, 4, 20];
const WORDS_EDITORS: usize = 50;
/// `F-batch`: edits folded into ONE commit. The question this settles is
/// whether the write ceiling is per **commit** or per **edit** — because if it
/// is per commit, an editor's answer never had to wait for one.
const BATCH_SIZES: [usize; 4] = [1, 8, 64, 256];
/// Body length. Big enough for 8 editors' 16-byte slots with room to spare,
/// small enough that the measurement is not about row size.
const BODY: usize = 1024;
/// Bytes of the body each editor owns in `field` mode.
const SLOT: usize = 16;
/// Editors per document in the document-scaling arm, held fixed while the
/// document count grows.
const EDITORS_PER_DOC: usize = 4;
/// Document counts to scale across, at [`EDITORS_PER_DOC`] editors each.
const DOCS: [usize; 3] = [1, 2, 4];
/// Block ids are `doc * BLOCK_STRIDE + slot`, so a block is a PK point and the
/// guard can name it exactly without a secondary index.
const BLOCK_STRIDE: i64 = 1000;

pub struct DocResult {
    pub engine: &'static str,
    pub arm: String,
    pub workers: usize,
    pub actions_per_sec: f64,
    pub p50_ms: u64,
    pub p99_ms: u64,
    pub retries: u64,
    /// Fraction inside the deadline, and inside twice it.
    pub within: (f64, f64),
    /// Engine share of one action, p50 µs — the service time a cap derives from.
    pub work_p50_us: u64,
    /// `(cleared, overlap, snapshot_too_old, ring_gap)` — WHY the guard
    /// refused, which a retry count on its own cannot say.
    pub verdicts: (u64, u64, u64, u64, u64),
    /// Edits that were written and then overwritten by a concurrent editor
    /// that had read the older value. `None` where the arm cannot lose one.
    pub lost: Option<u64>,
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

/// Slots the body holds. Beyond this, editors share a slot — which is fine for
/// the calibration arms (they measure timing, not lost updates) and never
/// happens in `field` mode, the only arm whose `lost` column is read.
const SLOTS: usize = BODY / SLOT;

/// Write `n` into worker `w`'s slot of `body`, leaving every other slot alone.
/// This is the read-modify-write that makes a lost update possible.
fn splice_slot(body: &str, w: usize, n: usize) -> String {
    let mut b = body.as_bytes().to_vec();
    b.resize(BODY, b'.');
    let at = (w % SLOTS) * SLOT;
    let mark = format!("{n:015} ");
    b[at..at + SLOT].copy_from_slice(mark.as_bytes());
    String::from_utf8_lossy(&b).into_owned()
}

/// The counter in worker `w`'s slot, or `None` if it was never written.
fn read_slot(body: &str, w: usize) -> Option<usize> {
    let b = body.as_bytes();
    let at = (w % SLOTS) * SLOT;
    if b.len() < at + SLOT {
        return None;
    }
    std::str::from_utf8(&b[at..at + SLOT]).ok()?.trim().parse().ok()
}

// ------------------------------------------------------------------ worker

/// One editor, in its own process. Prints `DOC <d>`, then one line per action
/// (`<micros>`), then `RETRIES <n>`.
///
/// Argument order: `<engine> <target> <mode> <worker> <workers> <docs>`.
pub fn worker_main(argv: &[String]) -> BResult<()> {
    let [engine, target, mode, w, _nw, nd] = argv else {
        return Err("doc-worker needs <engine> <target> <mode> <worker> <workers> <docs>".into());
    };
    let w: usize = w.parse().map_err(|_| "bad worker id")?;
    let nd: usize = nd.parse().map_err(|_| "bad doc count")?;
    let guarded = !mode.ends_with("-noguard");
    // The attribution control (#143). `-coarse` declares one extra statement —
    // a whole-table scan — and changes nothing else. A scan has no point key,
    // so the guard's region widens to "anywhere in this table", which is
    // exactly what the guard was before key regions existed. If document
    // independence came from anything other than the declared region being a
    // point, this arm would still scale. It does not, and that is the proof.
    let coarse = mode.ends_with("-coarse");
    let kind = mode.split('-').next().unwrap_or(mode);

    // Which document this editor has open, and which block inside it. Editors
    // are dealt round-robin so every document gets the same number of them.
    let doc = (w % nd) as i64;
    let seat = w / nd;
    // In `move` mode alternate seats are movers rather than editors: same row,
    // different column.
    let mover = kind == "move" && seat % 2 == 1;
    // `words<N>` splits the paragraph into N blocks; editor w takes block
    // `w % N`. `cap` is the N = 1 extreme, kept separate because its sweep is
    // over editors rather than over blocks.
    let words: Option<usize> = kind.strip_prefix("words").and_then(|n| n.parse().ok());
    let batch: Option<usize> = kind.strip_prefix("batch").and_then(|n| n.parse().ok());
    let calibrating = words.is_some() || batch.is_some() || kind == "cap";

    let n_actions = if calibrating { cap_actions() } else { actions() };
    // The calibration arms measure the engine, so they do not sleep.
    let think = Duration::from_millis(if calibrating { cap_delay_ms() } else { delay_ms() });
    // (total, engine-share) per action.
    let mut lat: Vec<(u64, u64)> = Vec::with_capacity(n_actions);
    let mut retries = 0u64;
    // (cleared, overlap, snapshot_too_old, ring_gap). A refusal count alone
    // cannot say whether the guard caught a real conflict or ran out of ring,
    // and those call for opposite fixes.
    let mut verdicts = (0u64, 0u64, 0u64, 0u64, 0u64);
    println!("DOC {doc}");

    match engine.as_str() {
        "mpedb" => {
            use mpedb::{Config, Database, Error, ExecResult, Value};
            let db = Database::open(Path::new(target)).or_else(|_| {
                Database::open_with_config(Config::from_toml_str(&std::fs::read_to_string(
                    target,
                )?)?)
            })?;
            // The declared surface: exactly the statements this editor may
            // run. Reads included — that is what makes the guard catch a lost
            // update rather than only a write-write overlap.
            let (read_sql, write_sql) = match (kind, mover) {
                ("field", _) => (
                    "SELECT body FROM doc WHERE id = $1",
                    "UPDATE doc SET body = $1 WHERE id = $2",
                ),
                (_, false) => (
                    "SELECT body FROM block WHERE id = $1",
                    "UPDATE block SET body = $1 WHERE id = $2",
                ),
                (_, true) => (
                    "SELECT ord FROM block WHERE id = $1",
                    "UPDATE block SET ord = $1 WHERE id = $2",
                ),
            };
            let key = match kind {
                "field" => doc,
                "blocks" => doc * BLOCK_STRIDE + seat as i64,
                // `words<N>`: editors spread over N blocks of one paragraph.
                _ if words.is_some() => {
                    doc * BLOCK_STRIDE + (w % words.unwrap()) as i64
                }
                // `move` and `cap`: every worker on the SAME block, so the only
                // thing separating them is which column they write (move), or
                // nothing at all (cap).
                _ => doc * BLOCK_STRIDE,
            };
            // Declared WITH the values. Without them `WHERE id = $2` names
            // every row of the table and the guard has to say so — which is
            // sound, and is what the `-coarse` control measures deliberately.
            // The placeholder for `$1` is never read: only the key parts are
            // resolved.
            let read_p = [Value::Int(key)];
            let write_p = [Value::Int(0), Value::Int(key)];
            let mut may_run: Vec<(&str, &[Value])> =
                vec![(read_sql, &read_p[..]), (write_sql, &write_p[..])];
            if coarse {
                may_run.push(("SELECT id FROM doc", &[]));
            }

            if let Some(k) = batch {
                // Each worker owns a disjoint slice of blocks, so the batch's
                // edits never conflict with each other — the arm is about the
                // COST of a commit, not about contention, which `F-cap` covers.
                let base = doc * BLOCK_STRIDE + (w * k) as i64;
                let mut declared: Vec<[Value; 2]> = Vec::with_capacity(k);
                for j in 0..k {
                    declared.push([Value::Int(0), Value::Int(base + j as i64)]);
                }
                for i in 0..n_actions {
                    let t0 = Instant::now();
                    loop {
                        let snap = db.snapshot_txn();
                        let mut may: Vec<(&str, &[Value])> = Vec::with_capacity(k);
                        for d in declared.iter().take(k) {
                            may.push((write_sql, &d[..]));
                        }
                        let mut s = if guarded {
                            db.begin_guarded_with(snap, &may)?
                        } else {
                            db.begin()?
                        };
                        for j in 0..k {
                            s.query(
                                write_sql,
                                &[
                                    Value::Text(format!("{i}-{j}")),
                                    Value::Int(base + j as i64),
                                ],
                            )?;
                        }
                        match s.commit() {
                            Ok(()) => break,
                            Err(Error::WriteConflict) => {
                                retries += 1;
                                continue;
                            }
                            Err(e) => return Err(format!("mpedb commit: {e}").into()),
                        }
                    }
                    let el = t0.elapsed().as_micros() as u64;
                    lat.push((el, el));
                }
                verdicts = db.guard_stats();
                for (total, work) in &lat {
                    println!("L {total} {work}");
                }
                println!("RETRIES {retries}");
                println!(
                    "VERDICTS {} {} {} {} {}",
                    verdicts.0, verdicts.1, verdicts.2, verdicts.3, verdicts.4
                );
                return Ok(());
            }
            for i in 0..n_actions {
                let t0 = Instant::now();
                let mut work = Duration::ZERO;
                loop {
                    let snap = db.snapshot_txn();
                    let cur = match db.query(read_sql, &[Value::Int(key)])? {
                        ExecResult::Rows { rows, .. } if !rows.is_empty() => rows[0][0].clone(),
                        _ => Value::Null,
                    };
                    // Think — holding nothing.
                    std::thread::sleep(think);
                    // From here to the commit's answer is the ENGINE's share of
                    // the action. Separating it is what lets a derived editor
                    // cap be free of the benchmark's own sleep.
                    let w0 = Instant::now();
                    let mut s = if guarded {
                        db.begin_guarded_with(snap, &may_run)?
                    } else {
                        db.begin()?
                    };
                    let next = if mover {
                        // A move rewrites the ordering column only.
                        let o = match cur {
                            Value::Int(o) => o,
                            _ => 0,
                        };
                        Value::Int(o + 1)
                    } else {
                        let body = match &cur {
                            Value::Text(t) => t.as_str(),
                            _ => "",
                        };
                        Value::Text(splice_slot(body, w, i))
                    };
                    s.query(write_sql, &[next, Value::Int(key)])?;
                    let done = s.commit();
                    work += w0.elapsed();
                    match done {
                        Ok(()) => break,
                        Err(Error::WriteConflict) => {
                            retries += 1;
                            continue;
                        }
                        Err(e) => return Err(format!("mpedb commit: {e}").into()),
                    }
                }
                lat.push((t0.elapsed().as_micros() as u64, work.as_micros() as u64));
            }
            verdicts = db.guard_stats();
        }
        "postgres" => {
            let mut c = postgres::Client::connect(target, postgres::NoTls)
                .map_err(|e| format!("pg connect: {e}"))?;
            let (sel, upd, key) = match kind {
                "field" => (
                    "SELECT body FROM doc WHERE id = $1 FOR UPDATE",
                    "UPDATE doc SET body = $1 WHERE id = $2",
                    doc,
                ),
                "blocks" => (
                    "SELECT body FROM block WHERE id = $1 FOR UPDATE",
                    "UPDATE block SET body = $1 WHERE id = $2",
                    doc * BLOCK_STRIDE + seat as i64,
                ),
                _ => (
                    "SELECT body FROM block WHERE id = $1 FOR UPDATE",
                    "UPDATE block SET body = $1 WHERE id = $2",
                    doc * BLOCK_STRIDE,
                ),
            };
            for i in 0..n_actions {
                let t0 = Instant::now();
                let mut txn = c.transaction().map_err(|e| format!("begin: {e}"))?;
                if mover {
                    // The same move, written the way PostgreSQL makes you write
                    // it: the row is locked, so it does not matter that only
                    // one column changes.
                    let row = txn
                        .query_one(
                            "SELECT ord FROM block WHERE id = $1 FOR UPDATE",
                            &[&key],
                        )
                        .map_err(|e| format!("select ord: {e}"))?;
                    let o: i64 = row.get(0);
                    std::thread::sleep(think);
                    txn.execute("UPDATE block SET ord = $1 WHERE id = $2", &[&(o + 1), &key])
                        .map_err(|e| format!("update ord: {e}"))?;
                } else {
                    let row = txn
                        .query_one(sel, &[&key])
                        .map_err(|e| format!("select for update: {e}"))?;
                    let body: String = row.get(0);
                    std::thread::sleep(think);
                    let next = splice_slot(&body, w, i);
                    txn.execute(upd, &[&next, &key])
                        .map_err(|e| format!("update: {e}"))?;
                }
                txn.batch_execute("NOTIFY doc_ch, 'x'").map_err(|e| format!("notify: {e}"))?;
                txn.commit().map_err(|e| format!("commit: {e}"))?;
                // PostgreSQL holds its row lock across the think time, so its
                // "engine share" is the whole transaction minus the sleep —
                // there is no equivalent split to make.
                let total = t0.elapsed();
                lat.push((total.as_micros() as u64, total.saturating_sub(think).as_micros() as u64));
            }
        }
        other => return Err(format!("unknown engine {other}").into()),
    }

    for (total, work) in &lat {
        println!("L {total} {work}");
    }
    println!("RETRIES {retries}");
    println!(
        "VERDICTS {} {} {} {} {}",
        verdicts.0, verdicts.1, verdicts.2, verdicts.3, verdicts.4
    );
    Ok(())
}

// ------------------------------------------------------------------ parent

struct Run {
    aps: f64,
    p50: u64,
    p99: u64,
    /// Engine share of one action (think time excluded), p50 in microseconds.
    /// This is the service time the editor cap is derived from.
    work_p50_us: u64,
    /// Fraction of actions that finished inside the deadline, and inside twice
    /// it. The contract is "everyone knows within a second", which is a
    /// fraction — a percentile answers a different question.
    within: (f64, f64),
    retries: u64,
    /// `(cleared, overlap, snapshot_too_old, ring_gap)` summed over editors.
    verdicts: (u64, u64, u64, u64, u64),
    /// actions/s per document, for the independence arm.
    per_doc: BTreeMap<i64, f64>,
}

fn spawn_and_collect(
    engine: &'static str,
    target: &str,
    mode: &str,
    workers: usize,
    docs: usize,
) -> BResult<Run> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let t0 = Instant::now();
    let mut kids = Vec::new();
    for w in 0..workers {
        kids.push(
            Command::new(&exe)
                .args([
                    "--doc-worker",
                    engine,
                    target,
                    mode,
                    &w.to_string(),
                    &workers.to_string(),
                    &docs.to_string(),
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("spawn worker: {e}"))?,
        );
    }
    let mut lat: Vec<u64> = Vec::new();
    let mut work: Vec<u64> = Vec::new();
    let mut retries = 0u64;
    let mut verd = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut doc_actions: BTreeMap<i64, usize> = BTreeMap::new();
    for k in kids {
        let out = k.wait_with_output().map_err(|e| format!("worker wait: {e}"))?;
        if !out.status.success() {
            return Err(format!("worker failed: {}", String::from_utf8_lossy(&out.stderr)).into());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut doc = 0i64;
        for line in text.lines() {
            if let Some(d) = line.strip_prefix("DOC ") {
                doc = d.trim().parse().unwrap_or(0);
            } else if let Some(n) = line.strip_prefix("RETRIES ") {
                retries += n.trim().parse::<u64>().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("VERDICTS ") {
                let n: Vec<u64> =
                    v.split_whitespace().map(|x| x.parse().unwrap_or(0)).collect();
                if n.len() == 5 {
                    verd.0 += n[0];
                    verd.1 += n[1];
                    verd.2 += n[2];
                    verd.3 += n[3];
                    verd.4 += n[4];
                }
            } else if let Some(v) = line.strip_prefix("L ") {
                let mut it = v.split_whitespace();
                if let (Some(t), Some(w)) = (it.next(), it.next()) {
                    let (t, w) = (t.parse().unwrap_or(0), w.parse().unwrap_or(0));
                    lat.push(t);
                    work.push(w);
                    *doc_actions.entry(doc).or_default() += 1;
                }
            }
        }
    }
    let elapsed = t0.elapsed().as_secs_f64();
    lat.sort_unstable();
    work.sort_unstable();
    let d1 = deadline_ms() * 1000;
    let d2 = d1 * 2;
    let n = lat.len().max(1) as f64;
    let within = (
        lat.iter().filter(|&&v| v <= d1).count() as f64 / n,
        lat.iter().filter(|&&v| v <= d2).count() as f64 / n,
    );
    let per_worker = if mode.starts_with("cap") || mode.starts_with("words") {
        cap_actions()
    } else {
        actions()
    };
    Ok(Run {
        aps: (workers * per_worker) as f64 / elapsed,
        p50: pct(&lat, 0.50) / 1000,
        p99: pct(&lat, 0.99) / 1000,
        work_p50_us: pct(&work, 0.50),
        within,
        retries,
        verdicts: verd,
        per_doc: doc_actions.into_iter().map(|(d, n)| (d, n as f64 / elapsed)).collect(),
    })
}

const SCHEMA: &str = r#"
[[table]]
name = "doc"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "body"
  type = "text"

[[table]]
name = "block"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "doc"
  type = "int64"

  [[table.column]]
  name = "ord"
  type = "int64"

  [[table.column]]
  name = "body"
  type = "text"
"#;

/// Fresh database per measurement — a run must not inherit the previous one's
/// row sizes, or the later arms are measured against a bigger tree.
fn mpedb_setup(dir: &Path, docs: usize, seats: usize) -> BResult<String> {
    let cfg = dir.join("doc-load.toml");
    let db = dir.join("doc-load.mpedb");
    let _ = std::fs::remove_file(&db);
    std::fs::write(
        &cfg,
        format!(
            "[database]\npath = \"{}\"\nsize_mb = 256\nmax_readers = 64\n\
             durability = \"wal\"\n{SCHEMA}",
            db.display()
        ),
    )?;
    let s = cfg.to_string_lossy().into_owned();
    {
        use mpedb::{Config, Database, Value};
        let d = Database::open_with_config(Config::from_toml_str(&std::fs::read_to_string(&s)?)?)?;
        let blank = ".".repeat(BODY);
        for doc in 0..docs as i64 {
            d.query(
                "INSERT INTO doc (id, body) VALUES ($1, $2)",
                &[Value::Int(doc), Value::Text(blank.clone())],
            )?;
            for seat in 0..seats as i64 {
                d.query(
                    "INSERT INTO block (id, doc, ord, body) VALUES ($1, $2, $3, $4)",
                    &[
                        Value::Int(doc * BLOCK_STRIDE + seat),
                        Value::Int(doc),
                        Value::Int(seat),
                        Value::Text(blank.clone()),
                    ],
                )?;
            }
        }
    }
    Ok(s)
}

/// Read every editor's slot back and count the ones that fell short of the
/// actions that editor ran. That count **is** the number of lost edits.
fn mpedb_lost(target: &str, workers: usize) -> BResult<u64> {
    use mpedb::{Config, Database, ExecResult, Value};
    let d = Database::open_with_config(Config::from_toml_str(&std::fs::read_to_string(target)?)?)?;
    let body = match d.query("SELECT body FROM doc WHERE id = $1", &[Value::Int(0)])? {
        ExecResult::Rows { rows, .. } if !rows.is_empty() => match &rows[0][0] {
            Value::Text(t) => t.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    Ok(lost_in(&body, workers))
}

fn lost_in(body: &str, workers: usize) -> u64 {
    let want = actions() - 1;
    (0..workers).filter(|&w| read_slot(body, w) != Some(want)).count() as u64
}

fn pg_setup(pg: &PgServer, docs: usize, seats: usize) -> BResult<String> {
    let conn = pg.conn_str();
    let mut c =
        postgres::Client::connect(&conn, postgres::NoTls).map_err(|e| format!("pg setup: {e}"))?;
    c.batch_execute(
        "DROP TABLE IF EXISTS doc; DROP TABLE IF EXISTS block;
         CREATE TABLE doc (id bigint primary key, body text not null);
         CREATE TABLE block (id bigint primary key, doc bigint, ord bigint, body text not null);",
    )
    .map_err(|e| format!("pg schema: {e}"))?;
    let blank = ".".repeat(BODY);
    for doc in 0..docs as i64 {
        c.execute("INSERT INTO doc (id, body) VALUES ($1, $2)", &[&doc, &blank])
            .map_err(|e| format!("pg seed doc: {e}"))?;
        for seat in 0..seats as i64 {
            c.execute(
                "INSERT INTO block (id, doc, ord, body) VALUES ($1, $2, $3, $4)",
                &[&(doc * BLOCK_STRIDE + seat), &doc, &seat, &blank],
            )
            .map_err(|e| format!("pg seed block: {e}"))?;
        }
    }
    Ok(conn)
}

fn pg_lost(conn: &str, workers: usize) -> BResult<u64> {
    let mut c =
        postgres::Client::connect(conn, postgres::NoTls).map_err(|e| format!("pg check: {e}"))?;
    let row = c
        .query_one("SELECT body FROM doc WHERE id = 0", &[])
        .map_err(|e| format!("pg check select: {e}"))?;
    let body: String = row.get(0);
    Ok(lost_in(&body, workers))
}

pub fn run(scratch: PathBuf) -> BResult<()> {
    std::fs::create_dir_all(&scratch)?;
    println!(
        "arm F: many editors on ONE document. {} actions/editor, {} ms think time between \
         reading the document and writing it back, REAL PROCESSES.",
        actions(),
        delay_ms()
    );
    println!(
        "  F-field  = the document is one field; every editor read-modify-writes it.\n  \
         F-blocks = the document is a table of blocks; editor w owns block w of the SAME doc.\n  \
         F-move   = one block, two columns: half edit `body`, half move it by rewriting `ord`.\n  \
         postgres holds SELECT ... FOR UPDATE across the think time; mpedb holds nothing.\n  \
         `lost` counts editors whose final edit was overwritten — a correctness column, not a \
         performance one."
    );
    println!();

    let mut rows: Vec<DocResult> = Vec::new();
    let seats = *WORKERS
        .iter()
        .max()
        .unwrap()
        .max(&EDITORS_PER_DOC)
        .max(WORDS_BLOCKS.iter().max().unwrap());

    for mode in ["field-noguard", "field", "blocks", "move"] {
        for &w in &WORKERS {
            let target = mpedb_setup(&scratch, 1, seats)?;
            let r = spawn_and_collect("mpedb", &target, mode, w, 1)?;
            let lost = if mode.starts_with("field") {
                Some(mpedb_lost(&target, w)?)
            } else {
                None
            };
            rows.push(DocResult {
                engine: "mpedb",
                arm: format!("F-{mode}"),
                workers: w,
                actions_per_sec: r.aps,
                p50_ms: r.p50,
                p99_ms: r.p99,
                retries: r.retries,
                within: r.within,
                work_p50_us: r.work_p50_us,
                verdicts: r.verdicts,
                lost,
            });
        }
    }

    // The independence arm: N documents, EDITORS_PER_DOC editors each, every
    // document internally contended on its own single field.
    let mut mpedb_two = Vec::new();
    for (mode, tag) in [("field", ""), ("field-coarse", "-coarse")] {
        for &nd in &DOCS {
            let w = nd * EDITORS_PER_DOC;
            let target = mpedb_setup(&scratch, nd, seats)?;
            let r = spawn_and_collect("mpedb", &target, mode, w, nd)?;
            if tag.is_empty() {
                mpedb_two.push((nd, r.per_doc.clone()));
            }
            rows.push(DocResult {
                engine: "mpedb",
                arm: format!("F-docs×{nd}{tag}"),
                workers: w,
                actions_per_sec: r.aps,
                p50_ms: r.p50,
                p99_ms: r.p99,
                retries: r.retries,
                within: r.within,
                work_p50_us: r.work_p50_us,
                verdicts: r.verdicts,
                lost: None,
            });
        }
    }

    // ---- C1: calibration. What does ONE edit cost the engine, and how many
    // editors can share a block before the deadline stops being met? Both arms
    // run with no artificial think time, so the answer is about the engine
    // rather than about this benchmark's sleep.
    let mut cap_rows: Vec<(usize, f64, u64, u64, u64)> = Vec::new();
    for &w in &CAP_SWEEP {
        let target = mpedb_setup(&scratch, 1, seats)?;
        let r = spawn_and_collect("mpedb", &target, "cap", w, 1)?;
        cap_rows.push((w, r.within.0, r.work_p50_us, r.p99, r.retries));
    }

    // ---- C1: does splitting multiply capacity? N blocks, 50 editors each.
    // The control matters more here than anywhere else on this page: identical
    // work with the guard OFF. If the unguarded arm flattens at the same rate,
    // the ceiling is the single writer lock (or the machine), and no amount of
    // further splitting can move it — the guard is not what is in the way.
    let mut word_rows: Vec<(usize, f64, f64, u64, u64, u64, f64)> = Vec::new();
    for &nb in &WORDS_BLOCKS {
        let editors = WORDS_EDITORS * nb;
        let ctl_target = mpedb_setup(&scratch, 1, seats)?;
        let ctl =
            spawn_and_collect("mpedb", &ctl_target, &format!("words{nb}-noguard"), editors, 1)?;
        let target = mpedb_setup(&scratch, 1, seats)?;
        let r = spawn_and_collect("mpedb", &target, &format!("words{nb}"), editors, 1)?;
        // `overlap` is the attribution that decides what this arm means. Zero
        // conflicts with flat throughput says the ceiling is the single writer
        // lock, not the guard — and no amount of further splitting moves it.
        word_rows.push((nb, r.aps, r.within.0, r.p99, r.retries, r.verdicts.1, ctl.aps));
    }

    // ---- C1b: is the write ceiling per COMMIT or per EDIT? Eight editors,
    // each folding K edits into one commit. If edits/s scales with K, an
    // editor's answer never had to wait for a commit of its own — and the
    // "total editors <= deadline x commit rate" limit was a limit on how the
    // benchmark was written, not on the engine.
    let mut batch_rows: Vec<(usize, f64, f64, u64)> = Vec::new();
    for &k in &BATCH_SIZES {
        let target = mpedb_setup(&scratch, 1, 8 * k)?;
        let r = spawn_and_collect("mpedb", &target, &format!("batch{k}"), 8, 1)?;
        // aps counts COMMITS; each carries k edits.
        batch_rows.push((k, r.aps, r.aps * k as f64, r.p99));
    }

    let datadir = scratch.join("pgdata-doc");
    let sockdir = scratch.join("pgsock-doc");
    let _ = std::fs::remove_dir_all(&datadir);
    std::fs::create_dir_all(&sockdir)?;
    let mut pg_two = Vec::new();
    match PgServer::start_general_conn(datadir, sockdir, "on", "on", 256) {
        Ok(pg) => {
            for mode in ["field", "blocks", "move"] {
                for &w in &WORKERS {
                    let conn = pg_setup(&pg, 1, seats)?;
                    let r = spawn_and_collect("postgres", &conn, mode, w, 1)?;
                    let lost = if mode == "field" { Some(pg_lost(&conn, w)?) } else { None };
                    rows.push(DocResult {
                        engine: "postgres",
                        arm: format!("F-{mode}"),
                        workers: w,
                        actions_per_sec: r.aps,
                        p50_ms: r.p50,
                        p99_ms: r.p99,
                        retries: 0,
                        within: r.within,
                        work_p50_us: r.work_p50_us,
                        verdicts: (0, 0, 0, 0, 0),
                        lost,
                    });
                }
            }
            for &nd in &DOCS {
                let w = nd * EDITORS_PER_DOC;
                let conn = pg_setup(&pg, nd, seats)?;
                let r = spawn_and_collect("postgres", &conn, "field", w, nd)?;
                pg_two.push((nd, r.per_doc.clone()));
                rows.push(DocResult {
                    engine: "postgres",
                    arm: format!("F-docs×{nd}"),
                    workers: w,
                    actions_per_sec: r.aps,
                    p50_ms: r.p50,
                    p99_ms: r.p99,
                    retries: 0,
                    within: r.within,
                    work_p50_us: r.work_p50_us,
                    verdicts: (0, 0, 0, 0, 0),
                    lost: None,
                });
            }
        }
        Err(e) => println!("postgres unavailable, mpedb-only run: {e}"),
    }

    println!(
        "{:<10} {:<18} {:>8} {:>12} {:>8} {:>8} {:>7} {:>11} {:>9} {:>6}",
        "engine",
        "arm",
        "editors",
        "actions/s",
        "p50 ms",
        "p99 ms",
        "≤1s",
        "engine µs",
        "retries",
        "lost"
    );
    for r in &rows {
        println!(
            "{:<10} {:<18} {:>8} {:>12.0} {:>8} {:>8} {:>6.0}% {:>11} {:>9} {:>6}",
            r.engine,
            r.arm,
            r.workers,
            r.actions_per_sec,
            r.p50_ms,
            r.p99_ms,
            r.within.0 * 100.0,
            r.work_p50_us,
            r.retries,
            r.lost.map(|l| l.to_string()).unwrap_or_else(|| "-".into())
        );
    }

    println!();
    println!(
        "C1 calibration, mpedb, {} ms think (the engine's own cost). `F-cap` piles editors onto \
         ONE block — the contention unit — and asks where the {} ms deadline stops being met:",
        cap_delay_ms(),
        deadline_ms()
    );
    println!(
        "{:>8} {:>14} {:>16} {:>9} {:>9}",
        "editors", "within deadline", "engine p50 µs", "p99 ms", "retries"
    );
    for (w, within, work, p99, retries) in &cap_rows {
        println!("{w:>8} {:>13.1}% {work:>16} {p99:>9} {retries:>9}", within * 100.0);
    }
    // The cap is the highest sweep point that still met the deadline —
    // MEASURED, not derived. `deadline / service time` overestimates badly,
    // because every conflict re-does the whole action rather than queueing
    // behind it: the work is not conserved the way a lock queue conserves it.
    let cap = cap_rows.iter().filter(|(_, w, ..)| *w >= 0.99).map(|(e, ..)| *e).max();
    match cap {
        Some(c) => println!(
            "  ⇒ measured cap = {c} editors per block at a {} ms deadline (99% met). \
             Naive deadline/service-time would have said {} — retries re-do work, so it \
             overestimates.",
            deadline_ms(),
            cap_rows.first().map(|(_, _, w, _, _)| (deadline_ms() * 1000) / (*w).max(1)).unwrap_or(0)
        ),
        None => println!("  ⇒ no sweep point met the deadline for 99% of actions"),
    }
    println!();
    println!(
        "`F-words`: a paragraph split into N blocks, {WORDS_EDITORS} editors on each. If a block \
         can be a word, capacity should be blocks × cap:"
    );
    println!(
        "{:>7} {:>8} {:>12} {:>14} {:>16} {:>9} {:>9}",
        "blocks", "editors", "actions/s", "*no guard*", "within deadline", "p99 ms", "overlap"
    );
    for (nb, aps, within, p99, _retries, overlap, ctl) in &word_rows {
        println!(
            "{nb:>7} {:>8} {aps:>12.0} {ctl:>14.0} {:>15.1}% {p99:>9} {overlap:>9}",
            nb * WORDS_EDITORS,
            within * 100.0
        );
    }
    println!();
    println!(
        "`F-batch`: 8 editors, K edits folded into one commit. Is the ceiling per commit or \
         per edit?"
    );
    println!("{:>7} {:>12} {:>14} {:>9}", "K", "commits/s", "edits/s", "p99 ms");
    for (k, cps, eps, p99) in &batch_rows {
        println!("{k:>7} {cps:>12.0} {eps:>14.0} {p99:>9}");
    }
    println!();
    println!(
        "`F-quorum`: flush when a MAJORITY of editors with outstanding work have delivered, \
         5 ms as the upper bound. A quarter of the editors are 10x slower. Transport is a \
         channel (not built \u{2014} DESIGN-COLLAB \u{a7}3b); this measures the FLUSH path, like \
         `F-batch` does."
    );
    println!(
        "{:>8} {:>10} {:>10} {:>7} {:>12} {:>11} {:>7} {:>7} {:>8} {:>8}",
        "editors",
        "on quorum",
        "on timeout",
        "avg K",
        "edits/s",
        "fast p50 ms",
        "lost",
        "wiped",
        "behind",
        "b.wiped"
    );
    for &e in &[8usize, 32, 64] {
        let (t, rate, fast, slow) = run_quorum(&scratch, e, seats.max(e * 2))?;
        println!(
            "{e:>8} {:>10} {:>10} {:>7.1} {rate:>12.0} {:>11} {:>7} {:>7} {:>8} {:>8}",
            t.on_quorum,
            t.on_timeout,
            t.members as f64 / (t.on_quorum + t.on_timeout).max(1) as f64,
            pct(&fast, 0.50) / 1000,
            t.lost,
            t.wiped,
            t.behind,
            t.behind_wiped
        );
        let _ = &slow;
    }
    println!();
    println!("guard verdicts (mpedb only) — a refusal is a CONFLICT or a LIMIT, never both:");
    println!(
        "{:<18} {:>8} {:>10} {:>10} {:>16} {:>10} {:>10}",
        "arm", "editors", "cleared", "overlap", "snapshot_too_old", "ring_gap", "deadline"
    );
    for r in rows.iter().filter(|r| r.engine == "mpedb") {
        let (c, o, s, g, d) = r.verdicts;
        println!(
            "{:<18} {:>8} {:>10} {:>10} {:>16} {:>10} {:>10}",
            r.arm, r.workers, c, o, s, g, d
        );
    }
    println!();
    println!(
        "per-document rates, {EDITORS_PER_DOC} editors per document. A document whose rate \
         holds as documents are added is one nothing else is locking:"
    );
    for (label, set) in [("mpedb", &mpedb_two), ("postgres", &pg_two)] {
        for (nd, per) in set {
            let parts: Vec<String> =
                per.iter().map(|(d, a)| format!("doc{d}={a:.0}/s")).collect();
            println!("  {label:<9} {nd} doc(s)  {}", parts.join("  "));
        }
    }
    println!();
    println!(
        "The floor for one action is the {} ms think time, so p50 at or near it means the \
         editor never waited on anyone.",
        delay_ms()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot codec has to survive being spliced by every editor in turn, or
    /// the lost-update column measures the codec instead of the engine.
    #[test]
    fn slots_are_independent() {
        let mut body = ".".repeat(BODY);
        for w in 0..8 {
            body = splice_slot(&body, w, w * 7);
        }
        for w in 0..8 {
            assert_eq!(read_slot(&body, w), Some(w * 7), "slot {w} was clobbered");
        }
    }

    /// And an editor that never wrote must read as missing, not as zero — the
    /// lost-update count depends on telling those apart.
    #[test]
    fn an_unwritten_slot_is_not_a_zero() {
        let body = splice_slot(&".".repeat(BODY), 0, 0);
        assert_eq!(read_slot(&body, 0), Some(0));
        assert_eq!(read_slot(&body, 1), None);
    }
}

// ------------------------------------------------------- F-quorum (#153 E3)

/// **Does quorum flushing reach the F-batch ceiling, and does the timeout stay
/// unfired?**
///
/// The flush rule Morten proposed, borrowed from etcd: do not wait on the
/// clock, wait until a *majority* of the editors with outstanding work have
/// delivered. The slowest do not gate the fast ones. The time limit is an upper
/// bound that should normally not fire — and *how often it fires* is the number
/// that says whether the idea worked, which is why it is reported first.
///
/// **What this measures, and what it models.** The flush path: submissions
/// already in the service's hands, applied through
/// [`Database::submit_batch`](mpedb::Database::submit_batch). Editors are
/// threads and the transport is a channel, because transport is deliberately
/// not built (DESIGN-COLLAB §3b) — modelling it as free is honest here, since
/// the comparison is against `F-batch`, which is also one process folding K
/// statements into one commit. A network would add latency to both sides
/// equally and change neither ratio.
///
/// A quarter of the editors are **ten times slower**. Without them the arm
/// could not tell "the fast do not wait" from "everyone is fast".
struct Job {
    sub: mpedb::collab::Submission,
    made: Instant,
    slow: bool,
    reply: std::sync::mpsc::Sender<(bool, Instant)>,
}

/// Flush trigger, counted separately: the whole claim is that the first one
/// dominates.
#[derive(Default)]
struct FlushTally {
    on_quorum: u64,
    on_timeout: u64,
    members: u64,
    committed: u64,
    lost: u64,
    /// Batches where EVERY member lost. Distinguishes "the batch as a whole was
    /// refused" from "individual members collided" — opposite causes.
    wiped: u64,
    /// Batches whose OLDEST member snapshot predates the previous batch's
    /// commit. `submit_batch` walks the committed ring from `min(snap)`, so one
    /// lagging member drags every other member's walk back over the previous
    /// batch's written range — which is the union of ITS members' spans.
    behind: u64,
    /// Of those, the ones that lost every member. If `behind_wiped ≈ wiped`,
    /// the losses are that compounding and not a collision at all.
    behind_wiped: u64,
}

fn quorum_of(active: usize) -> usize {
    // With two or fewer, a majority IS everyone and the quorum buys nothing —
    // flush on the first arrival instead of degenerating into the timeout.
    if active <= 2 {
        1
    } else {
        active / 2 + 1
    }
}

fn run_quorum(scratch: &Path, editors: usize, seats: usize) -> BResult<(FlushTally, f64, Vec<u64>, Vec<u64>)> {
    use mpedb::{Config, Database};
    let target = mpedb_setup(scratch, 1, seats)?;
    let db = std::sync::Arc::new(Database::open_with_config(Config::from_toml_str(
        &std::fs::read_to_string(&target)?,
    )?)?);

    let n_actions = cap_actions();
    let timeout = Duration::from_millis(5);
    let (tx, rx) = std::sync::mpsc::channel::<Job>();

    let mut handles = Vec::with_capacity(editors);
    for e in 0..editors {
        let tx = tx.clone();
        let db = db.clone();
        // Every editor owns a 16-byte slice of the block, so the common case is
        // a rebase and not a collision — which is the case the design claims to
        // serve. Collisions get their own arms.
        let at = (e * 16) as u64;
        // `MPEDB_F_SLOW_EVERY=0` makes everyone fast — the control that says
        // whether the losses come from the slow editors' stale snapshots.
        let every = env_u64("MPEDB_F_SLOW_EVERY", 4) as usize;
        let slow = every > 0 && e % every == 0;
        handles.push(std::thread::spawn(move || {
            for _ in 0..n_actions {
                let (rtx, rrx) = std::sync::mpsc::channel();
                let job = Job {
                    sub: mpedb::collab::Submission {
                        editor: e as i64,
                        snap: db.snapshot_txn(),
                        key: 0,
                        at,
                        remove: 4,
                        insert: "xxxx".into(),
                    },
                    made: Instant::now(),
                    slow,
                    reply: rtx,
                };
                if tx.send(job).is_err() {
                    return;
                }
                if rrx.recv().is_err() {
                    return;
                }
                if slow {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }));
    }
    drop(tx);

    let t0 = Instant::now();
    let mut tally = FlushTally::default();
    let (mut fast_lat, mut slow_lat) = (Vec::new(), Vec::new());
    let mut pending: Vec<Job> = Vec::new();
    let mut first_at: Option<Instant> = None;
    let mut last_txn = db.snapshot_txn();
    let need = quorum_of(editors);
    let mut done = false;
    while !done || !pending.is_empty() {
        let wait = match first_at {
            Some(t) => timeout.saturating_sub(t.elapsed()),
            None => Duration::from_millis(50),
        };
        match rx.recv_timeout(wait) {
            Ok(job) => {
                first_at.get_or_insert_with(Instant::now);
                pending.push(job);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => done = true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        let expired = first_at.map(|t| t.elapsed() >= timeout).unwrap_or(false);
        let quorum = pending.len() >= need;
        if pending.is_empty() || !(quorum || expired || done) {
            continue;
        }
        if quorum {
            tally.on_quorum += 1;
        } else {
            tally.on_timeout += 1;
        }
        let subs: Vec<_> = pending.iter().map(|j| j.sub.clone()).collect();
        // `submit_batch` walks the committed ring from the OLDEST member
        // snapshot. If that predates the previous batch's commit, every member
        // — however disjoint — is walked over that commit's written range,
        // which is the union of ITS members' spans.
        let behind = subs.iter().map(|s| s.snap).min().unwrap_or(0) < last_txn;
        let verdicts = db.submit_batch("block", "body", &subs)?;
        last_txn = db.snapshot_txn();
        tally.members += subs.len() as u64;
        let wiped = verdicts
            .iter()
            .all(|v| !matches!(v, mpedb::collab::EditVerdict::Committed));
        tally.wiped += u64::from(wiped);
        tally.behind += u64::from(behind);
        tally.behind_wiped += u64::from(behind && wiped);
        let now = Instant::now();
        for (j, v) in pending.drain(..).zip(verdicts) {
            let ok = matches!(v, mpedb::collab::EditVerdict::Committed);
            if ok {
                tally.committed += 1;
            } else {
                tally.lost += 1;
            }
            let us = now.duration_since(j.made).as_micros() as u64;
            if j.slow {
                slow_lat.push(us);
            } else {
                fast_lat.push(us);
            }
            let _ = j.reply.send((ok, now));
        }
        first_at = None;
    }
    for h in handles {
        let _ = h.join();
    }
    let elapsed = t0.elapsed().as_secs_f64();
    fast_lat.sort_unstable();
    slow_lat.sort_unstable();
    let rate = tally_rate(&tally, elapsed);
    Ok((tally, rate, fast_lat, slow_lat))
}

fn tally_rate(t: &FlushTally, elapsed: f64) -> f64 {
    t.members as f64 / elapsed.max(f64::MIN_POSITIVE)
}
