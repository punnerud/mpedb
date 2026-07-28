//! The receipt protocol (DESIGN-INGEST §10): the user pushes what they
//! fetched, mpedb works out exactly what changed.
//!
//! Streamed and chunk-committed, so memory is O(chunk) no matter how large
//! the dump, and **no DDL runs per receipt** — DDL bumps the schema
//! generation and invalidates every other process's prepared plans, which
//! a job running every five minutes must not do (learned in #53).
//!
//! A `dump` presents the WHOLE table, so absence means deletion: each
//! presented key is recorded in `ingest_seen`, and `finish` sweeps the
//! target for keys the dump never mentioned. A `delta` skips that sweep,
//! because "rows that changed" can never say a row is gone — and the
//! receipt records that fact rather than implying coverage.
//!
//! Every dump also re-earns the cursor's reputation (§5): for each row the
//! dump found changed, would the declared cursor candidate have caught it?
//! The first miss flips the verdict to `unsafe` and names the row. That is
//! the moment a lying `updated_at` becomes a fact in the database.

use mpedb_types::{Error, Result, Value};

use crate::ingest::{
    ensure_ingest_tables, read_state, read_state_in, write_state, EdgeKind, EdgeState, IngestSpec,
    Policy, ResolvedEdge, Strategy,
};
use crate::rretl::{chunk_rows, now_micros, pk_ref, rows_of};
use crate::WriteSession;

/// What a receipt found.
#[derive(Debug, Clone)]
pub struct IngestReport {
    pub run_id: i64,
    pub edge: String,
    pub table: String,
    pub mode: String,
    pub rows_in: i64,
    pub inserted: i64,
    pub updated: i64,
    pub deleted: i64,
    pub unchanged: i64,
    /// Rows the policy would not decide. Recorded in `ingest_conflicts`.
    pub conflicts: i64,
    pub calls: i64,
    pub bytes: i64,
    /// `unknown` | `safe` | `unsafe` — see [`IngestReport::cursor_note`].
    pub cursor_state: String,
    pub caught: i64,
    pub missed: i64,
    /// The key of one row this receipt found changed that the cursor would
    /// NOT have caught — so the verdict can be chased down, not just counted.
    pub missed_example: Value,
    pub watermark: Value,
    /// True when this receipt could see deletes at all.
    pub complete: bool,
}

impl Default for IngestReport {
    fn default() -> IngestReport {
        IngestReport {
            run_id: 0,
            edge: String::new(),
            table: String::new(),
            mode: String::new(),
            rows_in: 0,
            inserted: 0,
            updated: 0,
            deleted: 0,
            unchanged: 0,
            conflicts: 0,
            calls: 0,
            bytes: 0,
            cursor_state: "unknown".into(),
            caught: 0,
            missed: 0,
            missed_example: Value::Null,
            watermark: Value::Null,
            complete: false,
        }
    }
}

impl IngestReport {
    pub fn changed(&self) -> i64 {
        self.inserted + self.updated + self.deleted
    }

    /// The one sentence worth putting in a cron mail.
    pub fn note(&self) -> String {
        format!(
            "{} on `{}`: {} row(s) in — +{} ~{} -{} ={}, {} conflict(s), {} call(s) {} byte(s){}",
            self.mode,
            self.table,
            self.rows_in,
            self.inserted,
            self.updated,
            self.deleted,
            self.unchanged,
            self.conflicts,
            self.calls,
            self.bytes,
            if self.complete { "" } else { " — a delta cannot see deletes" }
        )
    }

    pub fn cursor_note(&self) -> String {
        match self.cursor_state.as_str() {
            "unsafe" => format!(
                "cursor UNSAFE: {} of {} changed rows would NOT have been caught by it{}",
                self.missed,
                self.caught + self.missed,
                match &self.missed_example {
                    Value::Null => String::new(),
                    v => format!(" (e.g. the row keyed {v:?})"),
                }
            ),
            "safe" => format!(
                "cursor safe so far: {} changed rows all carried a moved cursor",
                self.caught
            ),
            _ => "cursor unverified (no dump has tested it yet)".into(),
        }
    }
}

