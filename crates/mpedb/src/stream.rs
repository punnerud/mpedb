//! Streaming SELECT execution: [`Database::stream_query`] returns a
//! [`RowStream`] that pulls rows **incrementally** from a pinned read
//! snapshot instead of materializing the whole result set — the near-data
//! analytics path (`mpedb-proc` cursors are built on it, and it is equally
//! useful to any host program that folds over a large scan).
//!
//! # Semantics
//!
//! Identical to [`Database::execute`] on the same read-only SELECT plan:
//! same snapshot isolation (one snapshot for the whole stream — a stream is
//! *more* consistent than repeated point queries), same filter, projection,
//! `ORDER BY`, `LIMIT`/`OFFSET` handling, same rows in the same order.
//!
//! # Memory
//!
//! For plans without a surviving `ORDER BY` (the planner elides `ORDER BY`
//! that matches PK scan order), memory is O([`BATCH`]) regardless of result
//! size: rows are fetched in small batches, each batch resuming the B+tree
//! scan after the last row's encoded PK (the same memcmp key the tree is
//! ordered by). Plans that DO sort cannot stream by nature; they fall back
//! to full materialization inside the stream (documented, not hidden — the
//! rows still come out one at a time).
//!
//! # Lifetime / locking
//!
//! The stream holds a pinned reader slot until it is exhausted or dropped.
//! Readers never block writers in mpedb (MVCC snapshots), but a pinned
//! snapshot delays page reclamation — as with any long read transaction,
//! do not park a stream indefinitely. Streams never touch the writer lock,
//! so they are safe to hold across `prepare`/`query` calls on the same
//! thread (unlike a `WriteSession`).

use crate::exec::{range_bounds, validate_params, RawBound, ReadCtx};
use crate::{exec, Database, ExecResult};
use mpedb_core::ReadTxn;
use mpedb_sql::{AccessPath, CompiledPlan, PlanStmt, Projection, SelectPlan};
use mpedb_types::{keycode, Error, PlanHash, Result, Value};
use std::collections::VecDeque;
use std::sync::Arc;

/// Rows fetched per B+tree visit (kept rows, i.e. post-filter). Small
/// enough that a stream's working set stays trivial, large enough that the
/// per-batch tree re-descent amortizes to noise.
const BATCH: usize = 256;

impl Database {
    /// Execute a previously prepared **read-only SELECT** plan as a stream:
    /// rows are produced one at a time by [`RowStream::next`], with O(1)
    /// memory in the result size for non-sorting plans (module docs).
    ///
    /// Errors mirror [`Database::execute`]: [`Error::UnknownPlan`],
    /// [`Error::PlanInvalidated`], parameter count/type errors — plus
    /// [`Error::Unsupported`] when the plan is not a read-only SELECT
    /// (DML and EXPLAIN have nothing to stream).
    pub fn stream_query(&self, hash: &PlanHash, params: &[Value]) -> Result<RowStream<'_>> {
        // cached_or_load: same resolution as execute() — local cache, else
        // fully re-validating registry load.
        let plan = self.cached_or_load(hash)?;
        RowStream::open(self, plan, params)
    }
}

/// One streaming SELECT over one pinned read snapshot.
pub struct RowStream<'db> {
    /// `Some` while the scan can still produce rows; dropped (releasing the
    /// reader slot) as soon as the stream is exhausted.
    txn: Option<ReadTxn<'db>>,
    plan: Arc<CompiledPlan>,
    params: Vec<Value>,
    columns: Vec<String>,
    /// Projected rows ready to hand out.
    buf: VecDeque<Vec<Value>>,
    /// Streaming-mode scan state (unused after materialization).
    table: u32,
    pk_cols: Vec<usize>,
    /// Lower bound of the NEXT batch's scan; updated to resume strictly
    /// after the last scanned row.
    lo: Option<RawBound>,
    hi: Option<RawBound>,
    /// Remaining OFFSET rows to discard (post-filter).
    skip: usize,
    /// Remaining LIMIT rows to emit; `usize::MAX` when unlimited.
    take: usize,
}

