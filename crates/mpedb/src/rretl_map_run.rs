//! rRETL #53 — the daemon form of a table-set map sync: bounded work,
//! resumable, and restricted to the process that is supposed to do it.
//!
//! `map sync` is ONE transaction: the set moves together or not at all.
//! That is right for "migrate this set now" and wrong for a cron line —
//! a set too large for one budget can never make progress, and a killed
//! run leaves nothing behind to resume from. `map run` is the other
//! trade: it commits AS IT GOES, so a run that stops has still moved
//! everything it moved, and the next run picks up where this one left off.
//!
//! **Every commit advances the WHOLE set.** One pass over the tables per
//! transaction — a chunk from each table that still has work — rather
//! than finishing table 1 before table 2 begins. So an interrupted run
//! leaves the tables at comparable positions, never "customers fully
//! mirrored, orders untouched", and a reader between two commits sees one
//! step of the whole map rather than one table racing ahead.
//!
//! Four bounds, because a cron line needs all four:
//!
//! - **How much work**: `max_secs` (checked between transactions, so the
//!   overshoot is one chunk) and `max_rows` (rows CLASSIFIED, which makes
//!   a run deterministic in tests without touching a clock).
//! - **Where it left off**: `rretl_map_cursor` per (map, table) — which
//!   pass, and how far into it. A round is passes 1→2→3 over the whole
//!   set; finishing pass 3 everywhere completes the round, clears the
//!   cursors and bumps `round`. Rows that changed BEHIND the cursor are
//!   picked up by the next round, which is what "eventually consistent
//!   per round" means and is the honest description of any incremental
//!   mirror.
//! - **Who may run it**: an optional `runner` recorded per map. A run
//!   without the matching `--runner` is refused BY NAME. This is a policy
//!   guard against mistakes — a laptop running the cron line by accident
//!   — and NOT a security boundary: mpedb has no auth layer, and anything
//!   that can write the file can claim any runner name. The real fence is
//!   the OS: only the server has the file and the crontab entry.
//! - **Not twice at once**: a lease with a deadline. Overlap is harmless
//!   (the work is idempotent — a second runner would classify the rows
//!   the first already pushed as clean and skip them), so the lease buys
//!   wasted-work avoidance, not correctness, and a stale one expires
//!   rather than wedging the map forever.
//!
//! A CONFLICT is counted and SKIPPED, not fatal. `map sync` aborts whole
//! on the first, because a migration wants all-or-nothing; a daemon that
//! did that would let one unresolvable row block every other row's sync
//! forever. `map check` remains the place to see them all by name.

use mpedb_types::{ColumnType, Error, Result, Value};

use crate::rretl::{chunk_rows, next_run_id, now_micros, rows_of, spec_col, LineageRow};
use crate::rretl_map::{
    classify_p1, classify_p2, MapSql, MapSyncReport, MapWriter, ResolvedTable, P1, P2,
};
use crate::WriteSession;

pub const T_MAP_CURSOR: &str = "rretl_map_cursor";
pub const T_MAP_RUN: &str = "rretl_map_run";
const CURSOR_SHAPE: [&str; 5] = ["map", "tbl", "phase", "k", "pk_enc"];
const RUN_SHAPE: [&str; 6] = ["map", "runner", "round", "lease_owner", "lease_until", "note"];

/// How a run ended.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RunStop {
    /// The round completed: every table finished every pass.
    RoundComplete,
    /// `max_secs` or `max_rows` ran out; the cursors say where to resume.
    Budget,
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Wall-clock ceiling. Checked BETWEEN transactions, so a run may
    /// overshoot by one chunk. `None` = no time limit.
    pub max_secs: Option<u64>,
    /// Ceiling on rows classified. Clock-free, so tests are exact.
    pub max_rows: Option<u64>,
    /// Identity this process claims. Must match the map's recorded runner
    /// when one is set.
    pub runner: Option<String>,
    /// Lease length; defaults to `max_secs + 60`, or 300 s when untimed.
    pub lease_secs: Option<u64>,
}

#[derive(Debug, Default)]
pub struct MapRunReport {
    pub moved: MapSyncReport,
    /// Rows classified this run (the `max_rows` budget's unit).
    pub rows: u64,
    /// Transactions committed — each one advanced the whole set by a chunk.
    pub commits: u64,
    /// Rows a sync would have aborted on. Counted and skipped here.
    pub conflicts: u64,
    /// The first few, verbatim, so a cron mail says something useful.
    pub conflict_notes: Vec<String>,
    /// Journal entries consumed (§15). Zero on a map that is not streaming.
    pub streamed: u64,
    pub round: i64,
    pub stopped_by: Option<RunStop>,
}

