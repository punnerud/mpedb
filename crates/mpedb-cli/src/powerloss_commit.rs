//! `mpedb powerloss --dir D --durability commit [--rounds N] [--commits C]
//! [--cuts K] [--size-mb M] [--extent-kb N] [--sabotage reorder|drop-data]`
//! — power-loss simulation for `durability = commit` (#121, design/DESIGN.md §4.1a).
//!
//! # Why this is a different program from the WAL one
//!
//! `powerloss --durability wal|async` truncates the log at a random byte
//! offset, because a WAL only appends: its power-loss image *is* a prefix.
//! `commit` has no log. It publishes by mutating a mapped file **in place**, so
//! its power-loss image is not a prefix of anything — it is "an arbitrary
//! subset of the dirty pages never reached the platter". A tail-truncation
//! harness cannot express that, which is why until #121 the `commit` durability
//! claim rested on an ordering argument (§4.1) plus a SIGKILL harness that does
//! not model a device losing page-cache content at all.
//!
//! The property under attack is §4.1's reason for existing:
//!
//! > **No `meta_T` is checksum-valid on the platter while pointing at
//! > copy-on-write pages that were never written.**
//!
//! # The fault model — stated explicitly, because a wrong one is worse than none
//!
//! The engine's durability trace is captured *from inside the engine*
//! (`mpedb_core::plsim`, armed by `MPEDB_COMMIT_SYNC_LOG`): every
//! `msync(MS_SYNC)` return, every `sync_barrier` return, every meta publish,
//! with the page bytes as they were at that instant. The simulator then replays
//! that trace over the pre-workload file image and cuts it.
//!
//! **The ordering is read out of the implementation, not assumed.** That is the
//! one design decision that keeps this from being circular. A simulator that
//! *decides* "data pages are durable before the meta" and then checks that the
//! meta is never ahead of the data proves nothing at all — it would pass on an
//! engine with the flushes reordered. Here the trace moves when the code moves:
//! reorder `commit_inner`'s two flushes and the FLUSH events swap places in the
//! log, the generator starts producing meta-over-missing-data images, and
//! recovery is asked to survive them. It does not. (`--sabotage` reproduces
//! that without patching the engine; see below.)
//!
//! What the simulator assumes about *hardware*, and nothing more:
//!
//! 1. **A returned `msync(MS_SYNC)` is a durability edge.** Every page in that
//!    range is on the platter and stays there. On Linux this is exactly true —
//!    `msync(MS_SYNC)` is `vfs_fsync_range`, which ends in a device cache
//!    flush; `sync_barrier` is a documented no-op there. On Darwin the edge is
//!    the `F_FULLFSYNC` in `sync_barrier`, which is stricter (later) — but the
//!    commit path always runs a barrier between the data flush and the meta
//!    store, so both platforms put the same break in the same place, and the
//!    Linux reading is the *weaker* (more permissive to the engine) of the two.
//! 2. **A store that has not been flushed may or may not be on the platter.**
//!    The in-flight flush lands as an arbitrary subset of its pages, and a page
//!    may land torn. Stores after the cut are modelled as absent. Modelling
//!    spontaneous writeback as absent is safe *here* and only here, because
//!    every un-flushed page in `commit` mode is a COW page unreachable from any
//!    durable meta (shadow paging) — writing it back early can only add
//!    unreferenced garbage, never change a reachable byte. That is a property
//!    of this engine, not a general licence.
//! 3. **Torn writes are modelled at 512-byte sector granularity, and the meta
//!    page additionally at byte granularity** (`Fault::Sector` scribbles). The
//!    per-commit meta fields all live in bytes 64..120, i.e. inside sector 0,
//!    so a *sector-atomic* device can only ever leave a meta page wholly-new or
//!    wholly-old and the checksum-fallback path would never be exercised. Byte
//!    tearing is deliberately harsher than sector-atomic hardware: the meta
//!    checksum exists precisely to survive a device that does not keep the
//!    promise, so refusing to test it would be testing the assumption instead
//!    of the code.
//! 4. **Extent/blob payload is in the same ordering class as mapped pages.** It
//!    is `pwrite`n rather than mapped-stored, but Linux keeps `write(2)` and
//!    `MAP_SHARED` coherent through one page cache, and the commit path folds
//!    the extent ranges into the same pre-barrier `msync` span. The trace
//!    therefore captures extent bytes exactly like btree bytes, with no special
//!    case anywhere in this file — run `--extent-kb N` to exercise it.
//!
//! One known optimism, stated so it is not mistaken for rigour: the replay
//! starts from a byte copy of the quiesced seeded file, so the *control* pages —
//! lock area, reader table, intent ring — carry their pre-workload content
//! rather than a lost-writeback subset. They are never msync'd during a round,
//! so a real power loss could scramble them. It cannot matter: flipping the
//! stored boot id (which every cut does) makes `post_attach` reinitialise the
//! robust mutex, zero the reader table, and re-derive `durable_txn` from the
//! newest *valid* meta before anything else runs. The pages the harness is
//! honest about are exactly the pages recovery actually trusts.
//!
//! **Out of scope, deliberately:** concurrency. One writer process per round,
//! so the trace is a total order. Device semantics is what this harness is
//! about; multi-process interleaving is the SIGKILL harness's job (`crash`,
//! `stress`, `powerloss --durability wal`).
//!
//! # What each cut asserts
//!
//! The workload records, after every commit `n`, a digest of the entire logical
//! database, and stamps `n` into the trace (`plsim::mark`). For a cut inside
//! commit `C`, with `n_floor` = the last commit acknowledged strictly before
//! the cut:
//!
//! - reopening **must succeed** — one meta slot always holds a durable commit;
//! - page accounting (`db.verify()`) must pass;
//! - the workload invariants must hold (bank sum, `a + b = 0`, blob bytes);
//! - the recovered digest must be **exactly** `digest[n_floor]` when the cut is
//!   at or before `C`'s meta flush with that flush losing everything (the meta
//!   cannot have landed), and `digest[n_floor]` or `digest[C]` otherwise.
//!   Anything else — including a state that satisfies every invariant but
//!   matches no committed state — is a failure.
//!
//! # Non-vacuity
//!
//! `--sabotage reorder` rewrites the *captured trace* so each commit's data
//! flushes fall after its meta flush — i.e. the trace an engine that published
//! the meta first would have produced — and then requires the simulator to find
//! a violation. `--sabotage drop-data` deletes the data flushes outright (the
//! "barrier removed" engine). Either exits non-zero if nothing is found, so
//! "the injector never fails" and "the injector never ran" stay
//! distinguishable (INNOVATIONS.md §6.4).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use mpedb::{params, Database, ExecResult, Value};
use mpedb_core::shm::BOOT_ID_FILE_OFFSET;

