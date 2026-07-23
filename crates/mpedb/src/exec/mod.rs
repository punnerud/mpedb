//! Plan executor: runs a validated [`CompiledPlan`] against an engine
//! transaction. Shared by the autocommit paths on [`crate::Database`] and the
//! interactive [`crate::WriteSession`] via the [`TxnCtx`] abstraction.

use crate::trigger::{CompiledTrigger, TriggerSet};
use crate::ExecResult;
use mpedb_core::{FoldOpts, FoldStop, ReadTxn, WriteTxn};
use mpedb_sql::{
    AccessPath, AggCall, Aggregation, CompiledPlan, ConflictProbe, InsertSource, Join, JoinKind,
    CompoundPlan, GroupKey, OrderOver, PlanOnConflict, PlanStmt, Projection, RowMap, RowSide,
    Mask, RowPrune, SelectPlan, SetOp, SortDir, SubBody, SubPlan,
};
use mpedb_types::{
    exact_float_as_int, exact_int_as_float, keycode, Accum, Collation, DefaultExpr, Error,
    HostColls, OrderColl,
    ExprProgram, HostFns, KeyBound, KeyPart, Result, Schema, TableDef, Value,
};
use std::cmp::Ordering;
use std::sync::Arc;
use std::collections::BinaryHeap;

std::thread_local! {
    /// The rowid the most recent INSERT statement assigned/used, for the
    /// C-API's `sqlite3_last_insert_rowid`. Recorded per inserted row into a
    /// rowid-alias (INTEGER PRIMARY KEY) table by [`record_last_insert_rowid`],
    /// so the last row of a multi-row insert wins. Read (and cleared) by
    /// [`take_last_insert_rowid`] immediately after the statement returns, on
    /// the same thread that executed it — every write path (`Database::query`,
    /// `WriteSession::query`, the group-commit leader) runs `exec_stmt`
    /// synchronously in the caller's thread, so this needs no synchronization.
    static LAST_INSERT_ROWID: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Record the rowid of a row just inserted into a rowid-alias table (facade hook
/// for `sqlite3_last_insert_rowid`). Called from the INSERT loop after each
/// successful `insert_row`, so the final call reflects the last inserted row.
pub(crate) fn record_last_insert_rowid(rowid: i64) {
    LAST_INSERT_ROWID.with(|c| c.set(Some(rowid)));
}

/// Take (read and clear) the rowid assigned by the last INSERT executed on this
/// thread, or `None` if the last statement inserted nothing into a rowid-alias
/// table. The C-API shim calls this once after each statement and updates its
/// per-connection `last_insert_rowid` only when a value is present — matching
/// sqlite, where a non-insert statement leaves `last_insert_rowid` unchanged.
pub fn take_last_insert_rowid() -> Option<i64> {
    LAST_INSERT_ROWID.with(|c| c.take())
}

mod aggregate;
mod fts;
mod gather;
mod parallel;
mod recursive;
mod window;

pub(crate) use gather::{range_bounds, resolve_part, RawBound};
/// See [`crate::parallel_folds_engaged`].
pub(crate) fn parallel_folds_engaged() -> u64 {
    parallel::ENGAGED.load(std::sync::atomic::Ordering::Relaxed)
}
use aggregate::exec_aggregate;
use gather::{cmp_rows, gather_joined, gather_rows, gather_topk, sort_rows};

/// The declared collation of every slot in the BASE (or joined) row being
/// scanned — the concatenation of the scanned tables' column collations, in slot
/// order. GROUP BY and DISTINCT fold their keys through this so a `NOCASE`/`RTRIM`
/// column groups/deduplicates case-/space-insensitively (the collation is baked
/// into the schema, so this is derived at execution and always agrees with the
/// plan's `schema_hash`). The working-table sentinel (`CTE_TABLE`) resolves
/// through the plan's own node and contributes one BINARY slot per column —
/// PADDED, not skipped, so a joined table's collations stay aligned with the
/// joined row (skipping used to shift a collated join column onto the wrong
/// slot the day a working table joined a `NOCASE` table).
pub(super) fn base_row_collations(
    schema: &Schema,
    plan: &CompiledPlan,
    table: u32,
    joins: &[Join],
) -> Vec<Collation> {
    let mut out = Vec::new();
    for id in std::iter::once(table).chain(joins.iter().map(|j| j.table)) {
        if let Ok(t) = table_def(schema, plan, id) {
            out.extend(t.columns.iter().map(|c| c.collation));
        }
    }
    out
}

/// The declared collation of each PROJECTED output column: a bare column
/// (`Projection::Column`) carries its declared collation; a computed column has
/// none (BINARY), exactly as in sqlite. Used to fold `SELECT DISTINCT` keys.
pub(super) fn output_collations(
    schema: &Schema,
    plan: &CompiledPlan,
    table: u32,
    joins: &[Join],
    projection: &[Projection],
) -> Vec<Collation> {
    let base = base_row_collations(schema, plan, table, joins);
    projection
        .iter()
        .map(|p| match p {
            Projection::Column(i) => base.get(*i as usize).copied().unwrap_or(Collation::Binary),
            Projection::Expr { .. } => Collation::Binary,
        })
        .collect()
}

/// The row operations the executor needs, implemented by both transaction
/// kinds. Write operations on a read transaction are unreachable by
/// construction (routing is by the recomputed `footprint.read_only`) and
/// return `Error::Internal` if ever hit.
pub(crate) trait TxnCtx {
    /// Host-registered scalar UDFs in scope for this execution (design/DESIGN-UDF.md),
    /// or `None` where none are (the default). Both native contexts carry them —
    /// [`ReadCtx`] on the read path and [`WriteCtx`] on the write path — so a UDF
    /// called from DML, or from a read inside an open write transaction, resolves
    /// the same closure the read path would. A context that structurally cannot
    /// carry them (the streaming read path, the sqlite-backed contexts) keeps the
    /// `None` default, and the eval site then refuses with a clean "not in scope"
    /// error rather than silently dropping the call. Every eval site threads this
    /// through [`ExprProgram::eval_filter_host`]/`eval_host`.
    fn host_fns(&self) -> Option<&dyn HostFns> {
        None
    }
    /// Host-registered AGGREGATES in scope for this execution
    /// (design/DESIGN-UDF.md stage 2), or `None`. Same scope rule as
    /// [`host_fns`](Self::host_fns): both native contexts carry them, everything
    /// else refuses cleanly.
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        None
    }
    /// Host-registered COLLATING SEQUENCES in scope for this execution
    /// (design/DESIGN-UDF.md stage 3), or `None`. Same scope rule as
    /// [`host_fns`](Self::host_fns); a plan whose ORDER BY names one is
    /// connection-local, so every other context refuses it by name rather than
    /// sorting under a collation it does not have.
    fn host_colls(&self) -> Option<&dyn HostColls> {
        None
    }
    /// The pinned snapshot under this context, when this context IS a plain
    /// snapshot read — the parallel fold's precondition (`exec/parallel.rs`).
    /// Its workers share the returned transaction's pin, meter and page
    /// access, so only a context that is nothing BUT a `ReadTxn` may answer.
    /// Everything else (write contexts, overlay and streaming reads, the
    /// sqlite-backed contexts) keeps the `None` default and folds serially.
    fn snapshot_txn(&self) -> Option<&ReadTxn<'_>> {
        None
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>>;
    /// Decode only the listed column ordinals from a PK hit (projection order).
    /// Default: full row then project — override when the store can decode
    /// individual columns without materializing the rest.
    fn get_by_pk_cols(
        &mut self,
        table: u32,
        pk: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        match self.get_by_pk(table, pk)? {
            None => Ok(None),
            Some(row) => {
                let mut out = Vec::with_capacity(cols.len());
                for &c in cols {
                    out.push(
                        row.get(c as usize)
                            .cloned()
                            .ok_or_else(|| internal("projection column"))?,
                    );
                }
                Ok(Some(out))
            }
        }
    }
    fn get_by_index(&mut self, table: u32, index_no: u32, values: &[Value])
        -> Result<Option<Vec<Value>>>;
    /// Every row matching an index equality — N rows for a non-unique index,
    /// 0 or 1 for a unique one (the engine takes an exact-get fast path for
    /// those, so routing everything through here costs the unique case
    /// nothing).
    fn scan_by_index(&mut self, table: u32, index_no: u32, values: &[Value])
        -> Result<Vec<Vec<Value>>>;
    /// Every row whose indexed value falls in the raw-encoded bound range —
    /// `AccessPath::IndexRange`. Bounds use the same prefix construction as
    /// composite-PK ranges (see [`range_bounds`]).
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>>;
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>>;
    /// Scan with the residual filter applied per row and an optional cap on
    /// KEPT rows — the LIMIT/OFFSET pushdown (MPEE "stream under a memory
    /// budget" transfer: never materialize what the query will not return).
    /// The default collects the whole range first (used by WriteTxn contexts,
    /// where collect-then-mutate is the rule anyway); ReadCtx overrides it
    /// with true cursor streaming, which is the autocommit SELECT path.
    fn scan_rows_capped(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        let rows = self.scan_rows_raw(table, lo, hi)?;
        let host = self.host_fns();
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        for row in rows {
            let keep = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if keep {
                kept.push(row);
                if cap.is_some_and(|c| kept.len() >= c) {
                    break;
                }
            }
        }
        Ok(kept)
    }
    /// Streaming top-K for `ORDER BY … LIMIT`: return the `keep` smallest
    /// rows under `order_by` (already sorted), scanning under a bounded
    /// `keep`-sized heap so memory is O(keep) instead of O(matched rows) —
    /// the MPEE "stream under a memory budget" transfer applied to sorted
    /// pagination. The default materializes the whole range then sorts and
    /// truncates (used by WriteTxn contexts); ReadCtx overrides it with a
    /// true streaming heap.
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, SortDir, OrderColl)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
        gather::check_order_colls(order_by, self.host_colls())?;
        let rows = self.scan_rows_raw(table, lo, hi)?;
        let host = self.host_fns();
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        for row in rows {
            let ok = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if ok {
                kept.push(row);
            }
        }
        sort_rows(&mut kept, order_by, self.host_colls());
        kept.truncate(keep);
        Ok(kept)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()>;
    /// The next value to auto-assign to an INTEGER PRIMARY KEY rowid alias
    /// (`pk_col` is that column's index): `max(existing pk) + 1`, or 1 for an
    /// empty table — sqlite's plain, non-AUTOINCREMENT rule (a freed top id is
    /// reusable). The default scans the table and takes the maximum, which is
    /// correct for any backing store; `WriteTxn` overrides it with an
    /// O(tree-height) rightmost-key descent.
    fn next_rowid(&mut self, table: u32, pk_col: u16) -> Result<i64> {
        let rows = self.scan_rows_raw(table, None, None)?;
        let mut max: Option<i64> = None;
        for row in &rows {
            if let Some(Value::Int(v)) = row.get(pk_col as usize) {
                max = Some(max.map_or(*v, |m: i64| m.max(*v)));
            }
        }
        Ok(max.map_or(1, |m| m.saturating_add(1)))
    }
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool>;
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool>;
    /// Every posting entry whose key starts with `prefix`, as `(key, doclist)`
    /// pairs in key order — the FTS set-algebra primitive (design/DESIGN-FTS.md
    /// §4). Charges the #74 work meter per entry visited. The default errors:
    /// only the native engine contexts (`WriteTxn`, `ReadCtx`) serve FTS; the
    /// sqlite-backed contexts have no inverted index.
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let _ = (table, prefix);
        Err(mpedb_types::Error::Unsupported(
            "full-text search is not available in this context".into(),
        ))
    }
    /// Charge `n` work-rows against this execution's deterministic budget (#74)
    /// and surface [`Error::RuntimeBudget`] once it is exceeded. Routes to the
    /// SAME [`mpedb_core::WorkMeter`] the engine's scans charge, so the
    /// exec-layer bumps (nested-loop join, correlated subquery) and the scan
    /// bumps share one running count. `which` builds the attribution lazily —
    /// evaluated only on the abort path. Object-safe: `&dyn Fn`, not a generic.
    ///
    /// The default is a no-op: the sqlite-backed contexts (`SqliteCtx`,
    /// `MergeCtx`) are a different storage engine with no mpedb `WorkMeter`, so
    /// the #74 budget applies only to the native engine paths that override this
    /// (`ReadCtx`, `WriteTxn`).
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        let _ = (n, which);
        Ok(())
    }
    /// The live-cell budget for join materialization (`0` = unlimited): the
    /// nested-loop join in `gather::gather_joined` bounds the `Value` cells
    /// its intermediate product HOLDS against this — the memory-proportional
    /// twin of the work-row budget, which only bounds what a query READS.
    /// Default `0` for the sqlite-backed contexts (a different storage engine;
    /// their joins run through the same gather, but the mpedb config does not
    /// govern them — mirrors [`charge_work`](Self::charge_work)'s scoping).
    fn join_cells_budget(&self) -> u64 {
        0
    }
    /// Does [`scan_rows_capped`](Self::scan_rows_capped) STOP PULLING at the
    /// cap — i.e. is this context's scan a real cursor rather than a
    /// materialize-then-truncate?
    ///
    /// The distinction is what makes a **resumable batched scan**
    /// ([`gather::BatchScan`], #123 §5.1) worth doing. A cursor context answers
    /// a `cap = C` scan in O(C); the default `TxnCtx::scan_rows_capped`
    /// materializes the whole range and only then truncates, so batching over
    /// it would be O(n) PER BATCH — quadratic, and holding exactly what the
    /// batching exists to avoid. So the default is `false` and every context
    /// that has not proven otherwise keeps the single-pass materializing path
    /// it has today, with byte-identical results either way.
    ///
    /// Only [`ReadCtx`] overrides it: its `scan_rows_capped` breaks out of a
    /// live `RowCursor`, and its scans are keyed by the same memcmp PK the
    /// resume bound is encoded with.
    fn scans_incrementally(&self) -> bool {
        false
    }
    /// One batch of a resumable, DECODE-PRUNED scan — the scan-level half of
    /// #125: up to `cap` KEPT rows, each decoded only at the `keep`-true
    /// ordinals and truncated to `keep.len()` slots (holes read as NULL, the
    /// exact shape `gather::narrow_row` produces), plus the raw storage key
    /// of the last kept row when the cap was reached — the next batch's
    /// resume bound, obtained without re-encoding a PK.
    ///
    /// **`keep` must cover every column `filter` reads**: unlike
    /// [`scan_rows_capped`](Self::scan_rows_capped), the residual runs over
    /// the pruned row here. [`gather::scan_keep`] builds exactly that mask.
    ///
    /// Meaningful only where [`scans_incrementally`](Self::scans_incrementally)
    /// answers true — [`gather::BatchScan`], the sole caller, never opens
    /// elsewhere — so the default is a refusal, not a fallback.
    fn scan_rows_pruned(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: usize,
        keep: Option<&[bool]>,
    ) -> Result<PrunedBatch> {
        let _ = (table, lo, hi, filter, cap, keep);
        Err(internal("decode-pruned scan on a non-incremental context"))
    }
    /// `count(*)` over a raw-bounded PK range without materializing a row, or
    /// `Ok(None)` when this context has no such fast path and the caller must
    /// fold the scan. The #74 work charges of the counting context must be
    /// EXACTLY the drain-scan's — same total, same trip point, same label —
    /// because the budget is a deterministic, test-pinned contract and this
    /// is an optimization, not a discount ([`mpedb_core::WorkMeter`]'s
    /// `charge_many` states the same rule from the meter's side).
    fn count_rows_range(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<u64>> {
        let _ = (table, lo, hi);
        Ok(None)
    }
    /// Can this context serve an aggregate-over-index-tree plan (format 59) —
    /// count entries wholesale, fold leading key values, probe boundary rows?
    /// Default `false`: every non-snapshot context (write transactions, the
    /// sqlite-backed overlays, mirrors) keeps the row fold, which remains the
    /// semantics of record — the plan's `over_index` is then an access
    /// decision those contexts decline, not an obligation.
    fn agg_over_index_supported(&self) -> bool {
        false
    }
    /// Entry count of a secondary index tree, leaf-wholesale (#74 charges one
    /// work-row per entry — see `ReadTxn::count_index_entries`). Only called
    /// where [`agg_over_index_supported`](Self::agg_over_index_supported) is
    /// true; the default is therefore a refusal, not a fallback.
    fn count_index_entries(&mut self, table: u32, index_no: u32) -> Result<u64> {
        let _ = (table, index_no);
        Err(internal("index-entry count on an unsupported context"))
    }
    /// Visit every entry's decoded LEADING key value in key order (#74: one
    /// work-row per entry). Same support gate as above.
    fn fold_index_leading(
        &mut self,
        table: u32,
        index_no: u32,
        f: &mut dyn FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let _ = (table, index_no, f);
        Err(internal("index-leading fold on an unsupported context"))
    }
    /// The row behind the index tree's min (`max = false`) or max boundary
    /// entry, `None` for an empty tree (#74: one work-row per found row).
    /// Same support gate as above.
    fn index_boundary_row(
        &mut self,
        table: u32,
        index_no: u32,
        max: bool,
    ) -> Result<Option<Vec<Value>>> {
        let _ = (table, index_no, max);
        Err(internal("index boundary probe on an unsupported context"))
    }
    /// Fold ONE decoded column of every row in a raw-bounded PK range into
    /// `f`, in scan (PK) order, without materializing a row — the
    /// decode-to-accumulator fusion of the ungrouped single-column aggregate
    /// (`ReadTxn::fold_range_column`). `Ok(false)` when this context has no
    /// spine-free path; the caller then runs the batched fold, which stays
    /// the semantics of record. Only the pinned-snapshot read context answers
    /// `true`, and its #74 charges are EXACTLY the drain-scan's.
    fn fold_rows_column(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        col: u16,
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<Option<FoldStop>> {
        let _ = (table, lo, hi, col, opts, f);
        Ok(None)
    }

    /// How many rows does this table hold? `Ok(None)` = this context cannot
    /// say cheaply, and the caller must not depend on knowing.
    fn row_count(&mut self, table: u32) -> Result<Option<u64>> {
        let _ = table;
        Ok(None)
    }

    /// Selectivity-priced index range: `Ok(None)` = this context has no such
    /// path (or the shape declines) and the caller runs the plain range scan.
    /// Only the pinned-snapshot read context answers.
    fn scan_by_index_range_adaptive(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        let _ = (table, index_no, lo, hi);
        Ok(None)
    }

    /// [`fold_rows_column`](Self::fold_rows_column) with a PREDICATE: decode
    /// `need` (the filter's columns plus the aggregate's, from
    /// `ExprProgram::read_columns`) into one reused buffer, evaluate the
    /// filter, and fold `col` only for rows that pass — so a filtered
    /// aggregate over a wide table decodes two columns per row instead of
    /// materializing the whole row. `Ok(None)` = this context has no such
    /// path, and the caller runs the ordinary gather.
    #[allow(clippy::too_many_arguments)]
    fn fold_rows_column_filtered(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        need: &[u16],
        col: u16,
        filter: (&ExprProgram, &[Value]),
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<Option<FoldStop>> {
        let _ = (table, lo, hi, need, col, filter, opts, f);
        Ok(None)
    }
}

impl TxnCtx for WriteTxn<'_> {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk(self, table, pk)
    }
    fn get_by_pk_cols(
        &mut self,
        table: u32,
        pk: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk_cols(self, table, pk, cols)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_index(self, table, index_no, values)
    }
    fn scan_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_by_index(self, table, index_no, values)
    }
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_by_index_range(self, table, index_no, lo, hi)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_rows_raw(self, table, lo, hi)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()> {
        WriteTxn::insert_row(self, table, values)
    }
    fn next_rowid(&mut self, table: u32, _pk_col: u16) -> Result<i64> {
        // The PK tree key IS the single integer PK, so the rightmost key is the
        // maximum — no need to read `pk_col` out of a full row.
        WriteTxn::next_rowid(self, table)
    }
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool> {
        WriteTxn::update_by_pk(self, table, new_values)
    }
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool> {
        WriteTxn::delete_by_pk(self, table, pk)
    }
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        WriteTxn::fts_prefix(self, table, prefix)
    }
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        WriteTxn::charge_work(self, n, which)
    }
    fn join_cells_budget(&self) -> u64 {
        WriteTxn::join_cells_budget(self)
    }
}

