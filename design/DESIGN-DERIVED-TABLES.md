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
- `current_setting()` — and the literal `'now'`, which rides the same reserved
  slot — in the body or the outer are stage-1 refusals (same reserved-slot
  reconciliation cost across components as the recursive CTE).
- Lifted subqueries in the body are **no longer refused** — see §5.5. In the
  OUTER statement they still are.

## 5.5 Body-OWNED subplans (2026-07-20, PLAN_FORMAT 52)

Stage A shipped with "no lifted subqueries in either component". The refusal
was one line, but it blocked the single largest remaining Django shape:

```sql
SELECT count(*) FROM (
  SELECT …, EXISTS (SELECT 1 FROM i WHERE i.x = t.y) AS f
  FROM t
) s WHERE f
```

`.annotate(Exists(...))` followed by `.aggregate()` / `.filter()`. The body
projects a correlated `EXISTS` under an alias; the outer consumes it as an
ordinary column.

**Why the flat statement-level `subplans` list could not hold it.** The
executor fills `plan.subplans` in two places: uncorrelated ones ONCE before
dispatch, correlated ones PER OUTER ROW after the gather. For a derived plan
the "outer row" is a row of the MATERIALISED set, read through `CTE_TABLE` —
it has no base-table columns, so a slot correlated to `t.y` has no meaning
there. Putting the body's lifts on the statement would either fill them
against the wrong row or leave them as unfilled holes.

**The fix is ownership, not a new mechanism.** `DerivedPlan` carries
`body_subplans: Vec<SubPlan>` and `body_sub_base: u16`; the parameter layout
becomes `[user ‖ body subplans]`. `exec_derived` runs *exactly the discipline
`exec_stmt_impl` runs one level up*, against the body: fill the uncorrelated
lifts once, then execute the body through `exec_select_leveled` with
`(base = body_sub_base, subplans = body_subplans)` so the correlated ones are
filled per BODY row after the body's own gather. By the time the outer runs,
`f` is a materialised column and there is nothing left to fill — which is why
the statement-level list stays EMPTY, and `validate` enforces that it does.

Everything the two levels share is shared as CODE, not duplicated:
`check_slot_discipline`, `validate_subplan_rec`, `run_subplan`,
`exec_select_leveled`. A hand-crafted blob therefore cannot buy a weaker check
by moving its lifts onto the body. `validate_body_subplans` additionally pins
the layout (`base + len == n_params`, no context slots) and derives the
correlation row from the BODY's `[table0 ‖ … ‖ tableN]` — the one place a
forged `outer_arg` would otherwise read the wrong column.