use crate::args;
use crate::util::{fill_bytes, runtime, usage, write_config_durable, CliResult, Failure, Rng, Watchdog};

const PAGE: usize = 4096;
const SECTOR: usize = 512;
const ACCOUNTS: i64 = 40;
const BANK_TOTAL: i64 = 40_000;
const ROWS: i64 = 60;
const ROUND_TIMEOUT_SECS: u64 = 900;

/// Same shape as the WAL sim's schema, smaller: every cut rebuilds and reopens
/// the whole file, so the round's cost is linear in the file size.
const COMMIT_TOML: &str = r#"[[table]]
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

/// 4-12 KiB, content derived from (id, seq) so any process can recompute the
/// exact bytes a recovered row must hold.
fn blob_for(id: i64, seq: i64) -> Vec<u8> {
    let mut rng = Rng::seeded(&[id as u64, seq as u64, 0x9e37]);
    let len = (4 + rng.below(9)) * 1024;
    fill_bytes(&mut rng, len as usize)
}

// ------------------------------------------------------------- durability log

#[derive(Debug)]
enum Ev {
    /// Pages that a returned `msync` put on the platter: `(file_offset, index
    /// of the page's 4096 bytes inside the raw log)`.
    Flush(Vec<(u64, usize)>),
    Barrier,
    Publish { txn: u64 },
    Mark(u64),
}

impl Ev {
    /// Does this flush cover a meta page? That is what makes it the *meta*
    /// flush of its commit rather than one of its data flushes.
    fn touches_meta(&self) -> bool {
        match self {
            Ev::Flush(pages) => pages.iter().any(|&(off, _)| off < 2 * PAGE as u64),
            _ => false,
        }
    }
}

fn u64_at(raw: &[u8], p: usize) -> Result<u64, Failure> {
    raw.get(p..p + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| Failure::Runtime("truncated durability log".into()))
}

fn parse_log(raw: &[u8]) -> Result<Vec<Ev>, Failure> {
    use mpedb_core::plsim::{EV_BARRIER, EV_FLUSH, EV_MARK, EV_PUBLISH};
    let mut evs = Vec::new();
    let mut p = 0usize;
    while p < raw.len() {
        let kind = raw[p];
        p += 1;
        match kind {
            EV_FLUSH => {
                let n = u32::from_le_bytes(
                    raw.get(p..p + 4)
                        .ok_or_else(|| Failure::Runtime("truncated durability log".into()))?
                        .try_into()
                        .unwrap(),
                ) as usize;
                p += 4;
                let mut pages = Vec::with_capacity(n);
                for _ in 0..n {
                    let off = u64_at(raw, p)?;
                    p += 8;
                    if p + PAGE > raw.len() {
                        return runtime("truncated durability log (page body)");
                    }
                    pages.push((off, p));
                    p += PAGE;
                }
                evs.push(Ev::Flush(pages));
            }
            EV_BARRIER => evs.push(Ev::Barrier),
            EV_PUBLISH => {
                let txn = u64_at(raw, p)?;
                p += 16; // txn + slot
                evs.push(Ev::Publish { txn });
            }
            EV_MARK => {
                let tag = u64_at(raw, p)?;
                p += 8;
                evs.push(Ev::Mark(tag));
            }
            other => return runtime(format!("durability log: unknown event kind {other}")),
        }
    }
    Ok(evs)
}

/// One commit as the trace shows it, in event indices.
#[derive(Debug, Clone)]
struct CommitTrace {
    txn: u64,
    publish: usize,
    /// Flushes attributed to this commit's data class: everything flushed since
    /// the previous commit's meta flush.
    data: Vec<usize>,
    /// The flush that put the new meta slot on the platter.
    meta_flush: Option<usize>,
    barrier_before_publish: bool,
    /// The workload's commit counter, if this commit is one of its own.
    mark: Option<u64>,
}

fn group_commits(evs: &[Ev]) -> Vec<CommitTrace> {
    let mut out: Vec<CommitTrace> = Vec::new();
    let mut pending: Vec<usize> = Vec::new();
    let mut barrier_seen = false;
    for (i, ev) in evs.iter().enumerate() {
        match ev {
            Ev::Publish { txn } => {
                out.push(CommitTrace {
                    txn: *txn,
                    publish: i,
                    data: std::mem::take(&mut pending),
                    meta_flush: None,
                    barrier_before_publish: barrier_seen,
                    mark: None,
                });
                barrier_seen = false;
            }
            Ev::Flush(_) => match out.last_mut() {
                // The first meta-covering flush after a publish is that
                // commit's meta flush; anything else belongs to the data class
                // of the commit still being assembled.
                Some(c) if c.meta_flush.is_none() && ev.touches_meta() => c.meta_flush = Some(i),
                _ => pending.push(i),
            },
            Ev::Barrier => barrier_seen = true,
            Ev::Mark(tag) => {
                if let Some(c) = out.last_mut() {
                    c.mark = Some(*tag);
                }
            }
        }
    }
    out
}