/// A [`WriteTxn`] plus the connection's host-UDF closures — the WRITE-path twin
/// of [`ReadCtx`] (design/DESIGN-UDF.md).
///
/// `impl TxnCtx for WriteTxn` cannot carry them (the type lives in
/// `mpedb-core`, which knows nothing about a connection's UDF registry), so the
/// facade wraps the transaction for the duration of ONE statement whose plan
/// `contains_host_call()`. Every row operation delegates to the transaction
/// unchanged — the wrapper adds resolution, never behaviour: a statement with no
/// host call still runs on the bare `&mut WriteTxn`, byte for byte as before.
///
/// The closures reach the executor by value-passing only: `HostFns::call` gets
/// the already-evaluated argument `Value`s and returns one `Value`, and
/// `HostAggs::create` mints a state stepped with the same. Neither is handed the
/// transaction, the snapshot, the schema, or any engine handle, so a host UDF on
/// the write path sees exactly what it sees on the read path — its arguments.
pub(crate) struct WriteCtx<'a, 'e> {
    pub txn: &'a mut WriteTxn<'e>,
    pub host: Option<&'a dyn HostFns>,
    pub aggs: Option<&'a dyn mpedb_types::HostAggs>,
    /// Host COLLATING SEQUENCES in scope for this write (stage 3), so an
    /// `ORDER BY … COLLATE mycoll` inside DML (`INSERT … SELECT`, `RETURNING`)
    /// sorts through the same callbacks a read would.
    pub colls: Option<&'a dyn HostColls>,
}

impl<'a, 'e> WriteCtx<'a, 'e> {
    pub(crate) fn new(
        txn: &'a mut WriteTxn<'e>,
        host: Option<&'a dyn HostFns>,
        aggs: Option<&'a dyn mpedb_types::HostAggs>,
        colls: Option<&'a dyn HostColls>,
    ) -> WriteCtx<'a, 'e> {
        WriteCtx { txn, host, aggs, colls }
    }
}

impl TxnCtx for WriteCtx<'_, '_> {
    fn host_fns(&self) -> Option<&dyn HostFns> {
        self.host
    }
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        self.aggs
    }
    fn host_colls(&self) -> Option<&dyn HostColls> {
        self.colls
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk(self.txn, table, pk)
    }
    fn get_by_pk_cols(
        &mut self,
        table: u32,
        pk: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk_cols(self.txn, table, pk, cols)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_index(self.txn, table, index_no, values)
    }
    fn scan_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_by_index(self.txn, table, index_no, values)
    }
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_by_index_range(self.txn, table, index_no, lo, hi)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        WriteTxn::scan_rows_raw(self.txn, table, lo, hi)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()> {
        WriteTxn::insert_row(self.txn, table, values)
    }
    fn next_rowid(&mut self, table: u32, _pk_col: u16) -> Result<i64> {
        WriteTxn::next_rowid(self.txn, table)
    }
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool> {
        WriteTxn::update_by_pk(self.txn, table, new_values)
    }
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool> {
        WriteTxn::delete_by_pk(self.txn, table, pk)
    }
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        WriteTxn::fts_prefix(self.txn, table, prefix)
    }
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        WriteTxn::charge_work(self.txn, n, which)
    }
    fn join_cells_budget(&self) -> u64 {
        WriteTxn::join_cells_budget(self.txn)
    }
}

/// One pruned batch ([`TxnCtx::scan_rows_pruned`]): the kept rows and, when
/// the cap was reached, the raw storage key of the last kept row — the
/// resume bound of the next batch.
pub(crate) type PrunedBatch = (Vec<Vec<Value>>, Option<Vec<u8>>);

/// How a [`ReadCtx`]'s scans charge the #74 work meter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChargeMode {
    /// One work-row per row, charged before its decode — the serial contract,
    /// and every context's answer but a parallel fold worker's.
    PerRow,
    /// In batches — see [`mpedb_core::RowCursor::batch_charges`]. The workers
    /// of one statement share ONE atomic meter cell, and a per-row
    /// read-modify-write on it measured 1.4× SLOWER than serial on 11 cores.
    /// Sound only because a worker's every error abandons the attempt to a
    /// serial re-run that owns the authentic refusal.
    Batched,
}

/// Adapter over a pinned read snapshot.
pub(crate) struct ReadCtx<'t, 'e>(
    pub &'t ReadTxn<'e>,
    /// Host-registered scalar UDFs in scope for this read (design/DESIGN-UDF.md),
    /// or `None`. Set by [`crate::Database`] only for a plan that
    /// `contains_host_call`; the streaming and sqlite-overlay read paths pass
    /// `None` (host UDFs there are out of scope for stage 1).
    pub Option<&'t dyn HostFns>,
    /// Host-registered AGGREGATE factories in scope for this read (stage 2),
    /// gated by the same `contains_host_call` test as the scalars above.
    pub Option<&'t dyn mpedb_types::HostAggs>,
    /// Host-registered COLLATING SEQUENCES in scope for this read (stage 3),
    /// gated by the same `contains_host_call` test as the two above.
    pub Option<&'t dyn HostColls>,
    /// #74 charging discipline — [`ChargeMode::PerRow`] everywhere but inside
    /// a parallel fold worker.
    pub ChargeMode,
);

impl TxnCtx for ReadCtx<'_, '_> {
    fn snapshot_txn(&self) -> Option<&ReadTxn<'_>> {
        Some(self.0)
    }
    fn host_fns(&self) -> Option<&dyn HostFns> {
        self.1
    }
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        self.2
    }
    fn host_colls(&self) -> Option<&dyn HostColls> {
        self.3
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        self.0.get_by_pk(table, pk)
    }
    fn get_by_pk_cols(
        &mut self,
        table: u32,
        pk: &[Value],
        cols: &[u16],
    ) -> Result<Option<Vec<Value>>> {
        self.0.get_by_pk_cols(table, pk, cols)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        self.0.get_by_index(table, index_no, values)
    }
    fn scan_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        self.0.scan_by_index(table, index_no, values)
    }
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        self.0.scan_by_index_range(table, index_no, lo, hi)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut out = Vec::new();
        while let Some(row) = cursor.next()? {
            out.push(row);
        }
        Ok(out)
    }
    fn scan_rows_capped(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        // true streaming: stop pulling from the B+tree cursor the moment the
        // cap is reached — `SELECT ... LIMIT k` does O(offset+k) work
        let host = self.1;
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        while let Some(row) = cursor.next()? {
            let keep = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if keep {
                kept.push(row);
                if cap.is_some_and(|c| kept.len() >= c) {
                    break;
                }
            }
        }
        Ok(kept)
    }
    // A real B+tree cursor: the `scan_rows_capped` above stops pulling the
    // moment the cap is reached, which is the precondition a resumable
    // batched scan needs (see the trait default).
    fn scans_incrementally(&self) -> bool {
        true
    }
    fn scan_rows_pruned(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: usize,
        keep: Option<&[bool]>,
    ) -> Result<PrunedBatch> {
        let host = self.1;
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut kept = Vec::new();
        let mut stack = Vec::new();
        // The raw key of the row most recently yielded, written into a
        // reused buffer — when the loop breaks at the cap this holds the last
        // KEPT row's key, which is the batch's resume bound.
        let mut key_buf = Vec::new();
        if self.4 == ChargeMode::Batched {
            cursor.batch_charges(64);
        }
        while let Some(row) = cursor.next_masked(keep, Some(&mut key_buf))? {
            let ok = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if ok {
                kept.push(row);
                if kept.len() >= cap {
                    // A batching cursor is abandoned here, mid-range: its
                    // unflushed rows must reach the meter before it dies.
                    cursor.flush_charges()?;
                    return Ok((kept, Some(std::mem::take(&mut key_buf))));
                }
            }
        }
        cursor.flush_charges()?;
        // Exhausted short of the cap: no resume needed, and none is claimed.
        Ok((kept, None))
    }
    fn count_rows_range(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<u64>> {
        self.0.count_range(table, lo, hi).map(Some)
    }
    // The pinned-snapshot context is the one that owns real index trees, so it
    // is the one that serves the aggregate-over-index paths (format 59).
    fn agg_over_index_supported(&self) -> bool {
        true
    }
    fn count_index_entries(&mut self, table: u32, index_no: u32) -> Result<u64> {
        self.0.count_index_entries(table, index_no)
    }
    fn fold_index_leading(
        &mut self,
        table: u32,
        index_no: u32,
        f: &mut dyn FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        self.0.fold_index_leading(table, index_no, f)
    }
    fn index_boundary_row(
        &mut self,
        table: u32,
        index_no: u32,
        max: bool,
    ) -> Result<Option<Vec<Value>>> {
        self.0.index_boundary_row(table, index_no, max)
    }
    fn scan_by_index_range_adaptive(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        self.0.scan_by_index_range_adaptive(table, index_no, lo, hi)
    }

    fn row_count(&mut self, table: u32) -> Result<Option<u64>> {
        self.0.row_count(table).map(Some)
    }

    fn fold_rows_column(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        col: u16,
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<Option<FoldStop>> {
        self.0.fold_range_column(table, lo, hi, col, opts, f).map(Some)
    }

    fn fold_rows_column_filtered(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        need: &[u16],
        col: u16,
        filter: (&ExprProgram, &[Value]),
        opts: FoldOpts,
        f: &mut dyn FnMut(&Value) -> Result<()>,
    ) -> Result<Option<FoldStop>> {
        let host = self.1;
        let (prog, params) = filter;
        let mut stack = Vec::new();
        self.0
            .fold_range_columns(table, lo, hi, need, opts, &mut |buf| {
                if prog.eval_filter_host(&mut stack, buf, params, host)? {
                    f(&buf[col as usize])?;
                }
                Ok(())
            })
            .map(Some)
    }
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, SortDir, OrderColl)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
        gather::check_order_colls(order_by, self.host_colls())?;
        if keep == 0 {
            return Ok(Vec::new());
        }
        // Bounded max-heap of the `keep` smallest rows seen so far: the heap's
        // top is the *worst* kept row, so a newcomer that sorts before it
        // evicts it. Never more than `keep` rows are held, regardless of how
        // many the scan yields.
        let mut heap: BinaryHeap<Ranked<'_>> = BinaryHeap::with_capacity(keep + 1);
        let host = self.1;
        let mut cursor = self.0.scan_raw(table, lo, hi)?;
        let mut stack = Vec::new();
        // Scan sequence = PK order; used as a stable tiebreaker so equal
        // ORDER BY keys come out exactly as the engine's stable `sort_rows`
        // would order them (scan/PK order), matching the non-top-K path.
        let mut seq: u64 = 0;
        while let Some(row) = cursor.next()? {
            let ok = match filter {
                Some((f, params)) => f.eval_filter_host(&mut stack, &row, params, host)?,
                None => true,
            };
            if !ok {
                continue;
            }
            let cand = Ranked { row, order_by, colls: self.3, seq };
            seq += 1;
            if heap.len() < keep {
                heap.push(cand);
            } else if cand < *heap.peek().expect("keep >= 1") {
                heap.pop();
                heap.push(cand);
            }
        }
        Ok(heap.into_sorted_vec().into_iter().map(|r| r.row).collect())
    }
    fn insert_row(&mut self, _table: u32, _values: &[Value]) -> Result<()> {
        Err(read_txn_write_bug())
    }
    fn update_by_pk(&mut self, _table: u32, _new_values: &[Value]) -> Result<bool> {
        Err(read_txn_write_bug())
    }
    fn delete_by_pk(&mut self, _table: u32, _pk: &[Value]) -> Result<bool> {
        Err(read_txn_write_bug())
    }
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.0.fts_prefix(table, prefix)
    }
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        self.0.charge_work(n, which)
    }
    fn join_cells_budget(&self) -> u64 {
        self.0.join_cells_budget()
    }
}

/// A row wrapped with its `ORDER BY` spec so a [`BinaryHeap`] (max-heap)
/// keeps the smallest rows: `Ord` follows the sort order, so the heap's max
/// is the row that sorts *last*.
struct Ranked<'a> {
    row: Vec<Value>,
    order_by: &'a [(u16, SortDir, OrderColl)],
    /// The connection's HOST collating sequences, so a `COLLATE mycoll` key
    /// orders through the callback here exactly as it does in `sort_rows`.
    colls: Option<&'a dyn HostColls>,
    seq: u64,
}

impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Primary: the ORDER BY spec. Secondary: scan sequence ASCENDING
        // regardless of the ORDER BY direction — a stable sort keeps equal
        // keys in original (scan) order, so the tiebreak is never reversed.
        cmp_rows(&self.row, &other.row, self.order_by, self.colls).then(self.seq.cmp(&other.seq))
    }
}
impl PartialOrd for Ranked<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Ranked<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked<'_> {}

fn read_txn_write_bug() -> Error {
    Error::Internal("DML plan routed to a read transaction".into())
}

/// The `which` attribution (#74) for a table `id` in one of the exec-layer
/// budget bumps. Built lazily, only on the abort path.
fn table_name(schema: &Schema, id: u32) -> String {
    schema
        .table(id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| format!("table #{id}"))
}

fn internal(msg: &str) -> Error {
    Error::Internal(format!("validated plan out of bounds: {msg}"))
}

/// True when `e` is a constraint error that the engine's row mutators
/// (`insert_row`/`update_by_pk`) raise from their pre-checks, strictly
/// *before* mutating any tree: a call that failed this way left the
/// transaction untouched. Anything else (DbFull, Corrupt, Internal, Io, ...)
/// can fire mid-mutation and must be treated as a possible partial effect.
/// **§6.5 classification-oracle closure.** On an RLS-enabled table, collapse the
/// constraint-violation variants into one opaque rejection.
///
/// `rls` is `with_check.is_some()`, which is exact rather than a proxy: the
/// planner emits `with_check` for a write iff RLS is enabled on the target
/// (`write_check` returns `None` otherwise), so no plan-format flag is needed.
///
/// MUST be applied AFTER `precheck_failure` has decided `partial`: that function
/// matches on the very variants being collapsed, so normalizing first would make
/// it report a partial effect where the row never landed.
fn hide_constraint_variant(e: Error, table: &str, rls: bool) -> Error {
    if !rls {
        return e;
    }
    match e {
        Error::PrimaryKeyViolation { .. }
        | Error::UniqueViolation { .. }
        | Error::CheckViolation { .. } => Error::WriteRejected {
            table: table.to_string(),
        },
        other => other,
    }
}