**Still refused, by name:** lifts in the OUTER statement (they would number
their slots from the same base the body's occupy). Lifts in a COMPOUND body
were refused one stage longer — closed by §5.6, which pushes the same ownership
one level further down, to the ARM.

**Accounting.** A body-owned lift reserves a parameter slot exactly as a
statement-level one does, so `CompiledPlan::n_subplan_slots()` counts both and
`resolve_params` subtracts both — miss it and the facade demands one parameter
too many. The 16-subplan DoS ceiling and the footprint's table-read union
likewise count the body's tree.

Simple aliased projection/filter bodies still take the Stage-B flatten (better
plans: index access paths); materialization is the fallback, so every
previously-answered shape plans exactly as before.

## 5.6 ARM-owned compound lifts, and a correlated compound body (2026-07-20, PLAN_FORMAT 56)

§5.5's move transferred. Three refusals fell to it, and they are one refusal:

```sql
-- (1) a correlated subquery INSIDE a compound arm
SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a) UNION SELECT id FROM u
-- (2) a compound SUBQUERY BODY whose arms reach OUT
SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.b = t.a UNION SELECT 1 FROM u WHERE u.id = t.id)
-- (3) a lift inside a COMPOUND derived-table body
SELECT count(*) FROM (SELECT a FROM t WHERE a IN (SELECT b FROM u) UNION SELECT id FROM u) s
```

**The same argument as §5.5, one level down.** A compound has no outer row *at
all*: `exec_compound` runs each arm over its own rows and combines the
projected sets. So a lift HOISTED onto the enclosing statement (which is what
format 49 did with an arm's lifts) can only be filled once, before dispatch —
which is fine for an uncorrelated lift and *meaningless* for a correlated one.
That is why the correlated case was refused, and it is the identical shape as
"the derived body's outer row is a materialised `CTE_TABLE` row with no base
columns".

**Ownership.** `CompoundPlan` carries `arm_subplans: Vec<Vec<SubPlan>>` and one
`arm_sub_base: u16`; arm `k`'s lifts occupy `arm_base(k) + i`, with
`arm_base(k) = arm_sub_base + Σ_{j<k} |arm_subplans[j]|` — the layout
`[level params ‖ arm0 ‖ arm1 ‖ …]` each arm was already numbered against at
plan time, now stored instead of derived. `exec_compound_arm` runs *exactly the
discipline `exec_derived` runs one level up*: fill the arm's uncorrelated lifts
once, then `exec_select_leveled(base = arm_base(k), subplans = arm_lifts(k))`
so the correlated ones are filled per ARM row after the arm's own gather. The
statement-level `subplans` list is consequently EMPTY for a compound, and
`validate` enforces that — the same rule a derived plan follows.

**(2) is the correlation region, not ownership.** A compound *subquery body*
correlates the way a plain-SELECT body always has: one shared `Correlate` walks
every arm (its `inner_scope` swapped per arm, its `outer_args` accumulating and
deduped by outer slot), the compound is planned over `[user ‖ correlation]`, and
the whole compound is executed per outer row with the region ALREADY filled. So
each arm reads it as an ordinary parameter and nothing inside the compound needs
a per-row phase of its own. `Correlate::descend_body` descends into a nested
compound's arms too, so a transit correlation (§3.3 of DESIGN-SUBQUERY-NEXT)
crossing a compound is captured like any other.

**Shared as CODE, not copied.** `exec_select_leveled`, `run_subplan`,
`subplan_value`, `check_slot_discipline`/`check_correlated_slots`,
`validate_subplan_rec` and `select_row_types` are the same functions the
statement level and the derived body use. Two rules are *stronger* here than a
naive per-arm copy would be:

- the gather-side slot discipline runs over the WHOLE arm-lift region for EVERY
  arm — a slot owned by *another* arm is just as unfilled during this arm's
  gather, so reading one across arms is exactly as illegal as reading one's own;
- the executor rebuilds the parameter buffer from the level's params for each
  arm, so another arm's reserved slots are NULL rather than stale.

`validate_compound` additionally pins `arm_sub_base` to the caller's parameter
level (statement `n_user_params`, a subplan's `sub_base`, a derived body's
`body_sub_base`), types every arm slot from the lift's own `slot_type`, and
bounds each arm's list at `MAX_SUBPLANS` — so a forged blob cannot buy a weaker
check by moving its lifts onto an arm, and cannot point an arm's fill at live
user slots.

**Accounting.** Arm-owned lifts reserve parameter slots exactly as
statement-level ones do: `CompiledPlan::n_subplan_slots()` counts them (through
a compound statement, and through a compound derived-table body), the planner
refuses more than 16 across all arms, decode and validate draw every subplan
anywhere in the plan from ONE `MAX_SUBPLANS` tree budget, and both the footprint
and the RLS policy stamps walk the arms' trees. (The stamp walk also picked up
a §5.5 omission on the way: a derived BODY's own lifts were never stamped.)

**Still refused, by name:** `current_setting()` together with a subquery in one
compound (the context slots sit after the reserved region and no longer line up
across arms), and a derived table in a nested position — see §5.7.

## 5.7 What a derived table in a NESTED position still needs

`SELECT count(*) FROM (SELECT … FROM (SELECT DISTINCT … LIMIT 3) i …) o`, a
derived table inside a compound ARM, and one inside a lifted subquery's body all
still refuse by name ("only supported in a statement's outermost FROM"). §5.1
gave two reasons for making the derived table a STATEMENT node; §5.6 retires the
second of them ("makes a derived source representable in positions whose slot
layout is exactly what `plan_compound` refuses today" — it refuses nothing of
the sort any more). What is left is the first: `FromSource = Base | Derived`
touches every `SelectPlan` consumer.

The ownership move points at the shape: a SELECT should OWN its derived source
(`SelectPlan.derived: Option<Box<DerivedSource>>`, the body + its owned lifts,
read through `CTE_TABLE`), because a derived table belongs to the query that
reads it and to nothing else — exactly as an arm's lift belongs to the arm and a
body's lift to the body. That covers all three nested positions at once (a
compound arm, a derived body and a subquery body are each a `SelectPlan`) and
retires `PlanStmt::Derived`; the sharp edge is that `validate_select`'s
`CTE_TABLE` resolution currently comes from the STATEMENT node and would have to
come from the level being validated. Not attempted here.