/// Which receipt a caller is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The whole table. Finds deletes; re-verifies the cursor.
    Dump,
    /// Rows changed since the watermark. Cannot find deletes.
    Delta,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode> {
        Ok(match s {
            "dump" => Mode::Dump,
            "delta" => Mode::Delta,
            other => {
                return Err(Error::Unsupported(format!(
                    "ingest: `{other}` is not a mode (dump, delta)"
                )))
            }
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Dump => "dump",
            Mode::Delta => "delta",
        }
    }
}

/// An open receipt, as `ingest_stats` holds it between chunks. Persisted
/// per chunk so a killed process leaves an honest partial record rather
/// than a counter that only existed in memory.
struct OpenRun {
    source: String,
    edge: String,
    table: String,
    mode: Mode,
    rows_in: i64,
    inserted: i64,
    updated: i64,
    deleted: i64,
    unchanged: i64,
    conflicts: i64,
    calls: i64,
    bytes: i64,
    caught: i64,
    missed: i64,
    watermark: Value,
    /// Not persisted: one example key for THIS receipt's verdict. A run that
    /// resumes after a crash has no verdict of its own to name yet.
    missed_example: Value,
}

fn read_open(s: &mut WriteSession<'_>, run_id: i64) -> Result<OpenRun> {
    let rows = rows_of(s.query(
        "SELECT source, edge, tbl, mode, rows_in, inserted, updated, deleted, unchanged, \
         conflicts, calls, bytes, caught, missed, watermark, state FROM ingest_stats \
         WHERE run_id = $1",
        &[Value::Int(run_id)],
    )?)?;
    let r = rows
        .first()
        .ok_or_else(|| Error::Unsupported(format!("ingest: no receipt {run_id}")))?;
    if crate::rretl::as_text(&r[15]) != "open" {
        return Err(Error::Unsupported(format!(
            "ingest: receipt {run_id} is already closed"
        )));
    }
    Ok(OpenRun {
        source: crate::rretl::as_text(&r[0]),
        edge: crate::rretl::as_text(&r[1]),
        table: crate::rretl::as_text(&r[2]),
        mode: Mode::parse(&crate::rretl::as_text(&r[3]))?,
        rows_in: crate::rretl::as_int(&r[4])?,
        inserted: crate::rretl::as_int(&r[5])?,
        updated: crate::rretl::as_int(&r[6])?,
        deleted: crate::rretl::as_int(&r[7])?,
        unchanged: crate::rretl::as_int(&r[8])?,
        conflicts: crate::rretl::as_int(&r[9])?,
        calls: crate::rretl::as_int(&r[10])?,
        bytes: crate::rretl::as_int(&r[11])?,
        caught: crate::rretl::as_int(&r[12])?,
        missed: crate::rretl::as_int(&r[13])?,
        watermark: r[14].clone(),
        missed_example: Value::Null,
    })
}

fn wm_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Text(t) => t.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        other => format!("{other:?}"),
    }
}

/// Cursor ordering across types. A cursor is whatever the source orders
/// its "changed since" by — a timestamp string, an epoch integer, an LSN.
/// Numbers compare numerically, everything else lexically, and comparing
/// two different shapes is refused rather than guessed.
fn cursor_gt(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (_, Value::Null) => true,
        (Value::Null, _) => false,
        (Value::Int(x), Value::Int(y)) => x > y,
        (Value::Float(x), Value::Float(y)) => x > y,
        (Value::Int(x), Value::Float(y)) => (*x as f64) > *y,
        (Value::Float(x), Value::Int(y)) => *x > (*y as f64),
        _ => wm_text(a) > wm_text(b),
    }
}

fn write_open(s: &mut WriteSession<'_>, run_id: i64, r: &OpenRun, state: &str) -> Result<()> {
    s.query(
        "UPDATE ingest_stats SET rows_in = $2, inserted = $3, updated = $4, deleted = $5, \
         unchanged = $6, conflicts = $7, calls = $8, bytes = $9, caught = $10, missed = $11, \
         watermark = $12, state = $13 WHERE run_id = $1",
        &[
            Value::Int(run_id),
            Value::Int(r.rows_in),
            Value::Int(r.inserted),
            Value::Int(r.updated),
            Value::Int(r.deleted),
            Value::Int(r.unchanged),
            Value::Int(r.conflicts),
            Value::Int(r.calls),
            Value::Int(r.bytes),
            Value::Int(r.caught),
            Value::Int(r.missed),
            r.watermark.clone(),
            Value::Text(state.into()),
        ],
    )?;
    Ok(())
}