fn precheck_failure(e: &Error) -> bool {
    matches!(
        e,
        Error::TypeMismatch(_)
            | Error::NotNullViolation { .. }
            | Error::CheckViolation { .. }
            | Error::UniqueViolation { .. }
            | Error::PrimaryKeyViolation { .. }
    )
}

/// Execute one statement plan against `ctx`. `params` are validated first
/// (count, then declared types; NULL always passes the type check —
/// nullability is enforced by the engine where it matters).
///
/// `partial` is an out-flag for statement-level atomicity: when the returned
/// value is an `Err`, `*partial == true` means the failed statement may
/// already have applied some of its effects to `ctx` (e.g. a multi-row
/// INSERT that violated a constraint on its third row inserted the first
/// two). Callers that keep the transaction alive across statement failures
/// ([`crate::WriteSession`]) must then poison it; the autocommit path aborts
/// the whole transaction on any error and can ignore the flag. The flag is
/// never set spuriously *false* (never under-reports), but it may be
/// conservatively *true* for failures whose partial effects cannot be ruled
/// out.
pub(crate) fn exec_stmt(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
) -> Result<ExecResult> {
    // Read paths and any caller that cannot fire triggers use the trigger-free
    // set — one empty-map lookup per written row, no allocation.
    exec_stmt_triggered(ctx, schema, plan, params, partial, &TriggerSet::empty(), 0)
}

/// Maximum depth of the trigger cascade (DESIGN-TRIGGERS §4.4). Each level is a
/// full statement execution, so this is deliberately conservative — far below
/// sqlite's 1000. Exceeding it aborts the whole statement.
pub(crate) const MAX_TRIGGER_DEPTH: u32 = 32;

/// Like [`exec_stmt`], but with the trigger set to fire from (and the current
/// cascade `depth`). The write paths pass the leader's/session's gen-gated
/// [`TriggerSet`]; a trigger body re-enters here with `depth + 1` on the SAME
/// `ctx`, never through the facade (DESIGN-TRIGGERS §4.3).
pub(crate) fn exec_stmt_triggered(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
    triggers: &TriggerSet,
    depth: u32,
) -> Result<ExecResult> {
    // #40 instrument: statement-total time, so resolve + stmt reconciles
    // against execute()'s wall clock and nothing hides between the phases.
    #[cfg(feature = "leakstat")]
    {
        let t0 = std::time::Instant::now();
        let r = exec_stmt_impl(ctx, schema, plan, params, partial, triggers, depth);
        mpedb_core::engine::leakstat::add(
            &mpedb_core::engine::leakstat::EXEC_NS_STMT,
            t0.elapsed().as_nanos() as u64,
        );
        r
    }
    #[cfg(not(feature = "leakstat"))]
    exec_stmt_impl(ctx, schema, plan, params, partial, triggers, depth)
}

fn exec_stmt_impl(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
    triggers: &TriggerSet,
    depth: u32,
) -> Result<ExecResult> {
    let coerced = coerce_params(plan, params)?;
    let params: &[Value] = &coerced;
    // Uncorrelated subplans evaluate ONCE per execute, into their reserved
    // slots — before dispatch, so a PK probe built on `id = (SELECT max…)`
    // resolves like any other param. Correlated ones wait for their row.
    let filled;
    let params: &[Value] = if plan.subplans.iter().any(|s| s.outer_args.is_empty()) {
        let base = plan.subplan_base() as usize;
        let n_user = base;
        let mut buf = params.to_vec();
        for (i, sub) in plan.subplans.iter().enumerate() {
            if !sub.outer_args.is_empty() {
                continue;
            }
            // `run_subplan` fills this subplan's OWN uncorrelated nested lifts
            // (#73 §3) before running it — the recursion the flat two levels
            // became.
            let inner = run_subplan(ctx, schema, plan, &buf[..n_user], sub)?;
            buf[base + i] = subplan_value(inner, sub.kind)?;
        }
        filled = buf;
        &filled
    } else {
        params
    };
    match &plan.stmt {
        PlanStmt::Select(sp) => exec_select_top(ctx, schema, plan, params, sp),
        PlanStmt::Compound(c) => exec_compound(ctx, schema, plan, params, c),
        PlanStmt::RecursiveCte(rc) => recursive::exec_recursive_cte(ctx, schema, plan, params, rc),
        PlanStmt::Derived(dp) => recursive::exec_derived(ctx, schema, plan, params, dp),
        _other => exec_stmt_rest(ctx, schema, plan, params, partial, triggers, depth),
    }
}

/// A subquery's rows, reduced to the VALUE its reserved slot carries.
pub(super) fn subplan_value(r: ExecResult, kind: mpedb_sql::SubPlanKind) -> Result<Value> {
    use mpedb_sql::SubPlanKind as K;
    let ExecResult::Rows { rows, .. } = r else {
        return Err(internal("subplan produced no row set"));
    };
    match kind {
        K::Exists => return Ok(Value::Bool(!rows.is_empty())),
        K::List => {
            // `x IN (SELECT …)`: every value of the single output column,
            // order-irrelevant (membership). Bounded so a runaway subquery
            // cannot balloon one param slot unobserved.
            if rows.len() > 1_000_000 {
                return Err(Error::Unsupported(format!(
                    "an IN subquery returned {} rows — the membership list is \
                     capped at 1,000,000",
                    rows.len()
                )));
            }
            let mut items = Vec::with_capacity(rows.len());
            for mut r in rows {
                match (r.pop(), r.is_empty()) {
                    (Some(v), true) => items.push(v),
                    _ => return Err(internal("IN subplan output arity")),
                }
            }
            return Ok(Value::List(items));
        }
        K::Scalar => {}
    }
    match rows.len() {
        0 => Ok(Value::Null),
        1 => rows
            .into_iter()
            .next()
            .and_then(|mut r| if r.len() == 1 { r.pop() } else { None })
            .ok_or_else(|| internal("scalar subplan output arity")),
        // sqlite silently takes the first row; saying so is the strict line.
        // (The planner caps a scalar subplan at 2 rows — enough to detect this —
        // so `n` is the capped count, i.e. "at least 2", not the true total.)
        _ => Err(Error::Unsupported(
            "a scalar subquery returned more than one row — it must return at most one".into(),
        )),
    }
}

/// Run one subplan, first filling its OWN nested lifts (#73 §3).
///
/// `base_params` is `[user ‖ this subplan's correlation args]` — of length
/// `sub.sub_base` — so a plain leaf subplan (no nested lifts) runs exactly as
/// before. A leaf subplan's body may be a plain SELECT or a whole compound
/// (#56/format 31), run through [`exec_subbody`]. When `sub` HAS nested lifts
/// (only a SELECT body ever does):
///
/// - UNCORRELATED children depend only on `base_params`, so each is evaluated
///   ONCE here, bottom-up, into `[.. ‖ children results]` at `sub_base + i`,
///   before the select body's own gather.
/// - CORRELATED children (stage 2: correlated to THIS subplan's row) are NOT
///   filled here — they are filled PER ROW of the select body by
///   [`exec_select_leveled`], the same machinery the top level uses for its own
///   correlated subplans, plus the body's `post_filter` when the correlated
///   child feeds `sub`'s WHERE.
///
/// This generalizes the flat two-level fill (`exec_stmt_impl` once + top per-row)
/// into a recursion that bottoms out at the leaf case.
pub(super) fn run_subplan(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    base_params: &[Value],
    sub: &SubPlan,
) -> Result<ExecResult> {
    // A leaf subplan (no nested lifts) runs its body directly — a plain SELECT or
    // a whole compound (#56/format 31). A compound body is always a leaf.
    if sub.subplans.is_empty() {
        return exec_subbody(ctx, schema, plan, base_params, &sub.body);
    }
    // With nested lifts the body is guaranteed a plain SELECT (a compound body
    // never carries children — validate/planner enforce it).
    let Some(sp) = sub.body.as_select() else {
        return Err(internal("compound subplan body with nested lifts"));
    };
    let base = sub.sub_base as usize;
    let mut buf = base_params.to_vec();
    buf.resize(base + sub.subplans.len(), Value::Null);
    for (i, child) in sub.subplans.iter().enumerate() {
        // Only the uncorrelated children fill once here (into `sub_base + i`); a
        // correlated child correlates to `sp`'s row and is filled per row below.
        // `base_params` (== `buf[..base]`) is the `[user ‖ correlation]` prefix
        // each uncorrelated child inherits.
        if child.outer_args.is_empty() {
            let r = run_subplan(ctx, schema, plan, base_params, child)?;
            buf[base + i] = subplan_value(r, child.kind)?;
        }
    }
    exec_select_leveled(ctx, schema, plan, &buf, sp, base, &sub.subplans)
}

/// Execute a lifted subquery's body — a plain `SELECT` or a whole compound
/// `SELECT … UNION/… …` (#56/format 31) — into the row set its consumer
/// (`subplan_value`) reduces to a value / list / existence.
fn exec_subbody(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    body: &SubBody,
) -> Result<ExecResult> {
    match body {
        SubBody::Select(sp) => exec_select(ctx, schema, plan, params, sp),
        SubBody::Compound(c) => exec_compound(ctx, schema, plan, params, c),
    }
}

/// The top-level SELECT: routes to the leveled executor with the statement's
/// own lifts (result slots at `subplan_base + i`). See [`exec_select_leveled`].
fn exec_select_top(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<ExecResult> {
    exec_select_leveled(
        ctx,
        schema,
        plan,
        params,
        sp,
        plan.subplan_base() as usize,
        &plan.subplans,
    )
}

/// Execute one SELECT whose CORRELATED subplans (and any `post_filter`) are
/// handled PER ROW. `subplans` is this level's lift list, with result slots at
/// `base + i` in `params` — every UNCORRELATED slot already filled by the
/// caller. A correlated subplan is the ONLY place its result slot is filled:
/// per row, after the gather (and therefore after every policy) has produced
/// the row.
///
/// Shared by the top level (`base = subplan_base`, `subplans = plan.subplans`)
/// and — via [`run_subplan`] — each NESTED subplan (`base = sub.sub_base`,
/// `subplans = sub.subplans`). That is the recursion #73 §3 stage 2 turns the
/// two hardcoded levels into: a nested subquery correlated to its immediate
/// parent is filled per parent row here, exactly as the top level fills its
/// correlated subplans per outer row. Compound arms and leaf subplans instead
/// go through the plain [`exec_select`], which never fills slots.
pub(super) fn exec_select_leveled(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
    base: usize,
    subplans: &[SubPlan],
) -> Result<ExecResult> {
    let correlated: Vec<(usize, &SubPlan)> = subplans
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.outer_args.is_empty())
        .collect();
    if correlated.is_empty() && sp.post_filter.is_none() {
        return exec_select(ctx, schema, plan, params, sp);
    }
    // #73 §1: an aggregate over a correlated filter. The aggregate path consumes
    // rows in its gather, so the per-row correlated pre-filter must run BETWEEN
    // the gather and the grouping — `exec_aggregate` takes the correlated
    // subplans and the post-filter and runs the shared `correlated_survivors`
    // there. Everything downstream (empty-group zero row, HAVING, ORDER BY,
    // LIMIT-bounds-groups) is unchanged.
    if sp.aggregate.is_some() {
        return run_aggregate(
            ctx, schema, plan, params, sp, base, &correlated, sp.post_filter.as_ref(),
        );
    }
    exec_select_with(ctx, schema, plan, params, sp, base, &correlated)
}

/// Combine already-projected rows under one set operator, left-associatively.
/// `UNION`/`EXCEPT`/`INTERSECT` are SET ops: the result is deduplicated (and
/// NULLs count as equal — the set-op rule, same as DISTINCT); only
/// `UNION ALL` keeps duplicates. Keys are the storage-class GROUP encoding, for
/// the same reason DISTINCT uses it: Value is neither Hash nor Ord, the
/// encoding is total even across types, and set membership is decided by
/// sqlite's comparison — `SELECT 1 UNION SELECT 1.0` is one row.
fn apply_set_op(acc: Vec<Vec<Value>>, right: Vec<Vec<Value>>, op: SetOp) -> Vec<Vec<Value>> {
    use std::collections::HashSet;
    let dedup = |rows: Vec<Vec<Value>>| {
        let mut seen = HashSet::new();
        rows.into_iter()
            .filter(|r| seen.insert(keycode::encode_group_key(r, &[])))
            .collect::<Vec<_>>()
    };
    match op {
        SetOp::UnionAll => {
            let mut acc = acc;
            acc.extend(right);
            acc
        }
        SetOp::Union => {
            let mut acc = acc;
            acc.extend(right);
            dedup(acc)
        }
        SetOp::Except | SetOp::Intersect => {
            let rset: std::collections::HashSet<Vec<u8>> =
                right.iter().map(|r| keycode::encode_group_key(r, &[])).collect();
            let keep_present = matches!(op, SetOp::Intersect);
            dedup(acc)
                .into_iter()
                .filter(|r| rset.contains(&keycode::encode_group_key(r, &[])) == keep_present)
                .collect()
        }
    }
}

/// Execute compound ARM `k`, with the lifts that arm OWNS (format 56) filled
/// the way every other level fills its own: the UNCORRELATED ones once, up
/// front, and the CORRELATED ones per ARM row by [`exec_select_leveled`].
///
/// This is `exec_stmt_impl`'s discipline and `exec_derived`'s, applied to an
/// arm — the ownership move, not a new mechanism. An arm's correlated lift
/// names the ARM's row, which is the only row it CAN name: a compound has no
/// outer row of its own, which is exactly why hoisting these onto the statement
/// could never fill them.
///
/// The buffer is rebuilt from `params` for every arm, so another arm's reserved
/// slots are NULL rather than stale here — a forged cross-arm read (which
/// `validate_compound` rejects) can then only see NULL, never another row's
/// correlated value.
fn exec_compound_arm(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    c: &CompoundPlan,
    k: usize,
) -> Result<ExecResult> {
    let arm = c.arms.get(k).ok_or_else(|| internal("compound arm out of range"))?;
    match arm {
        mpedb_sql::CompoundArm::Derived(dp) => {
            // Nested derived as a compound arm: materialise body, scan outer.
            // Body slots live at dp.body_sub_base in the shared param buffer.
            recursive::exec_derived(ctx, schema, plan, params, dp)
        }
        mpedb_sql::CompoundArm::Select(sp) => {
            let lifts = c.arm_lifts(k);
            if lifts.is_empty() && c.n_arm_slots() == 0 {
                return exec_select(ctx, schema, plan, params, sp);
            }
            let base = c.arm_base(k) as usize;
            let mut buf = params.to_vec();
            buf.resize(
                buf.len()
                    .max(c.arm_sub_base as usize + c.n_arm_slots() as usize)
                    .max(params.len()),
                Value::Null,
            );
            for (i, sub) in lifts.iter().enumerate() {
                if !sub.outer_args.is_empty() {
                    continue;
                }
                let inner = run_subplan(ctx, schema, plan, &buf[..base], sub)?;
                buf[base + i] = subplan_value(inner, sub.kind)?;
            }
            exec_select_leveled(ctx, schema, plan, &buf, sp, base, lifts)
        }
    }
}

fn exec_compound(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    c: &CompoundPlan,
) -> Result<ExecResult> {
    // Arms carry no ORDER BY/LIMIT of their own (validate enforces it), so
    // each arm materializes exactly its projected rows. The FIRST arm names
    // the output — sqlite's and PG's rule.
    if c.arms.is_empty() {
        return Err(internal("compound with no arms"));
    }
    let ExecResult::Rows { columns, rows } = exec_compound_arm(ctx, schema, plan, params, c, 0)?
    else {
        return Err(internal("compound arm produced no rows"));
    };
    let mut acc = rows;
    for (k, op) in c.ops.iter().enumerate() {
        let ExecResult::Rows { rows, .. } = exec_compound_arm(ctx, schema, plan, params, c, k + 1)?
        else {
            return Err(internal("compound arm produced no rows"));
        };
        acc = apply_set_op(acc, rows, *op);
    }
    if !c.order_by.is_empty() {
        gather::check_order_colls(&c.order_by, ctx.host_colls())?;
        sort_rows(&mut acc, &c.order_by, ctx.host_colls());
    }
    let skip = c.offset.unwrap_or(0).min(usize::MAX as u64) as usize;
    let take = c.limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    if skip > 0 || take != usize::MAX {
        acc = acc.into_iter().skip(skip).take(take).collect();
    }
    Ok(ExecResult::Rows { columns, rows: acc })
}

