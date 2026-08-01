//! Working-table statements: [`RecursiveCtePlan`], [`DerivedPlan`], their synthetic [`TableDef`].

use super::*;

/// A `WITH RECURSIVE <name>(<columns>) AS (<anchor> UNION[ ALL] <recursive>)
/// <outer>` statement (design/DESIGN-CTE-RECURSIVE.md stage 1).
///
/// Unlike a non-recursive CTE — flattened onto its base table at bind time
/// (DESIGN-CTE.md) — this is a genuine **fixpoint** the executor iterates: the
/// anchor seeds a result set and a FIFO queue, the recursive term is
/// re-evaluated with the working table bound to the PREVIOUS step's new rows
/// (semi-naive), and survivors accumulate until a step adds nothing (natural
/// fixpoint), the outer `LIMIT` is satisfied, or #74's work budget trips.
///
/// `anchor`, `recursive` and `outer` are ordinary [`SelectPlan`]s. The recursive
/// term and the outer statement read the working table through the [`CTE_TABLE`]
/// sentinel, whose synthetic [`TableDef`] is [`RecursiveCtePlan::cte_def`]; the
/// executor binds it to the queue (recursive) or the full result (outer). The
/// anchor never references it.
#[derive(Debug, Clone, PartialEq)]
pub struct RecursiveCtePlan {
    /// The CTE name — used for the #74 attribution `recursive CTE "<name>"` and
    /// for EXPLAIN.
    pub name: String,
    /// Declared column names (the REQUIRED `t(c1, …)` list). `columns.len()` is
    /// the CTE's arity; the anchor's projection must match it.
    pub columns: Vec<String>,
    /// The CTE's column types, derived from the anchor's projection and aligned
    /// to `columns`. A rigid engine fixes them here; the recursive term's
    /// projection must agree (arity AND type).
    pub col_types: Vec<ColumnType>,
    /// One AFFINITY per output column (format 68), on the same rule the window
    /// path uses for a collation: a projection that IS a bare column carries
    /// that column's DECLARED affinity, anything computed carries none.
    ///
    /// It has to live in the PLAN rather than be recomputed after decode,
    /// because the compile-time and validate-time working-table defs must not
    /// drift (see `cte_working_table_def`). Without it, `Affinity::implied_by`
    /// turned `decimal(10,2)` — which is `(Any, Numeric)` — into `Blob`, and a
    /// materialized body's `WHERE price > '50'` compared REAL against TEXT by
    /// storage class and answered NOTHING where the same predicate over the
    /// base table answered correctly.
    pub col_affinities: Vec<mpedb_types::Affinity>,
    /// `UNION ALL` keeps every recursive row; `UNION` deduplicates each step's
    /// output against the full accumulated result (on the whole tuple).
    pub union_all: bool,
    /// Non-recursive seed. Reads real tables (or the dual row); NEVER the
    /// working table.
    pub anchor: SelectPlan,
    /// Recursive term. References the working table exactly once ([`CTE_TABLE`]),
    /// in a FROM/JOIN operand; `validate` re-enforces the §3 restrictions.
    pub recursive: SelectPlan,
    /// The outer statement, reading the CTE's full result via [`CTE_TABLE`].
    pub outer: SelectPlan,
}

impl RecursiveCtePlan {
    /// The synthetic [`TableDef`] the working table presents to the binder,
    /// validator, planner and EXPLAIN — id [`CTE_TABLE`], the declared columns
    /// typed by `col_types`, no PK and no indexes (so every access over it is a
    /// FullScan). Never registered in a schema; never reaches the row/key layer.
    pub fn cte_def(&self) -> TableDef {
        cte_working_table_def(&self.name, &self.columns, &self.col_types, &self.col_affinities)
    }
}

