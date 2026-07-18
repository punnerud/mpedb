# DESIGN-VIEW — `CREATE VIEW` (#73)

**Status: building.** `CREATE VIEW` is the single largest remaining sqllogictest
blocker (43 746 blocked statements over the full corpus). A view is a named
`SELECT`; a query that names it in `FROM` reads the view's rows as if it were a
table.

## 0. Approach: flatten, don't materialize

mpedb has no derived-table (subquery-in-`FROM`) machinery, and a view is exactly
that. Two ways to close the gap:

- **Materialize** — run the view's `SELECT`, stash the rows, scan them. Needs a
  table-valued row source the planner/executor don't have.
- **Flatten (chosen)** — at bind time, splice the view's definition into the
  referencing statement: read the view's base table, AND-merge the view's `WHERE`
  with the outer one, and rewrite outer column references through the view's
  projection. A view-over-view recurses until it reaches a base table.

Flattening reuses the entire existing single-table planner unchanged and adds
**zero** plan-format or executor surface. Its cost is a bounded grammar: it works
only for **simple views** and refuses the rest — never a wrong answer.

## 1. What a simple view is (the flatten grammar)

A view `CREATE VIEW v AS SELECT <proj> FROM <base> [WHERE <vpred>]` is flattenable
iff:

- exactly one `FROM` table (a base table **or** another simple view — recurse),
- no `JOIN`, `GROUP BY`, `HAVING`, `DISTINCT`, `LIMIT`/`OFFSET`, aggregate, or
  `ORDER BY` in the view body,
- `<proj>` is `*` or a list of expressions, each given an output name (a bare
  column keeps its name; an expression needs `AS name`, else it is unnameable and
  the view is refused — matching "a view column must have a name").

Anything else (`CREATE VIEW ... AS SELECT ... GROUP BY ...`, a join view, a
compound view) is stored but **refused at reference time** with a message. This
is honest: the view exists, but querying it says what is unsupported.

## 2. Flatten rule

Referencing statement `SELECT <ocols> FROM v [alias] WHERE <opred> ...`:

1. Resolve `v` → its stored `SELECT` AST. Recurse if its `FROM` is itself a view.
2. Build `name → expr` from the view's projection (output column name → its
   defining expression over the base). `*` maps every base column to itself.
3. Rewrite every column reference in `<ocols>`, `<opred>`, `ORDER BY`, etc. that
   targets a view column (bare `c` or `v.c`/`alias.c`) to the mapped expression.
   A reference to a name the view does not expose is "no such column" (the view
   hides the base's other columns — sqlite's rule).
4. Emit `SELECT <ocols'> FROM <base> WHERE <vpred> AND <opred'> <rest>`.
5. Plan the rewritten statement with the ordinary planner.

`GROUP BY`/aggregates/joins in the OUTER query are fine — only the VIEW body is
constrained; the outer query is planned normally after the base substitution.

## 3. Storage

`CREATE VIEW` is DDL (facade route, never a plan), like `CREATE TABLE`. The view
definition is stored in the catalog sys-keyspace under `view/<name>` → the raw
`SELECT` source text (re-parsed at reference time, like RLS policy predicates).
No schema/canonical-bytes change — views are not tables and do not take a table
id. `DROP VIEW [IF EXISTS]` deletes the sys record. `schema_gen` is bumped so
other processes drop cached plans that inlined the view.

A name collision between a view and a table is refused (`CREATE VIEW` errors if a
table or view of that name exists; `CREATE TABLE` errors if a view exists).

## 4. Staging

- **V1** — storage (`CREATE VIEW` / `DROP VIEW` + sys record + gen bump) and the
  flatten rewrite for simple single-base projection/filter views, recursion for
  view-over-view. Refuse complex views at reference time. Differential vs sqlite.
- **V2** (later) — aggregate/join views via real derived tables, if the corpus
  residue justifies the executor work.

## 5. Refusal boundaries (never a wrong answer)

- A complex view body (agg/join/group/distinct/limit) → refuse at reference.
- A recursive view cycle (`v1` → `v2` → `v1`) → refuse (bounded recursion depth).
- `INSERT`/`UPDATE`/`DELETE` on a view → refuse (mpedb has no updatable views).