/// **What the statement must PRODUCE, turned into a bound on what the pipeline
/// carries** (#125).
///
/// The row pipeline for one SELECT is `[table0 ‖ table1 ‖ …]`, and every column
/// of every table in it is materialized today whether or not anything
/// downstream can see it — `SELECT count(*)` over a join holds the entire
/// product to produce one integer. [`mpedb_sql::row_prune`] computes the slots
/// a later stage can observe; `None` means every slot is observed and the
/// executor's paths stay byte-for-byte what they were.
///
/// Two base-row reads go through no expression and so are passed in
/// explicitly: the outer table's PRIMARY KEY (sqlite's bare-column witness
/// picks a group's lowest-rowid row by reading it) and each correlated
/// subplan's `outer_args` (filled per row by [`correlated_survivors`]).
fn select_prune(
    schema: &Schema,
    plan: &CompiledPlan,
    sp: &SelectPlan,
    correlated: &[(usize, &SubPlan)],
) -> Result<Option<RowPrune>> {
    let t = table_def(schema, plan, sp.table)?;
    // One width per stage of the pipeline: the outer table, then each join's.
    let mut widths = Vec::with_capacity(sp.joins.len() + 1);
    widths.push(t.columns.len());
    for j in &sp.joins {
        widths.push(table_def(schema, plan, j.table)?.columns.len());
    }
    let mut args: Vec<u16> = Vec::new();
    for (_, s) in correlated {
        args.extend_from_slice(&s.outer_args);
    }
    Ok(mpedb_sql::row_prune(sp, &widths, &t.primary_key, &args))
}