impl MapRunReport {
    /// Count a conflict and keep the first few verbatim. ONE place, because
    /// the scan and the journal drain both report into the same run.
    pub(crate) fn note_conflict(&mut self, msg: String) {
        self.conflicts += 1;
        if self.conflict_notes.len() < MAX_CONFLICT_NOTES {
            self.conflict_notes.push(msg);
        }
    }

    pub fn note(&self) -> String {
        format!(
            "round {}, {} row(s) in {} commit(s): a→b {}, b→a {}, +b {}, +a {}, -a {}, \
             -b {}, clean {}, conflicts {} — {}",
            self.round,
            self.rows,
            self.commits,
            self.moved.a_to_b,
            self.moved.b_to_a,
            self.moved.created_b,
            self.moved.created_a,
            self.moved.deleted_a,
            self.moved.deleted_b,
            self.moved.unchanged,
            self.conflicts,
            match self.stopped_by {
                Some(RunStop::RoundComplete) => "round complete",
                Some(RunStop::Budget) => "budget spent, cursors saved",
                None => "nothing to do",
            }
        )
    }
}

/// Where one table stands in the current round.
#[derive(Clone)]
struct Cursor {
    /// 1, 2 or 3 = that pass is in progress; 0 = done for this round.
    phase: i64,
    /// Last key handled in pass 1/2 (`Null` = start of the pass).
    k: Value,
    /// Last `pk_enc` handled in pass 3 (`Null` = start).
    pk_enc: Value,
}

impl Cursor {
    fn start() -> Cursor {
        Cursor { phase: 1, k: Value::Null, pk_enc: Value::Null }
    }
}

const MAX_CONFLICT_NOTES: usize = 8;

fn ensure_run_tables(s: &mut WriteSession<'_>, have: &[(String, Vec<String>)]) -> Result<()> {
    use ColumnType::{Any, Blob, Int64, Text};
    if !crate::rretl::shape_gate(have, T_MAP_CURSOR, &CURSOR_SHAPE)? {
        crate::rretl::create_bookkeeping(
            s,
            T_MAP_CURSOR,
            vec![
                spec_col("map", Text),
                spec_col("tbl", Text),
                spec_col("phase", Int64),
                spec_col("k", Any),
                spec_col("pk_enc", Blob),
            ],
            &["map", "tbl"],
        )?;
    }
    if !crate::rretl::shape_gate(have, T_MAP_RUN, &RUN_SHAPE)? {
        crate::rretl::create_bookkeeping(
            s,
            T_MAP_RUN,
            vec![
                spec_col("map", Text),
                spec_col("runner", Text),
                spec_col("round", Int64),
                spec_col("lease_owner", Text),
                spec_col("lease_until", Int64),
                spec_col("note", Text),
            ],
            &["map"],
        )?;
    }
    Ok(())
}

const RUN_GET: &str =
    "SELECT runner, round, lease_owner, lease_until FROM rretl_map_run WHERE map = $1";

/// What the map's run record says right now.
struct RunRecord {
    runner: String,
    round: i64,
    lease_owner: String,
    lease_until: i64,
}

fn read_run(s: &mut WriteSession<'_>, name: &str) -> Result<RunRecord> {
    let rows = rows_of(s.query(RUN_GET, &[Value::Text(name.into())])?)?;
    Ok(match rows.first() {
        Some(r) => RunRecord {
            runner: crate::rretl::as_text(&r[0]),
            round: crate::rretl::as_int(&r[1])?,
            lease_owner: crate::rretl::as_text(&r[2]),
            lease_until: crate::rretl::as_int(&r[3])?,
        },
        None => RunRecord {
            runner: String::new(),
            round: 0,
            lease_owner: String::new(),
            lease_until: 0,
        },
    })
}

fn write_run(
    s: &mut WriteSession<'_>,
    name: &str,
    rec: &RunRecord,
    note: &str,
) -> Result<()> {
    let p = [
        Value::Text(name.into()),
        Value::Text(rec.runner.clone()),
        Value::Int(rec.round),
        Value::Text(rec.lease_owner.clone()),
        Value::Int(rec.lease_until),
        Value::Text(note.into()),
    ];
    let hit = matches!(
        s.query(
            "UPDATE rretl_map_run SET runner = $2, round = $3, lease_owner = $4, \
             lease_until = $5, note = $6 WHERE map = $1",
            &p,
        )?,
        crate::ExecResult::Affected(n) if n > 0
    );
    if !hit {
        s.query(
            "INSERT INTO rretl_map_run (map, runner, round, lease_owner, lease_until, note) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &p,
        )?;
    }
    Ok(())
}

