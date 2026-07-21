//! Recursive CTE execution — the semi-naive FIFO fixpoint
//! (design/DESIGN-CTE-RECURSIVE.md §2). A pure in-process, read-only feature:
//! the whole fixpoint runs inside one read snapshot, deriving every row from the
//! snapshot's base tables, so nothing here touches the commit path or durability.
//!
//! The working table is bound through [`WorkingTableCtx`], a [`TxnCtx`] that
//! answers a scan of the [`CTE_TABLE`] sentinel from an in-memory row set (the
//! queue for the recursive term, the full result for the outer statement) and
//! delegates every real table — and the #74 work meter — to the transaction
//! underneath.

use super::*;
use mpedb_sql::{RecursiveCtePlan, CTE_TABLE};
use std::collections::HashSet;

/// Execute a `WITH RECURSIVE` statement.
///
/// 1. Evaluate the anchor → seed the result set and the FIFO queue.
/// 2. Loop: evaluate the recursive term with the working table bound to the
///    PREVIOUS step's new rows (the queue); charge #74 one work-row per row
///    produced (before dedup, so the count is data-driven); for `UNION` drop
///    rows already in the result, for `UNION ALL` keep all; append survivors to
///    the result and the next queue. Stop when a step adds nothing, the outer
///    `LIMIT` is satisfied, or the work budget trips.
/// 3. Run the outer statement over the full result.
pub(super) fn exec_recursive_cte(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    rc: &RecursiveCtePlan,
) -> Result<ExecResult> {
    // 1. Anchor (reads real tables; never the working table).
    let anchor_rows = select_rows(&mut *ctx, schema, plan, params, &rc.anchor)?;

    // The full accumulated result (insertion order = sqlite's default output
    // order) and, for UNION, the whole-tuple dedup set.
    let mut result: Vec<Vec<Value>> = Vec::with_capacity(anchor_rows.len());
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut queue: Vec<Vec<Value>> = Vec::new();
    for row in anchor_rows {
        if rc.union_all || seen.insert(keycode::encode_group_key(&row, &[])) {
            queue.push(row.clone());
            result.push(row);
        }
    }

    // The outer LIMIT bounds the iteration only when the outer statement passes
    // rows through 1:1 (§2) — that is what makes an infinite generator finite.
    let iter_cap = outer_iteration_cap(&rc.outer);

    // 2. Semi-naive fixpoint.
    while !queue.is_empty() {
        if let Some(cap) = iter_cap {
            if outer_output_count(&rc.outer, &result, params)? >= cap {
                break;
            }
        }
        // The recursive term sees ONLY the previous step's new rows (the queue).
        let step_rows = {
            let mut wctx = WorkingTableCtx { inner: &mut *ctx, rows: &queue };
            select_rows(&mut wctx, schema, plan, params, &rc.recursive)?
        };
        // #74 termination backstop: one work-row per row PRODUCED by the
        // recursive term, charged BEFORE dedup so the count is data-driven and
        // reproducible. An unbounded UNION ALL recursion trips the budget here
        // at a fixed count with the `recursive CTE "<name>"` attribution.
        if !step_rows.is_empty() {
            ctx.charge_work(step_rows.len() as u64, &|| {
                format!("recursive CTE \"{}\"", rc.name)
            })?;
        }
        let mut next: Vec<Vec<Value>> = Vec::new();
        for row in step_rows {
            if rc.union_all || seen.insert(keycode::encode_group_key(&row, &[])) {
                next.push(row.clone());
                result.push(row);
            }
        }
        queue = next;
    }

    // 3. Outer statement over the full result.
    let mut wctx = WorkingTableCtx { inner: ctx, rows: &result };
    exec_select(&mut wctx, schema, plan, params, &rc.outer)
}