/// One SELECT — shared verbatim between a top-level SELECT and each compound
/// arm, so the two can never drift.
fn exec_select(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<ExecResult> {
    // Window functions are their own phase: materialize the base rows, compute
    // each window, project over the extended rows, then sort/trim/bound. Kept in
    // its own function so this executor's other paths stay untouched.
    if !sp.windows.is_empty() {
        return window::exec_select_windowed(ctx, schema, plan, params, sp);
    }
    // Micro-executor: single-table PK point probe with column-only projection.
    // The generic path materializes gather → project → sort/limit machinery;
    // for `SELECT cols FROM t WHERE pk = $1` that is pure overhead (~half of
    // prepare+bind SELECT wall time). Same result as the general path.
    if let Some(out) = try_exec_pk_point_hot(ctx, schema, plan, params, sp)? {
        return Ok(out);
    }
    let SelectPlan {
        table,
        access,
        joins,
        joined_filter,
        // Only the TOP-level statement routes post-filter/correlated work
        // (to `exec_select_with`); arms and subplans never carry one — the
        // planner cannot produce it there and validate refuses it.
        post_filter: _,
        filter,
        projection,
        order_by,
        limit,
        offset,
        aggregate,
        distinct,
        order_over,
        order_junk,
        windows: _,
    } = sp;
    {
        {
            // DISTINCT makes LIMIT bound DISTINCT rows, so the scan bound (and
            // the top-K path, which is the same bound wearing a hat) must not
            // apply — the same trap the aggregate path has. Forcing it to None
            // here keeps that in one place rather than at each use.
            // The scan bound only applies when the sort (and the dedup, if any)
            // happen on the base row — otherwise LIMIT bounds a tuple further
            // down the pipeline and cutting the scan short would drop input
            // that later stages still need.
            let skip_take_bound = || {
                // A join is gathered whole (the LIMIT bounds joined rows, not
                // outer rows), and any sort below the base row moves the bound
                // down the pipeline too.
                if !joins.is_empty() || *order_over != OrderOver::BaseRow {
                    return None;
                }
                limit.map(|l| {
                    let l = l.min(usize::MAX as u64) as usize;
                    let o = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
                    l.saturating_add(o)
                })
            };
            // Exact kNN (stage D, design/DESIGN-MPEE-GENERAL.md §3): `ORDER BY
            // vec_l2(col, $q) LIMIT k` over one table selects under a k-sized
            // heap with per-dimension early abandonment, instead of computing
            // every full distance and sorting every row.
            if let Some(out) = try_exec_knn(ctx, schema, plan, params, sp)? {
                return Ok(out);
            }
            if aggregate.is_some() {
                // Plain aggregate: no correlated subplans and no post-filter.
                // This function is the fill-free LEAF — every level that owns
                // correlated lifts (the statement, a derived body, a compound
                // ARM, a nested subplan) routes to `run_aggregate` from
                // `exec_select_leveled` with ITS own base. `base` is unused with
                // an empty correlated set.
                return run_aggregate(
                    ctx, schema, plan, params, sp, plan.subplan_base() as usize, &[], None,
                );
            }
            let rows = if !joins.is_empty() {
                // A join materializes: the sort, the dedup and the LIMIT all
                // apply to JOINED rows, so none of them can bound the scan.
                // #125: the join's product is the biggest thing this path
                // holds, and the projection above it is usually a handful of
                // columns. Computed only for a join — a single-table read
                // materializes exactly one row set and pruning it would rebuild
                // every row to save a slot the projection was about to read.
                let prune = select_prune(schema, plan, sp, &[])?;
                let mut r = gather_joined(
                    ctx,
                    plan,
                    params,
                    schema,
                    *table,
                    access,
                    filter.as_ref(),
                    joins,
                    joined_filter.as_ref(),
                    prune.as_ref(),
                )?;
                // `OrderOver::BaseRow` means "the tuple the scan produced", and
                // for a join that tuple IS the joined row — so the sort belongs
                // HERE, before the projection narrows it. Sorting the projected
                // rows instead would index the wrong tuple.
                if *order_over == OrderOver::BaseRow && !order_by.is_empty() {
                    gather::check_order_colls(order_by, ctx.host_colls())?;
                    gather::check_order_colls(order_by, ctx.host_colls())?;
                sort_rows(&mut r, order_by, ctx.host_colls());
                }
                r
            } else if *order_over != OrderOver::BaseRow {
                // The sort indexes a tuple further down (the projection), so the
                // base rows are left unsorted and unbounded here.
                gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?
            } else if order_by.is_empty() {
                // No surviving sort (the planner elides ORDER BY that matches
                // PK scan order): stream and stop after offset+limit rows.
                gather_rows(ctx, *table, access, filter.as_ref(), plan, params, skip_take_bound())?
            } else if let Some(keep) = skip_take_bound() {
                // ORDER BY … LIMIT: bounded top-K, O(offset+limit) memory
                // instead of materializing every match (already sorted).
                gather_topk(ctx, *table, access, filter.as_ref(), plan, params, order_by, keep)?
            } else {
                // ORDER BY with no LIMIT: must materialize and sort in full.
                let mut r = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
                gather::check_order_colls(order_by, ctx.host_colls())?;
                sort_rows(&mut r, order_by, ctx.host_colls());
                r
            };
            let skip = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
            let take = limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
            // Without DISTINCT, skip/take applies to base rows and there is no
            // reason to project the ones being skipped. With it, the projection
            // is what gets deduplicated, so it must happen first and skip/take
            // moves to the end.
            let (row_skip, row_take) = if *order_over == OrderOver::BaseRow {
                (skip, take)
            } else {
                (0, usize::MAX)
            };
            let mut out = Vec::new();
            let mut seen = std::collections::HashSet::new();
            // Per-output-column collation for DISTINCT: a NOCASE/RTRIM column
            // deduplicates case-/space-insensitively (`SELECT DISTINCT name`),
            // sqlite parity. Only built when DISTINCT (else unused).
            let distinct_colls = if *distinct {
                output_collations(schema, plan, *table, joins, projection)
            } else {
                Vec::new()
            };
            for row in rows.into_iter().skip(row_skip).take(row_take) {
                let mut orow = Vec::with_capacity(projection.len());
                for p in projection {
                    orow.push(match p {
                        Projection::Column(i) => row
                            .get(*i as usize)
                            .cloned()
                            .ok_or_else(|| internal("projection column"))?,
                        Projection::Expr { program, .. } => {
                            program.eval_host(&row, params, ctx.host_fns())?
                        }
                    });
                }
                // Keying on the storage-class GROUP encoding rather than on
                // Value: DISTINCT must treat NULLs as equal to each other
                // (unlike `=`), which is exactly what the key encoding does,
                // and Value is neither Hash nor Ord. It must ALSO treat `1` and
                // `1.0` as one value (sqlite's DISTINCT asks its comparison,
                // and the on-disk encoder answers by mpedb type — 3 values
                // where sqlite sees 2). Text keys are folded under the output
                // column's declared collation.
                if *distinct
                    && !seen.insert(keycode::encode_group_key(&orow, &distinct_colls))
                {
                    continue;
                }
                out.push(orow);
            }
            if *order_over != OrderOver::BaseRow {
                gather::check_order_colls(order_by, ctx.host_colls())?;
                gather::check_order_colls(order_by, ctx.host_colls())?;
        sort_rows(&mut out, order_by, ctx.host_colls());
                // Sort-only columns come off AFTER the sort and before the
                // caller sees anything. They are always trailing, so the trim
                // is a truncate — and it must reach `columns` below too, or the
                // header would name a column the rows no longer carry.
                if *order_junk > 0 {
                    let keep = projection.len() - *order_junk as usize;
                    for row in &mut out {
                        row.truncate(keep);
                    }
                }
                out = out.into_iter().skip(skip).take(take).collect();
            }
            let columns = select_output_columns(schema, plan, sp)?;
            Ok(ExecResult::Rows { columns, rows: out })
        }
    }
}

/// Exact kNN under a bounded heap with early abandonment — `Some` when the
/// plan is `SELECT … FROM t [WHERE …] ORDER BY vec_l2(col, <query>) LIMIT k`
/// (single table, single ascending sort key, the key a projection expression
/// of exactly that shape), `None` otherwise.
///
/// The abandonment is the monotone-bound argument of DESIGN-MPEE-GENERAL §3
/// made concrete: squared-difference terms are non-negative, so the partial
/// sum is a lower bound on the full distance, and a candidate is dropped the
/// moment its partial sum exceeds the current k-th best. **Exactness and
/// errors are both preserved:** the SHAPE of every row's blob is validated
/// before any summing (a malformed embedding raises exactly as the generic
/// projection would), only the arithmetic is skipped — and the skipped
/// arithmetic could only have grown the sum further. Ordering matches the
/// generic path's stable sort bit-exactly: candidates compare by
/// `(distance², scan order)`, and `sqrt` is monotone, so the selected set and
/// its order are the ones the full sort would produce.
///
/// NULL keys (a NULL embedding or a NULL query) sort BEFORE every real
/// distance — ascending storage-class order, sqlite's rule — and are kept in
/// scan order, exactly as `sort_rows` would place them.
fn try_exec_knn(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<Option<ExecResult>> {
    use mpedb_types::{Instr, ScalarFn};
    let SelectPlan {
        table,
        access,
        joins,
        filter,
        projection,
        order_by,
        limit,
        offset,
        distinct,
        order_over,
        order_junk,
        ..
    } = sp;

    // Shape gates — anything unproven falls back to the generic path, which
    // is the semantics of record.
    if !joins.is_empty()
        || *distinct
        || *order_over == OrderOver::BaseRow
        || order_by.len() != 1
        || *table == mpedb_sql::DUAL_TABLE
        || *table == mpedb_sql::CTE_TABLE
    {
        return Ok(None);
    }
    let (key_col, dir, coll) = (&order_by[0].0, order_by[0].1, &order_by[0].2);
    if dir != SortDir::ASC || !matches!(coll, OrderColl::Native(_)) {
        return Ok(None);
    }
    let Some(limit) = limit else { return Ok(None) };
    let Some(Projection::Expr { program, .. }) = projection.get(*key_col as usize) else {
        return Ok(None);
    };
    // vec_l2 is symmetric, so both argument orders qualify.
    let (emb_col, query) = match program.instrs.as_slice() {
        [Instr::PushCol(c), Instr::PushParam(p), Instr::Call(ScalarFn::VecL2, 2)] => {
            (*c, params.get(*p as usize))
        }
        [Instr::PushParam(p), Instr::PushCol(c), Instr::Call(ScalarFn::VecL2, 2)] => {
            (*c, params.get(*p as usize))
        }
        [Instr::PushCol(c), Instr::PushConst(ci), Instr::Call(ScalarFn::VecL2, 2)] => {
            (*c, program.consts.get(*ci as usize))
        }
        [Instr::PushConst(ci), Instr::PushCol(c), Instr::Call(ScalarFn::VecL2, 2)] => {
            (*c, program.consts.get(*ci as usize))
        }
        _ => return Ok(None),
    };
    // The query vector, validated ONCE. NULL or malformed → generic path, so
    // the NULL-key ordering and the canonical refusal message both come from
    // the code that owns them.
    let Some(Value::Blob(qb)) = query else { return Ok(None) };
    if qb.len() % 4 != 0 {
        return Ok(None);
    }
    let q: Vec<f64> = qb
        .chunks_exact(4)
        .map(|c| f64::from(f32::from_le_bytes([c[0], c[1], c[2], c[3]])))
        .collect();

    let keep = {
        let l = (*limit).min(usize::MAX as u64) as usize;
        let o = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
        l.saturating_add(o)
    };

    // The scan: same gather, same charges, same filter as the generic path.
    let rows = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;

    // NULL keys sort first (kept in scan order, capped at `keep`); real
    // distances go through the max-heap of the k best (d², seq) pairs.
    struct Cand {
        d2: f64,
        seq: usize,
        row: Vec<Value>,
    }
    impl PartialEq for Cand {
        fn eq(&self, other: &Self) -> bool {
            self.d2 == other.d2 && self.seq == other.seq
        }
    }
    impl Eq for Cand {}
    impl PartialOrd for Cand {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Cand {
        fn cmp(&self, other: &Self) -> Ordering {
            self.d2.total_cmp(&other.d2).then(self.seq.cmp(&other.seq))
        }
    }
    let mut nulls: Vec<Vec<Value>> = Vec::new();
    let mut heap: BinaryHeap<Cand> = BinaryHeap::with_capacity(keep + 1);
    for (seq, row) in rows.into_iter().enumerate() {
        let emb = row.get(emb_col as usize);
        let eb = match emb {
            Some(Value::Null) => {
                if nulls.len() < keep {
                    nulls.push(row);
                }
                continue;
            }
            Some(Value::Blob(b)) => b,
            // Not a blob: the canonical refusal, from the canonical code.
            Some(other) => {
                return Err(Error::TypeMismatch(format!(
                    "vec_l2() argument 1 must be a blob of little-endian f32, got {}",
                    other.type_name()
                )))
            }
            None => return Err(Error::Corrupt("kNN embedding column out of range".into())),
        };
        // Shape validation is NEVER abandoned — a malformed row must raise
        // here exactly as the generic projection would have raised.
        if eb.len() % 4 != 0 {
            return Err(Error::TypeMismatch(format!(
                "vec_l2() argument 1: blob length {} is not a multiple of 4",
                eb.len()
            )));
        }
        if eb.len() / 4 != q.len() {
            return Err(Error::TypeMismatch(format!(
                "vec_l2(): dimension mismatch ({} vs {})",
                eb.len() / 4,
                q.len()
            )));
        }
        let bound = if heap.len() == keep {
            match heap.peek() {
                Some(worst) => worst.d2,
                None => f64::INFINITY,
            }
        } else {
            f64::INFINITY
        };
        let mut d2 = 0.0f64;
        let mut abandoned = false;
        for (chunk, qv) in eb.chunks_exact(4).zip(&q) {
            let x = f64::from(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            let d = x - qv;
            d2 += d * d;
            // The partial sum only grows: past the current k-th best, the
            // remaining dimensions cannot un-lose.
            if d2 > bound {
                abandoned = true;
                break;
            }
        }
        if abandoned || (heap.len() == keep && keep > 0 && d2 >= bound) {
            // `>=`: an exact tie keeps the EARLIER row — the stable sort's
            // answer, since this row's seq is the largest so far.
            continue;
        }
        if keep == 0 {
            break;
        }
        heap.push(Cand { d2, seq, row });
        if heap.len() > keep {
            heap.pop();
        }
    }

    // NULLs first (scan order), then ascending distance — `sort_rows`' order.
    let mut chosen = nulls;
    let mut ranked: Vec<Cand> = heap.into_vec();
    ranked.sort();
    chosen.extend(ranked.into_iter().map(|c| c.row));
    chosen.truncate(keep);

    // The generic tail: project, trim sort-only columns, skip/take.
    let mut out = Vec::with_capacity(chosen.len());
    for row in &chosen {
        let mut orow = Vec::with_capacity(projection.len());
        for p in projection {
            orow.push(match p {
                Projection::Column(i) => row
                    .get(*i as usize)
                    .cloned()
                    .ok_or_else(|| internal("projection column"))?,
                Projection::Expr { program, .. } => {
                    program.eval_host(row, params, ctx.host_fns())?
                }
            });
        }
        out.push(orow);
    }
    if *order_junk > 0 {
        let width = projection.len() - *order_junk as usize;
        for row in &mut out {
            row.truncate(width);
        }
    }
    let skip = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
    let take = (*limit).min(usize::MAX as u64) as usize;
    let out: Vec<Vec<Value>> = out.into_iter().skip(skip).take(take).collect();

    let columns = select_output_columns(schema, plan, sp)?;
    Ok(Some(ExecResult::Rows { columns, rows: out }))
}

/// Precomputed shape for the PkPoint micro-executor. Built once at
/// [`crate::PreparedSelect`] prepare (or on first execute) so the hot path
/// never rebuilds column names or projection ordinals — SQLite's stmt keeps
/// the same state on the `sqlite3_stmt`.
#[derive(Debug, Clone)]
pub(crate) struct PkPointHot {
    pub table: u32,
    pub col_idxs: Vec<u16>,
    pub columns: std::sync::Arc<[String]>,
}

/// Hot path for the common point lookup:
/// `SELECT c1, c2, … FROM t WHERE pk0 = $a [AND pk1 = $b …]`
///
/// Shape (all required — anything else falls through to the general SELECT):
/// - single table, no join / residual filter / post_filter / aggregate /
///   DISTINCT / ORDER BY / windows / dual
/// - `AccessPath::PkPoint` with only `Param`/`Const` parts (no OuterCol)
/// - projection is only base columns (no Expr)
/// - `offset` is 0 or absent; `limit` is absent or ≥ 1 (PkPoint yields ≤ 1 row)
///
/// Correctness: same `get_by_pk` + column projection the gather path uses;
/// column names match [`select_output_columns`].
fn try_exec_pk_point_hot(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
) -> Result<Option<ExecResult>> {
    let Some(hot) = try_build_pk_point_hot(schema, plan, sp)? else {
        return Ok(None);
    };
    Ok(Some(exec_pk_point_hot(ctx, plan, params, sp, &hot)?))
}

/// Build [`PkPointHot`] if `sp` is eligible; `Ok(None)` means use the general
/// SELECT path (not an error).
pub(crate) fn try_build_pk_point_hot(
    schema: &Schema,
    plan: &CompiledPlan,
    sp: &SelectPlan,
) -> Result<Option<PkPointHot>> {
    if sp.table == mpedb_sql::DUAL_TABLE
        || !sp.joins.is_empty()
        || sp.filter.is_some()
        || sp.joined_filter.is_some()
        || sp.post_filter.is_some()
        || sp.aggregate.is_some()
        || sp.distinct
        || !sp.windows.is_empty()
        || !sp.order_by.is_empty()
        || sp.order_junk != 0
        || sp.offset.unwrap_or(0) != 0
        || matches!(sp.limit, Some(0))
    {
        return Ok(None);
    }
    let AccessPath::PkPoint(parts) = &sp.access else {
        return Ok(None);
    };
    if parts.is_empty() {
        return Ok(None);
    }
    for p in parts {
        match p {
            KeyPart::Param(_) | KeyPart::Const(_) => {}
            KeyPart::OuterCol(_) => return Ok(None),
        }
    }
    for p in &sp.projection {
        if !matches!(p, Projection::Column(_)) {
            return Ok(None);
        }
    }
    let mut col_idxs = Vec::with_capacity(sp.projection.len());
    for p in &sp.projection {
        let Projection::Column(i) = p else {
            unreachable!("filtered above");
        };
        col_idxs.push(*i);
    }
    let columns = pk_point_output_columns(schema, plan, sp)?;
    Ok(Some(PkPointHot {
        table: sp.table,
        col_idxs,
        columns: std::sync::Arc::from(columns),
    }))
}

/// Run the PkPoint micro-executor with precomputed column metadata.
///
/// `sp` is the SelectPlan `hot` was built from, and it is NOT always
/// `plan.stmt`: `exec_select_impl` runs compound arms and lifted subplans
/// through this same path, and for those the top-level `plan.stmt` is a
/// `Compound` — or an `Insert` whose VALUES carry a scalar subquery — while the
/// eligible PkPoint select is nested inside it. Re-deriving `sp` from
/// `plan.stmt` here is what made `SELECT a FROM t WHERE id = 1 UNION …` fail
/// with "pk-point hot needs a Select plan". `plan` is still needed, for its
/// const pool and table ids.
pub(crate) fn exec_pk_point_hot(
    ctx: &mut dyn TxnCtx,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
    hot: &PkPointHot,
) -> Result<ExecResult> {
    let AccessPath::PkPoint(parts) = &sp.access else {
        return Err(internal("pk-point hot needs PkPoint access"));
    };

    // Resolve PK. Common case: one Param — borrow the caller's Value, no clone.
    let owned_pk: Vec<Value>;
    let pk: &[Value] = if parts.len() == 1 {
        match &parts[0] {
            KeyPart::Param(i) => {
                let Some(v) = params.get(*i as usize) else {
                    return Err(internal("key param"));
                };
                std::slice::from_ref(v)
            }
            KeyPart::Const(i) => {
                let Some(v) = plan.consts.get(*i as usize) else {
                    return Err(internal("key const"));
                };
                std::slice::from_ref(v)
            }
            KeyPart::OuterCol(_) => return Err(internal("outer-col in pk-point hot")),
        }
    } else {
        owned_pk = parts
            .iter()
            .map(|p| gather::resolve_part(p, plan, params))
            .collect::<Result<Vec<_>>>()?;
        &owned_pk
    };

    let projected = ctx.get_by_pk_cols(hot.table, pk, &hot.col_idxs)?;
    // One Arc clone + one Vec from Arc — cheaper than rebuilding names from schema.
    let columns: Vec<String> = hot.columns.iter().cloned().collect();
    let rows = match projected {
        None => Vec::new(),
        Some(row) => vec![row],
    };
    Ok(ExecResult::Rows { columns, rows })
}

/// Column names for the PkPoint hot path: single-table, column projections only.
/// Same naming as [`select_output_columns`] for that shape, without join logic.
fn pk_point_output_columns(
    schema: &Schema,
    plan: &CompiledPlan,
    sp: &SelectPlan,
) -> Result<Vec<String>> {
    let t = table_def(schema, plan, sp.table)?;
    let mut cols = Vec::with_capacity(sp.projection.len());
    for p in &sp.projection {
        let Projection::Column(i) = p else {
            return Err(internal("pk-point hot projection"));
        };
        let name = t
            .columns
            .get(*i as usize)
            .map(|c| c.name.clone())
            .ok_or_else(|| internal("projection column name"))?;
        cols.push(name);
    }
    Ok(cols)
}

/// Output column names of one SELECT. A joined slot past the outer's width
/// belongs to an inner table and is named `<table>.<column>` (`id` alone would
/// not say which side); a single-table read keeps plain column names.
fn select_output_columns(schema: &Schema, plan: &CompiledPlan, sp: &SelectPlan) -> Result<Vec<String>> {
    // FROM-less: no table to name columns from. Every projection is an Expr
    // carrying its own name — the binder cannot produce a Column over the
    // zero-column dual row.
    if sp.table == mpedb_sql::DUAL_TABLE {
        return sp
            .projection
            .iter()
            .take(sp.projection.len() - sp.order_junk as usize)
            .map(|p| match p {
                Projection::Expr { name, .. } => Ok(name.clone()),
                Projection::Column(_) => Err(internal("column projection on a FROM-less select")),
            })
            .collect();
    }
    let t = table_def(schema, plan, sp.table)?;
    let joined_tables: Vec<std::borrow::Cow<TableDef>> = if sp.joins.is_empty() {
        vec![t]
    } else {
        let mut v = vec![t];
        for j in &sp.joins {
            v.push(table_def(schema, plan, j.table)?);
        }
        v
    };
    let name_slot = |mut i: usize| -> Result<String> {
        if joined_tables.len() == 1 {
            return joined_tables[0]
                .columns
                .get(i)
                .map(|c| c.name.clone())
                .ok_or_else(|| internal("projection column name"));
        }
        for jt in &joined_tables {
            if i < jt.columns.len() {
                return Ok(format!("{}.{}", jt.name, jt.columns[i].name));
            }
            i -= jt.columns.len();
        }
        Err(internal("projection column name"))
    };
    sp.projection
        .iter()
        .take(sp.projection.len() - sp.order_junk as usize)
        .map(|p| match p {
            Projection::Column(i) => name_slot(*i as usize),
            Projection::Expr { name, .. } => Ok(name.clone()),
        })
        .collect()
}

/// The correlated pipeline: gather UNBOUNDED (a per-row filter downstream
/// means no scan bound and no top-K is sound), then per row — fill each
/// correlated slot by running its subplan with the row's correlation args,
/// apply the post-filter, project, dedup — and only THEN sort/trim/bound.
/// The policies all ran inside the gather, so no subplan ever executes
/// against a row the caller was not allowed to see (the raise contract).
#[allow(clippy::too_many_arguments)]
fn exec_select_with(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
    // First reserved result slot of THIS level (`subplan_base` at the top,
    // `sub.sub_base` for a nested subplan) — where correlated slots are filled.
    base: usize,
    correlated: &[(usize, &SubPlan)],
) -> Result<ExecResult> {
    let SelectPlan {
        table,
        access,
        joins,
        joined_filter,
        post_filter,
        filter,
        projection,
        order_by,
        limit,
        offset,
        aggregate,
        distinct,
        order_over,
        order_junk,
        windows,
    } = sp;
    if aggregate.is_some() {
        // A correlated aggregate is routed to `run_aggregate` from
        // `exec_select_top`; reaching here with one is a routing bug.
        return Err(internal("correlated subplans in an aggregate plan"));
    }
    // The planner refuses windows together with a correlated subquery, so a
    // windowed plan never reaches this correlated path — its window results
    // would be silently dropped here. Reaching it with one is a routing bug.
    if !windows.is_empty() {
        return Err(internal("windows in a correlated select plan"));
    }
    // #125. Unlike the uncorrelated path this narrows the SINGLE-TABLE gather
    // too: `correlated_survivors` keeps a per-row scratch beside every gathered
    // row, so this shape holds the whole input at its widest and the columns
    // the correlation actually names are typically one or two.
    let prune = select_prune(schema, plan, sp, correlated)?;
    let mut rows = if !joins.is_empty() {
        gather_joined(
            ctx,
            plan,
            params,
            schema,
            *table,
            access,
            filter.as_ref(),
            joins,
            joined_filter.as_ref(),
            prune.as_ref(),
        )?
    } else {
        match &prune {
            Some(p) => {
                let t = table_def(schema, plan, *table)?;
                gather::gather_narrowed(
                    ctx,
                    *table,
                    access,
                    filter.as_ref(),
                    plan,
                    params,
                    &t,
                    p.stage(0),
                )?
            }
            None => gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?,
        }
    };
    if *order_over == OrderOver::BaseRow && !order_by.is_empty() {
        gather::check_order_colls(order_by, ctx.host_colls())?;
        sort_rows(&mut rows, order_by, ctx.host_colls());
    }

    // Fill every correlated slot per row and apply the post-filter, keeping each
    // survivor WITH the scratch that produced it — the projection may read a
    // correlated slot (a correlated scalar subquery in the SELECT list), so it
    // is evaluated against that scratch.
    let survivors = correlated_survivors(
        ctx, schema, plan, params, base, rows, correlated, post_filter.as_ref(),
    )?;

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // DISTINCT folds each output column under its declared collation (as in the
    // uncorrelated path above), so `SELECT DISTINCT name` on a NOCASE column
    // deduplicates case-insensitively.
    let distinct_colls = if *distinct {
        output_collations(schema, plan, *table, joins, projection)
    } else {
        Vec::new()
    };
    for (row, scratch) in survivors {
        let mut orow = Vec::with_capacity(projection.len());
        for p in projection {
            orow.push(match p {
                Projection::Column(i) => row
                    .get(*i as usize)
                    .cloned()
                    .ok_or_else(|| internal("projection column"))?,
                Projection::Expr { program, .. } => {
                    program.eval_host(&row, &scratch, ctx.host_fns())?
                }
            });
        }
        if *distinct && !seen.insert(keycode::encode_group_key(&orow, &distinct_colls)) {
            continue;
        }
        out.push(orow);
    }
    if *order_over != OrderOver::BaseRow {
        gather::check_order_colls(order_by, ctx.host_colls())?;
        sort_rows(&mut out, order_by, ctx.host_colls());
        if *order_junk > 0 {
            let keep = projection.len() - *order_junk as usize;
            for row in &mut out {
                row.truncate(keep);
            }
        }
    }
    // The post-filter changed the counts, so LIMIT/OFFSET bound the SURVIVING
    // rows — always applied here, whatever tuple the sort ran over.
    let skip = offset.unwrap_or(0).min(usize::MAX as u64) as usize;
    let take = limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    if skip > 0 || take != usize::MAX {
        out = out.into_iter().skip(skip).take(take).collect();
    }
    let columns = select_output_columns(schema, plan, sp)?;
    Ok(ExecResult::Rows { columns, rows: out })
}

/// Run the aggregate path for one SELECT, threading the per-row correlated
/// pre-filter. Shared by the plain aggregate dispatch ([`exec_select`], empty
/// correlated / no post-filter) and the correlated-aggregate dispatch
/// ([`exec_select_top`]) so the long argument wiring cannot drift.
#[allow(clippy::too_many_arguments)]
fn run_aggregate(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &SelectPlan,
    // First reserved result slot of THIS level — threaded to `correlated_survivors`
    // (unused when `correlated` is empty and `post_filter` is `None`).
    base: usize,
    correlated: &[(usize, &SubPlan)],
    post_filter: Option<&ExprProgram>,
) -> Result<ExecResult> {
    let t = table_def(schema, plan, sp.table)?;
    let agg = sp
        .aggregate
        .as_ref()
        .ok_or_else(|| internal("aggregate dispatch on a non-aggregate plan"))?;
    // #125: an aggregate is the shape whose output requirement is furthest from
    // its input width — `count(*)` observes NO column at all. The
    // materializing paths inside `exec_aggregate` narrow what they hold with
    // this, and the streaming fold (#123) pushes it into the SCAN so an
    // unobserved column is never even decoded (`gather::scan_keep`).
    let prune = select_prune(schema, plan, sp, correlated)?;
    // The parallel fold's shape gate — decided HERE, where the whole
    // SelectPlan is in hand, by the same predicate EXPLAIN prints. The
    // correlated machinery must also be absent: a correlated aggregate's
    // per-row scratch is exactly what the workers do not carry.
    let parallel_shape = correlated.is_empty()
        && post_filter.is_none()
        && mpedb_sql::parallel_fold_shape(sp, schema);
    exec_aggregate(
        ctx,
        plan,
        params,
        schema,
        &t,
        sp.table,
        &sp.access,
        sp.filter.as_ref(),
        &sp.joins,
        sp.joined_filter.as_ref(),
        agg,
        &sp.projection,
        &sp.order_by,
        sp.order_over,
        sp.order_junk,
        sp.limit,
        sp.offset,
        sp.distinct,
        base,
        correlated,
        post_filter,
        prune.as_ref(),
        parallel_shape,
    )
}

/// Per-row correlated pre-filter shared by the plain correlated SELECT
/// ([`exec_select_with`]) and the aggregate path ([`exec_aggregate`]) so the two
/// cannot drift (#73 §1). For each gathered row it fills every correlated
/// subplan slot into a scratch buffer — memoized per subplan by the encoded
/// correlation tuple, so two rows with the SAME tuple run the inner subplan once
/// (MPEE "buy the inner cells once, then only stream probes"; the memo is bounded
/// by the distinct tuples, itself ≤ `rows`, and `MPEDB_NO_SUBPLAN_MEMO=1`
/// restores per-row re-execution for A/B measurement) — then keeps the row iff
/// `post_filter` accepts it.
///
/// Each survivor is returned WITH the scratch that produced it, because a
/// non-aggregate projection may read a correlated slot (a correlated scalar
/// subquery in the SELECT list). The aggregate path discards the scratch:
/// validate and the planner forbid a correlated slot in any grouped program, so
/// grouping there reads `params`.
///
/// A scalar subplan's >1-row error still fires on the first occurrence of a key
/// (the miss path, before any memo insert), so error semantics are
/// byte-identical to per-row re-execution.
#[allow(clippy::too_many_arguments)]
fn correlated_survivors(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    // First reserved result slot of THIS level: `subplan_base` at the top,
    // `sub.sub_base` for a nested subplan. `params[..base]` is `[user ‖ this
    // level's correlation args]` — the prefix a correlated child inherits — and a
    // correlated subplan `i`'s result is written to `scratch[base + i]`.
    base: usize,
    rows: Vec<Vec<Value>>,
    correlated: &[(usize, &SubPlan)],
    post_filter: Option<&ExprProgram>,
) -> Result<Vec<(Vec<Value>, Vec<Value>)>> {
    let n_user = base;
    let mut scratch: Vec<Value> = params.to_vec();
    let mut stack: Vec<Value> = Vec::new();
    let mut memo: Vec<std::collections::HashMap<Vec<u8>, Value>> =
        vec![std::collections::HashMap::new(); correlated.len()];
    let use_memo = std::env::var_os("MPEDB_NO_SUBPLAN_MEMO").is_none();
    // #74: attribute this driver to the (first) correlated subquery's inner
    // table. The inner subplan's own scans additionally charge through the scan
    // layer, so an N-outer × M-inner correlated bomb is counted as ~N·M. A
    // correlated body may be a plain SELECT or (format 56) a whole compound —
    // then the first arm names it; either way the charge must not be skipped.
    let corr_table = correlated.first().and_then(|(_, s)| match &s.body {
        SubBody::Select(sp) => Some(sp.table),
        SubBody::Compound(c) => c.arms.first().map(|a| a.output_select().table),
    });
    let mut out = Vec::new();
    for row in rows {
        // One work-row per outer row this correlated subquery re-evaluates over.
        // Charged BEFORE the memo lookup, so the count is memo- (and
        // `MPEDB_NO_SUBPLAN_MEMO`-) independent and therefore deterministic.
        if let Some(t) = corr_table {
            ctx.charge_work(1, &|| {
                format!("correlated subquery over \"{}\"", table_name(schema, t))
            })?;
        }
        for (ci, &(i, sub)) in correlated.iter().enumerate() {
            let mut key_vals = Vec::with_capacity(sub.outer_args.len());
            for &a in &sub.outer_args {
                key_vals.push(
                    row.get(a as usize)
                        .cloned()
                        .ok_or_else(|| internal("correlation arg out of row"))?,
                );
            }
            // `encode_key_exact`, and neither of the other two encoders: this
            // is a CACHE keyed by the outer row's exact values, and the
            // subquery may distinguish what they merge (`typeof($1)`,
            // `printf`). The grouping key folds `1` and `1.0` on purpose; the
            // ORDERED key drops the mpedb type, so over a typeless (`any`)
            // column it collided the text `'1'` with the blob `x'31'` and the
            // integer `0` with the real `0.0` — and served one's result for the
            // other, which the differential caught as
            // `SELECT id, (SELECT typeof(o.v) FROM m) FROM o` answering "text"
            // where sqlite says "blob".
            let memo_key = keycode::encode_key_exact(&key_vals);
            scratch[base + i] = if let Some(v) = memo[ci].get(&memo_key) {
                v.clone()
            } else {
                let mut inner_params = Vec::with_capacity(n_user + key_vals.len());
                inner_params.extend_from_slice(&params[..n_user]);
                inner_params.extend(key_vals);
                // `inner_params` = `[user ‖ this subplan's correlation args]`,
                // width == `sub.sub_base`; `run_subplan` extends it with the
                // subplan's own (uncorrelated) nested lifts before running it.
                let r = run_subplan(ctx, schema, plan, &inner_params, sub)?;
                let v = subplan_value(r, sub.kind)?;
                if use_memo {
                    memo[ci].insert(memo_key, v.clone());
                }
                v
            };
        }
        let keep = match post_filter {
            Some(pf) => pf.eval_filter_host(&mut stack, &row, &scratch, ctx.host_fns())?,
            None => true,
        };
        if keep {
            out.push((row, scratch.clone()));
        }
    }
    Ok(out)
}

fn exec_stmt_rest(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    partial: &mut bool,
    triggers: &TriggerSet,
    depth: u32,
) -> Result<ExecResult> {
    match &plan.stmt {
        PlanStmt::Select(_)
        | PlanStmt::Compound(_)
        | PlanStmt::RecursiveCte(_)
        | PlanStmt::Derived(_) => {
            unreachable!("handled by exec_stmt_impl")
        }
        PlanStmt::Insert {
            table,
            rows,
            from_select,
            with_check,
            on_conflict,
            returning,
        } => {
            let t = table_def(schema, plan, *table)?;
            // Bind-time `now()`: captured exactly once per execute() call so
            // every DEFAULT now() in a multi-row INSERT gets the same value
            // (reviewed determinism requirement).
            let now = now_micros();
            // Materialize the rows to insert. INSERT … SELECT reads its source
            // FULLY first (so `INSERT INTO t SELECT … FROM t` reads the
            // pre-insert state — sqlite's semantics), then inserts; each source
            // tuple maps to the target columns via `col_map`, omitted columns
            // taking their DEFAULT / NULL.
            let built_rows: Vec<std::borrow::Cow<[Value]>> = if let Some(sel) = from_select {
                let src = match exec_select(ctx, schema, plan, params, &sel.plan)? {
                    ExecResult::Rows { rows, .. } => rows,
                    _ => return Err(internal("INSERT … SELECT source produced no row set")),
                };
                let mut built = Vec::with_capacity(src.len());
                for srow in src {
                    let mut row = Vec::with_capacity(t.columns.len());
                    for (ci, col) in t.columns.iter().enumerate() {
                        row.push(match sel.col_map[ci] {
                            Some(si) => coerce_insert_value(
                                srow.get(si as usize).cloned().unwrap_or(Value::Null),
                                col.ty,
                            ),
                            None => match &col.default {
                                Some(DefaultExpr::Const(v)) => v.clone(),
                                Some(DefaultExpr::Now) => Value::Timestamp(now),
                                None => Value::Null,
                            },
                        });
                    }
                    built.push(std::borrow::Cow::Owned(row));
                }
                built
            } else {
                let mut built = Vec::with_capacity(rows.len());
                for row_spec in rows {
                    built.push(build_insert_row(&t, plan, params, row_spec, now)?);
                }
                built
            };
            // `applied` = rows fully inserted before the current one.
            let mut written = 0u64;
            let mut out: Vec<Vec<Value>> = Vec::new();
            // INTEGER PRIMARY KEY rowid alias (sqlite): a NULL value in the PK
            // column — from an omitted column, an explicit NULL, or a NULL param
            // — auto-assigns `max(rowid)+1`. Resolved here, per row and in order,
            // AFTER earlier rows in the same statement have been inserted, so
            // `INSERT INTO t VALUES(NULL),(NULL)` yields consecutive ids.
            let rowid_col = t.rowid_alias_col();
            // sqlite's STORE-TIME AFFINITY, applied before anything else looks
            // at the row: `'1.50'` into a `decimal(10,2)` column IS the real
            // 1.5, so RLS, triggers, CHECK, uniqueness, the index keys and
            // RETURNING must all see 1.5 and `typeof()` must say `real`. Guarded
            // by `converts_on_store` so a table with no such column never leaves
            // the borrowed zero-copy row (#40).
            let converts = t.converts_on_store();
            let generates = t.has_generated();
            for (applied, mut row) in built_rows.into_iter().enumerate() {
                // The per-ROW guard matters as much as the per-table one now
                // that a shim `text` column carries TEXT affinity (#113): most
                // rows are already in their columns' classes and stay borrowed.
                if converts && t.needs_store_affinity(&row) {
                    t.apply_store_affinity(row.to_mut());
                }
                // GENERATED ALWAYS AS (…): computed HERE, before anything else
                // looks at the row — so RLS WITH CHECK, the BEFORE triggers, the
                // OR REPLACE conflict probes, the index keys, CHECK/NOT NULL and
                // RETURNING all see the value the engine will store. The rowid
                // alias is resolved just below, so a generated column reading it
                // is recomputed there.
                if generates {
                    if let Err(e) = t.apply_generated(row.to_mut(), &[]) {
                        *partial = applied > 0;
                        return Err(e);
                    }
                }
                if let Some(rc) = rowid_col {
                    if row.get(rc as usize).is_some_and(|v| v.is_null()) {
                        let next = ctx.next_rowid(*table, rc)?;
                        row.to_mut()[rc as usize] = Value::Int(next);
                        // The auto-assigned rowid is an input a generated column
                        // may read (`b AS (id * 2)`), and it did not exist on the
                        // pass above. Recompute — `apply_generated` is idempotent.
                        if generates {
                            if let Err(e) = t.apply_generated(row.to_mut(), &[]) {
                                *partial = applied > 0;
                                return Err(e);
                            }
                        }
                    }
                }
                // RLS WITH CHECK on the new row (before the engine's PK/unique
                // pre-checks): NULL and FALSE both reject (§3.7).
                if let Some(wc) = with_check {
                    match wc.eval_filter(&mut Vec::new(), &row, params) {
                        Ok(true) => {}
                        Ok(false) => {
                            *partial = applied > 0;
                            return Err(Error::PolicyViolation { table: t.name.clone() });
                        }
                        Err(e) => {
                            *partial = applied > 0;
                            return Err(e);
                        }
                    }
                }
                // BEFORE INSERT FOR EACH ROW triggers fire before the row is
                // written (DESIGN-TRIGGERS §4.1), NEW = the row about to be
                // inserted (read-only). A failing body may already have written
                // to other tables on the shared txn, so it poisons the statement.
                match fire_insert(ctx, schema, &triggers.before_insert, *table, &row, triggers, depth)
                {
                    Ok(crate::trigger::FireOutcome::Proceed) => {}
                    // RAISE(IGNORE): skip this row's insert and all its
                    // remaining trigger work, silently (sqlite semantics).
                    Ok(crate::trigger::FireOutcome::SkipRow) => continue,
                    Err(e) => {
                        *partial = true;
                        return Err(e);
                    }
                }
                // INSERT OR REPLACE: delete every existing row the proposed row
                // would collide with — on the PK AND on each secondary UNIQUE
                // index — so the insert below cannot trip a uniqueness
                // constraint (sqlite's delete-on-any-unique semantics). All
                // probes read BEFORE any delete; victims are de-duplicated so a
                // row conflicting on several constraints is removed once. A NULL
                // in a probed key means no entry and no conflict (UNIQUE and the
                // rowid-alias auto-assign both permit it), so it is skipped.
                if matches!(on_conflict, PlanOnConflict::Replace) {
                    let mut victims: Vec<Vec<Value>> = Vec::new();
                    let pk_of = |r: &[Value]| -> Vec<Value> {
                        t.primary_key.iter().map(|&c| r[c as usize].clone()).collect()
                    };
                    let pk = pk_of(&row);
                    if !pk.iter().any(|v| v.is_null()) {
                        if let Some(existing) = ctx.get_by_pk(*table, &pk)? {
                            victims.push(pk_of(&existing));
                        }
                    }
                    for (pos, ix) in t.indexes.iter().enumerate() {
                        if !ix.unique {
                            continue;
                        }
                        let vals: Vec<Value> =
                            ix.columns.iter().map(|&c| row[c as usize].clone()).collect();
                        if vals.iter().any(|v| v.is_null()) {
                            continue;
                        }
                        if let Some(existing) =
                            ctx.get_by_index(*table, (pos + 1) as u32, &vals)?
                        {
                            victims.push(pk_of(&existing));
                        }
                    }
                    let mut deleted: Vec<Vec<Value>> = Vec::new();
                    for v in victims {
                        if deleted.contains(&v) {
                            continue;
                        }
                        ctx.delete_by_pk(*table, &v)?;
                        deleted.push(v);
                    }
                }
                match ctx.insert_row(*table, &row) {
                    Ok(()) => {
                        written += 1;
                        // Surface the assigned/used rowid for the C-API's
                        // sqlite3_last_insert_rowid (facade hook). Only rowid-
                        // alias tables have a last-insert-rowid in sqlite; the
                        // last inserted row of the statement wins.
                        if let Some(rc) = rowid_col {
                            if let Some(Value::Int(id)) = row.get(rc as usize) {
                                record_last_insert_rowid(*id);
                            }
                        }
                        if let Some(proj) = returning {
                            out.push(project_row(proj, &row, params, ctx.host_fns())?);
                        }
                        // AFTER INSERT FOR EACH ROW triggers fire on the row just
                        // written, on the SAME txn (DESIGN-TRIGGERS §4.1/§4.3). A
                        // failing trigger poisons the statement: the row landed and
                        // the body may have written before it raised.
                        // A SkipRow here only abandons remaining trigger work —
                        // the row is already written and stays counted.
                        if let Err(e) =
                            fire_insert(ctx, schema, &triggers.after_insert, *table, &row, triggers, depth)
                        {
                            *partial = true;
                            return Err(e);
                        }
                    }
                    Err(e) if is_uniqueness(&e) && !matches!(on_conflict, PlanOnConflict::Error) => {
                        // ON CONFLICT covers uniqueness ONLY. A CHECK or
                        // NOT NULL violation is NOT a conflict and must still
                        // fail — PostgreSQL draws the same line, and swallowing
                        // them would turn `DO NOTHING` into "ignore my
                        // constraints", which is the opposite of the point.
                        match on_conflict {
                            PlanOnConflict::Error => unreachable!("guarded above"),
                            PlanOnConflict::DoNothing => { /* skip this row */ }
                            PlanOnConflict::Replace => {
                                // Replace pre-deletes every conflicting row above,
                                // so a uniqueness error here means a constraint we
                                // did not probe (should not happen) — surface it
                                // rather than silently swallow.
                                *partial = applied > 0 || !precheck_failure(&e);
                                return Err(hide_constraint_variant(
                                    e,
                                    &t.name,
                                    with_check.is_some(),
                                ));
                            }
                            PlanOnConflict::DoUpdate {
                                target,
                                probe,
                                set,
                                filter,
                            } => {
                                // Find the row this collided with, BY THE KEY
                                // THE CALLER NAMED. Probing by anything else
                                // would update a row they did not ask about.
                                let found = match probe {
                                    ConflictProbe::Pk => {
                                        let pk: Vec<Value> = target
                                            .iter()
                                            .map(|c| row[*c as usize].clone())
                                            .collect();
                                        ctx.get_by_pk(*table, &pk)?
                                    }
                                    ConflictProbe::Index(ino) => {
                                        // Probe values in the INDEX's column
                                        // order — a composite target's list
                                        // order may differ (#55).
                                        let cols = &t
                                            .indexes
                                            .get(*ino as usize - 1)
                                            .ok_or_else(|| {
                                                Error::Internal(
                                                    "conflict probe index out of range".into(),
                                                )
                                            })?
                                            .columns;
                                        let vals: Vec<Value> = cols
                                            .iter()
                                            .map(|&c| row[c as usize].clone())
                                            .collect();
                                        // UNIQUE permits many NULLs, so any
                                        // NULL here cannot have collided with
                                        // anything and there is no row to find.
                                        if vals.iter().any(|v| v.is_null()) {
                                            None
                                        } else {
                                            ctx.get_by_index(*table, *ino, &vals)?
                                        }
                                    }
                                };
                                let Some(existing) = found else {
                                    // The insert failed on SOME uniqueness
                                    // constraint, but not the one named: a
                                    // PK-target insert that tripped a secondary
                                    // UNIQUE, or an email-target insert that
                                    // tripped the PK. That conflict is not the
                                    // one the caller asked to handle, so it is
                                    // an error -- exactly as in PostgreSQL, and
                                    // the alternative (silently doing nothing)
                                    // would hide a real collision.
                                    *partial = applied > 0 || !precheck_failure(&e);
                                    return Err(hide_constraint_variant(
                                        e,
                                        &t.name,
                                        with_check.is_some(),
                                    ));
                                };
                                // SET/WHERE see [existing ‖ proposed]: that is
                                // what `excluded.<c>` = Col(n + i) resolves to.
                                let mut both = existing.clone();
                                both.extend_from_slice(&row);
                                if let Some(f) = filter {
                                    match f.eval_filter_host(
                                        &mut Vec::new(),
                                        &both,
                                        params,
                                        ctx.host_fns(),
                                    ) {
                                        Ok(true) => {}
                                        // NULL and FALSE both skip: SQL needs
                                        // exactly TRUE to act.
                                        Ok(false) => continue,
                                        Err(e) => {
                                            *partial = applied > 0;
                                            return Err(e);
                                        }
                                    }
                                }
                                let mut new_row = existing;
                                for (c, program) in set {
                                    let v = program.eval_host(&both, params, ctx.host_fns())?;
                                    new_row[*c as usize] = v;
                                }
                                // DO UPDATE assigns into the column like any
                                // other write, so the same store-time affinity —
                                // and the generated columns are recomputed from
                                // the post-image, exactly as on a plain UPDATE.
                                t.apply_store_affinity(&mut new_row);
                                if generates {
                                    if let Err(e) = t.apply_generated(&mut new_row, &[]) {
                                        *partial = applied > 0;
                                        return Err(e);
                                    }
                                }
                                if let Err(e) = ctx.update_by_pk(*table, &new_row) {
                                    *partial = applied > 0 || !precheck_failure(&e);
                                    return Err(hide_constraint_variant(
                                        e,
                                        &t.name,
                                        with_check.is_some(),
                                    ));
                                }
                                written += 1;
                                if let Some(proj) = returning {
                                    out.push(project_row(proj, &new_row, params, ctx.host_fns())?);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // A pre-check failure left even this row unapplied, so
                        // the statement is partial only if earlier rows landed.
                        // NOTE the order: `partial` is decided from the ORIGINAL
                        // error, then the variant is hidden (§6.5).
                        *partial = applied > 0 || !precheck_failure(&e);
                        return Err(hide_constraint_variant(e, &t.name, with_check.is_some()));
                    }
                }
            }
            match returning {
                Some(proj) => Ok(ExecResult::Rows {
                    columns: projection_names(proj, &t),
                    rows: out,
                }),
                None => Ok(ExecResult::Affected(written)),
            }
        }

        PlanStmt::Update {
            table,
            access,
            filter,
            set,
            with_check,
            returning,
        } => {
            let t = table_def(schema, plan, *table)?;
            // Collect-then-mutate: gather the matching CURRENT rows first
            // (read-only; a failure here has no effects).
            let old_rows = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
            // The UPDATE's SET target columns — an `UPDATE OF <cols>` trigger
            // fires only when one of its columns is among these (sqlite
            // semantics). Statement-wide, so computed once.
            let changed: Vec<u16> = set.iter().map(|(c, _)| *c).collect();
            let mut affected = 0u64;
            let mut out: Vec<Vec<Value>> = Vec::new();
            for old in &old_rows {
                let new_row = (|| -> Result<Vec<Value>> {
                    let mut new_row = old.clone();
                    for (c, program) in set {
                        // SQL semantics: ALL set-expressions evaluate against
                        // the OLD row, not against earlier assignments.
                        let slot = new_row
                            .get_mut(*c as usize)
                            .ok_or_else(|| internal("SET column"))?;
                        *slot = program.eval_host(old, params, ctx.host_fns())?;
                    }
                    // The assigned values enter the column exactly as an
                    // INSERT's do, so they take the same store-time affinity
                    // (sqlite applies it to `UPDATE … SET` too) — before the
                    // WITH CHECK, the triggers and RETURNING below see the row.
                    t.apply_store_affinity(&mut new_row);
                    // Generated columns are recomputed from the POST-image: a
                    // SET on one of their inputs changes them, which is why
                    // `UPDATE … SET <generated> = …` is refused at bind time —
                    // the expression is the only source of the value.
                    if t.has_generated() {
                        t.apply_generated(&mut new_row, &[])?;
                    }
                    Ok(new_row)
                })();
                let new_row = match new_row {
                    Ok(r) => r,
                    Err(e) => {
                        // Evaluation is side-effect-free; only rows already
                        // updated count.
                        *partial = affected > 0;
                        return Err(e);
                    }
                };
                // RLS WITH CHECK on the post-image (NULL and FALSE reject, §3.7).
                if let Some(wc) = with_check {
                    match wc.eval_filter(&mut Vec::new(), &new_row, params) {
                        Ok(true) => {}
                        Ok(false) => {
                            *partial = affected > 0;
                            return Err(Error::PolicyViolation { table: t.name.clone() });
                        }
                        Err(e) => {
                            *partial = affected > 0;
                            return Err(e);
                        }
                    }
                }
                // BEFORE UPDATE FOR EACH ROW triggers fire before the row is
                // rewritten (DESIGN-TRIGGERS §4.1): NEW = the post-image (read-
                // only), OLD = the pre-image. A failing body poisons the statement.
                match fire_update(
                    ctx,
                    schema,
                    &triggers.before_update,
                    *table,
                    &new_row,
                    old,
                    &changed,
                    triggers,
                    depth,
                ) {
                    Ok(crate::trigger::FireOutcome::Proceed) => {}
                    // RAISE(IGNORE): leave the row as it was, silently.
                    Ok(crate::trigger::FireOutcome::SkipRow) => continue,
                    Err(e) => {
                        *partial = true;
                        return Err(e);
                    }
                }
                match ctx.update_by_pk(*table, &new_row) {
                    Ok(true) => {
                        affected += 1;
                        // RETURNING on UPDATE projects the POST-image: SQL
                        // returns the row as it now is, not as it was.
                        if let Some(proj) = returning {
                            out.push(project_row(proj, &new_row, params, ctx.host_fns())?);
                        }
                        // AFTER UPDATE FOR EACH ROW triggers fire on the updated
                        // row, on the SAME txn (DESIGN-TRIGGERS §4.1): NEW = the
                        // post-image, OLD = the pre-image. A failing trigger
                        // poisons the statement — the row changed and the body may
                        // have written before it raised.
                        // SkipRow here only abandons remaining trigger work —
                        // the row is already rewritten and stays counted.
                        if let Err(e) = fire_update(
                            ctx,
                            schema,
                            &triggers.after_update,
                            *table,
                            &new_row,
                            old,
                            &changed,
                            triggers,
                            depth,
                        ) {
                            *partial = true;
                            return Err(e);
                        }
                    }
                    Ok(false) => {} // row vanished: nothing changed
                    Err(e) => {
                        // `partial` from the original variant, then hide it (§6.5).
                        *partial = affected > 0 || !precheck_failure(&e);
                        return Err(hide_constraint_variant(e, &t.name, with_check.is_some()));
                    }
                }
            }
            match returning {
                Some(proj) => Ok(ExecResult::Rows {
                    columns: projection_names(proj, &t),
                    rows: out,
                }),
                None => Ok(ExecResult::Affected(affected)),
            }
        }

        PlanStmt::Delete {
            table,
            access,
            filter,
            returning,
        } => {
            let t = table_def(schema, plan, *table)?;
            // Gather full old rows (the residual filter needs them), then
            // delete by PK values extracted from each row.
            let old_rows = gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?;
            let mut affected = 0u64;
            let mut out: Vec<Vec<Value>> = Vec::new();
            for old in &old_rows {
                let mut pk = Vec::with_capacity(t.primary_key.len());
                for &i in &t.primary_key {
                    let v = match old.get(i as usize) {
                        Some(v) => v.clone(),
                        None => {
                            *partial = affected > 0;
                            return Err(internal("PK column"));
                        }
                    };
                    pk.push(v);
                }
                // BEFORE DELETE FOR EACH ROW triggers fire before the row is
                // removed (DESIGN-TRIGGERS §4.1): only OLD is available. A failing
                // body poisons the statement.
                match fire_delete(ctx, schema, &triggers.before_delete, *table, old, triggers, depth)
                {
                    Ok(crate::trigger::FireOutcome::Proceed) => {}
                    // RAISE(IGNORE): keep the row, silently.
                    Ok(crate::trigger::FireOutcome::SkipRow) => continue,
                    Err(e) => {
                        *partial = true;
                        return Err(e);
                    }
                }
                match ctx.delete_by_pk(*table, &pk) {
                    Ok(true) => {
                        affected += 1;
                        // RETURNING on DELETE projects the row as it WAS: there
                        // is no post-image to show.
                        if let Some(proj) = returning {
                            out.push(project_row(proj, old, params, ctx.host_fns())?);
                        }
                        // AFTER DELETE FOR EACH ROW triggers fire on the deleted
                        // row, on the SAME txn (DESIGN-TRIGGERS §4.1): only OLD is
                        // available. A failing trigger poisons the statement.
                        // SkipRow here only abandons remaining trigger work —
                        // the row is already gone and stays counted.
                        if let Err(e) =
                            fire_delete(ctx, schema, &triggers.after_delete, *table, old, triggers, depth)
                        {
                            *partial = true;
                            return Err(e);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        // delete_by_pk has no pre-check failure class: any
                        // error may have fired mid index maintenance.
                        *partial = true;
                        return Err(e);
                    }
                }
            }
            match returning {
                Some(proj) => Ok(ExecResult::Rows {
                    columns: projection_names(proj, &t),
                    rows: out,
                }),
                None => Ok(ExecResult::Affected(affected)),
            }
        }

        PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => Err(Error::Unsupported(
            "transaction control cannot be executed as a plan; \
             use Database::begin() and WriteSession::commit()/rollback()"
                .into(),
        )),
        PlanStmt::Savepoint(_) | PlanStmt::Release(_) | PlanStmt::RollbackTo(_) => {
            Err(Error::Unsupported(
                "SAVEPOINT/RELEASE/ROLLBACK TO are handled by the write session, \
                 not executed as a plan; run them through WriteSession::query()"
                    .into(),
            ))
        }
    }
}

/// Fire `INSERT` triggers of one timing on `table` for one inserted row (only
/// `NEW` in scope). See [`fire_row_triggers`].
fn fire_insert(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    bucket: &std::collections::HashMap<u32, Vec<CompiledTrigger>>,
    table: u32,
    new_row: &[Value],
    triggers: &TriggerSet,
    depth: u32,
) -> Result<crate::trigger::FireOutcome> {
    match bucket.get(&table) {
        Some(trigs) => fire_row_triggers(ctx, schema, trigs, Some(new_row), None, &[], triggers, depth),
        None => Ok(crate::trigger::FireOutcome::Proceed),
    }
}

/// Fire `UPDATE` triggers of one timing on `table` for one updated row: `NEW` =
/// the post-image, `OLD` = the pre-image (DESIGN-TRIGGERS §4.1). `changed` names
/// the columns the UPDATE assigned (its SET target list) — an `UPDATE OF <cols>`
/// trigger fires only when one of its columns is among them. See
/// [`fire_row_triggers`].
#[allow(clippy::too_many_arguments)]
fn fire_update(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    bucket: &std::collections::HashMap<u32, Vec<CompiledTrigger>>,
    table: u32,
    new_row: &[Value],
    old_row: &[Value],
    changed: &[u16],
    triggers: &TriggerSet,
    depth: u32,
) -> Result<crate::trigger::FireOutcome> {
    match bucket.get(&table) {
        Some(trigs) => {
            fire_row_triggers(ctx, schema, trigs, Some(new_row), Some(old_row), changed, triggers, depth)
        }
        None => Ok(crate::trigger::FireOutcome::Proceed),
    }
}

/// Fire `DELETE` triggers of one timing on `table` for one deleted row (only
/// `OLD` in scope, the deleted row). See [`fire_row_triggers`].
fn fire_delete(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    bucket: &std::collections::HashMap<u32, Vec<CompiledTrigger>>,
    table: u32,
    old_row: &[Value],
    triggers: &TriggerSet,
    depth: u32,
) -> Result<crate::trigger::FireOutcome> {
    match bucket.get(&table) {
        Some(trigs) => fire_row_triggers(ctx, schema, trigs, None, Some(old_row), &[], triggers, depth),
        None => Ok(crate::trigger::FireOutcome::Proceed),
    }
}

/// Fire a set of matching `… FOR EACH ROW` triggers for one changed row, on the
/// SAME `ctx` (DESIGN-TRIGGERS §4). `UPDATE OF <cols>` triggers are skipped
/// unless one of their columns is in `changed` (the UPDATE's SET target list;
/// empty for INSERT/DELETE, where `update_of` is always empty too). Each
/// trigger's optional `WHEN` is a 3VL gate (only TRUE fires; NULL and FALSE
/// skip); the body is a SEQUENCE of ordinary plans, each whose leading
/// parameters are the `NEW`/`OLD` columns named by its row-slot map, filled from
/// the `new`/`old` images and executed in body order by recursing on the held
/// txn at `depth + 1` — never through the facade, so the writer lock and intent
/// ring are never re-entered. A hard depth cap bounds any cascade.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_row_triggers(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    trigs: &[CompiledTrigger],
    new: Option<&[Value]>,
    old: Option<&[Value]>,
    changed: &[u16],
    triggers: &TriggerSet,
    depth: u32,
) -> Result<crate::trigger::FireOutcome> {
    if trigs.is_empty() {
        return Ok(crate::trigger::FireOutcome::Proceed);
    }
    if depth + 1 > MAX_TRIGGER_DEPTH {
        return Err(Error::Unsupported(format!(
            "trigger recursion too deep (> {MAX_TRIGGER_DEPTH} levels)"
        )));
    }
    // Fill a row-slot map from the NEW/OLD images. A slot naming a side not
    // present for this event is an internal bug (the binder only emits slots the
    // event allows), so it fails closed rather than mis-binding.
    let pick = |map: &RowMap| -> Result<Vec<Value>> {
        map.iter()
            .map(|&(side, c)| {
                let row = match side {
                    RowSide::New => new,
                    RowSide::Old => old,
                };
                row.and_then(|r| r.get(c as usize).cloned())
                    .ok_or_else(|| internal("trigger NEW/OLD column out of row bounds"))
            })
            .collect()
    };
    for trig in trigs {
        // `UPDATE OF <cols>`: fire only when one named column is assigned by the
        // UPDATE (sqlite semantics — the SET target list, not a value change).
        if !trig.update_of.is_empty() && !trig.update_of.iter().any(|c| changed.contains(c)) {
            continue;
        }
        // `recursive_triggers` OFF (the default, sqlite's): a trigger that is
        // already ACTIVE in this cascade — its body is what (directly or via a
        // cycle) caused this fire — is not re-entered. This is what quietly
        // stops `AFTER INSERT ON t … INSERT INTO t` after one round instead of
        // erroring at the depth cap.
        if !triggers.recursive && trigger_is_active(&trig.name) {
            continue;
        }
        if let Some((prog, when_map)) = &trig.when {
            let wp = pick(when_map)?;
            let mut stack = Vec::new();
            if !prog.eval_filter(&mut stack, &[], &wp)? {
                continue;
            }
        }
        // The #74 work meter charges one row per (trigger, row) FIRE: the
        // depth cap bounds how DEEP a cascade goes, this bounds how WIDE —
        // an exponential fan-out trips `RuntimeBudget` at a fixed, repeatable
        // count instead of running 2^depth statements.
        ctx.charge_work(1, &|| format!("trigger \"{}\"", trig.name))?;
        let _active = ActiveTrigger::enter(&trig.name);
        match &trig.body {
            // Multi-statement body: each statement runs in order on the same txn.
            crate::trigger::TriggerBody::Sql(stmts) => {
                for stmt in stmts {
                    match stmt {
                        mpedb_sql::TriggerStmt::Dml(body_plan, body_map) => {
                            let body_params = pick(body_map)?;
                            let mut inner_partial = false;
                            exec_stmt_triggered(
                                ctx,
                                schema,
                                body_plan,
                                &body_params,
                                &mut inner_partial,
                                triggers,
                                depth + 1,
                            )?;
                        }
                        // `SELECT RAISE(…) [WHERE …]` (DESIGN-TRIGGERS §4.3):
                        // the gate is 3VL like WHEN — only TRUE raises.
                        mpedb_sql::TriggerStmt::Raise { kind, msg, gate } => {
                            if let Some((prog, gate_map)) = gate {
                                let gp = pick(gate_map)?;
                                let mut stack = Vec::new();
                                if !prog.eval_filter(&mut stack, &[], &gp)? {
                                    continue;
                                }
                            }
                            match kind {
                                mpedb_sql::TriggerRaise::Abort => {
                                    return Err(Error::Raise(msg.clone()));
                                }
                                // sqlite: IGNORE abandons the remainder of THIS
                                // trigger program, the row operation, and every
                                // subsequent trigger program for the row.
                                mpedb_sql::TriggerRaise::Ignore => {
                                    return Ok(crate::trigger::FireOutcome::SkipRow);
                                }
                            }
                        }
                    }
                }
            }
            // PySpell body (DESIGN-TRIGGERS §5): evaluate the argument
            // programs over the row images, then run the pinned procedure's IR
            // on THIS ctx through the bridge — its embedded statements recurse
            // like an SQL body's, never through the facade.
            crate::trigger::TriggerBody::Spell(sb) => {
                let ready = sb.ready.as_ref().map_err(|m| {
                    Error::Unsupported(format!("trigger `{}`: {m}", trig.name))
                })?;
                let mut args = Vec::with_capacity(sb.args.len());
                let mut stack = Vec::new();
                for (prog, arg_map) in &sb.args {
                    let slots = pick(arg_map)?;
                    args.push(prog.eval_with_stack(&mut stack, &[], &slots)?);
                }
                let mut bridge = CtxBridge {
                    ctx,
                    schema,
                    plans: &ready.plans,
                    triggers,
                    depth,
                    streams: Vec::new(),
                };
                mpedb_spell::interp::run(
                    &ready.proc,
                    &args,
                    &mut bridge,
                    crate::trigger::TRIGGER_BUDGET,
                )?;
            }
        }
    }
    Ok(crate::trigger::FireOutcome::Proceed)
}

std::thread_local! {
    /// Names of the triggers whose bodies are executing on THIS thread's
    /// statement cascade, innermost last. A statement executes synchronously
    /// on one thread and nested fires recurse on the same one, so a
    /// thread-local stack IS the cascade's activation record — no signature
    /// threading through `exec_stmt_triggered`'s many callers.
    static ACTIVE_TRIGGERS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn trigger_is_active(name: &str) -> bool {
    ACTIVE_TRIGGERS.with(|a| a.borrow().iter().any(|n| n == name))
}

/// RAII activation record: pushed around a trigger's body execution, popped on
/// drop — including the `?`-unwind paths, so an aborting body can never leave
/// its name stuck "active" for the session's next statement.
struct ActiveTrigger;

impl ActiveTrigger {
    fn enter(name: &str) -> ActiveTrigger {
        ACTIVE_TRIGGERS.with(|a| a.borrow_mut().push(name.to_string()));
        ActiveTrigger
    }
}

impl Drop for ActiveTrigger {
    fn drop(&mut self) {
        ACTIVE_TRIGGERS.with(|a| {
            a.borrow_mut().pop();
        });
    }
}

/// [`DbBridge`](mpedb_spell::interp::DbBridge) over the LIVE transaction a
/// trigger fires inside (DESIGN-TRIGGERS §5.2): each embedded statement was
/// pre-resolved by hash at catalog build, and runs here by recursing on the
/// same `ctx` at `depth + 1` — the procedure sees the triggering statement's
/// uncommitted writes and unwinds with it, and its own DML fires nested
/// triggers under the same depth cap. Cursors materialize their result on
/// open (the k-row streaming path needs a reader slot a held write txn cannot
/// nest), which preserves semantics exactly — the interpreter's row budget
/// still meters consumption.
struct CtxBridge<'a> {
    ctx: &'a mut dyn TxnCtx,
    schema: &'a Schema,
    plans: &'a std::collections::HashMap<[u8; 32], Arc<CompiledPlan>>,
    triggers: &'a TriggerSet,
    depth: u32,
    streams: Vec<Option<std::vec::IntoIter<Vec<Value>>>>,
}

impl CtxBridge<'_> {
    fn run_plan(
        &mut self,
        plan_ref: &mpedb_spell::ir::PlanRef,
        params: &[Value],
    ) -> Result<ExecResult> {
        let plan = self.plans.get(&plan_ref.hash.0).ok_or_else(|| {
            internal("trigger procedure references an unresolved plan (catalog-build bug)")
        })?;
        // Rebuild the full parameter buffer the way `session::resolve_params`
        // does, minus the session: user params, NULL holes for subplan slots
        // (the executor fills them), and the statement instant for a literal
        // `'now'`. Any other context key was refused at catalog build.
        let n_ctx = plan.context_keys.len();
        let n_sub = plan.n_subplan_slots() as usize;
        let n_user = plan.n_params as usize - n_ctx - n_sub;
        if params.len() != n_user {
            return Err(Error::WrongParamCount {
                expected: n_user,
                got: params.len(),
            });
        }
        let plan = plan.clone();
        let mut full = Vec::with_capacity(plan.n_params as usize);
        full.extend_from_slice(params);
        full.resize(n_user + n_sub, Value::Null);
        for key in &plan.context_keys {
            if key == mpedb_sql::STATEMENT_INSTANT_KEY {
                full.push(Value::Text(mpedb_types::sqlite_now_string(now_micros())));
            } else {
                return Err(Error::Unsupported(format!(
                    "current_setting('{key}') needs a session and is not \
                     available inside a trigger"
                )));
            }
        }
        let mut inner_partial = false;
        exec_stmt_triggered(
            self.ctx,
            self.schema,
            &plan,
            &full,
            &mut inner_partial,
            self.triggers,
            self.depth + 1,
        )
    }
}

impl mpedb_spell::interp::DbBridge for CtxBridge<'_> {
    fn query(
        &mut self,
        plan: &mpedb_spell::ir::PlanRef,
        params: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        match self.run_plan(plan, params)? {
            ExecResult::Rows { rows, .. } => Ok(rows),
            other => Err(internal(&format!(
                "trigger procedure query returned {other:?} (validator bug)"
            ))),
        }
    }

    fn exec(&mut self, plan: &mpedb_spell::ir::PlanRef, params: &[Value]) -> Result<u64> {
        match self.run_plan(plan, params)? {
            ExecResult::Affected(n) => Ok(n),
            // RETURNING inside a procedure's exec: row count is the answer.
            ExecResult::Rows { rows, .. } => Ok(rows.len() as u64),
            other => Err(internal(&format!(
                "trigger procedure exec returned {other:?} (validator bug)"
            ))),
        }
    }

    fn cursor_open(
        &mut self,
        plan: &mpedb_spell::ir::PlanRef,
        params: &[Value],
    ) -> Result<u32> {
        let rows = self.query(plan, params)?;
        let slot = self
            .streams
            .iter()
            .position(|s| s.is_none())
            .unwrap_or(self.streams.len());
        if slot == self.streams.len() {
            self.streams.push(None);
        }
        self.streams[slot] = Some(rows.into_iter());
        Ok(slot as u32)
    }

    fn cursor_advance(&mut self, stream: u32) -> Result<Option<Vec<Value>>> {
        let slot = self
            .streams
            .get_mut(stream as usize)
            .ok_or_else(|| internal("trigger procedure advanced an unknown cursor"))?;
        let Some(it) = slot else {
            return Ok(None);
        };
        let row = it.next();
        if row.is_none() {
            *slot = None;
        }
        Ok(row)
    }
}

/// Project one written row through a `RETURNING` clause.
///
/// `host` carries the connection's host UDF closures (design/DESIGN-UDF.md);
/// `RETURNING plus1(x)` is a write-path expression like any other and resolves
/// them exactly as a SELECT list would.
fn project_row(
    proj: &[Projection],
    row: &[Value],
    params: &[Value],
    host: Option<&dyn HostFns>,
) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(proj.len());
    for p in proj {
        out.push(match p {
            Projection::Column(i) => row
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| internal("RETURNING column out of row bounds"))?,
            Projection::Expr { program, .. } => program.eval_host(row, params, host)?,
        });
    }
    Ok(out)
}

/// Output column names for a `RETURNING` clause.
fn projection_names(proj: &[Projection], t: &TableDef) -> Vec<String> {
    proj.iter()
        .map(|p| match p {
            Projection::Column(i) => t
                .columns
                .get(*i as usize)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "?".to_string()),
            Projection::Expr { name, .. } => name.clone(),
        })
        .collect()
}

/// Does this error mean "a uniqueness constraint said no"?
///
/// `ON CONFLICT` covers uniqueness ONLY — PostgreSQL is explicit about that,
/// and it matters: if a CHECK or NOT NULL violation counted as a conflict,
/// `DO NOTHING` would quietly mean "ignore my constraints" and the rows you
/// thought you validated would just be missing.
fn is_uniqueness(e: &Error) -> bool {
    matches!(
        e,
        Error::PrimaryKeyViolation { .. } | Error::UniqueViolation { .. }
    )
}

/// Resolve one INSERT row spec (params/consts/defaults) to concrete values.
/// Pure: touches no transaction state.
fn build_insert_row<'a>(
    t: &TableDef,
    plan: &CompiledPlan,
    params: &'a [Value],
    row_spec: &[InsertSource],
    now: i64,
) -> Result<std::borrow::Cow<'a, [Value]>> {
    // #40 instrument: this is per ROW, so the timing only exists under the
    // leakstat feature — an unconditional Instant here would tax bulk loads.
    #[cfg(feature = "leakstat")]
    {
        let t0 = std::time::Instant::now();
        let r = build_insert_row_impl(t, plan, params, row_spec, now);
        mpedb_core::engine::leakstat::add(
            &mpedb_core::engine::leakstat::EXEC_NS_BUILDROW,
            t0.elapsed().as_nanos() as u64,
        );
        r
    }
    #[cfg(not(feature = "leakstat"))]
    build_insert_row_impl(t, plan, params, row_spec, now)
}