fn read_cursor(s: &mut WriteSession<'_>, name: &str, tbl: &str) -> Result<Cursor> {
    let rows = rows_of(s.query(
        "SELECT phase, k, pk_enc FROM rretl_map_cursor WHERE map = $1 AND tbl = $2",
        &[Value::Text(name.into()), Value::Text(tbl.into())],
    )?)?;
    Ok(match rows.first() {
        Some(r) => Cursor {
            phase: crate::rretl::as_int(&r[0])?,
            k: r[1].clone(),
            pk_enc: r[2].clone(),
        },
        None => Cursor::start(),
    })
}

fn write_cursor(s: &mut WriteSession<'_>, name: &str, tbl: &str, c: &Cursor) -> Result<()> {
    let p = [
        Value::Text(name.into()),
        Value::Text(tbl.into()),
        Value::Int(c.phase),
        c.k.clone(),
        c.pk_enc.clone(),
    ];
    let hit = matches!(
        s.query(
            "UPDATE rretl_map_cursor SET phase = $3, k = $4, pk_enc = $5 \
             WHERE map = $1 AND tbl = $2",
            &p,
        )?,
        crate::ExecResult::Affected(n) if n > 0
    );
    if !hit {
        s.query(
            "INSERT INTO rretl_map_cursor (map, tbl, phase, k, pk_enc) VALUES ($1, $2, $3, $4, $5)",
            &p,
        )?;
    }
    Ok(())
}

impl crate::Database {
    /// Record which runner may run this map (empty = anyone). See the
    /// module docs: a policy guard against mistakes, not a security
    /// boundary.
    pub fn rretl_map_set_runner(&self, name: &str, runner: &str) -> Result<()> {
        self.load_map(name)?; // the map must exist
        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let res = (|| -> Result<()> {
            ensure_run_tables(&mut s, &have)?;
            let mut rec = read_run(&mut s, name)?;
            rec.runner = runner.to_string();
            write_run(&mut s, name, &rec, "runner set")
        })();
        match res {
            Ok(()) => s.commit()?,
            Err(e) => {
                s.rollback();
                return Err(e);
            }
        }
        Ok(())
    }

