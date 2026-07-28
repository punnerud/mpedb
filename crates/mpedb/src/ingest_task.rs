//! The cascade (DESIGN-INGEST §2): calls whose PARAMETERS come from another
//! call's result.
//!
//! "List cases changed" returns keys; each key needs a detail call; a
//! detail may need a contract call; a result may need a write back into
//! another system. mpedb cannot make any of those calls — it carries the
//! parameters, meters the budget, and hands the worker the next batch.
//!
//! Two things fall out of doing it this way rather than in the fetcher's
//! own memory:
//!
//! - **The follow-ups are queued in the SAME transaction as the rows that
//!   produced them.** A crash between "I have the keys" and "the follow-ups
//!   are recorded" is impossible, so the cascade cannot silently lose a
//!   branch.
//! - **Fan-out is MEASURED here.** Every `derive` records how many keys one
//!   parent call produced, which is exactly the cardinality the planner
//!   needs to price the graph. Nothing declares it; the traffic reports it.
//!
//! `ingest_next` returns nothing when the window's budget is spent. That is
//! the budget working, not an error — the worker loop simply ends and the
//! next cron tick picks up where it stopped.

use mpedb_types::{ColumnType, Error, Result, Value};

use crate::ingest::{
    ensure_ingest_tables, read_state_in, write_state, EdgeKind, IngestSpec, ResolvedEdge,
};
use crate::rretl::{create_bookkeeping, now_micros, pk_ref, rows_of, shape_gate, spec_col};
use crate::WriteSession;

pub const T_TASK: &str = "ingest_task";
const TASK_SHAPE: [&str; 7] = ["source", "edge", "pk_ref", "k", "state", "lease", "ts_micros"];

/// A batch of derived calls the budget allows right now.
#[derive(Debug, Clone)]
pub struct IngestTask {
    pub lease: i64,
    pub edge: String,
    pub table: String,
    /// Up to the edge's `batch` keys. Pass them all in ONE call if the
    /// endpoint supports it — that is what the batch factor is for.
    pub keys: Vec<Value>,
}

/// What is left of this window's budget.
#[derive(Debug, Clone, Copy)]
pub struct BudgetLeft {
    pub profile: &'static str,
    pub calls: i64,
    pub bytes: i64,
    pub window_secs: i64,
}

pub(crate) fn ensure_task_table(
    s: &mut WriteSession<'_>,
    have: &[(String, Vec<String>)],
) -> Result<()> {
    use ColumnType::{Any, Blob, Int64, Text};
    if !shape_gate(have, T_TASK, &TASK_SHAPE)? {
        create_bookkeeping(
            s,
            T_TASK,
            vec![
                spec_col("source", Text),
                spec_col("edge", Text),
                spec_col("pk_ref", Blob),
                // The key VALUE, because the worker needs it to make the
                // call and a digest is one-way.
                spec_col("k", Any),
                spec_col("state", Text),
                spec_col("lease", Int64),
                spec_col("ts_micros", Int64),
            ],
            &["source", "edge", "pk_ref"],
        )?;
    }
    Ok(())
}

