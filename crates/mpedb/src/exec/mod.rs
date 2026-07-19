//! Plan executor: runs a validated [`CompiledPlan`] against an engine
//! transaction. Shared by the autocommit paths on [`crate::Database`] and the
//! interactive [`crate::WriteSession`] via the [`TxnCtx`] abstraction.

use crate::trigger::{CompiledTrigger, TriggerSet};
use crate::ExecResult;
use mpedb_core::{ReadTxn, WriteTxn};
use mpedb_sql::{
    AccessPath, AggCall, Aggregation, CompiledPlan, ConflictProbe, InsertSource, Join, JoinKind,
    CompoundPlan, GroupKey, OrderOver, PlanOnConflict, PlanStmt, Projection, RowMap, RowSide,
    SelectPlan, SetOp, SortDir, SubBody, SubPlan,
};
use mpedb_types::{
    exact_float_as_int, exact_int_as_float, keycode, Accum, Collation, DefaultExpr, Error,
    ExprProgram, HostFns, KeyBound, KeyPart, Result, Schema, TableDef, Value,
};
use std::cmp::Ordering;
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
mod recursive;
mod window;

pub(crate) use gather::{range_bounds, resolve_part, RawBound};
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
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>>;
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
        order_by: &[(u16, SortDir, Collation)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
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
        sort_rows(&mut kept, order_by);
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
}

impl TxnCtx for WriteTxn<'_> {
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk(self, table, pk)
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
}

impl<'a, 'e> WriteCtx<'a, 'e> {
    pub(crate) fn new(
        txn: &'a mut WriteTxn<'e>,
        host: Option<&'a dyn HostFns>,
        aggs: Option<&'a dyn mpedb_types::HostAggs>,
    ) -> WriteCtx<'a, 'e> {
        WriteCtx { txn, host, aggs }
    }
}

impl TxnCtx for WriteCtx<'_, '_> {
    fn host_fns(&self) -> Option<&dyn HostFns> {
        self.host
    }
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        self.aggs
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        WriteTxn::get_by_pk(self.txn, table, pk)
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
);

impl TxnCtx for ReadCtx<'_, '_> {
    fn host_fns(&self) -> Option<&dyn HostFns> {
        self.1
    }
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        self.2
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        self.0.get_by_pk(table, pk)
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
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, SortDir, Collation)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
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
            let cand = Ranked { row, order_by, seq };
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
    order_by: &'a [(u16, SortDir, Collation)],
    seq: u64,
}

