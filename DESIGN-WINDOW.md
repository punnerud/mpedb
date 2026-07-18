# DESIGN-WINDOW.md — SQL window functions for mpedb

Status: **design only, not implemented.** This document specifies how to add SQL
window functions (`<fn>(args) OVER (…)`) to mpedb in stages. It is grounded in the
existing planner/plan/executor code; every "change X" below names a real file and
function. No Rust here is meant to compile — it is shape, not source.

The guiding observation: **a window function is the aggregate-planning machinery
turned inside out.** `GROUP BY` collapses N base rows into one grouped tuple
`[keys ‖ aggs]`; a window function keeps all N rows and appends a per-row column,
producing an extended tuple `[base row ‖ window results]`. mpedb already lifts
aggregates out of the projection into a synthetic tuple and re-binds the
projection over it (`planner/aggregate.rs` `lift_aggs` + `synthetic_grouped_table`
+ `rescope`). Window functions reuse that exact pattern with a row-preserving
synthetic tuple instead of a collapsing one. Nothing in `mpedb-core`, the commit
path, concurrency, footprints, or the wire-durability code is touched — window
functions are a pure front-end + in-process executor feature, read-only, computed
over rows that `gather_rows`/`gather_joined` already produced.

---

## 0. Why this is worth staging carefully

Window functions are a genuinely large feature by surface area (grammar, a new
plan sub-structure, a new executor phase, a matrix of functions × frames ×
exclusions), but they concentrate almost all of their value in a small first
slice. sqlite supports them and mpedb refuses them (COMPAT ❌). The stages below
are ordered by value/effort so the project can ship the high-value slice and stop
at any stage boundary with a coherent, honestly-scoped feature.

| stage | ships | rough effort | value |
|---|---|---|---|
| **1a** | `row_number/rank/dense_rank` OVER (PARTITION BY … ORDER BY …), default frame | medium | **highest** — top-N-per-group, dedup-keep-latest, sequencing |
| **1b** | `sum/avg/min/max/count/total OVER (…)`, default frame | medium | high — running totals, % of partition |
| **2** | explicit `ROWS`/`RANGE` frames; `ntile`, `lag/lead/first_value/last_value/nth_value` | large | medium — moving averages, offsets |
| **3** | named windows (`WINDOW w AS …`), `GROUPS` frames, `EXCLUDE` | medium-large | low — ergonomics + long-tail SQL |

**Smallest genuinely-useful slice = Stage 1a** (ranking functions only). It answers
"assign a sequence number / rank within each partition", which is the single most
requested window use case and the one that has no clean rewrite in mpedb's current
SQL subset (it currently requires a correlated `COUNT(*)` subquery per row).
Stage 1a is self-contained: it needs the grammar, the `windows` plan field, and the
ranking half of the executor phase — no `Accum`, no frames.

---

## 1. Scope by stage — what each ships and what each refuses

### Stage 1 (1a + 1b): the natural fit

**Ships**

- Ranking (1a): `row_number()`, `rank()`, `dense_rank()` — zero-arg, require `OVER`.
- Aggregate windows (1b): `sum(x)`, `avg(x)`, `min(x)`, `max(x)`, `count(x)`,
  `count(*)`, `total(x)` with an `OVER (…)` clause. Reuse `mpedb_types::AggFn` and
  `mpedb_types::Accum` verbatim — same NULL rules, same overflow-is-an-error, same
  types.
- Window spec: `OVER ( [PARTITION BY <expr>, …] [ORDER BY <expr> [ASC|DESC], …] )`.
- **Default frame only** — computed implicitly, never written:
  - window has an `ORDER BY` → `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`
    (cumulative; peers share one value — see §4).
  - window has no `ORDER BY` → the whole partition (`RANGE BETWEEN UNBOUNDED
    PRECEDING AND UNBOUNDED FOLLOWING`).
- Multiple distinct windows in one SELECT (`row_number() OVER (…a…), sum(x) OVER
  (…b…)`).
- Window functions in the **SELECT list** and in the **outer `ORDER BY`**
  (`ORDER BY rank() OVER (…)`, even when unselected — the junk-column path).
