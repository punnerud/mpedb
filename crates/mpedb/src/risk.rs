//! Layer 1 of the runtime budget (#74, design/DESIGN-RUNTIME-BUDGET.md): an
//! MPEE-style, prepare-time, **read-only** worst-case cardinality estimate.
//!
//! Given an already-decoded [`CompiledPlan`], the live [`Schema`], and the
//! catalog's **transactionally-exact** per-table row counts, it multiplies
//! cardinalities among the plan's scans, joins and correlated-subquery
//! re-evaluations to bound the work a run may do — *before* it runs. A
//! correlated subquery over `N` outer rows against an `M`-row inner is `≈ N·M`;
//! a cross join of `N` and `M` rows is `N·M`.
//!
//! This never touches plan bytes and never executes anything: it reads the plan
//! and the catalog counts. The caller relates the estimate to `max_work_rows`
//! and can warn (or, via [`RiskEstimate::exceeds`], refuse) at prepare time.

use mpedb_sql::{AccessPath, CompiledPlan, CompoundPlan, PlanStmt, SelectPlan, SubPlan};
use mpedb_types::Schema;

/// A prepare-time worst-case estimate of the work one execution may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskEstimate {
    /// Worst-case "work rows" (the same unit the runtime budget counts): the
    /// dominant product term across the plan's scans, joins and correlated
    /// re-evaluations. Saturating — an astronomically large plan clamps at
    /// `u64::MAX` rather than wrapping.
    pub work_rows: u64,
    /// A human label for the single node contributing that dominant term — the
    /// attribution MPEE gives "at the start" (e.g. `nested-loop join with "b"`,
    /// `correlated subquery over "lines"`, `scan of table "orders"`).
    pub dominant: String,
    /// The dominant node's own contribution (equal to `work_rows`).
    pub dominant_rows: u64,
}

impl RiskEstimate {
    /// True when this estimate exceeds a finite `budget` — the hook a caller
    /// uses to warn, or (opt-in) to refuse before executing. `budget == 0`
    /// (unlimited) never exceeds.
    pub fn exceeds(&self, budget: u64) -> bool {
        budget != 0 && self.work_rows > budget
    }

    /// Does this plan structurally *multiply* — a join or a correlated subplan?
    /// A single-table point/scan can never blow the budget by cardinality, so
    /// the facade skips the (row-count-reading) estimate for such plans.
    pub fn plan_can_multiply(plan: &CompiledPlan) -> bool {
        fn sel(sp: &SelectPlan) -> bool {
            !sp.joins.is_empty()
        }
        let subs_correlated = |subs: &[SubPlan]| subs.iter().any(|s| !s.outer_args.is_empty());
        match &plan.stmt {
            PlanStmt::Select(sp) => sel(sp) || subs_correlated(&plan.subplans),
            PlanStmt::Compound(c) => c.arms.iter().any(sel) || subs_correlated(&plan.subplans),
            PlanStmt::Insert { from_select, .. } => {
                from_select.as_ref().is_some_and(|s| sel(&s.plan)) || subs_correlated(&plan.subplans)
            }
            PlanStmt::Update { .. } | PlanStmt::Delete { .. } => subs_correlated(&plan.subplans),
            PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => false,
        }
    }
}

/// A running (rows, dominant-node) pair as the walk folds a plan together.
#[derive(Clone)]
struct Acc {
    /// The dominant product term seen so far (the estimate).
    rows: u64,
    label: String,
    label_rows: u64,
}

impl Acc {
    fn new(rows: u64, label: String) -> Acc {
        Acc { rows, label_rows: rows, label }
    }
    /// Record a candidate dominant node; keep the larger contributor.
    fn consider(&mut self, rows: u64, label: impl FnOnce() -> String) {
        if rows >= self.label_rows {
            self.label_rows = rows;
            self.label = label();
        }
        self.rows = self.rows.max(rows);
    }
    fn into_estimate(self) -> RiskEstimate {
        RiskEstimate {
            work_rows: self.rows,
            dominant: self.label,
            dominant_rows: self.label_rows,
        }
    }
}

fn table_name(schema: &Schema, id: u32) -> String {
    if id == mpedb_sql::DUAL_TABLE {
        return "dual".to_string();
    }
    schema
        .table(id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| format!("table #{id}"))
}

/// Worst-case cardinality of one access path over `table`.
fn card_access(access: &AccessPath, table: u32, schema: &Schema, rc: &dyn Fn(u32) -> u64) -> u64 {
    if table == mpedb_sql::DUAL_TABLE {
        return 1; // the FROM-less synthetic single row
    }
    match access {
        // A PK equality pins at most one row.
        AccessPath::PkPoint(_) => 1,
        // A full-width probe of a UNIQUE secondary index pins at most one row;
        // any other index equality can match every row sharing the prefix.
        AccessPath::IndexPoint { index_no, parts } => {
            let uniq_full = schema
                .table(table)
                .and_then(|t| t.indexes.get(*index_no as usize - 1))
                .is_some_and(|ix| ix.unique && parts.len() == ix.columns.len());
            if uniq_full { 1 } else { rc(table) }
        }
        // Ranges and full scans are bounded only by the table itself. An
        // FtsScan is bounded by the table too (a term can match at most every
        // row); the tighter min(posting-list lengths) bound needs data the
        // schema-only estimator cannot see, and the layer-2 work meter enforces
        // the real cost per posting entry.
        AccessPath::PkRange { .. }
        | AccessPath::IndexRange { .. }
        | AccessPath::FullScan
        | AccessPath::FtsScan { .. } => rc(table),
    }
}