    /// The map's daemon status: recorded runner, current round, live lease.
    pub fn rretl_map_status(&self, name: &str) -> Result<MapRunStatus> {
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == T_MAP_RUN) {
            return Ok(MapRunStatus::default());
        }
        let rows = rows_of(self.query(
            "SELECT runner, round, lease_owner, lease_until, note FROM rretl_map_run \
             WHERE map = $1",
            &[Value::Text(name.into())],
        )?)?;
        let Some(r) = rows.first() else {
            return Ok(MapRunStatus::default());
        };
        let mut st = MapRunStatus {
            runner: crate::rretl::as_text(&r[0]),
            round: crate::rretl::as_int(&r[1])?,
            lease_owner: crate::rretl::as_text(&r[2]),
            lease_until: crate::rretl::as_int(&r[3])?,
            note: crate::rretl::as_text(&r[4]),
            in_progress: Vec::new(),
        };
        if have.iter().any(|(n, _)| n == T_MAP_CURSOR) {
            for row in rows_of(self.query(
                "SELECT tbl, phase FROM rretl_map_cursor WHERE map = $1 AND phase > 0 \
                 ORDER BY tbl",
                &[Value::Text(name.into())],
            )?)? {
                st.in_progress
                    .push((crate::rretl::as_text(&row[0]), crate::rretl::as_int(&row[1])?));
            }
        }
        Ok(st)
    }

    /// One bounded, resumable pass of the daemon. See the module docs for
    /// what it trades away versus [`rretl_map_sync`](Self::rretl_map_sync).
    pub fn rretl_map_run(&self, name: &str, opts: &RunOptions) -> Result<MapRunReport> {
        let spec = self.load_map(name)?;
        let resolved = self.resolve_map(&spec)?;
        let started = std::time::Instant::now();
        let lease_secs = opts
            .lease_secs
            .unwrap_or_else(|| opts.max_secs.map(|s| s + 60).unwrap_or(300));
        let me = opts.runner.clone().unwrap_or_else(|| format!("pid:{}", std::process::id()));

        // ---- claim: policy first, then the lease, in one transaction ----
        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let claimed = (|| -> Result<i64> {
            ensure_run_tables(&mut s, &have)?;
            crate::rretl_map::prepare_map_tables(&mut s, name, &resolved, &have, spec.stream)?;
            let mut rec = read_run(&mut s, name)?;
            if !rec.runner.is_empty() && opts.runner.as_deref() != Some(rec.runner.as_str()) {
                return Err(Error::Unsupported(format!(
                    "map `{name}` is restricted to runner `{}`; this process claims {} — \
                     pass the matching --runner, or clear the restriction with \
                     `map runner {name} \"\"`",
                    rec.runner,
                    match &opts.runner {
                        Some(r) => format!("`{r}`"),
                        None => "no runner".to_string(),
                    }
                )));
            }
            let now = now_micros();
            if rec.lease_until > now && rec.lease_owner != me {
                return Err(Error::Busy);
            }
            rec.lease_owner = me.clone();
            rec.lease_until = now + (lease_secs as i64) * 1_000_000;
            let round = rec.round;
            write_run(&mut s, name, &rec, "running")?;
            Ok(round)
        })();
        let round = match claimed {
            Ok(r) => {
                s.commit()?;
                r
            }
            Err(e) => {
                s.rollback();
                return Err(e);
            }
        };

        let mut report = MapRunReport { round, ..Default::default() };
        let chunk = chunk_rows();
        // A map can be streaming without the journal table existing yet (a
        // spec that declares it but has never seen a write), so both have to
        // be true before the drain reads.
        let streaming = spec.stream
            && self
                .committed_tables()?
                .iter()
                .any(|(n, _)| n == crate::rretl_map_stream::T_MAP_DIRTY);
        let over_budget = |rows: u64| {
            opts.max_rows.is_some_and(|m| rows >= m)
                || opts
                    .max_secs
                    .is_some_and(|m| started.elapsed().as_secs() >= m)
        };

        // ---- work: one transaction per pass over ALL tables -------------
        loop {
            if over_budget(report.rows) {
                report.stopped_by = Some(RunStop::Budget);
                break;
            }
            let mut s = self.begin()?;
            let step = (|| -> Result<bool> {
                let mut any = false;
                for rt in &resolved {
                    // The journal FIRST, and in the same transaction as the
                    // scan chunk below (§15.5): a kill between commits can
                    // then never separate "the row was synced" from "the
                    // entry was consumed". Draining costs nothing when the
                    // map is not streaming — the table does not exist.
                    if streaming {
                        let n = crate::rretl_map_stream::drain_chunk(
                            &mut s, name, rt, chunk, &mut report,
                        )?;
                        report.streamed += n as u64;
                        any |= n > 0;
                    }
                    let mut cur = read_cursor(&mut s, name, &rt.dst)?;
                    if cur.phase == 0 {
                        continue;
                    }
                    any = true;
                    run_one_chunk(&mut s, name, rt, &mut cur, chunk, &mut report)?;
                    write_cursor(&mut s, name, &rt.dst, &cur)?;
                }
                Ok(any)
            })();
            match step {
                Ok(true) => {
                    s.commit()?;
                    report.commits += 1;
                }
                Ok(false) => {
                    // Every table finished every pass: the round is done.
                    s.query(
                        "DELETE FROM rretl_map_cursor WHERE map = $1",
                        &[Value::Text(name.into())],
                    )?;
                    let mut rec = read_run(&mut s, name)?;
                    rec.round += 1;
                    report.round = rec.round;
                    write_run(&mut s, name, &rec, "round complete")?;
                    s.commit()?;
                    report.commits += 1;
                    report.stopped_by = Some(RunStop::RoundComplete);
                    break;
                }
                Err(e) => {
                    s.rollback();
                    self.release_lease(name, &me, &format!("failed: {e}"))?;
                    let _ = self.record_failed_run(&format!("map:{name}"), "", name, &e);
                    return Err(e);
                }
            }
        }

        self.release_lease(name, &me, &report.note())?;
        // Lineage only when something happened: a cron line firing every
        // minute over a quiet map must not grow the log forever (#124 is
        // about exactly that).
        if report.moved.changed_total() > 0 || report.conflicts > 0 {
            let mut s = self.begin()?;
            let res = (|| -> Result<()> {
                let run_id = next_run_id(&mut s)?;
                LineageRow {
                    run_id,
                    lens: format!("map:{name}"),
                    forward_hash: format!("map:{name}"),
                    rex_hash: String::new(),
                    inverse_hash: format!("map:{name}"),
                    table: String::new(),
                    column: name.into(),
                    source_hash: String::new(),
                    output_hash: String::new(),
                    residual_hash: String::new(),
                    rows: report.moved.changed_total() as i64,
                    outcome: "mapped",
                    error: report.note(),
                }
                .insert(&mut s)
            })();
            match res {
                Ok(()) => s.commit()?,
                Err(e) => {
                    s.rollback();
                    return Err(e);
                }
            }
        }
        Ok(report)
    }

    fn release_lease(&self, name: &str, me: &str, note: &str) -> Result<()> {
        let mut s = self.begin()?;
        let res = (|| -> Result<()> {
            let mut rec = read_run(&mut s, name)?;
            if rec.lease_owner == me {
                rec.lease_owner = String::new();
                rec.lease_until = 0;
            }
            write_run(&mut s, name, &rec, note)
        })();
        match res {
            Ok(()) => s.commit()?,
            Err(e) => {
                s.rollback();
                return Err(e);
            }
        }
        Ok(())
    }
}