/// Execute a MATERIALIZED derived table (design/DESIGN-DERIVED-TABLES.md §5):
/// run the body EXACTLY ONCE into an in-memory row set — against the same
/// snapshot (`ctx`) the outer then reads, duplicates preserved (a derived table
/// is a bag) — and run the outer statement with the working table bound to it.
///
/// #74: the materialized rows are charged to the work meter with the
/// `derived table "<alias>"` attribution (the recursive-CTE convention), so a
/// runaway body trips the budget instead of growing the Vec unbounded; the
/// body's own table scans were additionally charged per row read, as
/// everywhere.
pub(super) fn exec_derived(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    dp: &mpedb_sql::DerivedPlan,
) -> Result<ExecResult> {
    // The body OWNS its lifted subqueries (format 52): their result slots are
    // filled HERE, while the body materialises, and the outer never sees them.
    //
    // The discipline is `exec_stmt_impl`'s, applied one level down — the
    // UNCORRELATED lifts evaluate once, up front (so a `WHERE x = (SELECT
    // max…)` inside the body resolves like any other parameter), and the
    // CORRELATED ones are left to `exec_select_leveled`, which fills them per
    // BODY row after the gather. That is what makes Django's
    // `SELECT count(*) FROM (SELECT …, EXISTS(SELECT … WHERE i.x = t.y) AS f
    // FROM t) s WHERE f` mean what it says: the EXISTS correlates to `t`'s row
    // inside the body, and `f` is just a materialised column by the time the
    // outer filters on it.
    let base = dp.body_sub_base as usize;
    let filled;
    let params: &[Value] = if dp.body_subplans.iter().any(|s| s.outer_args.is_empty()) {
        let mut buf = params.to_vec();
        for (i, sub) in dp.body_subplans.iter().enumerate() {
            if !sub.outer_args.is_empty() {
                continue;
            }
            let inner = run_subplan(ctx, schema, plan, &buf[..base], sub)?;
            buf[base + i] = subplan_value(inner, sub.kind)?;
        }
        filled = buf;
        &filled
    } else {
        params
    };
    let body_rows = match &dp.body {
        mpedb_sql::SubBody::Select(sp) => {
            match exec_select_leveled(&mut *ctx, schema, plan, params, sp, base, &dp.body_subplans)?
            {
                ExecResult::Rows { rows, .. } => rows,
                _ => return Err(internal("derived-table body produced no row set")),
            }
        }
        // A compound body's lifts belong to its ARMS (format 56);
        // `exec_compound` fills them per arm, so nothing is left to do here.
        mpedb_sql::SubBody::Compound(c) => {
            match exec_compound(&mut *ctx, schema, plan, params, c)? {
                ExecResult::Rows { rows, .. } => rows,
                _ => return Err(internal("derived-table body produced no row set")),
            }
        }
    };
    if !body_rows.is_empty() {
        ctx.charge_work(body_rows.len() as u64, &|| {
            format!("derived table \"{}\"", dp.name)
        })?;
        // #101's memory-proportional twin: the materialized set is HELD for the
        // whole outer scan, so its resident `Value` cells are checked against
        // the same `max_join_cells` budget a join's intermediate product is.
        // (The growth phase is covered by the body's own scan/join meters; this
        // is the deterministic backstop on what stays resident.)
        let budget = ctx.join_cells_budget();
        if budget != 0 {
            let cells: u64 = body_rows.iter().map(|r| r.len() as u64).sum();
            if cells > budget {
                return Err(Error::RuntimeBudget {
                    kind: mpedb_types::BudgetKind::JoinCells,
                    limit: budget,
                    used: cells,
                    which: format!("derived table \"{}\" (materialized rows)", dp.name),
                });
            }
        }
    }
    let mut wctx = WorkingTableCtx { inner: ctx, rows: &body_rows };
    // Install this derived's working-table def for `table_def(CTE_TABLE)` while
    // the outer scans — required when the statement node is a Compound with a
    // nested Derived arm (format 58), not PlanStmt::Derived itself.
    let def = dp.derived_def();
    super::with_working_table_def(def, || exec_select(&mut wctx, schema, plan, params, &dp.outer))
}

/// Run one recursive-CTE component and return just its projected rows.
fn select_rows(
    ctx: &mut dyn TxnCtx,
    schema: &Schema,
    plan: &CompiledPlan,
    params: &[Value],
    sp: &mpedb_sql::SelectPlan,
) -> Result<Vec<Vec<Value>>> {
    match exec_select(ctx, schema, plan, params, sp)? {
        ExecResult::Rows { rows, .. } => Ok(rows),
        _ => Err(internal("recursive CTE component produced no row set")),
    }
}

/// The iteration cap `offset + limit` when the outer statement passes rows
/// through 1:1 — no join, aggregate, DISTINCT, window or ORDER BY (any of which
/// changes cardinality or needs the whole result, so the fixpoint must complete
/// or the #74 budget must catch a runaway). `None` = no early stop.
fn outer_iteration_cap(outer: &mpedb_sql::SelectPlan) -> Option<usize> {
    if !outer.joins.is_empty()
        || outer.aggregate.is_some()
        || outer.distinct
        || !outer.windows.is_empty()
        || !outer.order_by.is_empty()
    {
        return None;
    }
    let limit = outer.limit?;
    let offset = outer.offset.unwrap_or(0);
    Some(limit.saturating_add(offset).min(usize::MAX as u64) as usize)
}

/// How many output rows the (pass-through) outer statement would currently
/// produce: every result row that passes its residual filter (the projection
/// does not change the count). Only called when [`outer_iteration_cap`] is
/// `Some`, so the outer carries no join / post-filter.
fn outer_output_count(
    outer: &mpedb_sql::SelectPlan,
    result: &[Vec<Value>],
    params: &[Value],
) -> Result<usize> {
    match &outer.filter {
        None => Ok(result.len()),
        Some(f) => {
            let mut stack = Vec::with_capacity(f.max_stack());
            let mut n = 0;
            for row in result {
                if f.eval_filter(&mut stack, row, params)? {
                    n += 1;
                }
            }
            Ok(n)
        }
    }
}