impl crate::Database {
    /// Queue derived calls from a receipt's keys, ATOMICALLY with that
    /// receipt. Returns how many were queued (a key already pending is not
    /// queued twice — the cascade is idempotent, which is what makes a
    /// re-read with overlap free).
    pub fn ingest_derive(&self, run_id: i64, edge: &str, keys: &[Value]) -> Result<u64> {
        let have = self.committed_tables()?;
        let mut s = self.begin()?;
        let out = (|| -> Result<u64> {
            ensure_ingest_tables(&mut s, &have)?;
            ensure_task_table(&mut s, &have)?;
            let rows = rows_of(s.query(
                "SELECT source, edge FROM ingest_stats WHERE run_id = $1",
                &[Value::Int(run_id)],
            )?)?;
            let r = rows
                .first()
                .ok_or_else(|| Error::Unsupported(format!("ingest: no receipt {run_id}")))?;
            let source = crate::rretl::as_text(&r[0]);
            let parent = crate::rretl::as_text(&r[1]);
            let spec = self.load_ingest(&source)?;
            let e = spec.edge(edge).ok_or_else(|| {
                Error::Unsupported(format!("ingest `{source}`: no edge `{edge}`"))
            })?;
            if e.kind == EdgeKind::Root {
                return Err(Error::Unsupported(format!(
                    "ingest `{source}`: edge `{edge}` is a root — roots are SCHEDULED by the \
                     plan, not driven by another call's keys"
                )));
            }
            match &e.parent {
                Some(p) if p.eq_ignore_ascii_case(&parent) => {}
                Some(p) => {
                    return Err(Error::Unsupported(format!(
                        "ingest `{source}`: edge `{edge}`'s parent is `{p}`, but receipt \
                         {run_id} is `{parent}` — a derived call may only be driven by the \
                         parent it declared"
                    )))
                }
                None => unreachable!("validate() refuses a parentless derived edge"),
            }
            let mut queued = 0u64;
            for k in keys {
                if matches!(k, Value::Null) {
                    return Err(Error::Unsupported(format!(
                        "ingest `{source}`: edge `{edge}` was handed a NULL key"
                    )));
                }
                let p = [
                    Value::Text(source.clone()),
                    Value::Text(e.name.clone()),
                    Value::Blob(pk_ref(k)),
                    k.clone(),
                    Value::Text("pending".into()),
                    Value::Int(0),
                    Value::Int(now_micros()),
                ];
                let existing = rows_of(s.query(
                    "SELECT state FROM ingest_task WHERE source = $1 AND edge = $2 \
                     AND pk_ref = $3",
                    &p[..3],
                )?)?;
                if existing.is_empty() {
                    s.query(
                        "INSERT INTO ingest_task (source, edge, pk_ref, k, state, lease, \
                         ts_micros) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                        &p,
                    )?;
                    queued += 1;
                }
            }
            // THE fan-out measurement: this many keys from one parent call.
            let fp = e.fingerprint();
            let mut st = read_state_in(&mut s, &source, &e.name, &fp)?;
            st.fanout += keys.len() as i64;
            st.parent_calls += 1;
            write_state(&mut s, &source, &e.name, &fp, &st)?;
            Ok(queued)
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

    /// What is left of this window's call and byte budget, from what the
    /// receipts actually reported spending.
    pub fn ingest_budget_left(&self, source: &str) -> Result<BudgetLeft> {
        let spec = self.load_ingest(source)?;
        let now = now_micros();
        // Hour-of-day in UTC. Profiles are declared in whatever the
        // operator's clock is; a source that needs local time declares its
        // work window shifted, and the doc says so rather than mpedb
        // guessing a timezone it cannot know.
        let hour = (now / 1_000_000 / 3600) % 24;
        let Some(b) = spec.budget_for(hour) else {
            return Ok(BudgetLeft { profile: "none", calls: 0, bytes: 0, window_secs: 0 });
        };
        let have = self.committed_tables()?;
        let (mut used_c, mut used_b) = (0i64, 0i64);
        if have.iter().any(|(n, _)| n == crate::ingest::T_STATS) {
            let since = now - b.window_secs.max(1) * 1_000_000;
            let rows = rows_of(self.query(
                "SELECT sum(calls), sum(bytes) FROM ingest_stats \
                 WHERE source = $1 AND ts_micros >= $2",
                &[Value::Text(source.into()), Value::Int(since)],
            )?)?;
            if let Some(r) = rows.first() {
                used_c = crate::rretl::as_int(&r[0]).unwrap_or(0);
                used_b = crate::rretl::as_int(&r[1]).unwrap_or(0);
            }
        }
        Ok(BudgetLeft {
            profile: if b.profile == "work" { "work" } else { "off" },
            calls: (b.calls - used_c).max(0),
            bytes: if b.bytes > 0 { (b.bytes - used_b).max(0) } else { i64::MAX },
            window_secs: b.window_secs,
        })
    }

    /// The next batch of derived calls the budget allows, or `None` when
    /// the window is spent (which is the budget working, not an error).
    ///
    /// Keys are handed out up to the edge's `batch` factor, oldest first,
    /// under a lease so two workers cannot fetch the same keys.
    pub fn ingest_next(&self, source: &str) -> Result<Option<IngestTask>> {
        let left = self.ingest_budget_left(source)?;
        if left.calls <= 0 {
            return Ok(None);
        }
        let spec = self.load_ingest(source)?;
        let resolved = self.resolve_ingest(&spec)?;
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == T_TASK) {
            return Ok(None);
        }
        let mut s = self.begin()?;
        let out = (|| -> Result<Option<IngestTask>> {
            // The edge with the most waiting keys goes first: it is the one
            // whose parent's work is least converted into rows.
            let waiting = rows_of(s.query(
                "SELECT edge, count(*) FROM ingest_task WHERE source = $1 AND state = 'pending' \
                 GROUP BY edge ORDER BY count(*) DESC, edge LIMIT 1",
                &[Value::Text(source.into())],
            )?)?;
            let Some(w) = waiting.first() else { return Ok(None) };
            let edge_name = crate::rretl::as_text(&w[0]);
            let edge: &ResolvedEdge = resolved
                .iter()
                .find(|e| e.spec.name.eq_ignore_ascii_case(&edge_name))
                .ok_or_else(|| {
                    Error::Unsupported(format!(
                        "ingest `{source}`: queued work for `{edge_name}`, which is no longer \
                         a declared edge — drop the source's tasks or restore the edge"
                    ))
                })?;
            let batch = edge.spec.batch.max(1);
            let rows = rows_of(s.query(
                &format!(
                    "SELECT pk_ref, k FROM ingest_task WHERE source = $1 AND edge = $2 \
                     AND state = 'pending' ORDER BY ts_micros, pk_ref LIMIT {batch}"
                ),
                &[Value::Text(source.into()), Value::Text(edge_name.clone())],
            )?)?;
            if rows.is_empty() {
                return Ok(None);
            }
            let lease = now_micros();
            let mut keys = Vec::with_capacity(rows.len());
            for r in &rows {
                s.query(
                    "UPDATE ingest_task SET state = 'claimed', lease = $4 \
                     WHERE source = $1 AND edge = $2 AND pk_ref = $3",
                    &[
                        Value::Text(source.into()),
                        Value::Text(edge_name.clone()),
                        r[0].clone(),
                        Value::Int(lease),
                    ],
                )?;
                keys.push(r[1].clone());
            }
            Ok(Some(IngestTask {
                lease,
                edge: edge.spec.name.clone(),
                table: edge.table.clone(),
                keys,
            }))
        })();
        match out {
            Ok(t) => {
                s.commit()?;
                Ok(t)
            }
            Err(e) => {
                s.rollback();
                Err(e)
            }
        }
    }

    /// Mark a leased batch done. The keys leave the queue; what they
    /// FETCHED went in through an ordinary receipt, which is where the
    /// rows and the cost were recorded.
    pub fn ingest_done(&self, source: &str, lease: i64) -> Result<u64> {
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == T_TASK) {
            return Ok(0);
        }
        let mut s = self.begin()?;
        let out = (|| -> Result<u64> {
            let n = rows_of(s.query(
                "SELECT count(*) FROM ingest_task WHERE source = $1 AND lease = $2",
                &[Value::Text(source.into()), Value::Int(lease)],
            )?)?;
            let n = crate::rretl::as_int(&n[0][0])? as u64;
            s.query(
                "DELETE FROM ingest_task WHERE source = $1 AND lease = $2",
                &[Value::Text(source.into()), Value::Int(lease)],
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

    /// Return a leased batch to the queue — a fetch that failed, a worker
    /// that is giving up. Nothing is lost: the keys go back to pending and
    /// the next `ingest_next` hands them out again.
    pub fn ingest_release(&self, source: &str, lease: i64) -> Result<u64> {
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == T_TASK) {
            return Ok(0);
        }
        let mut s = self.begin()?;
        let out = (|| -> Result<u64> {
            let n = rows_of(s.query(
                "SELECT count(*) FROM ingest_task WHERE source = $1 AND lease = $2",
                &[Value::Text(source.into()), Value::Int(lease)],
            )?)?;
            let n = crate::rretl::as_int(&n[0][0])? as u64;
            s.query(
                "UPDATE ingest_task SET state = 'pending', lease = 0 \
                 WHERE source = $1 AND lease = $2",
                &[Value::Text(source.into()), Value::Int(lease)],
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

    /// How much derived work is waiting, per edge. A queue that only grows
    /// means the budget cannot keep up with the fan-out, which is a fact
    /// worth seeing rather than inferring from latency.
    pub fn ingest_pending(&self, source: &str) -> Result<Vec<(String, i64, i64)>> {
        let have = self.committed_tables()?;
        if !have.iter().any(|(n, _)| n == T_TASK) {
            return Ok(Vec::new());
        }
        rows_of(self.query(
            "SELECT edge, count(*), sum(lease) FROM ingest_task WHERE source = $1 \
             GROUP BY edge ORDER BY edge",
            &[Value::Text(source.into())],
        )?)?
        .into_iter()
        .map(|r| {
            Ok((
                crate::rretl::as_text(&r[0]),
                crate::rretl::as_int(&r[1])?,
                i64::from(crate::rretl::as_int(&r[2]).unwrap_or(0) > 0),
            ))
        })
        .collect()
    }
}

/// Reclaim leases older than `max_age_secs` — a worker that died holding
/// keys. Safe to run any time: the work is idempotent, so the worst case
/// of reclaiming too early is one duplicate fetch.
pub fn reap_leases(db: &crate::Database, source: &str, max_age_secs: i64) -> Result<u64> {
    let have = db.committed_tables()?;
    if !have.iter().any(|(n, _)| n == T_TASK) {
        return Ok(0);
    }
    let cutoff = now_micros() - max_age_secs.max(0) * 1_000_000;
    let mut s = db.begin()?;
    let out = (|| -> Result<u64> {
        let n = rows_of(s.query(
            "SELECT count(*) FROM ingest_task WHERE source = $1 AND state = 'claimed' \
             AND lease < $2",
            &[Value::Text(source.into()), Value::Int(cutoff)],
        )?)?;
        let n = crate::rretl::as_int(&n[0][0])? as u64;
        s.query(
            "UPDATE ingest_task SET state = 'pending', lease = 0 \
             WHERE source = $1 AND state = 'claimed' AND lease < $2",
            &[Value::Text(source.into()), Value::Int(cutoff)],
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

/// The spec's edges that are driven rather than scheduled, for the CLI.
pub fn driven_edges(spec: &IngestSpec) -> Vec<&str> {
    spec.edges
        .iter()
        .filter(|e| e.kind != EdgeKind::Root)
        .map(|e| e.name.as_str())
        .collect()
}
