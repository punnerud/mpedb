# DESIGN-DERIVED-TABLES — subquery in `FROM` (#74)

**Status: Stage B SHIPPED (2026-07-18); Stage A (materialization) SHIPPED
(2026-07-19) — see §5 below for the as-built design.** A simple
projection/filter derived table `FROM (SELECT …) t` is flattened onto its base
at bind time by `crate::view` — the AST carries an optional `from_derived`
which the view-inline pass splices away (merge WHERE, keep the derived alias,
remap the body's own qualifier onto it) BEFORE planning, so the planner and
executor are untouched. Aggregate/join/DISTINCT/LIMIT/renamed-projection bodies
are refused (never answered wrongly); those are Stage A. Verified against sqlite
3.45 (`crates/mpedb/tests/derived_table.rs`) and the corpus `index/view` sets
(zero wrong, zero error-mismatch). Stage A (materialized derived tables) below
remains the follow-up for the complex bodies.

The original design follows.

## Original context

After live DDL, `CREATE VIEW`, `INSERT … SELECT`,
and the scalar/aggregate batch, the single largest remaining sqllogictest
blocker is the `subquery` category (~9 168 blocked statements over the full
corpus). Almost all of it is one missing shape: a **derived table** — a subquery
used as a `FROM` source, `SELECT … FROM (SELECT …) [AS] t …`. This is also the
feature that lifts the `CREATE VIEW` refusal boundary: an aggregate/join/DISTINCT
view can only be flattened once its body can appear as a `FROM` source.

Closing it is the one substantial step from **99.7% → ~99.9%** on the corpus
(the rest — MySQL-only casts `AS SIGNED`/`AS DECIMAL`, `div-by-zero` raising,
first-class `bool`, the 64-table cap, name-less `DROP INDEX` — are deliberate
deviations or known design choices, not gaps).

## 0. The problem the current planner has

`SelectPlan` assumes every `FROM` item is a **base table**: it carries a
`table: u32` plus `joins: Vec<Join{table: u32, …}>`, and access paths
(`PkPoint`/`IndexPoint`/`FullScan`) resolve against a `TableDef` with real
columns, a PK, and index trees. A derived table has none of that: its "columns"
are the subquery's projection, and its "rows" come from executing the subquery.
So the FROM/scan/join machinery needs a source that is *either* a base table *or*
a subplan.

## 1. Two implementation strategies

- **A — materialize (general, bigger).** Add a `FromSource = Base(u32) |
  Derived(Box<SelectPlan>)` to `SelectPlan`/`Join`. The executor runs a
  `Derived` source to a row buffer (like `INSERT … SELECT` already gathers rows),
  then scans/joins over it. Handles *every* derived table (aggregates, joins,
  DISTINCT, LIMIT inside the subquery). Costs a plan-format change, executor
  scan-source plumbing, and column/type resolution off the subquery's
  projection instead of a `TableDef`.

- **B — flatten where possible (cheaper, partial).** Reuse the `CREATE VIEW`
  flatten (`crate::view`): a derived table whose body is a simple
  projection/filter over one base table folds into the outer query (merge
  `WHERE`, remap columns), exactly as a view does. Refuse the complex bodies.
  Near-zero new surface, but leaves aggregate/join derived tables unsupported.

**Recommendation: B first, then A.** B is a small extension of code that already
exists and ships correctness for the common corpus shape immediately; A is the
follow-up that closes the tail and unifies with a real derived-table row source.
Stage them like DROP-TABLE / CREATE-VIEW were staged.

## 2. Stage B (flatten a derived table) — concrete

1. **Parse.** `from_item` currently accepts a table name (and `( join group )`).
   Add: `( SELECT … ) [AS] alias` → an AST `FromItem::Derived(Box<SelectStmt>,
   alias)`. The alias is mandatory in the corpus's usage and names the derived
   columns.
2. **Rewrite (bind-time, no planner change).** Extend the `crate::view` flatten
   to accept an inline `SelectStmt` (not only a stored view): if the outer
   `FROM` is a simple-derived table, splice it onto its base exactly as a view
   reference is spliced — same simple-body grammar, same `SELECT *` expansion,
   same column-name pass-through (bare columns, no rename → no remap). Reuse
   `check_simple` and `merge_where` verbatim; the only new part is sourcing the
   inner `SelectStmt` from the AST instead of the view catalog.
3. **Refuse** a derived table whose body is not simple (aggregate/join/DISTINCT/
   LIMIT/renamed-or-computed projection) — never answer it wrongly. That residue
   is Stage A's job.
4. **Verify** differentially against sqlite3 + the corpus `subquery` files;
   expect a large chunk of `subquery`/`select-without-from` to clear.

## 3. Stage A (materialized derived table) — sketch, later

`SelectPlan.from` and each `Join.from` become `FromSource`; add
`FromSource::Derived(Box<SelectPlan>, out_cols)`. Executor: a `Derived` source
runs its subplan once into a `Vec<Vec<Value>>` (the `exec_select` path already
returns exactly this) and presents it as a scan cursor; the binder resolves the
derived table's columns from the subplan's `projection` types. Correlated
derived tables stay out of scope for v1 (as correlated-subquery-in-aggregate
already is). One `PLAN_FORMAT` bump.

## 4. Non-goals / refusal boundaries (never a wrong answer)

- Correlated derived tables (referencing the outer row) — refuse in both stages.
- `LATERAL` — refuse.
- A derived table in a write target — refuse (as views are).
- Anything Stage B cannot flatten and Stage A is not yet built for — a clean
  "not supported" message, categorized, never a silent divergence.

## 5. Stage A as built (2026-07-19, PLAN_FORMAT 49)

The body the flattener refuses is MATERIALIZED: run once into an in-memory row
set against the statement's snapshot, then scanned by the outer query. The
primitive is the recursive-CTE working table (`CTE_TABLE` +
`exec/recursive.rs::WorkingTableCtx`), reused verbatim.

### 5.1 Plan shape: a statement node, not a `FromSource`

`PlanStmt::Derived(DerivedPlan { name, columns, col_types, body: SubBody,
outer: SelectPlan })` — the exact shape `RecursiveCtePlan` proved, minus the
fixpoint. Why NOT §3's `FromSource = Base | Derived` generalization of
`SelectPlan`: that touches every `SelectPlan` consumer (codec, validate,
explain, exec, gather, footprint — at every recursion site) and makes a derived
source representable in positions whose slot layout is exactly what
`plan_compound` refuses today (a subquery inside a compound arm). The statement
node makes those shapes UNREPRESENTABLE, and EXPLAIN/validate/footprint each
get one new arm that mirrors the recursive-CTE arm line for line. Why not a
"table-valued `SubPlan` slot kind": a subplan's result is a VALUE in a
parameter slot; a table-valued result is consumed by the GATHER as a scan
source — a different consumer, which the sentinel working table already models.

The `CTE_TABLE` sentinel is REUSED rather than minting a `DERIVED_TABLE`: the
semantics are identical (an in-memory row set answering FullScans only; no
PK/indexes; no catalog identity; no footprint bit; no policy; def resolved from
THIS plan's node — `RecursiveCte` or `Derived`, the only two meanings the
sentinel can have). `WorkingTableCtx`, validate's FullScan-only rule, the
footprint skip and the policy-stamp filter apply unchanged.

The body is a `SubBody` (`Select` | `Compound`), so a compound
(`UNION`/`EXCEPT`/`INTERSECT`) body rides the existing compound plan/executor.
Aggregate / GROUP BY / HAVING / DISTINCT / window / join / ORDER BY+LIMIT
bodies are all just `SelectPlan`s — the body's own LIMIT binds INSIDE the body
because the body plan carries it and is executed as a unit.

### 5.2 Equivalence argument (zero wrong answers)

- **Exactly once, one snapshot**: the executor runs the body plan ONCE per
  execute, against the same `TxnCtx` (same MVCC snapshot) the outer then reads;
  the outer scans the materialized Vec through `WorkingTableCtx`.
- **Bag semantics**: `exec_select`/`exec_compound` return bags; nothing dedups
  the working set (only a body-level set operator dedups, as SQL says).
- **Column order/names**: the synthetic `TableDef` carries the body's output
  columns in projection order; names follow sqlite's rule — item alias, else a
  bare column's own (short) name, else the rendered expression — so outer
  references resolve exactly against the body's projection, and `SELECT *`
  exposes exactly the body's columns. Types are the body's inferred output
  types; an untyped output (bare NULL) becomes `any`, decided per value.
- **RLS**: the body plan bakes the read policies of every table it reads
  (stamped like any select); the working set is exactly the visible rows.

### 5.3 Memory bound (#74 from day one)

Materialized rows are charged to the runtime work meter — `charge_work(n)` with
attribution `derived table "<alias>"` — the same convention the recursive-CTE
fixpoint uses, so a runaway body trips `Error::RuntimeBudget` naming the
`max_work_rows` knob before the Vec grows unbounded (the body's own scans are
additionally charged per row read, as everywhere).

### 5.4 Scope / refusals (by name)

- Correlated derived tables (LATERAL): the body is planned with the outer scope
  NOT visible, so an outer reference fails as an unknown table/column — the
  same error sqlite gives (sqlite has no LATERAL either).
- One derived table per statement, in the outermost FROM. Nested positions
  (compound arm, subquery body, `INSERT … SELECT` source, recursive-CTE
  components, another derived body) keep a clean refusal. A derived table as a
  JOIN operand (`FROM t JOIN (SELECT …) d`) does not parse (join operands are
  table names) — `FROM (SELECT …) d JOIN t` covers the joined case, on either
  side via the RIGHT-join rewrite.
- The alias is optional (sqlite allows `FROM (SELECT …)`); an absent alias gets
  an unreferenceable synthetic name. Re-referencing the alias as a join operand
  (`FROM (…) d JOIN d`) is refused (sqlite: "no such table: d").
- Lifted subqueries and `current_setting()` in the body or the outer are stage-1
  refusals (same `[user]`-only parameter layout as the recursive CTE, same
  reserved-slot reconciliation cost to lift later).

Simple aliased projection/filter bodies still take the Stage-B flatten (better
plans: index access paths); materialization is the fallback, so every
previously-answered shape plans exactly as before.