/// Structural sanity on the trace — and a deliberately *modest* claim.
///
/// §4.1's real property is **not statically decidable from the trace shape**,
/// and it is worth being explicit about why, because the first version of this
/// harness thought it was and reported a violation against correct code. A
/// correct engine emits
///
/// ```text
/// … PUBLISH(T) FLUSH(meta_T) BARRIER   FLUSH(data_{T+1}) BARRIER PUBLISH(T+1) …
/// ```
///
/// and an engine that published the meta *before* flushing its data emits
///
/// ```text
/// … PUBLISH(T) FLUSH(meta_T) BARRIER   FLUSH(data_T)     BARRIER PUBLISH(T+1) …
/// ```
///
/// — the same shape. What differs is which commit's meta *references* the pages
/// in that flush, which the trace does not carry and which no amount of
/// position-matching recovers. So the ordering is falsified the only way it can
/// be: cut between the meta flush and the data flush, reopen, and see whether
/// the database survives. That is what the waves below do.
fn audit_order(commits: &[CommitTrace]) -> Result<(), Failure> {
    for c in commits {
        if c.meta_flush.is_none() {
            return runtime(format!(
                "commit txn={} published a meta slot that was never flushed — \
                 durability=commit must msync the slot before acking it",
                c.txn
            ));
        }
        if !c.data.is_empty() && !c.barrier_before_publish {
            return runtime(format!(
                "§4.1 VIOLATION: commit txn={} published its meta with no sync_barrier \
                 after its data flushes",
                c.txn
            ));
        }
    }
    Ok(())
}

// ------------------------------------------------------------- image building

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// The in-flight flush reached the platter with none of its pages.
    None,
    /// Each page independently landed or did not.
    Subset,
    /// Each page landed as an independent mix of 512-byte sectors, and (meta
    /// pages only) sectors themselves may land partially — see the module doc.
    Sector,
    /// It completed after all; the cut is really one event later.
    All,
}

impl Fault {
    fn label(self) -> &'static str {
        match self {
            Fault::None => "none",
            Fault::Subset => "subset",
            Fault::Sector => "sector",
            Fault::All => "all",
        }
    }
}

struct Cut {
    at: usize,
    fault: Fault,
    wave: usize,
}

/// Apply the trace to `img` up to (but not including) event `at`, then apply
/// event `at` under `fault`. Returns how many pages the fault dropped.
fn build_image(img: &mut [u8], raw: &[u8], evs: &[Ev], cut: &Cut, rng: &mut Rng) -> u64 {
    let put = |img: &mut [u8], off: u64, src: usize| {
        let o = off as usize;
        if o + PAGE <= img.len() {
            img[o..o + PAGE].copy_from_slice(&raw[src..src + PAGE]);
        }
    };
    for ev in &evs[..cut.at] {
        if let Ev::Flush(pages) = ev {
            for &(off, src) in pages {
                put(img, off, src);
            }
        }
    }
    let mut dropped = 0u64;
    if let Some(Ev::Flush(pages)) = evs.get(cut.at) {
        for &(off, src) in pages {
            match cut.fault {
                Fault::All => put(img, off, src),
                Fault::None => dropped += 1,
                Fault::Subset => {
                    if rng.below(2) == 0 {
                        put(img, off, src);
                    } else {
                        dropped += 1;
                    }
                }
                Fault::Sector => {
                    let o = off as usize;
                    if o + PAGE > img.len() {
                        continue;
                    }
                    let mut any_old = false;
                    for s in 0..PAGE / SECTOR {
                        let (a, b) = (o + s * SECTOR, o + (s + 1) * SECTOR);
                        match rng.below(3) {
                            0 => any_old = true, // this sector never landed
                            1 => img[a..b].copy_from_slice(&raw[src + s * SECTOR..src + (s + 1) * SECTOR]),
                            // a sector that landed PARTIALLY: harsher than
                            // sector-atomic hardware, and the only way to reach
                            // the meta checksum's fallback path at all (the
                            // per-commit fields all sit inside sector 0).
                            _ => {
                                let k = rng.below(SECTOR as u64) as usize;
                                img[a..a + k].copy_from_slice(&raw[src + s * SECTOR..src + s * SECTOR + k]);
                                any_old = true;
                            }
                        }
                    }
                    dropped += u64::from(any_old);
                }
            }
        }
    }
    dropped
}

// ------------------------------------------------------------------ verifying

/// A 64-bit digest of the ENTIRE logical database. Computed identically by the
/// workload (after each commit) and by the verifier (after each recovery), so a
/// recovered state can be matched against the exact commit it must be.
pub fn state_digest(db: &Database) -> Result<u64, Failure> {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |x: u64| {
        h ^= x;
        h = h.wrapping_mul(0x1000_0000_01b3);
        h ^= h >> 29;
    };
    let ExecResult::Rows { rows, .. } = db.query("SELECT id, balance FROM accounts", &[])? else {
        return runtime("expected rows from accounts");
    };
    let mut acc: Vec<(i64, i64)> = Vec::with_capacity(rows.len());
    for r in &rows {
        acc.push((int(&r[0])?, int(&r[1])?));
    }
    acc.sort_unstable();
    mix(acc.len() as u64);
    for (id, bal) in acc {
        mix(id as u64);
        mix(bal as u64);
    }
    let ExecResult::Rows { rows, .. } =
        db.query("SELECT id, a, b, check_sum, blob_seq, data FROM rows", &[])?
    else {
        return runtime("expected rows from rows");
    };
    let mut rs: Vec<(i64, i64, i64, i64, i64, u64, u64)> = Vec::with_capacity(rows.len());
    for r in &rows {
        let (seq, blen, bh) = match (&r[4], &r[5]) {
            (Value::Null, Value::Null) => (-1, 0u64, 0u64),
            (Value::Int(s), Value::Blob(b)) => {
                let mut bh = 0xcbf2_9ce4_8422_2325u64;
                for &x in b.iter() {
                    bh ^= u64::from(x);
                    bh = bh.wrapping_mul(0x1000_0000_01b3);
                }
                (*s, b.len() as u64, bh)
            }
            _ => return runtime("data/blob_seq inconsistent"),
        };
        rs.push((int(&r[0])?, int(&r[1])?, int(&r[2])?, int(&r[3])?, seq, blen, bh));
    }
    rs.sort_unstable();
    mix(rs.len() as u64);
    for (id, a, b, cs, seq, blen, bh) in rs {
        for x in [id, a, b, cs, seq] {
            mix(x as u64);
        }
        mix(blen);
        mix(bh);
    }
    Ok(h)
}

fn int(v: &Value) -> Result<i64, Failure> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(Failure::Runtime(format!("expected int, got {other}"))),
    }
}

