//! **Arm G — many mpedb clients syncing against one authority (#157).**
//!
//! Arm F asked what many editors cost *inside one file*. This asks the shape a
//! local-first application actually has: **every client owns its own `.mpedb`**,
//! works against it with the ordinary API, and reconciles with a central one.
//!
//! Four sub-arms, because "sync" is three different questions plus a control:
//!
//! * **G-rows** — general sync. N replicas each own a disjoint slice of the key
//!   space, write locally, and `sync()`. The number is converged rows per
//!   second; the *assertion* is that every replica ends byte-identical to the
//!   authority, because a sync benchmark without a convergence check measures
//!   how fast data can be lost.
//! * **G-cell** — many clients editing **one cell**. Sub-edits carry
//!   `(seq, editor)` and merge through `submit_batch` at the authority (#155),
//!   so this is the collaborative-document case with a real file boundary
//!   between the editors rather than threads in one process.
//! * **G-offline** — one replica goes dark for K edits and then catches up. The
//!   point is the *shape*: the CDC dirty set is coalesced, so catch-up cost
//!   tracks changed ROWS, not edits. If it tracked edits, an hour offline would
//!   be an hour of replay.
//! * **G-role** — the control. Identical local work with `role = "standalone"`
//!   and `role = "replica"`, nothing synced. If these differ, "same API, same
//!   engine, the role is only a deployment fact" is false and has to be said
//!   out loud rather than left as a claim.
//!
//! ## What is modelled away, stated once
//!
//! There is no network and no process boundary: replicas are threads, each with
//! its **own file and therefore its own writer lock**, so the contention being
//! measured is real. A network would add latency to every arm equally. The
//! multi-process property is proven elsewhere (`mirror-collide`, the stress and
//! crash harnesses); repeating it here would measure `fork` rather than sync.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use mpedb::collab::{EditVerdictExt as _, Submission};
use mpedb::sync::{fingerprint, SyncLink};
use mpedb::{Config, Database, Value};

use crate::util::BResult;

/// Replica counts to scale across.
const CLIENTS: [usize; 3] = [2, 8, 32];
/// Rows each replica writes per round in `G-rows`.
const ROWS_PER_CLIENT: usize = 50;
/// Sync rounds in `G-rows` — more than one, because the interesting cost is the
/// *incremental* sync, not the first one against an empty upstream.
const ROUNDS: usize = 4;

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
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

  [[table.column]]
  name = "rev"
  type = "int64"
"#;