impl<'db> RowStream<'db> {
    fn open(db: &'db Database, plan: Arc<CompiledPlan>, params: &[Value]) -> Result<RowStream<'db>> {
        let PlanStmt::Select(SelectPlan {
            table,
            access,
            order_by,
            projection,
            limit,
            offset,
            joins,
            distinct,
            aggregate,
            ..
        }) = &plan.stmt
        else {
            return Err(Error::Unsupported(
                "stream_query requires a read-only SELECT plan".into(),
            ));
        };
        if !plan.footprint.read_only {
            return Err(Error::Unsupported(
                "stream_query requires a read-only SELECT plan".into(),
            ));
        }
        validate_params(&plan, params)?;

        let table = *table;
        let schema = db.schema();
        let tdef = schema
            .table(table)
            .ok_or_else(|| Error::Internal("validated plan table out of range".into()))?;
        let pk_cols: Vec<usize> = tdef.primary_key.iter().map(|&i| i as usize).collect();

        let mut stream = RowStream {
            txn: None,
            plan: plan.clone(),
            params: params.to_vec(),
            columns: Vec::new(), // filled per branch below
            buf: VecDeque::new(),
            table,
            pk_cols,
            lo: None,
            hi: None,
            skip: offset.unwrap_or(0).min(usize::MAX as u64) as usize,
            take: limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize),
        };

        // Sorting plans and point accesses cannot / need not stream: run the
        // ordinary executor once and drain from the buffer (module docs).
        // Joins, DISTINCT and aggregates DEFINITELY cannot: the streaming
        // path is a bare outer-table scan, and running such a plan through it
        // silently returned the outer rows as if the rest of the plan did not
        // exist (adversarial review find) — they take the materializing
        // fallback below, which runs the real executor.
        let can_stream = plan.subplans.is_empty()
            && order_by.is_empty()
            && joins.is_empty()
            && !*distinct
            && aggregate.is_none()
            && matches!(access, AccessPath::PkRange { .. } | AccessPath::FullScan);
        if !can_stream {
            let r = db.engine.begin_read()?;
            let mut partial = false;
            let res = {
                let mut ctx = ReadCtx(&r, None, None);
                exec::exec_stmt(&mut ctx, &schema, &plan, params, &mut partial)
            }?;
            r.finish()?; // SnapshotEvicted here invalidates the rows
            let ExecResult::Rows { rows, columns } = res else {
                return Err(Error::Internal(
                    "SELECT plan did not produce rows".into(),
                ));
            };
            // The executor already named the output — over a joined tuple, a
            // grouped tuple, whatever the plan built. Resolving the projection
            // against the OUTER table alone here was wrong for every one of
            // those (out-of-range for a joined slot).
            stream.columns = columns;
            stream.buf = rows.into();
            // skip/take were already applied by exec_stmt.
            stream.skip = 0;
            stream.take = usize::MAX;
            return Ok(stream);
        }

        // Streaming path: a single-table scan, so the projection names resolve
        // against this table's columns.
        stream.columns = projection
            .iter()
            .map(|p| match p {
                Projection::Column(i) => tdef
                    .columns
                    .get(*i as usize)
                    .map(|c| c.name.clone())
                    .ok_or_else(|| Error::Internal("projection column out of range".into())),
                Projection::Expr { name, .. } => Ok(name.clone()),
            })
            .collect::<Result<Vec<String>>>()?;

        // Streaming path: resolve the (possibly parameterized) key range.
        match access {
            AccessPath::FullScan => {}
            AccessPath::PkRange { lo, hi } => {
                match range_bounds(lo.as_ref(), hi.as_ref(), &plan, params)? {
                    // A NULL bound: the range predicate is UNKNOWN for every
                    // row — the stream is born exhausted.
                    None => return Ok(stream),
                    Some((lo_k, hi_k)) => {
                        stream.lo = lo_k;
                        stream.hi = hi_k;
                    }
                }
            }
            _ => unreachable!("can_stream checked the access path"),
        }
        stream.txn = Some(db.engine.begin_read()?);
        Ok(stream)
    }

    /// Output column names, in projection order.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Pull the next row. `Ok(None)` means the stream is exhausted (and its
    /// snapshot has been released). After an error the stream is dead.
    // Fallible + streaming, so deliberately not std::iter::Iterator (the
    // same call shape as mpedb-core's RowCursor).
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Vec<Value>>> {
        if self.buf.is_empty() && self.txn.is_some() {
            self.refill()?;
        }
        Ok(self.buf.pop_front())
    }

    /// Fetch the next batch of kept rows, then remember where to resume.
    fn refill(&mut self) -> Result<()> {
        let Some(txn) = &self.txn else {
            return Ok(());
        };
        let PlanStmt::Select(SelectPlan {
            filter, projection, ..
        }) = &self.plan.stmt
        else {
            unreachable!("open() checked the statement kind");
        };
        // The pin is re-validated inside RowCursor every 256 steps; check
        // once per batch too so short batches cannot dodge it forever.
        if !txn.still_pinned() {
            self.txn = None;
            return Err(Error::SnapshotEvicted);
        }

        let mut cursor = txn.scan_raw(
            self.table,
            self.lo.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
            self.hi.as_ref().map(|(k, inc)| (k.as_slice(), *inc)),
        )?;
        let mut stack: Vec<Value> = Vec::new();
        let mut resume: Option<Vec<u8>> = None;
        let mut exhausted = false;
        loop {
            if self.take == 0 {
                exhausted = true; // LIMIT reached: nothing more can be emitted
                break;
            }
            let Some(row) = cursor.next()? else {
                exhausted = true;
                break;
            };
            let keep = match filter {
                Some(f) => f.eval_filter(&mut stack, &row, &self.params)?,
                None => true,
            };
            if !keep {
                continue;
            }
            if self.skip > 0 {
                self.skip -= 1; // OFFSET discards kept rows
                continue;
            }
            self.take = self.take.saturating_sub(1);
            let mut orow = Vec::with_capacity(projection.len());
            for p in projection {
                orow.push(match p {
                    Projection::Column(i) => row
                        .get(*i as usize)
                        .cloned()
                        .ok_or_else(|| Error::Internal("projection column".into()))?,
                    Projection::Expr { program, .. } => program.eval(&row, &self.params)?,
                });
            }
            self.buf.push_back(orow);
            if self.buf.len() >= BATCH {
                // Resume strictly after this row: its encoded PK is exactly
                // the B+tree key it is stored under (memcmp keycode).
                let mut pk = Vec::with_capacity(self.pk_cols.len());
                for &c in &self.pk_cols {
                    pk.push(
                        row.get(c)
                            .cloned()
                            .ok_or_else(|| Error::Internal("PK column".into()))?,
                    );
                }
                resume = Some(keycode::encode_key(&pk));
                break;
            }
        }
        drop(cursor);
        if exhausted {
            // Release the reader slot promptly; buffered rows stay valid
            // (they are owned copies). finish() surfaces SnapshotEvicted.
            if let Some(t) = self.txn.take() {
                t.finish()?;
            }
        } else {
            self.lo = resume.map(|k| (k, false)); // false = exclusive bound
        }
        Ok(())
    }
}