/// Estimate one SELECT together with `subplans` — this level's lift list (the
/// statement's own `plan.subplans` at the top; a nested subplan's `sub.subplans`
/// in recursion). Returns the dominant product term and its attribution.
fn estimate_select(
    sp: &SelectPlan,
    subplans: &[SubPlan],
    schema: &Schema,
    rc: &dyn Fn(u32) -> u64,
) -> Acc {
    // Outer scan, then each join multiplies the running product — that product
    // IS the nested-loop pairing work (the cross-join cost).
    let base = card_access(&sp.access, sp.table, schema, rc);
    let mut acc = Acc::new(base, format!("scan of table \"{}\"", table_name(schema, sp.table)));
    let mut product = base;
    for join in &sp.joins {
        let inner = card_access(&join.access, join.table, schema, rc);
        product = product.saturating_mul(inner);
        let jt = join.table;
        acc.consider(product, || {
            format!("nested-loop join with \"{}\"", table_name(schema, jt))
        });
    }
    // `product` now = the number of rows a correlated subplan re-evaluates over.
    for sub in subplans {
        let inner = estimate_select(&sub.plan, &sub.subplans, schema, rc);
        if sub.outer_args.is_empty() {
            // Uncorrelated: evaluated once. Its own worst case is a candidate.
            let inner_rows = inner.rows;
            let inner_label = inner.into_estimate();
            acc.consider(inner_rows, || inner_label.dominant);
        } else {
            // Correlated: re-evaluated per outer row ⇒ product · inner.
            let sub_work = product.saturating_mul(inner.rows);
            let it = sub.plan.table;
            acc.consider(sub_work, || {
                format!("correlated subquery over \"{}\"", table_name(schema, it))
            });
        }
    }
    acc
}

fn estimate_compound(
    c: &CompoundPlan,
    subplans: &[SubPlan],
    schema: &Schema,
    rc: &dyn Fn(u32) -> u64,
) -> Acc {
    // Every arm is scanned; the dominant node is the worst arm, and the total
    // work rows are their (saturating) sum.
    let mut total: u64 = 0;
    let mut best: Option<Acc> = None;
    for arm in &c.arms {
        let a = estimate_select(arm, subplans, schema, rc);
        total = total.saturating_add(a.rows);
        best = Some(match best {
            Some(b) if b.label_rows >= a.label_rows => b,
            _ => a,
        });
    }
    let mut acc = best.unwrap_or_else(|| Acc::new(0, "empty compound".to_string()));
    acc.rows = total.max(acc.rows);
    acc
}

/// The prepare-time worst-case risk estimate for `plan` (#74). `row_count`
/// returns the catalog's exact live row count for a table id (0 for an unknown
/// id). Pure and read-only — no execution, no plan-byte change.
pub fn estimate_plan_risk(
    plan: &CompiledPlan,
    schema: &Schema,
    row_count: &dyn Fn(u32) -> u64,
) -> RiskEstimate {
    let acc = match &plan.stmt {
        PlanStmt::Select(sp) => estimate_select(sp, &plan.subplans, schema, row_count),
        PlanStmt::Compound(c) => estimate_compound(c, &plan.subplans, schema, row_count),
        PlanStmt::Insert { table, from_select, rows, .. } => match from_select {
            Some(sel) => estimate_select(&sel.plan, &plan.subplans, schema, row_count),
            None => Acc::new(
                rows.len() as u64,
                format!("insert of {} row(s) into \"{}\"", rows.len(), table_name(schema, *table)),
            ),
        },
        PlanStmt::Update { table, access, .. } | PlanStmt::Delete { table, access, .. } => {
            let base = card_access(access, *table, schema, row_count);
            let mut acc = Acc::new(base, format!("scan of table \"{}\"", table_name(schema, *table)));
            // A correlated subquery in the WHERE re-evaluates per matched row.
            for sub in &plan.subplans {
                let inner = estimate_select(&sub.plan, &sub.subplans, schema, row_count);
                if sub.outer_args.is_empty() {
                    let r = inner.rows;
                    let e = inner.into_estimate();
                    acc.consider(r, || e.dominant);
                } else {
                    let it = sub.plan.table;
                    let w = base.saturating_mul(inner.rows);
                    acc.consider(w, || {
                        format!("correlated subquery over \"{}\"", table_name(schema, it))
                    });
                }
            }
            acc
        }
        PlanStmt::Begin | PlanStmt::Commit | PlanStmt::Rollback => {
            Acc::new(0, "no-op statement".to_string())
        }
    };
    acc.into_estimate()
}