/// The workload invariants, checked for their error messages — the digest match
/// below is the sharper test, this one says *what* broke.
fn check_invariants(db: &Database, what: &str) -> CliResult {
    let ExecResult::Rows { rows, .. } = db.query("SELECT balance FROM accounts", &[])? else {
        return runtime("expected rows");
    };
    if rows.len() as i64 != ACCOUNTS {
        return runtime(format!(
            "{what}: {} accounts after recovery, want {ACCOUNTS}",
            rows.len()
        ));
    }
    let mut sum = 0i64;
    for r in &rows {
        sum += int(&r[0])?;
    }
    if sum != BANK_TOTAL {
        return runtime(format!(
            "{what}: BANK SUM VIOLATION after recovery: {sum} != {BANK_TOTAL} — \
             a commit was applied partially"
        ));
    }
    let ExecResult::Rows { rows, .. } =
        db.query("SELECT id, a, b, check_sum, data, blob_seq FROM rows", &[])?
    else {
        return runtime("expected rows");
    };
    if rows.len() as i64 != ROWS {
        return runtime(format!("{what}: {} rows after recovery, want {ROWS}", rows.len()));
    }
    for r in &rows {
        let (id, a, b, cs) = (int(&r[0])?, int(&r[1])?, int(&r[2])?, int(&r[3])?);
        if a + b != 0 || cs != id {
            return runtime(format!(
                "{what}: ROW INVARIANT VIOLATION after recovery: id={id} a={a} b={b} \
                 check_sum={cs}"
            ));
        }
        match (&r[4], &r[5]) {
            (Value::Null, Value::Null) => {}
            (Value::Blob(got), Value::Int(seq)) => {
                let want = blob_for(id, *seq);
                if *got != want {
                    return runtime(format!(
                        "{what}: BLOB CONTENT VIOLATION after recovery: id={id} blob_seq={seq} \
                         (len {} vs expected {}) — a torn overflow chain / extent run survived",
                        got.len(),
                        want.len()
                    ));
                }
            }
            _ => {
                return runtime(format!(
                    "{what}: data/blob_seq inconsistent after recovery for id={id}"
                ))
            }
        }
    }
    Ok(())
}

// -------------------------------------------------------------------- parent

