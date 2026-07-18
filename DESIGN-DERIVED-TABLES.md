# DESIGN-DERIVED-TABLES — subquery in `FROM` (#74)

**Status: design, not built.** After live DDL, `CREATE VIEW`, `INSERT … SELECT`,
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