- Window functions over a single table **or over a join** (they run over whatever
  base row the gather produced — joins are free here).

**Refuses (Stage 1), each with a clean message:**

- An explicit frame clause (`ROWS`/`RANGE`/`GROUPS BETWEEN …`):
  `"explicit window frames are not supported yet (window stage 2) — only the
  default frame is available"`.
- `ntile`, `lag`, `lead`, `first_value`, `last_value`, `nth_value`, `percent_rank`,
  `cume_dist`: `"window function \`lag\` is not supported yet (window stage 2)"`.
- Named windows and the `WINDOW` clause: `"named windows (WINDOW w AS …) are not
  supported yet (window stage 3)"`.
- Window function **together with GROUP BY or any aggregate in the same SELECT**:
  `"window functions together with GROUP BY / aggregates in one SELECT are not
  supported yet"`. (SQL runs windows *after* GROUP BY/HAVING over the grouped
  tuples; deferring this keeps Stage 1 to one tuple model. See §3.4.)
- `DISTINCT` inside a window aggregate (`sum(DISTINCT x) OVER …`): `"DISTINCT is not
  allowed in a window aggregate"` (sqlite refuses it too).
- `FILTER (WHERE …)` on a window aggregate: `"FILTER on a window function is not
  supported"`.
- A window function anywhere it has no meaning — `WHERE`, `HAVING`, `GROUP BY`, an
  aggregate's argument, a CHECK/DEFAULT/RLS-policy expression, or inside another
  window's `PARTITION BY`/`ORDER BY`/argument (nested window): `"window functions
  may only appear in the SELECT list and ORDER BY"`.

### Stage 2: explicit frames and offset/positional functions

**Ships**

- Explicit frames: `{ROWS | RANGE} BETWEEN <bound> AND <bound>` and the shorthand
  `{ROWS | RANGE} <bound>` (= `BETWEEN <bound> AND CURRENT ROW`), with bounds
  `UNBOUNDED PRECEDING`, `<n> PRECEDING`, `CURRENT ROW`, `<n> FOLLOWING`,
  `UNBOUNDED FOLLOWING`. `RANGE` offset frames (`RANGE n PRECEDING`) require exactly
  one numeric `ORDER BY` key (SQL rule); `ROWS` offset frames need none.
- `ntile(n)` (Int64), `lag(x[, offset[, default]])`, `lead(…)`, `first_value(x)`,
  `last_value(x)`, `nth_value(x, n)` — argument-typed results.

**Refuses**: `GROUPS` frames, `EXCLUDE`, named windows (still stage 3).

### Stage 3: ergonomics + the long tail

**Ships**

- Named windows: `SELECT … OVER w … WINDOW w AS (PARTITION BY … ORDER BY …)`, plus
  window-reference chaining (`WINDOW w2 AS (w ORDER BY …)`). **This is a pure
  front-end desugaring** — a named window is resolved to an inline spec at bind
  time and never reaches the plan bytes (like a table alias). **No PLAN_FORMAT
  bump.**
- `GROUPS` frames and `EXCLUDE {CURRENT ROW | GROUP | TIES | NO OTHERS}` — these DO
  change the wire (frame fields), so they need a bump.
- `percent_rank()`, `cume_dist()` (Float64).

---

## 2. Parse — grammar and AST

### 2.1 Token strategy (no `token.rs` change in Stage 1)

mpedb recognizes `EXISTS`, `CAST`, `NATURAL`, `LEFT`, `CROSS`, etc. **positionally**
as bare identifier words (`Tok::Ident` compared `eq_ignore_ascii_case`), not as
reserved `Kw::` keywords, precisely so a user may still name a column `cast` or a
table `left`. Window keywords must follow the same rule — `over`, `partition`,
`rows`, `range`, `groups`, `window`, `exclude`, `following`, `preceding`,
`unbounded`, `nulls`, `first`, `last` are common column names and must NOT become
reserved. So **Stage 1 adds no `token.rs` entries**; the parser recognizes them
positionally. (The existing `Kw::Order`/`Kw::By`/`Kw::Partition`? — `PARTITION` is
not currently a `Kw`; keep it positional. `ORDER`/`BY` inside `OVER(…)` reuse the
existing `Kw::Order`/`Kw::By`.)