fn build_insert_row_impl<'a>(
    t: &TableDef,
    plan: &CompiledPlan,
    params: &'a [Value],
    row_spec: &[InsertSource],
    now: i64,
) -> Result<std::borrow::Cow<'a, [Value]>> {
    // The identity fast path: the common single-row INSERT where every column
    // comes straight from the caller's params, in declaration order — borrow
    // instead of cloning. This was the THIRD full deep-clone of a blob on its
    // way in (#40: ~2.3 ms of a warm 16 MiB insert, measured 2026-07-16 with
    // blob_warm --features leakstat). Any Default/Const/now() or reordered
    // spec takes the owned path below, so default resolution and the
    // partial-effects semantics of multi-row INSERT are untouched.
    if row_spec.len() == params.len()
        && row_spec
            .iter()
            .enumerate()
            .all(|(ci, s)| matches!(s, InsertSource::Param(i) if *i as usize == ci))
    {
        return Ok(std::borrow::Cow::Borrowed(params));
    }
    let mut row = Vec::with_capacity(row_spec.len());
    for (ci, src) in row_spec.iter().enumerate() {
        row.push(match src {
            InsertSource::Param(i) => params
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| internal("insert param"))?,
            InsertSource::Const(i) => plan
                .consts
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| internal("insert const"))?,
            InsertSource::Default => {
                let col = t.columns.get(ci).ok_or_else(|| internal("insert col"))?;
                match &col.default {
                    Some(DefaultExpr::Const(v)) => v.clone(),
                    Some(DefaultExpr::Now) => Value::Timestamp(now),
                    None => Value::Null, // plan-validated: column is nullable
                }
            }
            InsertSource::Expr(prog) => {
                // Dual row: empty tuple. Program carries its own const pool.
                prog.eval(&[], params)?
            }
        });
    }
    Ok(std::borrow::Cow::Owned(row))
}

