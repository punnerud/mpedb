# DESIGN-RUNTIME-BUDGET (#74)

A per-statement-execution **runtime budget** that aborts a runaway query
deterministically, and a prepare-time **risk estimate** that flags a query as
dangerous *before* it runs. It is the deterministic loop-counter that #75
(recursive CTEs) will run under.

No compiled-plan bytes change and `PLAN_FORMAT` stays **23**: layer 1 is a pure
runtime/analysis pass that reads the already-decoded plan plus the catalog's
exact row counts; nothing is persisted into plan bytes.

## Why a count, never a clock

The repo's ethos is deterministic count-based limits (`MAX_VIEW_DEPTH = 16`,
`MAX_SUBPLANS = 16`, `MAX_EXPR_DEPTH = 2000`, `MAX_TRIGGER_DEPTH = 32`,
`MAX_DROP_PAGES`, …). The budget follows the same rule: it counts **work rows**,
not wall-clock time.

A work counter makes every abort **reproducible**. The same query over the same
data aborts at the *exact same count* on every machine — a fast server and a
loaded CI box abort identically. That is debuggable (the `used` count and the
`which` attribution point at the culprit), regression-testable (an assertion can
pin the count), and immune to the "passes locally, times out in CI" flakiness a
timeout guarantees. A timeout measures the machine; a work counter measures the
query.

Determinism requirement: every increment is **data-driven** — one tick per row a
scan yields, per nested-loop join candidate, and per correlated-subquery
re-evaluation. Never time, never random, never dependent on an env flag (in
particular the counter is charged per outer row of a correlated subquery
*before* the memo lookup, so `MPEDB_NO_SUBPLAN_MEMO` does not change the count).

## The three layers

### Layer 2 — the deterministic work counter (the core)

A per-transaction-execution counter of **work rows** lives in the engine
(`crates/mpedb-core`), the cheapest correct home: it sits at the row-yield hot
points, so nothing above it has to remember to count.

- `WorkMeter { budget: u64, used: AtomicU64 }` (engine/mod.rs). `charge(n, which)`
  adds `n` to `used` (Relaxed — one transaction runs on one thread; there is no
  cross-thread ordering to establish, only a running sum) and returns
  `Error::RuntimeBudget` once `used > budget`. `budget == 0` is the **unlimited**
  sentinel and skips the comparison entirely.
- Both `ReadTxn` and `WriteTxn` own a `WorkMeter`, built at `begin_*` from
  `Engine::work_budget` (seeded from config). Each autocommit statement opens a
  fresh transaction, so for the common path *per-transaction-execution ==
  per-statement*. A long-lived `WriteSession` shares one budget across the
  statements it runs — a reasonable session-level cap.
- Charge sites (each a single localized bump, all funnelling through the same
  meter):
  - **Scan/cursor layer** (`RowCursor::next`, and the materializing
    `scan_by_index` / `scan_by_index_range` / `scan_rows_raw` loops on both txn
    kinds): +1 per row yielded — `which = scan of table "<t>"`.
  - **Nested-loop join** (`exec/gather.rs::gather_joined`): +1 per inner
    candidate considered — this is the O(n·m) work of a cross join, which is
    otherwise an in-memory product a held inner scan never re-reads —
    `which = nested-loop join with "<t>"`.
  - **Correlated subquery** (`exec/mod.rs::correlated_survivors`, the existing
    per-outer-row loop): +1 per outer row — `which = correlated subquery over
    "<t>"`. The inner subplan's own scans additionally charge through the scan
    layer, so an N-outer × M-inner correlated bomb is counted as ~N·M.
  - **(reserved for #75)** a recursive-CTE iteration will charge +1 per produced
    row with `which = recursive CTE "<name>"` — same meter, same abort.
- Point lookups (`get_by_pk`, `get_by_index` single-row) deliberately do **not**
  charge: they are O(1), and the scan that drives them is already counted. This
  also keeps attribution honest — a correlated subquery whose inner is a PK point
  trips on the correlated-subquery counter, not on a scan.

`charge()` / `work_used()` / `work_budget()` are public on both txn kinds and
exposed to the SQL executor through one new `TxnCtx::charge_work` trait method,
so the exec-layer bumps hit the very same counter as the engine scans.

### Layer 3 — the attributed error

New variant, distinct from `Corrupt`:

```rust
Error::RuntimeBudget { limit: u64, used: u64, which: String }
```

`which` names *where the work went* — a coarse-but-correct label built lazily at
the trip site (only on the error path). Display tells the user how to fix it:

```
runtime budget exceeded: {used} work-rows > limit {limit} while evaluating
{which}; raise [runtime] max_work_rows in the config to allow more
```

### Layer 1 — the MPEE-style prepare-time risk estimate

`crates/mpedb/src/risk.rs` (a new file, so the parallel window-function planner
work is untouched): a read-only function

```rust
pub fn estimate_plan_risk(plan: &CompiledPlan, row_count: &dyn Fn(u32) -> u64)
    -> RiskEstimate
```

walks the decoded plan and multiplies cardinalities among its scans, joins and
correlated subplans, using the catalog's **transactionally-exact** per-table row
counts (`ReadTxn::row_count`) — not a histogram guess. A FullScan/range
contributes `row_count(table)`; a PK/unique-full point contributes 1; a join
multiplies the running product by its inner card; a correlated subplan multiplies
by its inner select's card (a re-evaluation per outer row). All math is
saturating. It returns the worst-case `work_rows`, the **dominant** contributing
node, and that node's running contribution — the attribution MPEE wants "at the
start".

Surface: `Database::estimate_risk_sql(sql)` and `estimate_plan_risk` are public.
At prepare/query time, for a plan that structurally multiplies (has a join or a
correlated subplan) the facade computes the estimate and **logs a warning** when
it exceeds the warn threshold (`max_work_rows` when finite, else a fixed
ceiling). A **hard refuse** before executing is available but off by default:
`RiskEstimate::exceeds(budget)` is the hook — a deployment that wants
fail-before-run calls it and returns `RuntimeBudget` at prepare time.

### Layer 2b — the join-materialization cell budget (`max_join_cells`)

The work-row counter bounds what a query READS; it cannot see what a join
HOLDS. `select5.test`'s `join-17-4` (a 17-way comma join whose only constant
anchor is the 16th of 17 conjuncts) materializes gigabytes of intermediate
product while still far under the 10^9 work-row default — the process dies on
an allocation abort (or the OOM killer) before the counter ever trips. The
memory-proportional twin:

- `JoinCells { budget, live }` lives in `exec/gather.rs`, per
  `gather_joined` call (memory is per-pipeline, not per-statement): `live`
  counts the `Value` cells currently held — the accumulated tuple set, the
  held inner side, and the next stage being built. Charged per RETAINED
  joined row (`+row width`; ON-rejected candidates are transient and not
  charged), per held-inner gather (`+rows × width`), and RELEASED when a
  stage is superseded — so a legitimate multi-step join is charged for its
  peak footprint, not its history. Deterministic: a pure function of data
  and plan, same trip point on every machine.
- The limit reaches the executor through `TxnCtx::join_cells_budget()`
  (engine-seeded from config, like the work meter; `0` for the
  sqlite-backed contexts, mirroring `charge_work`'s scoping).
- Trips as `Error::RuntimeBudget { kind: BudgetKind::JoinCells, .. }`;
  `BudgetKind` carries the unit ("live joined cells") and the knob
  (`max_join_cells`), so the Display hint always names the right knob.
- Fallible allocation, two tiers. The accumulator's LARGE spines (the
  `next` stage vec, the survivor vec) use `try_reserve` ALWAYS — one
  predicted branch per retained row buys a clean `Error::OutOfMemory`
  (capi: `SQLITE_NOMEM`) when the single biggest allocation hits a memory
  rlimit / cgroup cap. The PER-ROW allocations (the row spine, each
  text/blob payload clone) are fallible only under the explicit
  `max_join_cells = 0` opt-out: with a finite budget the deterministic cap
  is the guard, and the O(n·m) candidate loop keeps the plain
  `with_capacity` + `extend_from_slice` build it always had (measured: the
  always-fallible per-value build taxed a 400M-candidate join ~3-7%).
  The backstop is best-effort by nature — a small foreign allocation at
  the very wall can still abort, and Linux overcommit can OOM-kill before
  malloc ever fails — which is exactly why the deterministic cell budget
  is the primary guard and ships with a finite default: pick a
  `max_join_cells` that fits the deployment's memory ceiling (~40 B
  resident per cell on the corpus shapes) and the join errors before the
  wall.

## Config

New top-level TOML section:

```toml
[runtime]
max_work_rows = 1000000000   # 0 = unlimited
max_join_cells = 268435456   # 0 = unlimited
```

`DbOptions::max_work_rows: u64`. Absent ⇒ `DEFAULT_MAX_WORK_ROWS = 1_000_000_000`.
`DbOptions::max_join_cells: u64`. Absent ⇒ `DEFAULT_MAX_JOIN_CELLS` (calibrated
against the heaviest legitimate corpus query — see the constant's doc).

**Default rationale.** One billion work rows is far above any legitimate OLTP or
report query on an embedded database (a 1e9-row scan on this engine is already
minutes of work and a multi-GB file), yet a genuine runaway — an accidental
cross join of two large tables, an unbounded correlated subquery — crosses it
long before it exhausts memory or wedges the process. It is a backstop, not a
quota: normal queries never come close. `0` means unlimited (a deliberate opt-out
for a trusted batch process); it is a sentinel rather than "off by accident"
because the default is finite, so a runaway is caught unless someone explicitly
turns the guard off.

## The #75 hook

Recursive CTE iteration is exactly the unbounded loop the budget exists to make
safe. #75 charges the shared meter once per row a recursion step produces
(`which = recursive CTE "<name>"`); no new machinery, no new error, no format
change — the deterministic count already gives a recursive query a reproducible
abort point instead of an infinite loop.