### 2.2 Where `OVER` attaches — `parser/expr.rs`

The attachment point is `call_suffix` (the `name(args…)` parser). After it builds
the call expression (both the aggregate `Expr::Agg` branch and the scalar/ranking
branch), peek for a bare `OVER` word followed by `(`:

```
fn call_suffix(name):
    …parse args as today… -> call_expr        // Expr::Agg | Expr::Func | ranking marker
    if peek_word("OVER") && peek_at(1) == LParen:
        self.pos += 1                          // consume OVER
        let spec = self.window_spec()?         // parses ( [PARTITION BY …] [ORDER BY …] [frame] )
        return Ok(Expr::Window { func, arg, distinct, spec })
    return call_expr
```

Two wrinkles handled here:

- **Ranking functions have no scalar meaning** without `OVER`. `row_number`, `rank`,
  `dense_rank` (and stage-2 `ntile` etc.) are not scalar functions and must not fall
  through to `Expr::Func`. Recognize them by name in `call_suffix`: if the name is a
  ranking function, require `OVER` (`"row_number() is a window function and requires
  an OVER clause"`) and require zero args (`"row_number() takes no arguments"`).
- **Aggregates gain an optional `OVER`.** Today `count(*)`/`sum(x)` produce
  `Expr::Agg`. When followed by `OVER`, they become a *window aggregate*
  `Expr::Window { func: WindowFunc::Agg(f), … }` instead. The aggregate-vs-window
  fork is exactly the presence of `OVER`, decided in one place.

`window_spec()` (new, in `parser/expr.rs` or a small `parser/window.rs`):

```
fn window_spec():
    expect LParen
    partition_by = []
    if peek_word("PARTITION"):
        consume; expect Kw::By
        partition_by = comma-list of self.expr()      // cap at MAX_ORDER_BY_ITEMS
    order_by = []
    if peek Kw::Order:
        consume; expect Kw::By
        order_by = comma-list of (self.expr(), asc/desc)   // reuse ORDER BY tail logic
    // Stage 1: any frame keyword here is a clean refusal.
    if peek_word("ROWS") || peek_word("RANGE") || peek_word("GROUPS"):
        return Err("explicit window frames are not supported yet (window stage 2)")
    expect RParen
    WindowSpecAst { partition_by, order_by /*, frame: Default */ }
```

Stage 2 replaces the frame refusal with a real `frame()` parser; Stage 3 accepts a
bare window name after `OVER` (`OVER w`) and resolves it from the `WINDOW` clause.

The `WINDOW w AS (…)` clause (stage 3) parses in `parser/select.rs` `select_core`,
between `HAVING` and `ORDER BY`, into `SelectStmt.windows: Vec<(String,
WindowSpecAst)>`.

### 2.3 AST shape — `ast.rs`

Add one expression node and its support types:

```rust
pub(crate) enum Expr {
    …existing…
    /// `<fn>(args) OVER (<spec>)`. Its own node (not Agg/Func) because it is
    /// neither a per-row scalar nor a group-collapsing aggregate: it produces one
    /// value per row from a whole partition. Conflating it with Agg is how a
    /// window function would wrongly reach the GROUP BY machinery.
    Window {
        func: WindowFunc,
        /// The aggregate/value argument. `None` for count(*) and the ranking fns.
        arg: Option<Box<Expr>>,
        /// Reserved: DISTINCT inside a window aggregate — refused in stage 1.
        distinct: bool,
        spec: WindowSpecAst,
    },
}

pub(crate) enum WindowFunc {
    RowNumber, Rank, DenseRank,          // stage 1a
    Agg(mpedb_types::AggFn),             // stage 1b (reuses the aggregate enum)
    // stage 2: Ntile, Lag, Lead, FirstValue, LastValue, NthValue
    // stage 3: PercentRank, CumeDist
}

pub(crate) struct WindowSpecAst {
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<(Expr, bool)>,     // (key, desc)
    // stage 2: pub frame: Option<FrameAst>,
}
```