/// Coerce one `INSERT … SELECT` source value toward the target column type.
/// Only the loss-less integer→float widening is applied (the same the VALUES
/// path does at plan time via `coerce_const`); everything else passes through
/// and the engine's `validate_row` enforces the rigid type at write time.
fn coerce_insert_value(v: Value, ty: mpedb_types::ColumnType) -> Value {
    match (&v, ty) {
        (Value::Int(i), mpedb_types::ColumnType::Float64) => Value::Float(*i as f64),
        _ => v,
    }
}

/// Validate bound parameters against the plan's inferred types, applying the
/// implicit conversions that are **provably lossless** for the value at hand.
///
/// # bool ⇄ int64
///
/// CPython's `sqlite3` binds Python `True`/`False` through `sqlite3_bind_int64`
/// as 1/0, and Django does exactly that for every `BooleanField` lookup — so a
/// slot the binder pinned to `Bool` is handed an `Int`. 0 and 1 convert, since
/// that IS sqlite's representation of a boolean. Any other integer is REFUSED
/// rather than truthy-tested: mpedb's rigid `Bool` cannot hold it, and sqlite
/// would have stored and returned the integer itself. The reverse — a real
/// `Bool` in an int64 slot, which the Rust/Python SDKs can produce — is always
/// exact (`TRUE` -> 1).
///
/// # int64 ⇄ float64 (task #74)
///
/// The same shape, one level up. sqlite has no parameter types at all: a
/// `sqlite3_bind_int64(1)` against `WHERE real_col > ?` is compared numerically
/// against the real column, and a `sqlite3_bind_double(1.0)` into an INTEGER
/// column is stored as the integer 1 by INTEGER affinity. mpedb infers a type
/// per slot instead (`WHERE r > ?` pins `$1` to `float64`), so the driver's
/// choice of bind function — which for Django/CPython follows the *Python*
/// value's type, not the column's — decided whether the statement ran.
///
/// Bridging at BIND, like the bool case, rather than widening the type lattice:
/// the lattice is what makes a plan's operand types static, and `unify_operands`
/// already inserts a `ToFloat` for a genuinely mixed *expression*. What was
/// missing is only that a bound scalar cannot carry its own coercion.
///
/// **Both directions convert only when the round trip is exact**, and the
/// inexact cases are refused by name rather than rounded:
///
/// * `Int -> Float`: refused above 2^53-ish magnitudes, where `n as f64` is no
///   longer `n`. sqlite compares an integer against a real EXACTLY
///   (`sqlite3IntFloatCompare`), so rounding the parameter first could flip a
///   `>` on a large key — a wrong answer, not a wider one.
/// * `Float -> Int`: refused for a non-integral value (`1.5`) and for anything
///   outside the i64 range. Truncating would answer `i > 1.5` as `i > 1` (or
///   `i > 2`), and storing it would silently lose the fraction. sqlite's own
///   INTEGER affinity converts a real only when it is losslessly integral, and
///   this is that rule.
///
/// Returns `Cow::Borrowed` (no copy) whenever nothing needed converting, which
/// is every statement whose parameters already match.
pub(crate) fn coerce_params<'a>(
    plan: &CompiledPlan,
    params: &'a [Value],
) -> Result<std::borrow::Cow<'a, [Value]>> {
    use std::borrow::Cow;
    if params.len() != plan.n_params as usize {
        return Err(Error::WrongParamCount {
            expected: plan.n_params as usize,
            got: params.len(),
        });
    }
    let mut out: Option<Vec<Value>> = None;
    for (i, pt) in plan.param_types.iter().enumerate() {
        let (Some(t), Some(v)) = (pt, params.get(i)) else {
            continue;
        };
        if v.fits(*t) {
            continue;
        }
        let bridged = match (v, t) {
            (Value::Int(n @ (0 | 1)), mpedb_types::ColumnType::Bool) => Some(Value::Bool(*n == 1)),
            (Value::Bool(b), mpedb_types::ColumnType::Int64) => Some(Value::Int(*b as i64)),
            (Value::Int(n), mpedb_types::ColumnType::Float64) => {
                exact_int_as_float(*n).map(Value::Float)
            }
            (Value::Float(f), mpedb_types::ColumnType::Int64) => {
                exact_float_as_int(*f).map(Value::Int)
            }
            // sqlite affinity: a full integer/float text against an int/real slot
            // converts (Django and CPython often bind numbers as text).
            //
            // ⚠ A NON-numeric text must NOT become 0 here. `CAST('abc' AS
            // INTEGER)` is 0 in sqlite, but BINDING A PARAMETER IS NOT A CAST,
            // and the two disagree exactly where it matters: sqlite evaluates
            // `id = 'abc'` by comparing storage classes (integer sorts below
            // text) and answers FALSE for every row, whereas coercing to 0
            // makes it MATCH a row whose id is 0. That is a wrong answer, not
            // a lenient one — and it is the distinction E3(b) settled: apply
            // the conversion and stay rigid about its RESULT. Returning None
            // falls through to a named type error, which is narrower than
            // sqlite and never different from it.
            //
            // The arithmetic case (`num_chairs + ?`, where sqlite really does
            // coerce to 0) is a genuine gap, but it cannot be decided here:
            // `coerce_params` sees the slot, not whether the parameter is used
            // in arithmetic or in a comparison. Closing it needs the use site,
            // not a blanket rule. See C-API-COMPAT.md's named refusal for
            // `test_expressions_not_introduce_sql_injection_via_untrusted_string_inclusion`.
            (Value::Text(s), mpedb_types::ColumnType::Int64) => {
                s.trim().parse::<i64>().ok().map(Value::Int)
            }
            (Value::Text(s), mpedb_types::ColumnType::Float64) => s
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .map(Value::Float),
            _ => None,
        };
        match bridged {
            Some(nv) => out.get_or_insert_with(|| params.to_vec())[i] = nv,
            None => {
                // Name the reason when the two types WOULD have bridged and it
                // was this particular value that could not — "1.5 is not an
                // integer" is actionable where "float64 vs int64" is not. Every
                // other pair keeps the exact wording it has always had (the
                // Python SDK matches on the timestamp one).
                let why = match (v, t) {
                    (Value::Int(_), mpedb_types::ColumnType::Float64) => {
                        " (too large to convert to float64 without losing precision)"
                    }
                    (Value::Float(f), mpedb_types::ColumnType::Int64) => {
                        if f.is_finite() && f.fract() == 0.0 {
                            " (outside the int64 range)"
                        } else {
                            " (not an exact integer)"
                        }
                    }
                    _ => "",
                };
                return Err(Error::TypeMismatch(format!(
                    "parameter ${} is {}, statement requires {}{}",
                    i + 1,
                    v.type_name(),
                    t,
                    why
                )));
            }
        }
    }
    Ok(match out {
        Some(v) => Cow::Owned(v),
        None => Cow::Borrowed(params),
    })
}