pub fn run_parent(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &[
            "dir", "rounds", "commits", "cuts", "size-mb", "extent-kb", "durability", "sabotage",
            // accepted and ignored: keeps `--workers` shared with the WAL arm
            "workers",
        ],
        &[],
    )?;
    let dir = PathBuf::from(p.require("dir")?);
    let rounds = p.u64_or("rounds", 3)?;
    let commits = p.u64_or("commits", 120)?;
    let cuts_per_round = p.u64_or("cuts", 96)?;
    let extent_kb = match p.u64_or("extent-kb", 0)? {
        0 => None,
        kb => Some(kb),
    };
    let size_mb = p.u64_or("size-mb", if extent_kb.is_some() { 64 } else { 16 })?;
    let sabotage = match p.value("sabotage") {
        None => None,
        Some("reorder") => Some(Sabotage::Reorder),
        Some("drop-data") => Some(Sabotage::DropData),
        Some(other) => return usage(format!("--sabotage must be reorder or drop-data, got {other}")),
    };
    if rounds == 0 || commits < 4 || cuts_per_round == 0 {
        return usage("--rounds >= 1, --commits >= 4, --cuts >= 1");
    }

    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let cfg = dir.join("config.toml");
    let dbf = dir.join("powerloss-commit.mpedb");
    let d0p = dir.join("powerloss-commit.d0");
    let logp = dir.join("commit-sync.log");
    let statep = dir.join("commit-state.log");
    let exe = std::env::current_exe()?;

    let mut tot_cuts = 0u64;
    let mut tot_old = 0u64;
    let mut tot_new = 0u64;
    let mut tot_dropped = 0u64;
    let mut violations: Vec<String> = Vec::new();

    for round in 0..rounds {
        let _wd = Watchdog::arm(ROUND_TIMEOUT_SECS, &format!("powerloss-commit round {round}"));
        let mut rng = Rng::seeded(&[round, 0xc0117]);

        // 1. fresh commit-mode database, seeded, then closed: D0 is a quiesced
        //    image, so "the platter held exactly this and nothing newer" is a
        //    legal starting point for the replay.
        for f in [&dbf, &d0p, &logp, &statep] {
            let _ = std::fs::remove_file(f);
        }
        write_config_durable(&cfg, &dbf, size_mb, COMMIT_TOML, "commit", extent_kb)?;
        {
            let db = Database::open(&cfg)?;
            let ins_a = db.prepare("INSERT INTO accounts (id, balance) VALUES ($1, $2)")?;
            let ins_r = db.prepare("INSERT INTO rows (id, a, b, check_sum) VALUES ($1, $2, $3, $4)")?;
            let mut s = db.begin()?;
            for i in 0..ACCOUNTS {
                s.execute(&ins_a, &params![i, BANK_TOTAL / ACCOUNTS])?;
            }
            for i in 0..ROWS {
                s.execute(&ins_r, &params![i, 0i64, 0i64, i])?;
            }
            s.commit()?;
        }
        std::fs::copy(&dbf, &d0p)?;

        // 2. one writer, instrumented. Exits normally: this harness models the
        //    DEVICE, so nothing is killed and the trace is complete.
        let status = Command::new(&exe)
            .arg("powerloss-commit-child")
            .arg("--dir")
            .arg(&dir)
            .args(["--commits", &commits.to_string()])
            .args(["--seed", &round.to_string()])
            .env("MPEDB_COMMIT_SYNC_LOG", &logp)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            return runtime(format!("round {round}: workload child failed: {status}"));
        }

        // 3. the trace, and the states it is supposed to be able to recover to
        let raw = std::fs::read(&logp)?;
        let evs = parse_log(&raw)?;
        let digests = read_states(&statep)?;
        let commits_t = group_commits(&evs);
        let order = match audit_order(&commits_t) {
            Ok(()) => "ok".to_owned(),
            Err(Failure::Runtime(m)) if sabotage.is_some() => format!("VIOLATION ({m})"),
            Err(e) => return Err(e),
        };
        let (evs, commits_t) = match sabotage {
            Some(s) => apply_sabotage(evs, &commits_t, s),
            None => (evs, commits_t),
        };

        // 4. cut it, every way that matters
        let plans = plan_cuts(&evs, &commits_t, cuts_per_round, &mut rng);
        let d0 = std::fs::read(&d0p)?;
        let mut img = vec![0u8; d0.len()];
        let mut waves = [Wave::default(); WAVES.len()];
        for cut in &plans {
            img.copy_from_slice(&d0);
            let dropped = build_image(&mut img, &raw, &evs, cut, &mut rng);
            // A different boot id is what a reboot looks like to post_attach:
            // it discards the volatile control state (robust mutex, reader
            // table) and re-derives durable_txn from the newest VALID meta.
            let bo = BOOT_ID_FILE_OFFSET as usize;
            img[bo] ^= 0xFF;
            std::fs::write(&dbf, &img)?;

            let expect = expected(&evs, &commits_t, &digests, cut);
            let w = &mut waves[cut.wave];
            w.cuts += 1;
            w.dropped += dropped;
            match verify_cut(&cfg, cut, &expect) {
                Ok(true) => w.new_state += 1,
                Ok(false) => w.old_state += 1,
                Err(Failure::Runtime(msg)) => {
                    w.failed += 1;
                    violations.push(format!("round {round}: {msg}"));
                    if sabotage.is_none() && violations.len() >= 8 {
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        println!(
            "round {round}: txns={} workload-commits={} events={} order={order}",
            commits_t.len(),
            digests.len().saturating_sub(1),
            evs.len()
        );
        for (i, w) in waves.iter().enumerate() {
            if w.cuts == 0 {
                continue;
            }
            println!(
                "round {round}:   wave {:<13} cuts={:<4} pages-lost={:<6} recovered old={:<4} \
                 new={:<4} {}",
                WAVES[i],
                w.cuts,
                w.dropped,
                w.old_state,
                w.new_state,
                if w.failed == 0 {
                    "verify=ok".to_owned()
                } else {
                    format!("verify=FAILED ({} cuts)", w.failed)
                }
            );
            tot_cuts += w.cuts;
            tot_old += w.old_state;
            tot_new += w.new_state;
            tot_dropped += w.dropped;
        }
        if !violations.is_empty() && sabotage.is_some() {
            break; // the sabotage arm only needs one round to make its point
        }
    }

    let _ = std::fs::remove_file(&d0p);
    match sabotage {
        None => {
            // An injector that never fails is indistinguishable from one that
            // never ran (INNOVATIONS.md §6.4) — so refuse to report "clean"
            // unless the run can show it cut something. A silent instrumentation
            // failure (env var not reaching the child, an empty trace, a
            // planner that found no usable commit) lands here, not in a green
            // summary line.
            if tot_cuts == 0 || tot_dropped == 0 {
                return runtime(format!(
                    "powerloss[commit]: {tot_cuts} cuts and {tot_dropped} pages lost — the \
                     injector did not inject anything, so 'no findings' means nothing. \
                     Check that the workload child saw MPEDB_COMMIT_SYNC_LOG."
                ));
            }
            if violations.is_empty() {
                println!(
                    "powerloss[commit]: rounds={rounds} cuts={tot_cuts} pages-lost={tot_dropped} \
                     recovered old={tot_old} new={tot_new} — no meta_T ever survived over \
                     unwritten data; all invariants held"
                );
                Ok(())
            } else {
                for v in &violations {
                    eprintln!("{v}");
                }
                runtime(format!(
                    "powerloss[commit]: {} cut(s) out of {tot_cuts} broke recovery",
                    violations.len()
                ))
            }
        }
        Some(s) => {
            if violations.is_empty() {
                runtime(format!(
                    "powerloss[commit] --sabotage {}: {tot_cuts} cuts, {tot_dropped} pages lost, \
                     and NOTHING was caught. The injector is vacuous — an injector that never \
                     fails is indistinguishable from one that never ran (INNOVATIONS.md §6.4).",
                    s.label()
                ))
            } else {
                println!(
                    "powerloss[commit] --sabotage {}: CAUGHT {} violation(s) in {tot_cuts} cuts \
                     — the injector is live. First: {}",
                    s.label(),
                    violations.len(),
                    violations[0]
                );
                Ok(())
            }
        }
    }
}

const WAVES: [&str; 4] = ["data-inflight", "meta-inflight", "pre-publish", "random"];

#[derive(Clone, Copy, Default)]
struct Wave {
    cuts: u64,
    dropped: u64,
    old_state: u64,
    new_state: u64,
    failed: u64,
}

#[derive(Clone, Copy)]
enum Sabotage {
    Reorder,
    DropData,
}

impl Sabotage {
    fn label(self) -> &'static str {
        match self {
            Sabotage::Reorder => "reorder",
            Sabotage::DropData => "drop-data",
        }
    }
}

/// Rewrite the captured trace into the trace a BROKEN engine would have
/// produced, without patching the engine.
///
/// - `Reorder` moves each commit's data flushes to just after its OWN meta
///   flush: the trace of an engine that publishes and flushes the meta first
///   and only then makes the data durable (equivalently, one whose data barrier
///   moved to the wrong side of `write_meta_slot`).
/// - `DropData` deletes them: an engine with no data barrier at all.
///
/// The commit index is remapped rather than re-derived, because after the
/// rewrite the trace is shape-indistinguishable from a correct one (see
/// [`audit_order`]) — only the rewriter knows which flush now belongs to which
/// commit, which is exactly the information the harness needs to aim its cuts.
fn apply_sabotage(evs: Vec<Ev>, commits: &[CommitTrace], how: Sabotage) -> (Vec<Ev>, Vec<CommitTrace>) {
    let mut dest: Vec<Option<usize>> = vec![None; evs.len()]; // data flush → after this event
    let mut drop_it = vec![false; evs.len()];
    for c in commits {
        let Some(mf) = c.meta_flush else { continue };
        for &d in &c.data {
            match how {
                Sabotage::Reorder => dest[d] = Some(mf),
                Sabotage::DropData => drop_it[d] = true,
            }
        }
    }
    let mut out: Vec<Ev> = Vec::with_capacity(evs.len());
    let mut newidx: Vec<Option<usize>> = vec![None; evs.len()];
    let mut held: Vec<(usize, usize, Ev)> = Vec::new(); // (after, old index, event)
    for (i, ev) in evs.into_iter().enumerate() {
        if drop_it[i] {
            continue;
        }
        if let Some(to) = dest[i] {
            held.push((to, i, ev));
            continue;
        }
        newidx[i] = Some(out.len());
        out.push(ev);
        let mut k = 0;
        while k < held.len() {
            if held[k].0 == i {
                let (_, old, ev) = held.remove(k);
                newidx[old] = Some(out.len());
                out.push(ev);
            } else {
                k += 1;
            }
        }
    }
    for (_, old, ev) in held {
        newidx[old] = Some(out.len());
        out.push(ev);
    }
    let remapped = commits
        .iter()
        .filter_map(|c| {
            Some(CommitTrace {
                txn: c.txn,
                publish: newidx[c.publish]?,
                data: c.data.iter().filter_map(|&d| newidx[d]).collect(),
                meta_flush: c.meta_flush.and_then(|m| newidx[m]),
                barrier_before_publish: c.barrier_before_publish,
                mark: c.mark,
            })
        })
        .collect();
    (out, remapped)
}

fn read_states(path: &Path) -> Result<Vec<u64>, Failure> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((n, d)) = line.split_once(' ') else {
            return runtime("malformed state log line");
        };
        let (n, d): (usize, u64) = (
            n.parse().map_err(|_| Failure::Runtime("bad state index".into()))?,
            d.parse().map_err(|_| Failure::Runtime("bad state digest".into()))?,
        );
        if n != out.len() {
            return runtime(format!("state log out of order at {n}"));
        }
        out.push(d);
    }
    if out.is_empty() {
        return runtime("empty state log — the workload recorded nothing");
    }
    Ok(out)
}