/// Pick the edge a caller means. An edge NAME is unambiguous; a table name
/// plus a mode picks the edge that presents the table that way.
fn pick_edge<'a>(
    spec: &'a IngestSpec,
    resolved: &'a [ResolvedEdge],
    target: &str,
    mode: Mode,
) -> Result<&'a ResolvedEdge> {
    if let Some(e) = resolved.iter().find(|e| e.spec.name.eq_ignore_ascii_case(target)) {
        return Ok(e);
    }
    let want_complete = mode == Mode::Dump;
    let mut hits = resolved
        .iter()
        .filter(|e| {
            e.table.eq_ignore_ascii_case(target)
                && e.spec.presents_whole_table() == want_complete
        })
        .collect::<Vec<_>>();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(Error::Unsupported(format!(
            "ingest `{}`: no edge presents `{target}` as a {}. Declared edges: {}",
            spec.name,
            mode.as_str(),
            resolved
                .iter()
                .map(|e| format!("{} ({} {})", e.spec.name, e.spec.strategy.as_str(), e.table))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
        n => Err(Error::Unsupported(format!(
            "ingest `{}`: {n} edges present `{target}` as a {} — name the edge instead",
            spec.name,
            mode.as_str()
        ))),
    }
}

impl crate::Database {
    /// Open a streamed receipt. Refuses a second open receipt for the same
    /// table: a dump and a delta running at once would need a watermark
    /// dedupe rule (Debezium DDD-3), and v1's answer is that the window
    /// cannot open (DESIGN-INGEST P8).
    pub fn ingest_begin(&self, source: &str, target: &str, mode: Mode) -> Result<i64> {
        let spec = self.load_ingest(source)?;
        let resolved = self.resolve_ingest(&spec)?;
        let edge = pick_edge(&spec, &resolved, target, mode)?;
        if mode == Mode::Dump && !edge.spec.presents_whole_table() {
            return Err(Error::Unsupported(if edge.spec.kind == EdgeKind::Root {
                format!(
                    "ingest `{source}`: edge `{}` is a {}, which does not present the whole \
                     table — a dump receipt through it would read every absent row as a delete",
                    edge.spec.name,
                    edge.spec.strategy.as_str()
                )
            } else {
                format!(
                    "ingest `{source}`: edge `{}` is {}, so it is SCOPED to the keys that drove \
                     it — a dump receipt through it would read every row it was never asked \
                     about as a delete. Take it as a delta: a derived receipt upserts what it \
                     fetched and never infers a delete (DESIGN-INGEST §2)",
                    edge.spec.name,
                    edge.spec.kind.as_str()
                )
            }));
        }
        if mode == Mode::Delta && edge.spec.presents_whole_table() {
            return Err(Error::Unsupported(format!(
                "ingest `{source}`: edge `{}` presents the WHOLE table, so a delta receipt \
                 through it would record a partial read as a complete one. Name the delta \
                 edge for `{}`, or take this one as a dump",
                edge.spec.name, edge.table
            )));
        }
        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let out = (|| -> Result<i64> {
            ensure_ingest_tables(&mut s, &have)?;
            let open = rows_of(s.query(
                "SELECT run_id FROM ingest_stats WHERE source = $1 AND tbl = $2 AND state = 'open'",
                &[Value::Text(source.into()), Value::Text(edge.table.clone())],
            )?)?;
            if let Some(r) = open.first() {
                return Err(Error::Unsupported(format!(
                    "ingest `{source}`: receipt {} is still open on `{}` — one receipt per \
                     table at a time, so a dump and a delta cannot interleave. Finish it, or \
                     `ingest abandon` it",
                    crate::rretl::as_int(&r[0])?,
                    edge.table
                )));
            }
            // Ingest numbers its own receipts: reusing rRETL's counter would
            // couple the two features' bookkeeping, and a source can be
            // pulled into a file that has never seen a lens pair.
            let last = rows_of(s.query("SELECT max(run_id) FROM ingest_stats", &[])?)?;
            let run_id = match last.first().map(|r| &r[0]) {
                Some(Value::Int(n)) => n + 1,
                _ => 1,
            };
            s.query(
                "INSERT INTO ingest_stats (run_id, source, edge, tbl, mode, ts_micros, rows_in, \
                 inserted, updated, deleted, unchanged, conflicts, calls, bytes, changed, \
                 caught, missed, watermark, verdict, state, note) \
                 VALUES ($1, $2, $3, $4, $5, $6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, NULL, '', \
                 'open', '')",
                &[
                    Value::Int(run_id),
                    Value::Text(source.into()),
                    Value::Text(edge.spec.name.clone()),
                    Value::Text(edge.table.clone()),
                    Value::Text(mode.as_str().into()),
                    Value::Int(now_micros()),
                ],
            )?;
            Ok(run_id)
        })();
        match out {
            Ok(run_id) => {
                s.commit()?;
                Ok(run_id)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Push one chunk of what you fetched. `calls`/`bytes` are what the
    /// call actually cost — mpedb cannot see the wire, so it trusts them,
    /// and they are what the next plan is computed against.
    pub fn ingest_rows(
        &self,
        run_id: i64,
        cols: &[String],
        rows: &[Vec<Value>],
        calls: i64,
        bytes: i64,
    ) -> Result<IngestReport> {
        let mut s = self.begin()?;
        let out = (|| -> Result<IngestReport> {
            let mut run = read_open(&mut s, run_id)?;
            let spec = self.load_ingest(&run.source)?;
            let resolved = self.resolve_ingest(&spec)?;
            let edge = resolved
                .iter()
                .find(|e| e.spec.name.eq_ignore_ascii_case(&run.edge))
                .ok_or_else(|| {
                    Error::Unsupported(format!(
                        "ingest: receipt {run_id}'s edge `{}` is no longer declared",
                        run.edge
                    ))
                })?;
            // An empty chunk is not an error: a paged fetch whose row count
            // is an exact multiple of the page size ends with one, and the
            // call it cost is real even though it placed nothing. Charging
            // it and placing nothing is the only honest answer.
            if !rows.is_empty() {
                apply_chunk(&mut s, &spec, edge, &mut run, cols, rows)?;
            }
            run.calls += calls;
            run.bytes += bytes;
            write_open(&mut s, run_id, &run, "open")?;
            Ok(report_of(run_id, &run, edge))
        })();
        match out {
            Ok(r) => {
                s.commit()?;
                Ok(r)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Close a receipt. For a `dump` this is where deletes are found: every
    /// key the dump never presented is gone from the source.
    pub fn ingest_finish(&self, run_id: i64) -> Result<IngestReport> {
        let chunk = chunk_rows();
        let mut s = self.begin()?;
        let out = (|| -> Result<IngestReport> {
            let mut run = read_open(&mut s, run_id)?;
            let spec = self.load_ingest(&run.source)?;
            let resolved = self.resolve_ingest(&spec)?;
            let edge = resolved
                .iter()
                .find(|e| e.spec.name.eq_ignore_ascii_case(&run.edge))
                .ok_or_else(|| {
                    Error::Unsupported(format!(
                        "ingest: receipt {run_id}'s edge `{}` is no longer declared",
                        run.edge
                    ))
                })?;
            if run.mode == Mode::Dump {
                sweep_deletes(&mut s, &spec, edge, &mut run, chunk)?;
                s.query(
                    "DELETE FROM ingest_seen WHERE source = $1 AND tbl = $2",
                    &[Value::Text(run.source.clone()), Value::Text(run.table.clone())],
                )?;
            }
            let rep = report_of(run_id, &run, edge);
            // Fold this receipt into the observed model: the change-rate
            // estimator counts receipts and receipts-that-changed, the
            // cursor verdict accumulates, and the watermark advances.
            let fp = edge.spec.fingerprint();
            let mut st = read_state_in(&mut s, &run.source, &edge.spec.name, &fp)?;
            st.receipts += 1;
            if rep.changed() > 0 {
                st.changed_receipts += 1;
            }
            // The verdict and the watermark belong to the edge that OWNS the
            // cursor. A dump has none — it is the JUDGE, and the values it
            // tried came from the delta's column, so recording them here would
            // give a cursorless edge a position and a verdict about a column
            // it never asks for. Its tally goes to the edge it judged, below.
            if let Some(c) = edge.cursor_col() {
                st.cursor_col = c;
                if run.mode == Mode::Dump {
                    st.caught += run.caught;
                    st.missed += run.missed;
                    st.cursor_state = verdict_of(st.caught, st.missed);
                }
                if cursor_gt(&run.watermark, &st.watermark) {
                    st.watermark = run.watermark.clone();
                }
            }
            write_state(&mut s, &run.source, &edge.spec.name, &fp, &st)?;
            // A DELTA edge's watermark is the thing a dump verifies against,
            // so a dump that advanced the high-water mark shares it.
            let mut judged: Option<String> = None;
            if run.mode == Mode::Dump {
                for other in resolved.iter().filter(|o| {
                    o.table.eq_ignore_ascii_case(&edge.table)
                        && o.spec.strategy == Strategy::Delta
                }) {
                    let ofp = other.spec.fingerprint();
                    let mut ost = read_state_in(&mut s, &run.source, &other.spec.name, &ofp)?;
                    ost.caught += run.caught;
                    ost.missed += run.missed;
                    ost.cursor_state = verdict_of(ost.caught, ost.missed);
                    if let Some(c) = other.cursor_col() {
                        ost.cursor_col = c;
                    }
                    write_state(&mut s, &run.source, &other.spec.name, &ofp, &ost)?;
                    // The receipt reports the verdict it just produced, which
                    // is the JUDGED edge's, not this cursorless dump's.
                    judged = Some(ost.cursor_state.clone());
                }
            }
            s.query(
                "UPDATE ingest_stats SET changed = $2 WHERE run_id = $1",
                &[Value::Int(run_id), Value::Int(i64::from(rep.changed() > 0))],
            )?;
            write_open(&mut s, run_id, &run, "closed")?;
            let mut rep = rep;
            // caught/missed stay THIS receipt's count; the verdict is the
            // accumulated one, because one clean dump does not clear a
            // cursor that has already been caught lying.
            rep.cursor_state = judged.unwrap_or_else(|| st.cursor_state.clone());
            // A cursorless dump reports the high-water it SAW in the column
            // it tried — useful, and not a position it may resume from. Only
            // an edge that owns a cursor reports its stored one.
            if edge.cursor_col().is_some() {
                rep.watermark = st.watermark.clone();
            }
            Ok(rep)
        })();
        match out {
            Ok(r) => {
                s.commit()?;
                Ok(r)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Abandon an open receipt (a crashed fetch, a source that went away).
    /// The rows already applied STAY — they were real observations; only
    /// the run is closed and, for a dump, the presented-key set cleared so
    /// the next dump starts honest.
    pub fn ingest_abandon(&self, run_id: i64) -> Result<()> {
        let mut s = self.begin()?;
        let out = (|| -> Result<()> {
            let run = read_open(&mut s, run_id)?;
            s.query(
                "DELETE FROM ingest_seen WHERE source = $1 AND tbl = $2",
                &[Value::Text(run.source.clone()), Value::Text(run.table.clone())],
            )?;
            write_open(&mut s, run_id, &run, "abandoned")
        })();
        match out {
            Ok(()) => s.commit()?,
            Err(e) => {
                s.rollback();
                return Err(e);
            }
        }
        Ok(())
    }

    /// The whole of a small dump in one call.
    pub fn ingest_dump(
        &self,
        source: &str,
        target: &str,
        cols: &[String],
        rows: &[Vec<Value>],
        calls: i64,
        bytes: i64,
    ) -> Result<IngestReport> {
        let run = self.ingest_begin(source, target, Mode::Dump)?;
        if let Err(e) = self.ingest_rows(run, cols, rows, calls, bytes) {
            let _ = self.ingest_abandon(run);
            return Err(e);
        }
        self.ingest_finish(run)
    }

    /// The whole of a small delta in one call.
    pub fn ingest_delta(
        &self,
        source: &str,
        target: &str,
        cols: &[String],
        rows: &[Vec<Value>],
        calls: i64,
        bytes: i64,
    ) -> Result<IngestReport> {
        let run = self.ingest_begin(source, target, Mode::Delta)?;
        if let Err(e) = self.ingest_rows(run, cols, rows, calls, bytes) {
            let _ = self.ingest_abandon(run);
            return Err(e);
        }
        self.ingest_finish(run)
    }
}

/// One clean dump does not clear a cursor already caught lying, so the
/// verdict is a function of the ACCUMULATED tally, never of one receipt.
fn verdict_of(caught: i64, missed: i64) -> String {
    if missed > 0 {
        "unsafe".into()
    } else if caught > 0 {
        "safe".into()
    } else {
        "unknown".into()
    }
}

fn report_of(run_id: i64, run: &OpenRun, edge: &ResolvedEdge) -> IngestReport {
    IngestReport {
        run_id,
        edge: run.edge.clone(),
        table: run.table.clone(),
        mode: run.mode.as_str().into(),
        rows_in: run.rows_in,
        inserted: run.inserted,
        updated: run.updated,
        deleted: run.deleted,
        unchanged: run.unchanged,
        conflicts: run.conflicts,
        calls: run.calls,
        bytes: run.bytes,
        cursor_state: String::new(),
        caught: run.caught,
        missed: run.missed,
        missed_example: run.missed_example.clone(),
        watermark: run.watermark.clone(),
        complete: run.mode == Mode::Dump && edge.spec.presents_whole_table(),
    }
}

/// One chunk: classify every row against the target and apply it.
fn apply_chunk(
    s: &mut WriteSession<'_>,
    spec: &IngestSpec,
    edge: &ResolvedEdge,
    run: &mut OpenRun,
    cols: &[String],
    rows: &[Vec<Value>],
) -> Result<()> {
    if cols.is_empty() {
        return Err(Error::Unsupported(
            "ingest: a receipt chunk must name its columns".into(),
        ));
    }
    // Resolve the caller's column names against the schema's spelling, and
    // refuse an unknown one BY NAME rather than dropping it silently.
    let mut idx = Vec::with_capacity(cols.len());
    for c in cols {
        let hit = edge.cols.iter().find(|s| s.eq_ignore_ascii_case(c)).ok_or_else(|| {
            Error::Unsupported(format!(
                "ingest: `{}` has no column `{c}` — the row keys must be columns of the \
                 table the edge fills",
                edge.table
            ))
        })?;
        idx.push(hit.clone());
    }
    let pk_at = idx
        .iter()
        .position(|c| c.eq_ignore_ascii_case(&edge.pk_col))
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "ingest: every row must carry `{}`, the row identity of `{}` — without it a \
                 re-read cannot be harmless",
                edge.pk_col, edge.table
            ))
        })?;
    // Which cursor is on trial here. A DUMP has no cursor of its own — it
    // is the judge, not the accused — so the candidate it tests is the
    // DELTA edge's for the same table, judged against where that delta's
    // watermark stood before this dump. A row the dump found changed whose
    // candidate did not move past that mark is a row the delta would have
    // lost.
    let complete = edge.spec.presents_whole_table();
    let (trial_col, judge_against) = if complete {
        match spec
            .edges
            .iter()
            .find(|o| o.table.eq_ignore_ascii_case(&edge.table) && o.strategy == Strategy::Delta)
        {
            Some(d) => {
                let wm = read_state_in(s, &run.source, &d.name, &d.fingerprint())?.watermark;
                // A candidate tried against an EMPTY watermark passes every
                // row trivially — everything is "greater than nothing" — and
                // that is a false acquittal, not evidence. Until a delta has
                // actually stood somewhere, there is no position to judge
                // against and the dump records no verdict.
                match wm {
                    Value::Null => (None, None),
                    wm => (d.cursor.clone(), Some(wm)),
                }
            }
            None => (None, None),
        }
    } else {
        (edge.spec.cursor.clone(), None)
    };
    let cursor_at = trial_col
        .and_then(|c| idx.iter().position(|x| x.eq_ignore_ascii_case(&c)));

    let quoted = idx.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ");
    let get = format!(
        "SELECT {quoted} FROM \"{}\" WHERE \"{}\" = $1",
        edge.table, edge.pk_col
    );
    let ins = format!(
        "INSERT INTO \"{}\" ({quoted}) VALUES ({})",
        edge.table,
        (1..=idx.len()).map(|i| format!("${i}")).collect::<Vec<_>>().join(", ")
    );
    // The identity is the WHERE, never the SET — an UPDATE that names the
    // primary key column is refused by the engine, and rightly so.
    let set_at: Vec<usize> = (0..idx.len()).filter(|i| *i != pk_at).collect();
    let upd = format!(
        "UPDATE \"{}\" SET {} WHERE \"{}\" = $1",
        edge.table,
        set_at
            .iter()
            .enumerate()
            .map(|(n, i)| format!("\"{}\" = ${}", idx[*i], n + 2))
            .collect::<Vec<_>>()
            .join(", "),
        edge.pk_col
    );
    for row in rows {
        if row.len() != idx.len() {
            return Err(Error::Unsupported(format!(
                "ingest: a row has {} value(s) but {} column(s) were named",
                row.len(),
                idx.len()
            )));
        }
        run.rows_in += 1;
        let key = &row[pk_at];
        if matches!(key, Value::Null) {
            return Err(Error::Unsupported(format!(
                "ingest: a row carries NULL in `{}`, the row identity",
                edge.pk_col
            )));
        }
        let existing = rows_of(s.query(&get, std::slice::from_ref(key))?)?.into_iter().next();
        let mut changed = true;
        match existing {
            None => {
                s.query(&ins, row)?;
                run.inserted += 1;
            }
            Some(cur) => {
                if cur.iter().zip(row).all(|(a, b)| crate::lens::same_value(a, b)) {
                    run.unchanged += 1;
                    changed = false;
                } else {
                    match spec.policy {
                        Policy::Source => {
                            let mut p = vec![key.clone()];
                            p.extend(set_at.iter().map(|i| row[*i].clone()));
                            s.query(&upd, &p)?;
                            run.updated += 1;
                        }
                        Policy::Local => {
                            record_conflict(
                                s,
                                &run.source,
                                &edge.table,
                                key,
                                "differs",
                                "the local row differs from the source row and the policy is \
                                 `local`, so the local row stands",
                            )?;
                            run.conflicts += 1;
                            changed = false;
                        }
                    }
                }
            }
        }
        if let Some(at) = cursor_at {
            let v = &row[at];
            if cursor_gt(v, &run.watermark) {
                run.watermark = v.clone();
            }
            // §5: would the cursor have caught this changed row?
            if changed {
                if let Some(mark) = &judge_against {
                    if cursor_gt(v, mark) {
                        run.caught += 1;
                    } else {
                        run.missed += 1;
                        if matches!(run.missed_example, Value::Null) {
                            // The guide promises the verdict NAMES a row, and
                            // a count alone cannot be chased down in the
                            // source. One example is enough to go looking.
                            run.missed_example = row[pk_at].clone();
                        }
                    }
                }
            }
        }
        if complete {
            s.query(
                "INSERT OR REPLACE INTO ingest_seen (source, tbl, pk_ref, run_id) \
                 VALUES ($1, $2, $3, $4)",
                &[
                    Value::Text(run.source.clone()),
                    Value::Text(edge.table.clone()),
                    Value::Blob(pk_ref(key)),
                    Value::Int(0),
                ],
            )?;
        }
    }
    Ok(())
}

/// The dump's second half: every key the dump did NOT present is gone from
/// the source. Streamed in `pk > last` chunks, so a dump of any size costs
/// O(chunk) memory here too.
fn sweep_deletes(
    s: &mut WriteSession<'_>,
    spec: &IngestSpec,
    edge: &ResolvedEdge,
    run: &mut OpenRun,
    chunk: usize,
) -> Result<()> {
    let pk = &edge.pk_col;
    let first = format!(
        "SELECT \"{pk}\" FROM \"{}\" ORDER BY \"{pk}\" LIMIT {chunk}",
        edge.table
    );
    let next = format!(
        "SELECT \"{pk}\" FROM \"{}\" WHERE \"{pk}\" > $1 ORDER BY \"{pk}\" LIMIT {chunk}",
        edge.table
    );
    let del = format!("DELETE FROM \"{}\" WHERE \"{pk}\" = $1", edge.table);
    let mut last: Option<Value> = None;
    let mut doomed: Vec<Value> = Vec::new();
    loop {
        let rows = match &last {
            None => rows_of(s.query(&first, &[])?)?,
            Some(k) => rows_of(s.query(&next, std::slice::from_ref(k))?)?,
        };
        let got = rows.len();
        if got == 0 {
            break;
        }
        for r in &rows {
            let key = &r[0];
            let seen = rows_of(s.query(
                "SELECT run_id FROM ingest_seen WHERE source = $1 AND tbl = $2 AND pk_ref = $3",
                &[
                    Value::Text(run.source.clone()),
                    Value::Text(edge.table.clone()),
                    Value::Blob(pk_ref(key)),
                ],
            )?)?;
            if seen.is_empty() {
                doomed.push(key.clone());
            }
        }
        last = Some(rows[got - 1][0].clone());
        if got < chunk {
            break;
        }
    }
    for key in doomed {
        match spec.policy {
            Policy::Source => {
                s.query(&del, std::slice::from_ref(&key))?;
                run.deleted += 1;
            }
            Policy::Local => {
                record_conflict(
                    s,
                    &run.source,
                    &edge.table,
                    &key,
                    "vanished",
                    "the row is gone from the source but the policy is `local`, so it stands",
                )?;
                run.conflicts += 1;
            }
        }
    }
    Ok(())
}

pub(crate) fn record_conflict(
    s: &mut WriteSession<'_>,
    source: &str,
    table: &str,
    key: &Value,
    kind: &str,
    detail: &str,
) -> Result<()> {
    s.query(
        "INSERT OR REPLACE INTO ingest_conflicts (source, tbl, pk_ref, k, kind, detail) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            Value::Text(source.into()),
            Value::Text(table.into()),
            Value::Blob(pk_ref(key)),
            key.clone(),
            Value::Text(kind.into()),
            Value::Text(detail.into()),
        ],
    )?;
    Ok(())
}

/// One conflict, as `ingest conflicts` reports it.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub table: String,
    pub key: Value,
    pub kind: String,
    pub detail: String,
}

impl crate::Database {
    /// Everything the policy would not decide. Queryable IS the alert.
    pub fn ingest_conflicts(&self, source: &str) -> Result<Vec<Conflict>> {
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == crate::ingest::T_CONFLICTS) {
            return Ok(Vec::new());
        }
        rows_of(self.query(
            "SELECT tbl, k, kind, detail FROM ingest_conflicts WHERE source = $1 \
             ORDER BY tbl, pk_ref",
            &[Value::Text(source.into())],
        )?)?
        .into_iter()
        .map(|r| {
            Ok(Conflict {
                table: crate::rretl::as_text(&r[0]),
                key: r[1].clone(),
                kind: crate::rretl::as_text(&r[2]),
                detail: crate::rretl::as_text(&r[3]),
            })
        })
        .collect()
    }

    /// Clear conflicts, having decided. `take = local` simply drops the
    /// records (the local rows already stand); `take = source` is refused
    /// here on purpose — replaying the source's version means fetching it,
    /// which is a call, which is the next receipt's job.
    pub fn ingest_resolve(&self, source: &str, take: &str) -> Result<u64> {
        if take != "local" {
            return Err(Error::Unsupported(format!(
                "ingest resolve: `{take}` is not offered. `local` clears the records (the \
                 local rows already stand). To take the source's version, run a dump with \
                 policy `source` — replaying it means fetching it, and that is a call"
            )));
        }
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == crate::ingest::T_CONFLICTS) {
            return Ok(0);
        }
        let mut s = self.begin()?;
        let out = (|| -> Result<u64> {
            let n = rows_of(s.query(
                "SELECT count(*) FROM ingest_conflicts WHERE source = $1",
                &[Value::Text(source.into())],
            )?)?;
            let n = crate::rretl::as_int(&n[0][0])? as u64;
            s.query(
                "DELETE FROM ingest_conflicts WHERE source = $1",
                &[Value::Text(source.into())],
            )?;
            Ok(n)
        })();
        match out {
            Ok(n) => {
                s.commit()?;
                Ok(n)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// The observed model, per edge: watermark, cursor verdict, change
    /// rate, fan-out.
    pub fn ingest_state(&self, source: &str) -> Result<Vec<(String, EdgeState, i64)>> {
        let spec = self.load_ingest(source)?;
        let mut out = Vec::with_capacity(spec.edges.len());
        for e in &spec.edges {
            let st = read_state(self, source, &e.name, &e.fingerprint())?;
            out.push((e.name.clone(), st, e.overlap_secs));
        }
        Ok(out)
    }
}