/// The daemon status of one map.
#[derive(Debug, Default)]
pub struct MapRunStatus {
    pub runner: String,
    pub round: i64,
    pub lease_owner: String,
    pub lease_until: i64,
    pub note: String,
    /// (table, phase) for every table still mid-round.
    pub in_progress: Vec<(String, i64)>,
}

/// One chunk of one table, in whichever pass its cursor says. Advances the
/// cursor — to the next key, or to the next pass when the chunk came up
/// short. A conflict is counted and skipped; everything else is applied.
fn run_one_chunk(
    s: &mut WriteSession<'_>,
    name: &str,
    rt: &ResolvedTable,
    cur: &mut Cursor,
    chunk: usize,
    report: &mut MapRunReport,
) -> Result<()> {
    let sql = MapSql::new(rt, chunk);
    let mut w = MapWriter::new(name, rt, &sql);
    let note = |report: &mut MapRunReport, msg: String| report.note_conflict(msg);
    match cur.phase {
        1 => {
            let rows = match &cur.k {
                Value::Null => rows_of(s.query(&sql.p1_first, &[])?)?,
                k => rows_of(s.query(&sql.p1_next, std::slice::from_ref(k))?)?,
            };
            let got = rows.len();
            for row in &rows {
                let (key, xs) = (&row[0], &row[1..]);
                let st = w.state_of(s, key)?;
                let b = w.target_row(s, key)?;
                report.rows += 1;
                match classify_p1(rt, name, key, xs, st, b, false)? {
                    P1::Conflict(msg) => note(report, msg),
                    action => w.apply_p1(s, key, action, &mut report.moved)?,
                }
            }
            if got > 0 {
                cur.k = rows[got - 1][0].clone();
            }
            if got < chunk {
                cur.phase = 2;
                cur.k = Value::Null;
            }
        }
        2 => {
            let rows = match &cur.k {
                Value::Null => rows_of(s.query(&sql.p2_first, &[])?)?,
                k => rows_of(s.query(&sql.p2_next, std::slice::from_ref(k))?)?,
            };
            let got = rows.len();
            for row in &rows {
                let (key, ybs) = (&row[0], &row[1..]);
                if !rows_of(s.query(&sql.src_exists, std::slice::from_ref(key))?)?.is_empty() {
                    continue; // handled in pass 1
                }
                let st = w.state_of(s, key)?;
                report.rows += 1;
                match classify_p2(rt, name, key, ybs, st)? {
                    P2::Conflict(msg) => note(report, msg),
                    action => w.apply_p2(s, key, action, &mut report.moved)?,
                }
            }
            if got > 0 {
                cur.k = rows[got - 1][0].clone();
            }
            if got < chunk {
                cur.phase = 3;
                cur.k = Value::Null;
                cur.pk_enc = Value::Null;
            }
        }
        _ => {
            let mut p = w.map_tbl();
            let rows = match &cur.pk_enc {
                Value::Null => rows_of(s.query(&sql.p3_first, &p)?)?,
                pk => {
                    p.push(pk.clone());
                    rows_of(s.query(&sql.p3_next, &p)?)?
                }
            };
            let got = rows.len();
            for row in &rows {
                report.rows += 1;
                w.sweep_state_row(s, &row[0], &row[1])?;
            }
            if got > 0 {
                cur.pk_enc = rows[got - 1][0].clone();
            }
            if got < chunk {
                cur.phase = 0;
                cur.k = Value::Null;
                cur.pk_enc = Value::Null;
            }
        }
    }
    Ok(())
}