/// Cut positions, four waves. Deterministic per round given the seed.
fn plan_cuts(evs: &[Ev], commits: &[CommitTrace], budget: u64, rng: &mut Rng) -> Vec<Cut> {
    // Only commits the workload owns (they have a recorded digest) and that
    // follow another such commit (so `n_floor` is defined).
    let usable: Vec<&CommitTrace> = commits
        .iter()
        .filter(|c| c.mark.is_some_and(|m| m >= 1) && c.meta_flush.is_some())
        .collect();
    if usable.is_empty() {
        return Vec::new();
    }
    let per_wave = (budget / 4).max(1) as usize;
    let mut out = Vec::new();
    let mut push = |wave: usize, at: usize, fault: Fault| out.push(Cut { at, fault, wave });
    let pick = |rng: &mut Rng, n: usize| -> usize { rng.below(n as u64) as usize };

    for k in 0..per_wave {
        // wave 0: inside a commit's data flush — the meta cannot have landed,
        // so recovery MUST show the previous state exactly.
        let c = usable[pick(rng, usable.len())];
        if !c.data.is_empty() {
            // A random one, not the last: with `MPEDB_MSYNC_PER_RUN=1` (the
            // Darwin arm) a commit has one flush per contiguous run, and cutting
            // mid-sequence leaves some runs durable and the rest not — a state
            // the single-span arm cannot produce.
            let d = c.data[pick(rng, c.data.len())];
            push(0, d, [Fault::None, Fault::Subset, Fault::Sector][k % 3]);
        }
        // wave 1: inside the meta flush — data is durable, the meta may land,
        // vanish, or tear.
        let c = usable[pick(rng, usable.len())];
        push(1, c.meta_flush.unwrap(), [Fault::None, Fault::Sector, Fault::All][k % 3]);
        // wave 2: after the barrier, before the meta was even stored.
        let c = usable[pick(rng, usable.len())];
        push(2, c.publish, Fault::None);
        // wave 3: anywhere at all.
        let lo = usable[0].publish;
        let at = lo + pick(rng, evs.len() - lo);
        push(3, at, [Fault::None, Fault::Subset, Fault::Sector, Fault::All][k % 4]);
    }
    out
}

struct Expect<'a> {
    /// The last state acknowledged strictly before the cut. Recovery may never
    /// show anything older.
    floor: u64,
    floor_digest: u64,
    /// The state the in-flight commit would publish, when it is possible for
    /// its meta to have landed. `None` = it provably cannot have.
    next: Option<(u64, u64)>,
    /// Every recorded state, so a failure can say *which* commit came back
    /// instead of only that the digest was wrong.
    all: &'a [u64],
}