impl Ord for Ranked<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Primary: the ORDER BY spec. Secondary: scan sequence ASCENDING
        // regardless of the ORDER BY direction — a stable sort keeps equal
        // keys in original (scan) order, so the tiebreak is never reversed.
        cmp_rows(&self.row, &other.row, self.order_by).then(self.seq.cmp(&other.seq))
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
fn subplan_value(r: ExecResult, kind: mpedb_sql::SubPlanKind) -> Result<Value> {
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
fn run_subplan(
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
fn exec_select_leveled(
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
    let mut arms = c.arms.iter();
    let first = arms.next().ok_or_else(|| internal("compound with no arms"))?;
    let ExecResult::Rows { columns, rows } = exec_select(ctx, schema, plan, params, first)? else {
        return Err(internal("compound arm produced no rows"));
    };
    let mut acc = rows;
    for (arm, op) in arms.zip(&c.ops) {
        let ExecResult::Rows { rows, .. } = exec_select(ctx, schema, plan, params, arm)? else {
            return Err(internal("compound arm produced no rows"));
        };
        acc = apply_set_op(acc, rows, *op);
    }
    if !c.order_by.is_empty() {
        sort_rows(&mut acc, &c.order_by);
    }
    let skip = c.offset.unwrap_or(0).min(usize::MAX as u64) as usize;
    let take = c.limit.map_or(usize::MAX, |l| l.min(usize::MAX as u64) as usize);
    if skip > 0 || take != usize::MAX {
        acc = acc.into_iter().skip(skip).take(take).collect();
    }
    Ok(ExecResult::Rows { columns, rows: acc })
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
            if aggregate.is_some() {
                // Plain aggregate: no correlated subplans and no post-filter (a
                // correlated aggregate is routed straight to `run_aggregate`
                // from `exec_select_leveled`, never through here — compound arms
                // cannot carry either, and a correlated nested aggregate goes via
                // `run_subplan`). `base` is unused with an empty correlated set.
                return run_aggregate(
                    ctx, schema, plan, params, sp, plan.subplan_base() as usize, &[], None,
                );
            }
            let rows = if !joins.is_empty() {
                // A join materializes: the sort, the dedup and the LIMIT all
                // apply to JOINED rows, so none of them can bound the scan.
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
                )?;
                // `OrderOver::BaseRow` means "the tuple the scan produced", and
                // for a join that tuple IS the joined row — so the sort belongs
                // HERE, before the projection narrows it. Sorting the projected
                // rows instead would index the wrong tuple.
                if *order_over == OrderOver::BaseRow && !order_by.is_empty() {
                    sort_rows(&mut r, order_by);
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
                sort_rows(&mut r, order_by);
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
                sort_rows(&mut out, order_by);
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
        )?
    } else {
        gather_rows(ctx, *table, access, filter.as_ref(), plan, params, None)?
    };
    if *order_over == OrderOver::BaseRow && !order_by.is_empty() {
        sort_rows(&mut rows, order_by);
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
        sort_rows(&mut out, order_by);
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
    // correlated subplan always has a plain SELECT body (a compound body is
    // uncorrelated), so `as_select` is `Some`.
    let corr_table = correlated
        .first()
        .and_then(|(_, s)| s.body.as_select())
        .map(|sp| sp.table);
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
            for (applied, mut row) in built_rows.into_iter().enumerate() {
                if converts {
                    t.apply_store_affinity(row.to_mut());
                }
                if let Some(rc) = rowid_col {
                    if row.get(rc as usize).is_some_and(|v| v.is_null()) {
                        let next = ctx.next_rowid(*table, rc)?;
                        row.to_mut()[rc as usize] = Value::Int(next);
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
                if let Err(e) =
                    fire_insert(ctx, schema, &triggers.before_insert, *table, &row, triggers, depth)
                {
                    *partial = true;
                    return Err(e);
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
                                // other write, so the same store-time affinity.
                                t.apply_store_affinity(&mut new_row);
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
                if let Err(e) = fire_update(
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
                    *partial = true;
                    return Err(e);
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
                if let Err(e) =
                    fire_delete(ctx, schema, &triggers.before_delete, *table, old, triggers, depth)
                {
                    *partial = true;
                    return Err(e);
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
) -> Result<()> {
    match bucket.get(&table) {
        Some(trigs) => fire_row_triggers(ctx, schema, trigs, Some(new_row), None, &[], triggers, depth),
        None => Ok(()),
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
) -> Result<()> {
    match bucket.get(&table) {
        Some(trigs) => {
            fire_row_triggers(ctx, schema, trigs, Some(new_row), Some(old_row), changed, triggers, depth)
        }
        None => Ok(()),
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
) -> Result<()> {
    match bucket.get(&table) {
        Some(trigs) => fire_row_triggers(ctx, schema, trigs, None, Some(old_row), &[], triggers, depth),
        None => Ok(()),
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
fn fire_row_triggers(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    trigs: &[CompiledTrigger],
    new: Option<&[Value]>,
    old: Option<&[Value]>,
    changed: &[u16],
    triggers: &TriggerSet,
    depth: u32,
) -> Result<()> {
    if trigs.is_empty() {
        return Ok(());
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
        if let Some((prog, when_map)) = &trig.when {
            let wp = pick(when_map)?;
            let mut stack = Vec::new();
            if !prog.eval_filter(&mut stack, &[], &wp)? {
                continue;
            }
        }
        // Multi-statement body: each statement runs in order on the same txn.
        for (body_plan, body_map) in &trig.body {
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
    }
    Ok(())
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
    // The working table resolves to the synthetic def carried by THIS plan's
    // node — `RecursiveCte` or `Derived`, the only two meanings `CTE_TABLE` has
    // here. Owned (built from the plan's columns/types): no schema entry.
    if table == mpedb_sql::CTE_TABLE {
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
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(_) => 0, // clock before the epoch: store 0 rather than panic
    }
}
