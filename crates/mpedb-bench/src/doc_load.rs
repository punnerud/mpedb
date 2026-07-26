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
fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Editors to scale across. Small, because the serialized arms cost
/// `workers × actions × think` seconds by construction.
const WORKERS: [usize; 3] = [2, 4, 8];
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
    /// `(cleared, overlap, snapshot_too_old, ring_gap)` — WHY the guard
    /// refused, which a retry count on its own cannot say.
    pub verdicts: (u64, u64, u64, u64),
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

/// Write `n` into worker `w`'s slot of `body`, leaving every other slot alone.
/// This is the read-modify-write that makes a lost update possible.
fn splice_slot(body: &str, w: usize, n: usize) -> String {
    let mut b = body.as_bytes().to_vec();
    b.resize(BODY, b'.');
    let at = w * SLOT;
    let mark = format!("{n:015} ");
    b[at..at + SLOT].copy_from_slice(mark.as_bytes());
    String::from_utf8_lossy(&b).into_owned()
}

/// The counter in worker `w`'s slot, or `None` if it was never written.
fn read_slot(body: &str, w: usize) -> Option<usize> {
    let b = body.as_bytes();
    let at = w * SLOT;
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

    let n_actions = actions();
    let think = Duration::from_millis(delay_ms());
    let mut lat = Vec::with_capacity(n_actions);
    let mut retries = 0u64;
    // (cleared, overlap, snapshot_too_old, ring_gap). A refusal count alone
    // cannot say whether the guard caught a real conflict or ran out of ring,
    // and those call for opposite fixes.
    let mut verdicts = (0u64, 0u64, 0u64, 0u64);
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
                // `move`: every worker on the SAME block, so the only thing
                // separating them is which column they write.
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

            for i in 0..n_actions {
                let t0 = Instant::now();
                loop {
                    let snap = db.snapshot_txn();
                    let cur = match db.query(read_sql, &[Value::Int(key)])? {
                        ExecResult::Rows { rows, .. } if !rows.is_empty() => rows[0][0].clone(),
                        _ => Value::Null,
                    };
                    // Think — holding nothing.
                    std::thread::sleep(think);
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
                    match s.commit() {
                        Ok(()) => break,
                        Err(Error::WriteConflict) => {
                            retries += 1;
                            continue;
                        }
                        Err(e) => return Err(format!("mpedb commit: {e}").into()),
                    }
                }
                lat.push(t0.elapsed().as_micros() as u64);
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
                lat.push(t0.elapsed().as_micros() as u64);
            }
        }
        other => return Err(format!("unknown engine {other}").into()),
    }

    for l in &lat {
        println!("{l}");
    }
    println!("RETRIES {retries}");
    println!("VERDICTS {} {} {} {}", verdicts.0, verdicts.1, verdicts.2, verdicts.3);
    Ok(())
}

// ------------------------------------------------------------------ parent

struct Run {
    aps: f64,
    p50: u64,
    p99: u64,
    retries: u64,
    /// `(cleared, overlap, snapshot_too_old, ring_gap)` summed over editors.
    verdicts: (u64, u64, u64, u64),
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
    let mut lat = Vec::new();
    let mut retries = 0u64;
    let mut verd = (0u64, 0u64, 0u64, 0u64);
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
                if n.len() == 4 {
                    verd.0 += n[0];
                    verd.1 += n[1];
                    verd.2 += n[2];
                    verd.3 += n[3];
                }
            } else if let Ok(v) = line.trim().parse::<u64>() {
                lat.push(v);
                *doc_actions.entry(doc).or_default() += 1;
            }
        }
    }
    let elapsed = t0.elapsed().as_secs_f64();
    lat.sort_unstable();
    Ok(Run {
        aps: (workers * actions()) as f64 / elapsed,
        p50: pct(&lat, 0.50) / 1000,
        p99: pct(&lat, 0.99) / 1000,
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
    let seats = *WORKERS.iter().max().unwrap().max(&EDITORS_PER_DOC);

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
                verdicts: r.verdicts,
                lost: None,
            });
        }
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
                        verdicts: (0, 0, 0, 0),
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
                    verdicts: (0, 0, 0, 0),
                    lost: None,
                });
            }
        }
        Err(e) => println!("postgres unavailable, mpedb-only run: {e}"),
    }

    println!(
        "{:<10} {:<18} {:>8} {:>12} {:>8} {:>8} {:>9} {:>6}",
        "engine", "arm", "editors", "actions/s", "p50 ms", "p99 ms", "retries", "lost"
    );
    for r in &rows {
        println!(
            "{:<10} {:<18} {:>8} {:>12.0} {:>8} {:>8} {:>9} {:>6}",
            r.engine,
            r.arm,
            r.workers,
            r.actions_per_sec,
            r.p50_ms,
            r.p99_ms,
            r.retries,
            r.lost.map(|l| l.to_string()).unwrap_or_else(|| "-".into())
        );
    }

    println!();
    println!("guard verdicts (mpedb only) — a refusal is a CONFLICT or a LIMIT, never both:");
    println!(
        "{:<18} {:>8} {:>10} {:>10} {:>16} {:>10}",
        "arm", "editors", "cleared", "overlap", "snapshot_too_old", "ring_gap"
    );
    for r in rows.iter().filter(|r| r.engine == "mpedb") {
        let (c, o, s, g) = r.verdicts;
        println!("{:<18} {:>8} {:>10} {:>10} {:>16} {:>10}", r.arm, r.workers, c, o, s, g);
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