fn expected<'a>(evs: &[Ev], commits: &[CommitTrace], digests: &'a [u64], cut: &Cut) -> Expect<'a> {
    // Marks are consecutive (0, 1, 2, …), so the last one before the cut is the
    // floor and the only other state reachable is floor + 1.
    let floor = evs[..cut.at]
        .iter()
        .filter_map(|e| match e {
            Ev::Mark(t) => Some(*t),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let nx = floor + 1;
    let nd = digests.get(nx as usize).copied();
    // Could commit `nx`'s meta slot be on the platter? Only if its meta flush
    // completed before the cut, or is the cut and did not lose everything.
    // NOTE this reads the flush's position out of the TRACE: in a reordered
    // engine the meta flush sits before the data flushes, so the answer here
    // flips to "yes" for cuts the correct engine forbids — which is how the
    // sabotage arm gets past this gate and is then caught by the digest.
    let reachable = commits
        .iter()
        .find(|c| c.mark == Some(nx))
        .is_none_or(|c| match c.meta_flush {
            Some(mf) => cut.at > mf || (cut.at == mf && cut.fault != Fault::None),
            None => true,
        });
    Expect {
        floor,
        floor_digest: digests.get(floor as usize).copied().unwrap_or(0),
        next: if reachable { nd.map(|d| (nx, d)) } else { None },
        all: digests,
    }
}

/// Open the reconstructed image and hold it to §4.1. `Ok(true)` = recovered to
/// the in-flight commit, `Ok(false)` = to the floor.
fn verify_cut(cfg: &Path, cut: &Cut, expect: &Expect) -> Result<bool, Failure> {
    let what = format!(
        "cut at event {} fault={} (floor state {})",
        cut.at,
        cut.fault.label(),
        expect.floor
    );
    let db = Database::open(cfg).map_err(|e| {
        Failure::Runtime(format!(
            "{what}: recovery FAILED TO OPEN: {e} — one meta slot must always hold a \
             durable commit"
        ))
    })?;
    // Engine errors raised while READING the recovered database (a corrupt row,
    // a bad freelist entry) are findings too, and must carry the cut that
    // produced them — a bare "truncated row" in the report names no cut.
    let ctx = |e: Failure| match e {
        Failure::Runtime(m) if m.starts_with(&what) => Failure::Runtime(m),
        Failure::Runtime(m) => Failure::Runtime(format!("{what}: {m}")),
        other => other,
    };
    check_invariants(&db, &what).map_err(ctx)?;
    db.verify()
        .map_err(|e| Failure::Runtime(format!("{what}: page accounting: {e}")))?;
    let got = state_digest(&db).map_err(ctx)?;
    if got == expect.floor_digest {
        return Ok(false);
    }
    if expect.next.is_some_and(|(_, d)| got == d) {
        return Ok(true);
    }
    // Name the state that did come back, if it is one at all: "an older commit"
    // and "no committed state" are very different bugs.
    let named = expect
        .all
        .iter()
        .position(|&d| d == got)
        .map_or_else(|| "NO committed state".to_owned(), |m| format!("commit {m}"));
    let allowed = match expect.next {
        Some((n, _)) => format!("commit {} or commit {n}", expect.floor),
        None => format!(
            "commit {} only (the next commit's meta provably never reached the platter)",
            expect.floor
        ),
    };
    Err(Failure::Runtime(format!(
        "{what}: recovery landed on {named} (digest {got:#x}); the only legal outcomes were \
         {allowed} — a meta survived over data that never reached the platter, or an \
         acknowledged commit was lost"
    )))
}

// --------------------------------------------------------------------- child

/// Hidden subcommand: the instrumented single writer. Runs exactly `--commits`
/// commits, recording a full-database digest after each one and stamping the
/// commit number into the durability trace.
pub fn run_child(argv: &[String]) -> CliResult {
    use std::io::Write;
    let p = args::parse(argv, &["dir", "commits", "seed"], &[])?;
    let dir = PathBuf::from(p.require("dir")?);
    let commits = p.require_u64("commits")?;
    let seed = p.require_u64("seed")?;
    let db = Database::open(&dir.join("config.toml"))?;
    let mut rng = Rng::seeded(&[seed, 0x5eed]);

    let sel = db.prepare("SELECT balance FROM accounts WHERE id = $1")?;
    let upd_bal = db.prepare("UPDATE accounts SET balance = $1 WHERE id = $2")?;
    let upd_row = db.prepare("UPDATE rows SET a = $1, b = $2 WHERE id = $3")?;
    let upd_blob = db.prepare("UPDATE rows SET data = $1, blob_seq = $2 WHERE id = $3")?;

    let mut states = std::io::BufWriter::new(std::fs::File::create(dir.join("commit-state.log"))?);
    // State 0 is the seeded database: the floor every cut in commit 1 must
    // recover to. Marked BEFORE the first workload commit so the trace has a
    // defined floor from its very first event.
    mpedb_core::plsim::mark(0);
    writeln!(states, "0 {}", state_digest(&db)?)?;

    for n in 1..=commits {
        match rng.below(10) {
            // multi-statement session commit — the direct writer-lock path
            0..=3 => {
                let a = rng.below(ACCOUNTS as u64) as i64;
                let b = (a + 1 + rng.below(ACCOUNTS as u64 - 1) as i64) % ACCOUNTS;
                let amount = 1 + rng.below(50) as i64;
                let mut s = db.begin()?;
                let bal_a = one_int(s.execute(&sel, &params![a])?)?;
                let bal_b = one_int(s.execute(&sel, &params![b])?)?;
                s.execute(&upd_bal, &params![bal_a - amount, a])?;
                s.execute(&upd_bal, &params![bal_b + amount, b])?;
                s.commit()?;
            }
            // autocommit — rides the intent ring's group-commit leader, which
            // is active exactly in durability = commit
            4..=7 => {
                let key = rng.below(ROWS as u64) as i64;
                let x = rng.below(1_000_000) as i64;
                db.execute(&upd_row, &params![x, -x, key])?;
            }
            // multi-page value: overflow chains, or extent runs with
            // --extent-kb. Params exceed the ring cap, so the direct path.
            _ => {
                let key = rng.below(ROWS as u64) as i64;
                let seq = rng.below(1 << 30) as i64;
                db.execute(&upd_blob, &params![blob_for(key, seq), seq, key])?;
            }
        }
        mpedb_core::plsim::mark(n);
        writeln!(states, "{n} {}", state_digest(&db)?)?;
    }
    states.flush()?;
    Ok(())
}

fn one_int(res: ExecResult) -> Result<i64, Failure> {
    match res {
        ExecResult::Rows { mut rows, .. } if rows.len() == 1 => match rows.pop().unwrap().pop() {
            Some(Value::Int(v)) => Ok(v),
            other => Err(Failure::Runtime(format!("expected int, got {other:?}"))),
        },
        other => Err(Failure::Runtime(format!("expected one row, got {other:?}"))),
    }
}

// ---------------------------------------------------------------------- tests
//
// The trace algebra only — the harness itself is a multi-process affair and is
// exercised by running it (`powerloss --durability commit`, and its `--sabotage`
// arm, which fails the command if it finds nothing). What is worth pinning here
// is that a cut lands where the planner says it lands: an off-by-one in
// `build_image` would silently turn every cut into a legal one and the whole
// harness into the vacuous injector it exists not to be.

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic log: `pages` per flush, in the canonical commit shape.
    fn synth(commits: &[(u64, &[u64])]) -> Vec<u8> {
        use mpedb_core::plsim::{EV_BARRIER, EV_FLUSH, EV_MARK, EV_PUBLISH};
        let mut raw = Vec::new();
        let flush = |raw: &mut Vec<u8>, pages: &[u64], fill: u8| {
            raw.push(EV_FLUSH);
            raw.extend_from_slice(&(pages.len() as u32).to_le_bytes());
            for &p in pages {
                raw.extend_from_slice(&(p * PAGE as u64).to_le_bytes());
                raw.extend(std::iter::repeat_n(fill, PAGE));
            }
        };
        for (i, (txn, pages)) in commits.iter().enumerate() {
            flush(&mut raw, pages, *txn as u8);
            raw.push(EV_BARRIER);
            raw.push(EV_PUBLISH);
            raw.extend_from_slice(&txn.to_le_bytes());
            raw.extend_from_slice(&(txn % 2).to_le_bytes());
            flush(&mut raw, &[txn % 2], *txn as u8);
            raw.push(EV_BARRIER);
            raw.push(EV_MARK);
            raw.extend_from_slice(&(i as u64).to_le_bytes());
        }
        raw
    }

    #[test]
    fn trace_parses_and_groups_into_commits() {
        let raw = synth(&[(1, &[10, 11]), (2, &[12])]);
        let evs = parse_log(&raw).unwrap();
        assert_eq!(evs.len(), 12); // 6 events per commit
        let cs = group_commits(&evs);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].txn, 1);
        assert_eq!(cs[0].data, vec![0]);
        assert_eq!(cs[0].meta_flush, Some(3));
        assert_eq!(cs[0].mark, Some(0));
        assert!(cs[0].barrier_before_publish);
        // the SECOND commit's data flush must not be charged to the first —
        // the shape `PUBLISH FLUSH(meta) BARRIER FLUSH(data')` is correct code,
        // and reading it as "data flushed after the meta" was this harness's
        // first bug: it reported a §4.1 violation against a healthy engine.
        assert_eq!(cs[1].data, vec![6]);
        assert!(audit_order(&cs).is_ok());
    }

    #[test]
    fn a_meta_slot_that_is_never_flushed_is_a_violation() {
        use mpedb_core::plsim::{EV_BARRIER, EV_PUBLISH};
        let mut raw = Vec::new();
        raw.push(EV_BARRIER);
        raw.push(EV_PUBLISH);
        raw.extend_from_slice(&7u64.to_le_bytes());
        raw.extend_from_slice(&0u64.to_le_bytes());
        let evs = parse_log(&raw).unwrap();
        let err = audit_order(&group_commits(&evs)).unwrap_err();
        assert!(format!("{err:?}").contains("never flushed"), "{err:?}");
    }

    #[test]
    fn truncated_log_is_an_error_not_a_panic() {
        let raw = synth(&[(1, &[10])]);
        for cut in 1..raw.len().min(200) {
            let _ = parse_log(&raw[..cut]); // must not panic
        }
        assert!(parse_log(&raw[..raw.len() - 1]).is_err());
    }

    #[test]
    fn build_image_applies_exactly_the_prefix() {
        let raw = synth(&[(1, &[10, 11]), (2, &[12])]);
        let evs = parse_log(&raw).unwrap();
        let mut rng = Rng::seeded(&[1]);
        let mut img = vec![0u8; 32 * PAGE];
        // Cut at the meta flush of commit 1 (event 3) losing everything: its
        // data pages are on the platter, its meta is not.
        let dropped = build_image(
            &mut img,
            &raw,
            &evs,
            &Cut { at: 3, fault: Fault::None, wave: 0 },
            &mut rng,
        );
        assert_eq!(dropped, 1);
        assert_eq!(img[10 * PAGE], 1, "commit 1's data must be durable");
        assert_eq!(img[11 * PAGE], 1);
        assert_eq!(img[PAGE], 0, "commit 1's meta slot must NOT be durable");
        assert_eq!(img[12 * PAGE], 0, "nothing after the cut may appear");
    }

    #[test]
    fn sabotage_reorder_moves_data_after_its_own_meta_flush() {
        let raw = synth(&[(1, &[10, 11]), (2, &[12])]);
        let evs = parse_log(&raw).unwrap();
        let cs = group_commits(&evs);
        let (evs2, cs2) = apply_sabotage(evs, &cs, Sabotage::Reorder);
        assert_eq!(evs2.len(), 12, "reordering moves events, never drops them");
        for c in &cs2 {
            let mf = c.meta_flush.unwrap();
            for &d in &c.data {
                assert!(d > mf, "data flush {d} must now follow the meta flush {mf}");
            }
        }
        // …and the remapped indices must still point at the right events.
        assert!(matches!(evs2[cs2[0].meta_flush.unwrap()], Ev::Flush(_)));
        assert!(matches!(evs2[cs2[0].publish], Ev::Publish { txn: 1 }));
        // A cut in the moved data flush now has the new meta on the platter,
        // which is the state the correct trace makes unreachable.
        let digests = vec![100, 200, 300];
        let cut = Cut { at: *cs2[1].data.last().unwrap(), fault: Fault::None, wave: 0 };
        assert!(expected(&evs2, &cs2, &digests, &cut).next.is_some());
    }

    #[test]
    fn a_correct_trace_forbids_the_new_state_before_its_meta_flush() {
        let raw = synth(&[(1, &[10, 11]), (2, &[12])]);
        let evs = parse_log(&raw).unwrap();
        let cs = group_commits(&evs);
        let digests = vec![100, 200, 300];
        // inside commit 2's data flush: only commit 1's state is legal
        let cut = Cut { at: *cs[1].data.last().unwrap(), fault: Fault::Subset, wave: 0 };
        let e = expected(&evs, &cs, &digests, &cut);
        assert_eq!(e.floor, 0);
        assert!(e.next.is_none(), "the meta cannot have landed before it was flushed");
        // at commit 2's meta flush, losing nothing: both are legal
        let cut = Cut { at: cs[1].meta_flush.unwrap(), fault: Fault::All, wave: 1 };
        assert!(expected(&evs, &cs, &digests, &cut).next.is_some());
        // at commit 2's meta flush, losing everything: only the floor again
        let cut = Cut { at: cs[1].meta_flush.unwrap(), fault: Fault::None, wave: 1 };
        assert!(expected(&evs, &cs, &digests, &cut).next.is_none());
    }
}