/// A [`TxnCtx`] that binds the recursive CTE's working table ([`CTE_TABLE`]) to
/// an in-memory row set and delegates everything else — every real table AND the
/// #74 work meter — to the transaction underneath.
///
/// The working table has no PK and no indexes, so the planner reads it only by
/// FullScan: the three scan entry points are the only ones that intercept
/// `CTE_TABLE`. A keyed access on it is an internal error (validate rejects such
/// a plan), and writes never target it (a recursive CTE is read-only).
struct WorkingTableCtx<'a, 'b> {
    inner: &'a mut dyn TxnCtx,
    rows: &'b [Vec<Value>],
}

impl WorkingTableCtx<'_, '_> {
    fn keyed_bug() -> Error {
        internal("keyed access on a recursive CTE working table")
    }
}

impl TxnCtx for WorkingTableCtx<'_, '_> {
    // A wrapper must not narrow scope: a recursive CTE runs on whatever context
    // it wraps, so the host UDF closures in scope there are in scope here too
    // (design/DESIGN-UDF.md). Without this forwarding a UDF called inside a
    // `WITH RECURSIVE` body silently left scope and refused.
    fn host_fns(&self) -> Option<&dyn mpedb_types::HostFns> {
        self.inner.host_fns()
    }
    fn host_aggs(&self) -> Option<&dyn mpedb_types::HostAggs> {
        self.inner.host_aggs()
    }
    fn host_colls(&self) -> Option<&dyn mpedb_types::HostColls> {
        self.inner.host_colls()
    }
    fn get_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        if table == CTE_TABLE {
            return Err(Self::keyed_bug());
        }
        self.inner.get_by_pk(table, pk)
    }
    fn get_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        if table == CTE_TABLE {
            return Err(Self::keyed_bug());
        }
        self.inner.get_by_index(table, index_no, values)
    }
    fn scan_by_index(
        &mut self,
        table: u32,
        index_no: u32,
        values: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        if table == CTE_TABLE {
            return Err(Self::keyed_bug());
        }
        self.inner.scan_by_index(table, index_no, values)
    }
    fn scan_by_index_range(
        &mut self,
        table: u32,
        index_no: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        if table == CTE_TABLE {
            return Err(Self::keyed_bug());
        }
        self.inner.scan_by_index_range(table, index_no, lo, hi)
    }
    fn scan_rows_raw(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
    ) -> Result<Vec<Vec<Value>>> {
        if table == CTE_TABLE {
            // FullScan: the planner never bounds a CTE read, so lo/hi are None.
            return Ok(self.rows.to_vec());
        }
        self.inner.scan_rows_raw(table, lo, hi)
    }
    fn scan_rows_capped(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        cap: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        if table == CTE_TABLE {
            let mut kept = Vec::new();
            let mut stack = Vec::new();
            for row in self.rows {
                let keep = match filter {
                    Some((f, params)) => f.eval_filter(&mut stack, row, params)?,
                    None => true,
                };
                if keep {
                    kept.push(row.clone());
                    if cap.is_some_and(|c| kept.len() >= c) {
                        break;
                    }
                }
            }
            return Ok(kept);
        }
        self.inner.scan_rows_capped(table, lo, hi, filter, cap)
    }
    fn scan_rows_topk(
        &mut self,
        table: u32,
        lo: Option<(&[u8], bool)>,
        hi: Option<(&[u8], bool)>,
        filter: Option<(&ExprProgram, &[Value])>,
        order_by: &[(u16, SortDir, mpedb_types::OrderColl)],
        keep: usize,
    ) -> Result<Vec<Vec<Value>>> {
        if table == CTE_TABLE {
            let mut kept = Vec::new();
            let mut stack = Vec::new();
            for row in self.rows {
                let ok = match filter {
                    Some((f, params)) => f.eval_filter(&mut stack, row, params)?,
                    None => true,
                };
                if ok {
                    kept.push(row.clone());
                }
            }
            gather::check_order_colls(order_by, self.inner.host_colls())?;
            sort_rows(&mut kept, order_by, self.inner.host_colls());
            kept.truncate(keep);
            return Ok(kept);
        }
        self.inner.scan_rows_topk(table, lo, hi, filter, order_by, keep)
    }
    fn insert_row(&mut self, table: u32, values: &[Value]) -> Result<()> {
        self.inner.insert_row(table, values)
    }
    fn update_by_pk(&mut self, table: u32, new_values: &[Value]) -> Result<bool> {
        self.inner.update_by_pk(table, new_values)
    }
    fn delete_by_pk(&mut self, table: u32, pk: &[Value]) -> Result<bool> {
        self.inner.delete_by_pk(table, pk)
    }
    fn fts_prefix(&mut self, table: u32, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.inner.fts_prefix(table, prefix)
    }
    fn charge_work(&self, n: u64, which: &dyn Fn() -> String) -> Result<()> {
        self.inner.charge_work(n, which)
    }
    fn join_cells_budget(&self) -> u64 {
        self.inner.join_cells_budget()
    }
}