fn open(dir: &Path, tag: &str, role: &str) -> BResult<Database> {
    let path = dir.join(format!("{tag}.mpedb"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("mpedb-wal"));
    let toml = format!(
        "[database]\npath = \"{}\"\nsize_mb = 64\nmax_readers = 64\n\
         durability = \"wal\"\n\n[sync]\nrole = \"{role}\"\n{SCHEMA}",
        mpedb::toml_escape(&path.display().to_string())
    );
    Ok(Database::open_with_config(Config::from_toml_str(&toml)?)?)
}

fn put(db: &Database, id: i64, body: &str, rev: i64) -> BResult<()> {
    db.query(
        "INSERT OR REPLACE INTO doc (id, body, rev) VALUES ($1, $2, $3)",
        &[Value::Int(id), Value::Text(body.into()), Value::Int(rev)],
    )?;
    Ok(())
}

/// One `G-rows` cell.
///
/// Local writes and the sync are timed **separately** and that is the whole
/// point of the cell: the first run of this arm reported ~400 rows/s and it was
/// measuring the disk's commit rate, because each local write is its own
/// autocommit transaction. The sync cost was invisible underneath it.
struct RowsCell {
    clients: usize,
    rows: u64,
    local_secs: f64,
    sync_secs: f64,
    converged: bool,
    conflicts: u64,
}

/// **G-rows.** N replicas write disjoint slices, then everybody syncs until the
/// whole set agrees.
fn run_rows(dir: &Path, clients: usize) -> BResult<RowsCell> {
    let up = Arc::new(open(dir, "g-up", "authority")?);
    let mut reps = Vec::with_capacity(clients);
    for c in 0..clients {
        reps.push(Arc::new(open(dir, &format!("g-r{c}"), "replica")?));
    }
    for (c, r) in reps.iter().enumerate() {
        SyncLink::new(r, &up, c as u64 + 1).enable(&["doc"])?;
    }

    let mut rows = 0u64;
    let mut conflicts = 0u64;
    let mut local_secs = 0.0;
    let mut sync_secs = 0.0;
    for round in 0..ROUNDS {
        // Local work: each replica owns `id % clients == c`, so the row plane
        // sees no genuine conflicts and the number is the sync path's own cost
        // rather than a conflict-resolution cost.
        let t = Instant::now();
        for (c, r) in reps.iter().enumerate() {
            for i in 0..ROWS_PER_CLIENT {
                let id = (i * clients + c) as i64;
                put(r, id, &format!("r{c}-round{round}-{i}"), round as i64)?;
                rows += 1;
            }
        }
        local_secs += t.elapsed().as_secs_f64();

        // Push everyone's work up, then pull it all back down. Two passes:
        // pass one lands every replica's writes at the authority, pass two
        // gives every replica everyone else's.
        let t = Instant::now();
        for (c, r) in reps.iter().enumerate() {
            conflicts += SyncLink::new(r, &up, c as u64 + 1).sync(&["doc"])?.pushed.conflicts;
        }
        for (c, r) in reps.iter().enumerate() {
            SyncLink::new(r, &up, c as u64 + 1).pull(&["doc"])?;
        }
        sync_secs += t.elapsed().as_secs_f64();
    }

    // The assertion that makes the number mean anything.
    let want = fingerprint(&up, &["doc"])?;
    let converged = reps
        .iter()
        .all(|r| fingerprint(r, &["doc"]).map(|f| f == want).unwrap_or(false));

    Ok(RowsCell { clients, rows, local_secs, sync_secs, converged, conflicts })
}

/// One `G-cell` cell.
struct CellCell {
    clients: usize,
    edits: u64,
    landed: u64,
    secs: f64,
    len_ok: bool,
}

/// **G-cell.** Every client owns a 16-byte slice of one document and submits
/// sub-edits to the authority.
///
/// `len_ok` is the correctness check and it is deliberately a *length*: each
/// landed edit replaces 4 bytes with 4, so the value's length must never move.
/// A merge that clobbered instead of splicing would change it.
fn run_cell(dir: &Path, clients: usize) -> BResult<CellCell> {
    let up = Arc::new(open(dir, "gc-up", "authority")?);
    let seed: String = std::iter::repeat_n('.', clients * 16).collect();
    put(&up, 1, &seed, 0)?;

    let rounds = env_u64("MPEDB_G_CELL_ROUNDS", 8) as usize;
    let t0 = Instant::now();
    let mut edits = 0u64;
    let mut landed = 0u64;
    let mut seq = 1u64;
    for _ in 0..rounds {
        // One flush per round carrying every client's edit — the shape #155
        // measured, now with the clients on separate files.
        let snap = up.snapshot_txn();
        let mut subs = Vec::with_capacity(clients);
        for c in 0..clients {
            subs.push(Submission {
                editor: c as i64,
                seq,
                snap,
                key: 1,
                at: (c * 16) as u64,
                remove: 4,
                insert: "xxxx".into(),
            });
            seq += 1;
        }
        edits += subs.len() as u64;
        for v in up.submit_batch("doc", "body", &subs)? {
            if v.is_committed() {
                landed += 1;
            }
        }
    }
    let secs = t0.elapsed().as_secs_f64();

    let len_ok = match up.query("SELECT body FROM doc WHERE id = 1", &[])? {
        mpedb::ExecResult::Rows { rows, .. } => match rows.first().map(|r| &r[0]) {
            Some(Value::Text(t)) => t.len() == seed.len(),
            _ => false,
        },
        _ => false,
    };

    Ok(CellCell { clients, edits, landed, secs, len_ok })
}

/// **G-offline.** One replica misses `edits` upstream writes over `distinct`
/// rows, then catches up in one pull.
fn run_offline(dir: &Path, distinct: usize, per_row: usize) -> BResult<(u64, u64, f64, bool)> {
    let up = open(dir, "go-up", "authority")?;
    let r = open(dir, "go-r", "replica")?;
    let link = SyncLink::new(&r, &up, 1);
    link.enable(&["doc"])?;

    for round in 0..per_row {
        for id in 0..distinct as i64 {
            put(&up, id, &format!("v{round}-{id}"), round as i64)?;
        }
    }
    let edits = (distinct * per_row) as u64;

    let t0 = Instant::now();
    let rep = link.pull(&["doc"])?;
    let secs = t0.elapsed().as_secs_f64();
    let converged = fingerprint(&up, &["doc"])? == fingerprint(&r, &["doc"])?;
    Ok((edits, rep.upserts, secs, converged))
}

fn run_role_once(dir: &Path, role: &str, tag: usize, ops: usize) -> BResult<f64> {
    let db = open(dir, &format!("gr-{role}-{tag}"), role)?;
    let t0 = Instant::now();
    for i in 0..ops as i64 {
        put(&db, i, "the quick brown fox jumps over the lazy dog", i)?;
    }
    Ok(ops as f64 / t0.elapsed().as_secs_f64().max(f64::MIN_POSITIVE))
}

/// **G-role.** The control: does declaring a role cost anything locally?
///
/// **Paired and interleaved**, following the method the rest of this suite
/// already uses (#122, `benchmarks/README.md`): one standalone run, then one
/// replica run, repeated, and the answer is the median of the per-pair ratios.
/// A single unpaired sample said the replica was 26.5 % *faster*, which is not
/// a result — it is host noise on two cores, and pairing is what removes it.
fn run_role(dir: &Path, ops: usize, reps: usize) -> BResult<(f64, f64, f64)> {
    let mut ratios = Vec::with_capacity(reps);
    let (mut s_sum, mut r_sum) = (0.0, 0.0);
    for k in 0..reps {
        // Alternate which arm goes first. Always running standalone first made
        // it pay every pair's cold-file cost and handed the replica a warm page
        // cache — worth a spurious +5.7 % before this line existed.
        let (s, r) = if k % 2 == 0 {
            let s = run_role_once(dir, "standalone", k, ops)?;
            let r = run_role_once(dir, "replica", k, ops)?;
            (s, r)
        } else {
            let r = run_role_once(dir, "replica", k, ops)?;
            let s = run_role_once(dir, "standalone", k, ops)?;
            (s, r)
        };
        ratios.push(r / s);
        s_sum += s;
        r_sum += r;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let median = ratios[ratios.len() / 2];
    Ok((s_sum / reps as f64, r_sum / reps as f64, (median - 1.0) * 100.0))
}

pub fn run(scratch: PathBuf) -> BResult<()> {
    std::fs::create_dir_all(&scratch)?;
    println!("# Arm G — mpedb clients syncing against one mpedb authority (#157)\n");
    println!(
        "Every client owns its OWN `.mpedb` (own writer lock). No network and no process \
         boundary is modelled \u{2014} both would cost every arm equally, and the multi-process \
         property is proven by `mirror-collide` and the stress/crash harnesses rather than here."
    );
    println!();

    // ---- G-role: the control comes FIRST, because every other number here is
    // only interesting if the role itself is free.
    let reps = env_u64("MPEDB_G_ROLE_REPS", 9) as usize;
    println!(
        "`G-role` (control): identical local work, nothing synced, **paired and interleaved** \
         over {reps} repetitions with the median of the per-pair ratios (#122). If the role \
         costs anything, \"same API, same engine, the role is only a deployment fact\" is false."
    );
    let ops = env_u64("MPEDB_G_ROLE_OPS", 3000) as usize;
    let (standalone, replica, delta) = run_role(&scratch, ops, reps)?;
    println!("{:>14} {:>12} {:>16}", "role", "writes/s", "paired median");
    println!("{:>14} {standalone:>12.0} {:>16}", "standalone", "-");
    println!("{:>14} {replica:>12.0} {delta:>15.1}%", "replica");
    println!();

    // ---- G-rows
    println!(
        "`G-rows`: {ROWS_PER_CLIENT} rows/client/round x {ROUNDS} rounds, disjoint key slices. \
         Local writes and the sync are timed SEPARATELY \u{2014} each local write is its own \
         autocommit transaction, so a combined number would report the disk's commit rate and \
         hide the sync entirely. `converged` is the assertion: a rate with `false` beside it is \
         the rate at which data was lost."
    );
    println!(
        "{:>9} {:>8} {:>11} {:>11} {:>12} {:>11} {:>10}",
        "clients", "rows", "local/s", "sync secs", "synced/s", "converged", "conflicts"
    );
    for &n in &CLIENTS {
        let c = run_rows(&scratch, n)?;
        println!(
            "{:>9} {:>8} {:>11.0} {:>11.3} {:>12.0} {:>11} {:>10}",
            c.clients,
            c.rows,
            c.rows as f64 / c.local_secs.max(f64::MIN_POSITIVE),
            c.sync_secs,
            c.rows as f64 / c.sync_secs.max(f64::MIN_POSITIVE),
            c.converged,
            c.conflicts
        );
    }
    println!();

    // ---- G-cell
    println!(
        "`G-cell`: every client owns a 16-byte slice of ONE document; sub-edits merge at the \
         authority (#155). `len ok` is the correctness check \u{2014} each edit replaces 4 bytes \
         with 4, so a clobber instead of a splice would move the length."
    );
    println!(
        "{:>9} {:>10} {:>10} {:>12} {:>9}",
        "clients", "edits", "landed", "edits/s", "len ok"
    );
    for &n in &CLIENTS {
        let c = run_cell(&scratch, n)?;
        println!(
            "{:>9} {:>10} {:>10} {:>12.0} {:>9}",
            c.clients,
            c.edits,
            c.landed,
            c.edits as f64 / c.secs.max(f64::MIN_POSITIVE),
            c.len_ok
        );
    }
    println!();

    // ---- G-offline
    println!(
        "`G-offline`: a replica misses N upstream edits over M distinct rows, then catches up in \
         ONE pull. `images` must track ROWS, not edits \u{2014} that is what the coalesced dirty \
         set buys, and without it an hour offline is an hour of replay."
    );
    println!(
        "{:>10} {:>10} {:>10} {:>10} {:>11}",
        "rows", "edits", "images", "secs", "converged"
    );
    for (distinct, per_row) in [(50usize, 2usize), (50, 20), (50, 200)] {
        let (edits, images, secs, ok) = run_offline(&scratch, distinct, per_row)?;
        println!("{distinct:>10} {edits:>10} {images:>10} {secs:>10.3} {ok:>11}");
    }
    println!();
    Ok(())
}