/// A MATERIALIZED derived table (design/DESIGN-DERIVED-TABLES.md §5, format 49):
/// `SELECT … FROM (<body>) [AS] alias …` whose body the Stage-B flattener could
/// not splice (aggregate / GROUP BY / HAVING / DISTINCT / join / ORDER BY+LIMIT
/// / window / compound bodies). The executor runs `body` EXACTLY ONCE into an
/// in-memory row set — duplicates preserved (a derived table is a bag) — then
/// runs `outer` with the [`CTE_TABLE`] sentinel bound to that set (the same
/// working-table primitive the recursive CTE uses, minus the fixpoint).
///
/// # Subquery OWNERSHIP across the two components (format 52)
///
/// The body may lift its OWN subqueries — the Django shape
/// `SELECT count(*) FROM (SELECT …, EXISTS(SELECT … WHERE i.x = t.y) AS f
/// FROM t) s WHERE f`, whose `EXISTS` correlates to the BODY's row, not to the
/// outer statement's. Those lifts belong to the BODY: they are listed in
/// [`body_subplans`](Self::body_subplans), their result slots start at
/// [`body_sub_base`](Self::body_sub_base), and the executor fills them WHILE
/// MATERIALISING the body — uncorrelated ones once, correlated ones per body
/// row, exactly as a top-level SELECT fills its own.
///
/// The OUTER statement never sees them. That is the whole point of the
/// ownership split: the outer scans a materialised row set through
/// [`CTE_TABLE`], so a slot correlated to a base-table row of the BODY has no
/// meaning there, and the statement-level `subplans` list (which the executor
/// fills before dispatch, against the OUTER row) stays EMPTY for a derived
/// plan — `validate` re-enforces both.
///
/// The parameter layout is therefore `[user ‖ body subplans]`: the outer
/// carries no lifts of its own (refused) and neither component may reference
/// `current_setting()` (the recursive-CTE rule, unchanged).
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedPlan {
    /// The derived alias — how the outer query addresses the body's columns,
    /// used for EXPLAIN and the #74 attribution `derived table "<name>"`. An
    /// alias-less derived table (`FROM (SELECT …)`) carries a synthetic,
    /// unreferenceable name.
    pub name: String,
    /// The body's output column NAMES, in projection order: the item's alias,
    /// else a bare column's own (short) name, else the rendered expression —
    /// sqlite's naming rule, which is what makes outer references resolve.
    pub columns: Vec<String>,
    /// The body's output column types, aligned to `columns`. An output the body
    /// leaves untyped (a bare NULL) is `any`, decided per value at runtime.
    pub col_types: Vec<ColumnType>,
    /// One AFFINITY per output column (format 68), on the same rule the window
    /// path uses for a collation: a projection that IS a bare column carries
    /// that column's DECLARED affinity, anything computed carries none.
    ///
    /// It has to live in the PLAN rather than be recomputed after decode,
    /// because the compile-time and validate-time working-table defs must not
    /// drift (see `cte_working_table_def`). Without it, `Affinity::implied_by`
    /// turned `decimal(10,2)` — which is `(Any, Numeric)` — into `Blob`, and a
    /// materialized body's `WHERE price > '50'` compared REAL against TEXT by
    /// storage class and answered NOTHING where the same predicate over the
    /// base table answered correctly.
    pub col_affinities: Vec<mpedb_types::Affinity>,
    /// The materialized body — a plain `SELECT` or a whole compound. Never
    /// references [`CTE_TABLE`] (a derived table cannot see itself).
    pub body: SubBody,
    /// The BODY's own lifted subqueries (format 52). Empty for every plan a
    /// format-51 reader could produce. Non-empty only for a `Select` body: a
    /// compound body's arms have no per-row fill phase, so a lift there stays
    /// refused (the same rule a compound subquery body follows).
    ///
    /// Filled by the executor DURING materialisation, against the body's own
    /// row — never by the pre-dispatch pass that fills the statement-level
    /// `subplans`, and never visible to `outer`.
    pub body_subplans: Vec<SubPlan>,
    /// First reserved slot of `body_subplans`: child `i`'s result lives at
    /// `body_sub_base + i`. Equal to the user parameter count (the body was
    /// planned with exactly the caller's parameters in scope), carried
    /// EXPLICITLY rather than re-derived so exec and validate read one number
    /// instead of two arithmetic identities that could drift.
    pub body_sub_base: u16,
    /// The outer statement, reading the materialized rows via [`CTE_TABLE`]
    /// (exactly one reference, FullScan only — no PK, no indexes).
    pub outer: SelectPlan,
}

impl DerivedPlan {
    /// Every reserved parameter slot this derived plan owns: its own body lifts
    /// plus whatever its BODY reserves in turn (a compound body's arms, or —
    /// format 65 — a nested derived body). One recursive definition, so the
    /// three places that need the width cannot disagree about a nesting.
    pub fn reserved_slots(&self) -> u16 {
        self.body_subplans.len() as u16
            + match &self.body {
                SubBody::Select(_) => 0,
                SubBody::Compound(c) => c.n_arm_slots() + c.n_derived_body_slots(),
                SubBody::Derived(dp) => dp.reserved_slots(),
            }
    }

    /// The synthetic [`TableDef`] the materialized body presents to the binder,
    /// validator, executor and EXPLAIN — the same working-table shape as
    /// [`RecursiveCtePlan::cte_def`].
    pub fn derived_def(&self) -> TableDef {
        cte_working_table_def(&self.name, &self.columns, &self.col_types, &self.col_affinities)
    }
}

/// Build the synthetic working-table [`TableDef`] for a recursive CTE. The
/// SINGLE source of the working table's shape — used by the planner (at compile
/// time) and by [`RecursiveCtePlan::cte_def`] (validate / footprint / EXPLAIN),
/// so the def a plan is built against can never drift from the def it is
/// re-validated against. Columns are nullable (sqlite treats every value as
/// nullable; the anchor may seed one and the recursion NULL it — the permissive
/// 3VL choice, never a wrong answer); no PK and no indexes ⇒ every access is a
/// FullScan.
pub(crate) fn cte_working_table_def(
    name: &str,
    columns: &[String],
    col_types: &[ColumnType],
    // Parallel to `col_types`. Shorter (or empty) falls back to
    // `implied_by`, which is what every caller did before format 68.
    col_affinities: &[mpedb_types::Affinity],
) -> TableDef {
    TableDef {
        id: CTE_TABLE,
        name: name.to_string(),
        columns: columns
            .iter()
            .zip(col_types)
            .enumerate()
            .map(|(i, (name, &ty))| mpedb_types::ColumnDef { generated: None, default_text: None, decl: None,
                name: name.clone(),
                ty,
                nullable: true,
                unique: false,
                indexed: false,
                default: None,
                check: None,
                collation: mpedb_types::Collation::Binary,
                affinity: col_affinities
                    .get(i)
                    .copied()
                    .unwrap_or_else(|| mpedb_types::Affinity::implied_by(ty)),
            })
            .collect(),
        primary_key: Vec::new(),
        indexes: Vec::new(),
        dead: false,
        implicit_rowid: false, autoincrement: false,
        kind: mpedb_types::TableKind::Standard,
        foreign_keys: Vec::new(),
    }
}