// Active nested-derived working table while `exec_derived` runs an outer scan
// whose statement node is NOT itself `PlanStmt::Derived` (format 58 compound
// arms). Single-threaded per execute; cleared on the way out.
thread_local! {
    static ACTIVE_WORKING_TABLE: std::cell::RefCell<Option<TableDef>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) fn with_working_table_def<R>(def: TableDef, f: impl FnOnce() -> R) -> R {
    ACTIVE_WORKING_TABLE.with(|c| {
        let prev = c.replace(Some(def));
        let out = f();
        c.replace(prev);
        out
    })
}

fn table_def<'a>(
    schema: &'a Schema,
    plan: &'a CompiledPlan,
    table: u32,
) -> Result<std::borrow::Cow<'a, TableDef>> {
    use std::borrow::Cow;
    // FROM-less SELECT: the DUAL sentinel resolves to the shared zero-column
    // def — every downstream width/name computation degrades correctly over
    // zero columns, and the gather never reaches a TxnCtx call.
    if table == mpedb_sql::DUAL_TABLE {
        return Ok(Cow::Borrowed(mpedb_sql::dual_def()));
    }
    // The working table resolves to the synthetic def of the active derived /
    // recursive CTE. Nested Derived compound arms (format 58) install theirs
    // via [`with_working_table_def`] for the outer scan; top-level
    // PlanStmt::Derived / RecursiveCte carry theirs on the statement node.
    if table == mpedb_sql::CTE_TABLE {
        if let Some(def) = ACTIVE_WORKING_TABLE.with(|c| c.borrow().clone()) {
            return Ok(Cow::Owned(def));
        }
        return match &plan.stmt {
            PlanStmt::RecursiveCte(rc) => Ok(Cow::Owned(rc.cte_def())),
            PlanStmt::Derived(dp) => Ok(Cow::Owned(dp.derived_def())),
            _ => Err(internal("CTE working table outside a recursive CTE / derived table")),
        };
    }
    schema
        .table(table)
        .map(Cow::Borrowed)
        .ok_or_else(|| internal("table id out of range"))
}

/// Microseconds since the Unix epoch, captured once per execute() call.
fn now_micros() -> i64 {
    // Via `crate::os` so the wasm32 build reads the HOST's clock; a direct
    // `SystemTime::now()` panics there. See `os::wall_clock_micros`.
    mpedb_core::wall_clock_micros()
}