`contains_agg` in `planner/aggregate.rs` already recurses over `Expr`; add an arm so
it does **not** descend into a `Window`'s argument (an aggregate inside a window is
the window's own business, like the existing "aggregate inside a subquery stops the
walk" rule). Add a parallel `contains_window(&Expr) -> bool` that recurses
everywhere EXCEPT into a nested window's spec/arg (a window inside a window is
refused — see §4).

---

## 3. Plan + execute

### 3.1 The window phase in the SQL evaluation order

sqlite/standard order, and the order mpedb must implement:

```
FROM/JOIN → WHERE → GROUP BY → HAVING → [WINDOW FUNCTIONS] → DISTINCT → ORDER BY → LIMIT/OFFSET
```

Window functions consume the post-HAVING row set and emit an extended row set;
`DISTINCT`, the outer `ORDER BY`, and `LIMIT` all run *after*. In Stage 1 (which
refuses windows + GROUP BY together) the input to the window phase is simply the
`gather_rows`/`gather_joined` output (already `WHERE ∧ policy`-filtered — the same
rows the plain projection would see).

### 3.2 Plan node — `plan/mod.rs` (PLAN_FORMAT bump to 23)

Add one field to `SelectPlan`, parallel to `aggregate`:

```rust
pub struct SelectPlan {
    …existing…
    pub aggregate: Option<Aggregation>,
    /// Window functions, in output-slot order. Empty = none. Each produces one
    /// extra column appended to the base row; the projection reads it at slot
    /// `base_width + k` via the synthetic windowed tuple (§3.3). Present only on a
    /// top-level SELECT — validate refuses it on a compound arm or an aggregate
    /// plan (stage 1).
    pub windows: Vec<WindowSpec>,
}

pub struct WindowSpec {
    pub func: WindowFunc,
    /// Aggregate/value argument, over the BASE row. `None` for count(*)/ranking.
    pub arg: Option<ExprProgram>,
    pub distinct: bool,                      // always false in stage 1
    /// PARTITION BY expressions, over the BASE row.
    pub partition_by: Vec<ExprProgram>,
    /// Window ORDER BY: (program over base row, desc).
    pub order_by: Vec<(ExprProgram, bool)>,
    // stage 2: pub frame: Frame,
}

#[repr(u8)]
pub enum WindowFunc {          // wire tags, closed like AggFn/ScalarFn
    RowNumber = 1, Rank = 2, DenseRank = 3,
    Agg(AggFn) = …,            // encode as tag 4 ‖ AggFn tag byte
}
```

Put `WindowFunc`/`WindowSpec` in `mpedb-sql`'s `plan` module (like `AccessPath`),
not `mpedb-types` — ranking is SQL-only, and the aggregate half reuses
`mpedb_types::AggFn`. `WindowFunc::Agg` result typing = the existing aggregate
typing in `synthetic_grouped_table` (Count→Int64, Avg/Total→Float64,
GroupConcat→Text, Sum/Min/Max→arg type); ranking fns → Int64, never NULL.

**Why a bump, and to what.** `encode_select` (shared by top-level SELECT, compound
arms, and `INSERT … SELECT`) gains a trailing window list, so a format-22 reader
would desync on the extra bytes exactly as every prior additive `Select` change did
(the `PLAN_FORMAT` history in `plan/mod.rs` documents this pattern). **Stage 1 =
PLAN_FORMAT 23.** If 1a ships before 1b, 1b adds the `WindowFunc::Agg` tag = a
second additive bump (24). If shipped together, one bump covers both.

Per-stage bumps:
- **Stage 1**: bump (new `windows` list). → 23 (24 if 1a/1b split).
- **Stage 2**: bump (frame fields on `WindowSpec`; new `WindowFunc` variants for
  ntile/lag/…). → next.
- **Stage 3 named windows**: **no bump** (resolved to inline specs at bind time).
- **Stage 3 GROUPS/EXCLUDE**: bump (frame-mode + exclude bytes).

### 3.3 Planner — new `planner/window.rs`, mirroring `aggregate.rs`

The structure is a direct analogue of `plan_aggregate_select`:

1. **Detect** in `planner/select.rs` `plan_select`: after `access`/`filter` are
   built and the aggregate branch is checked, compute
   `has_window = items/order_by contain a Window node`. If `has_window && has_agg`
   → refuse (§1). Else if `has_window` → route to `plan_window_select` (never the
   plain projection tail).
2. **Lift** (`lift_windows`, the analogue of `lift_aggs`): walk each SELECT item and
   each ORDER BY key; replace every `Expr::Window` with `Expr::Col("__w{k}")`,
   collecting a `Vec<WindowSpecAst-with-func>` and de-duplicating identical windows
   (same func+arg+spec → one slot, so `SELECT rank() OVER w … ORDER BY rank() OVER
   w` computes once — exactly `lift_aggs`'s slot reuse). A bare column stays a
   column; the window's own `arg`/`partition_by`/`order_by` sub-expressions are NOT
   rewritten (they bind over the base row, §3, like aggregate arguments).
3. **Compile the window's sub-expressions over the BASE row**: `arg`,
   `partition_by[i]`, `order_by[i]` each become an `ExprProgram` bound with the base
   scope (`Scope::single(table)` or the join scope) — identical to how
   `plan_aggregate_select` binds aggregate arguments and `GROUP BY` keys.
4. **Synthetic windowed table** (`synthetic_windowed_table`, analogue of
   `synthetic_grouped_table`): a `TableDef` whose columns are **the base row's
   columns followed by one `__w{k}` column per window** (typed per §3.2). Then
   `binder.rescope(Scope::single(&windowed))` and bind the rewritten
   projection/ORDER BY over it — reusing the binder so type rules/3VL/const-folding
   are the ones used everywhere.
5. **Naming**: like `plan_aggregate_select`, name each output column from the
   ORIGINAL item (its alias or a rendered form — `rank() OVER (…)`, `x`), never from
   the synthetic `__w`/base slot. **Always emit `Projection::Expr`** for lifted
   items so `exec::select_output_columns` names them correctly (a bare
   `Projection::Column(base_width+k)` would send `name_slot` past the base table's
   columns — the same reason the aggregate path never emits `Projection::Column` for
   grouped slots).
6. **order_over / junk**: set `order_over = OrderOver::Projection` (the window result
   columns live in the projection, and the sort must follow the window phase). A
   window fn in `ORDER BY` that is unselected is lifted into `__w{k}` and appended as
   a sort-only junk column via the existing `push_junk`/`order_junk` mechanism —
   again exactly the aggregate ORDER BY path. Base columns ordered-by are allowed as
   junk when not `DISTINCT`.

`plan_window_select` returns the usual `PlannedStmt` with `SelectPlan { …, windows,
aggregate: None, order_over: Projection, … }`.

Add a guard in `planner/mod.rs` next to `reject_correlated_in_aggregate` —
`reject_window_misuse` — enforcing that no window appears in WHERE/HAVING/GROUP
BY/aggregate-arg/nested-window (the direct query path runs without a decode
round-trip, so validate's mirror is not reached in-process; both must check).

### 3.4 Composition with GROUP BY, DISTINCT, ORDER BY, HAVING, LIMIT

- **GROUP BY / aggregates**: **refused together in Stage 1** (clean message, §1). A
  later stage runs the window phase over the *grouped tuples* (`exec_aggregate`'s
  output) instead of base rows — the same window-phase code, fed a different tuple.
  The plan already distinguishes tuples via `OrderOver`; the window phase would key
  off "input = grouped tuple when `aggregate.is_some()`".
- **DISTINCT**: runs after windows. Compute windows → project → dedup projected rows
  (the existing `distinct` dedup in `exec_select`, keyed on `keycode::encode_key`).
  `SELECT DISTINCT rank() OVER (…)` dedups rank values — correct. DISTINCT's rule
  "every ORDER BY key must be selected" is unchanged.
- **outer ORDER BY**: over the projection (`OrderOver::Projection`), after windows.
  Distinct from each window's *internal* ORDER BY (which only orders the window
  computation). Both coexist.
- **HAVING**: only with GROUP BY, hence refused-with-windows in Stage 1.
- **LIMIT/OFFSET**: bound output rows after windows — so **no scan LIMIT bound and
  no top-K** when `!windows.is_empty()` (the window phase needs every row).
  `exec_select` already disables those whenever `order_over != BaseRow`; windows
  force `Projection`, so this falls out for free.

### 3.5 Executor — new `exec/window.rs` + a branch in `exec/mod.rs`

`exec_select` (and `exec_select_with` for the correlated path) gains a branch: when
`!sp.windows.is_empty()`, after the gather (materialized in full — same as the
distinct/aggregate materialization) and before projection, call
`compute_windows(&mut rows, &sp.windows, params)` to turn each base row into an
extended row `[base ‖ w0..wk]`, then project over the extended rows exactly as the
grouped path projects over grouped tuples. Force projection-order sort + junk trim
(already the code path for `order_over == Projection`).

`compute_windows` (new):

```
compute_windows(rows: &mut Vec<Vec<Value>>, windows, params):
    n = rows.len()
    // Pre-extend every row with k NULL placeholders (slots base_width..base_width+k).
    for each window k in 0..K:
        // 1. Per-row partition key and order key, evaluated over the BASE row.
        keys[i] = encode_key(partition_by[j].eval(rows[i], params) for j)   // NULLs group
        // 2. Stable index sort by (partition_key, window_order_by).
        //    order comparison uses Value::sql_cmp with NULLS FIRST asc / reverse for
        //    desc — the exact cmp_order used by sort_rows; stability keeps ties in
        //    gather (PK/scan) order, matching row_number's tie-break.
        idx = (0..n) stable-sorted by (keys, order_cmp)
        // 3. One left-to-right pass over idx, resetting at each partition boundary.
        assign_window_values(window_k, idx, rows, params)   // writes rows[idx].push slot
```

`assign_window_values` per function family:

- **row_number**: counter starting at 1 within each partition, ++ per row in `idx`
  order.
- **rank**: track position-in-partition `p` (1-based). At a new *peer group* (order
  keys differ from the previous row, by the same comparison as the sort), set the
  running rank to `p`; peers share it. (Standard rank: gaps after ties.)
- **dense_rank**: ++ a per-partition counter at each new peer group; no gaps.
- **aggregate window** (`WindowFunc::Agg(f)`), default frame — reuse
  `mpedb_types::Accum`:
  - **window has ORDER BY** (cumulative, `RANGE … CURRENT ROW`): walk peer group by
    peer group; for each peer group first `push` every peer's `arg` value into the
    `Accum` (or `push(None)` for count(\*)), then assign `accum.clone().finish()` to
    **every** row in that peer group (peers share the cumulative value through the
    end of their group — the RANGE-vs-ROWS distinction that matters for ties). Reset
    the `Accum` at each partition boundary. `Accum` is `Clone`, so a non-consuming
    snapshot is `accum.clone().finish()`.
  - **window has no ORDER BY** (whole partition): one pass to `push` the whole
    partition, one `finish()` value assigned to every row in it.

Ranking fns need no `Accum`; that is why Stage 1a is buildable without 1b.

Output order: `compute_windows` sorts *indices*, never the `rows` vector, and writes
each result back at its original index — so the base rows stay in gather order and
the outer `ORDER BY` (over the projection) decides final order. Absent an outer
ORDER BY the output is gather order, which mpedb already documents as the only
non-guarantee (sqllogictest window cases carry an ORDER BY); note this as a
deliberate, documented incidental-order choice rather than a bug.

### 3.6 Encode / decode / validate / explain

- `plan/encode.rs` `encode_select`: after the `aggregate` block, write
  `w_u16(windows.len())` then each `WindowSpec` (func tag [+ AggFn byte], optional
  `arg` program, `distinct` byte, a `partition_by` program list, an `order_by`
  `(program, desc)` list). Compound arms / INSERT…SELECT sources encode an empty
  list (planner never puts windows there).
- `plan/decode.rs` `decode_select`: mirror, with the standard bounded reads — cap
  `windows.len()` (reuse `MAX_SELECT_ITEMS`), cap each list at `MAX_ORDER_BY_ITEMS`,
  reject an unknown `WindowFunc` tag as `Corrupt` (closed enum, like `AggFn::from_tag`
  / `SubPlanKind::from_tag`), reject `distinct && arg.is_none()` and `distinct` at all
  in Stage 1.
- `plan/validate.rs` `validate_select`: when `!windows.is_empty()`:
  - refuse if `aggregate.is_some()` (`"windows with an aggregate"`), if it is a
    compound arm (add the check next to the existing `post_filter`/`order_by` arm
    guards), or if `distinct` is set (stage 1).
  - bound every window sub-program (`arg`, each `partition_by`, each `order_by`)
    against `base_width` via `check_program_width` (they read the base/joined row).
  - the projection and outer ORDER BY are bounded against
    `base_width + windows.len()` — extend the existing `order_width`/projection
    checks so the window result slots `base_width..base_width+K` are in range (today
    a plain SELECT bounds the projection by `base_width`; windows widen it, exactly as
    the aggregate branch bounds the projection by the grouped `out_width`).
  - Footprint: **no change** — `select_footprint` destructures `SelectPlan { table,
    access, joins, .. }` with `..`, so the new field is invisible to it, and windows
    add no table/index/key access. `compute_footprint`'s recompute-and-compare in
    `validate` therefore stays consistent for free.
- `plan/explain.rs`: render the window list (`WINDOW row_number() OVER (PARTITION BY
  … ORDER BY …)`) so `EXPLAIN` shows the phase, like it shows joins/aggregates.

---

## 4. Correctness, NULLs, ties, refusals

- **Rigid typing.** Ranking fns are `Int64`, never NULL. Aggregate windows adopt the
  aggregate result types and raise on the same errors (`sum` of text, integer `sum`
  overflow) via the reused `Accum` — mpedb stays strict exactly where the aggregate
  path is strict, no new dialect. `avg`/`total` → `Float64`; `count` → `Int64`.
- **Partitioning NULLs.** `PARTITION BY` groups NULLs together (they share one
  partition), using `keycode::encode_key` of the partition expressions — the same
  total, NULL-equal keying `GROUP BY` uses in `exec_aggregate`. This is SQL's rule
  (partitioning treats NULLs as equal, unlike `=`).
- **Window ORDER BY NULLs.** Uses `Value::sql_cmp` with **NULLS FIRST for ASC**,
  reversed (NULLS LAST) for DESC — the exact `cmp_order` semantics of `sort_rows`.
  This matches sqlite's window ORDER BY default. `NULLS FIRST/LAST` overrides are a
  later addition (not Stage 1); document the default.
- **Ties: the three ranking functions differ, and the difference is the point.**
  For rows equal on all window ORDER BY keys (peers):
  - `row_number()` — distinct sequential numbers; ties broken by sort stability =
    gather/PK order (deterministic, matching the top-K tiebreak in `exec/mod.rs`).
  - `rank()` — all peers get the position of the group's first row; the next group
    skips (1,1,3).
  - `dense_rank()` — all peers get the same rank; no gaps (1,1,2).
  - Aggregate windows under the default (RANGE) frame — all peers get the SAME
    cumulative value (the running total through the end of their peer group), NOT a
    row-by-row running total. This RANGE-vs-ROWS distinction is a classic
    correctness trap and is why the executor accumulates a whole peer group before
    assigning.
- **Determinism across processes.** Every input is a pure `ExprProgram` over the
  row; partitioning/ordering use the canonical keycode/`sql_cmp`; ties resolve by
  stable gather order. So the same plan hash yields the same window results in every
  process — the content-hashed-plan contract holds.
- **Refusals (Stage 1)** — all listed in §1; each is a `bind_err` at plan time with a
  message naming the stage that will ship it, and each is mirrored in `validate` (for
  a decoded blob) as `Corrupt`/refusal so the direct path and the registry path agree.

---

## 5. Exact files/functions to change, per stage

### Stage 1 (PLAN_FORMAT → 23; 1a alone → 23, 1b adds a tag → 24 if split)

| file | change |
|---|---|
| `crates/mpedb-sql/src/parser/expr.rs` | `call_suffix`: recognize ranking fns; attach `OVER` to aggregates/ranking → `Expr::Window`. Add `window_spec()` (PARTITION BY / ORDER BY; refuse frames). |
| `crates/mpedb-sql/src/ast.rs` | add `Expr::Window`, `WindowFunc`, `WindowSpecAst`. |
| `crates/mpedb-sql/src/planner/aggregate.rs` | `contains_agg`: don't descend into a `Window` arg. |
| `crates/mpedb-sql/src/planner/window.rs` | **new**: `contains_window`, `lift_windows`, `synthetic_windowed_table`, `plan_window_select`. |
| `crates/mpedb-sql/src/planner/select.rs` | `plan_select`: detect windows; refuse windows+aggregate; route to `plan_window_select`; force `OrderOver::Projection`. |
| `crates/mpedb-sql/src/planner/mod.rs` | wire `mod window`; add `reject_window_misuse` guard (WHERE/HAVING/GROUP BY/agg-arg/nested). |
| `crates/mpedb-sql/src/plan/mod.rs` | add `SelectPlan.windows`, `WindowSpec`, `WindowFunc`; **bump `PLAN_FORMAT`** with a doc-comment entry. |
| `crates/mpedb-sql/src/plan/encode.rs` | `encode_select`: encode window list. |
| `crates/mpedb-sql/src/plan/decode.rs` | `decode_select`: decode window list, bounded; reject bad tags/`distinct`. |
| `crates/mpedb-sql/src/plan/validate.rs` | `validate_select`: refuse windows+aggregate / on arms / distinct; bound sub-programs by `base_width`; widen projection/order bound to `base_width + K`. |
| `crates/mpedb-sql/src/plan/explain.rs` | render windows. |
| `crates/mpedb/src/exec/window.rs` | **new**: `compute_windows` + `assign_window_values` (ranking + `Accum`-based aggregate). |
| `crates/mpedb/src/exec/mod.rs` | `exec_select`/`exec_select_with`: window branch (materialize, `compute_windows`, project over extended rows, projection-sort). |
| `COMPAT.md` | flip window functions ❌ → partial, listing the Stage-1 subset and refusals. |

No changes to: `mpedb-core` (engine/commit/concurrency), `mpedb-types` (Stage 1
reuses `AggFn`/`Accum`; no new shared type), `footprint.rs` (`..` destructure +
read-only, key-neutral), the ring/durability/WAL code. Window functions are entirely
front-end + in-process executor.

### Stage 2 (PLAN_FORMAT bump)

`parser` (frame grammar, ntile/lag/lead/first_value/last_value/nth_value names),
`ast.rs` (`FrameAst`, new `WindowFunc` variants), `plan/mod.rs` (`Frame` on
`WindowSpec`, new tags), `encode/decode/validate` (frame bytes + bounds:
`RANGE offset` needs exactly one numeric ORDER BY key), `exec/window.rs` (frame
evaluation — sliding `ROWS`/`RANGE` bounds; offset functions read the sorted
partition directly). `mpedb-types` may gain nothing (offset fns are SQL-level).

### Stage 3

Named windows: `parser/select.rs` (`WINDOW` clause) + `planner/window.rs` (resolve
names to inline specs before encoding) — **no wire change, no PLAN_FORMAT bump**.
`GROUPS` frames + `EXCLUDE`: `ast`/`plan`/`encode`/`decode`/`validate`/`exec` (frame
mode + exclude byte) — **bump**.

---

## 6. Summary

Window functions map cleanly onto mpedb's existing aggregate-planning pattern
(`lift_aggs` + `synthetic_grouped_table` + `rescope`), reuse `Accum` and the
keycode/`sql_cmp` ordering primitives verbatim, and touch none of the load-bearing
concurrency/commit/durability code. The work is real but additive and low-risk in
the dimensions that matter for this codebase. Ship **Stage 1a** (ranking) first — it
is the smallest slice that removes a genuine capability gap (rank/sequence within a
partition, currently only expressible as a per-row correlated subquery) — then
Stage 1b (aggregate windows), gated behind PLAN_FORMAT 23. Frames and offset
functions (Stage 2) and named windows / GROUPS / EXCLUDE (Stage 3) follow, each with
the format-bump discipline the plan registry already enforces, and named windows
notably needing no bump at all.
