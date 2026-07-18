# DESIGN-CTE-RECURSIVE — `WITH RECURSIVE` (recursive CTEs)

**Status: design (2026-07-18). Extends the non-recursive CTE support in
[DESIGN-CTE.md](DESIGN-CTE.md). Gated on #74 (runtime budget) — SHIPPED — which is the
termination backstop this feature needs. This is the last SQL-surface gap for
Turing-completeness / 100% sqlite parity (#75).**

## 0. Why this one needs #74 first

Non-recursive CTEs flatten onto their base tables at bind time (DESIGN-CTE.md). A recursive CTE is a
different mechanism entirely: a **fixpoint iteration**. `WITH RECURSIVE t AS (… UNION ALL SELECT …
FROM t)` can loop forever by construction (`SELECT x+1 FROM t` never stops) — window functions +
recursive CTEs are exactly what make SQL Turing-complete. #74's deterministic work counter is the
safety net: an unbounded recursion aborts at `max_work_rows` with `Error::RuntimeBudget { which:
"recursive CTE \"t\"" }` (the attribution slot #74 already reserved) — same count on every machine,
never a wall-clock flake. **The counter, not a timeout, is the loop guard** (this repo's ethos).

## 1. Syntax + shape

```
WITH RECURSIVE t(c1, c2, …) AS (
    <anchor-select>              -- non-recursive; does NOT reference t
  UNION [ALL]
    <recursive-select>          -- references t exactly once, in FROM
)
<outer statement using t>
```

- The **column list is REQUIRED** for a recursive CTE (sqlite enforces this) — the recursive term's
  reference to `t` binds to these names, and the anchor's projection must be arity-compatible.
- Compound operator is `UNION` (dedup against the full accumulated result) or `UNION ALL` (keep
  every row). Both supported. `INTERSECT`/`EXCEPT` between anchor and recursive term: refused.
- The anchor may itself be a compound of non-recursive selects (`a UNION b UNION ALL <recursive>`);
  the recursive term is the arm(s) that reference `t`. Stage 1 supports the common single-anchor /
  single-recursive-arm shape; multi-arm is a clean refusal note if it sprawls.

## 2. Evaluation — semi-naive fixpoint (matches sqlite exactly)

sqlite's algorithm (reproduce row-for-row):
1. Evaluate `<anchor-select>` → append rows to the **result** and to the **queue** (working table).
2. While the queue is non-empty:
   a. Take the queue as the current binding of `t` (the recursive term sees **only the rows added
      in the previous step**, not all of `t` — this is semi-naive evaluation, and it is what makes
      the row counts match sqlite, not just the final set).
   b. Evaluate `<recursive-select>` with `t` = queue.
   c. For `UNION` (not ALL): drop rows already present in the result (dedup on the full row tuple).
      For `UNION ALL`: keep all.
   d. Append surviving rows to the result and to the **next** queue; swap queues.
3. Stop when a step adds no new rows (natural fixpoint), or the outer `LIMIT` is satisfied, or the
   #74 work counter trips.
- **Queue discipline is FIFO (breadth-first)** — sqlite's default; a plain `SELECT * FROM t` returns
  rows in insertion order. (sqlite's `ORDER BY` inside the recursive CTE to switch to depth-first is
  a stage-2 refinement; stage 1 is FIFO, verified against sqlite's default output order.)
- `LIMIT`/`OFFSET` on the outer statement bounds the iteration: sqlite stops producing once the
  limit is met even if the fixpoint hasn't closed — support this (it is the idiomatic way to make an
  infinite generator finite, e.g. `WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c)
  SELECT x FROM c LIMIT 10`).

## 3. Restrictions (match sqlite; each a clean refusal, never a wrong answer)

The recursive term must reference `t`:
- **exactly once**, and in a `FROM`/`JOIN` operand — not twice, not in a subquery, not on the null-
  supplying side of an outer join, not as the right operand of `LEFT JOIN` where it would be
  null-extended. Refuse the disallowed positions by name.
- not inside an aggregate, `GROUP BY`, `DISTINCT`, or a window function within the recursive term
  (sqlite restrictions). Refuse cleanly.
- **Mutual recursion** (two CTEs referencing each other) and multiple recursive references: refused.

## 4. Plan + format

- New plan node `RecursiveCte { name, columns, anchor: SelectPlan, recursive: SelectPlan, union_all:
  bool }` referenced as a FROM source. The recursive `SelectPlan` carries a distinguished
  **recursive-working-table source** (a new access-path/source kind) that exec binds to the current
  queue at each step.
- `PLAN_FORMAT` bump — lands AFTER FTS's format 25, so this is **26**. Canonical encode/decode/
  validate + `explain.rs` rendering; fully re-validating decode; truncation-at-every-offset test for
  the new bytes. Validate-enforce the §3 restrictions at decode so a hand-crafted plan can't smuggle
  an illegal recursive reference past the binder.

## 5. Exec + the #74 hook

- Executor (`crates/mpedb/src/exec/`, likely a new `exec/recursive.rs`): materialize the result set
  and two queue buffers; run the fixpoint loop of §2; for `UNION` dedup with a row-hash set.
- **Charge `TxnCtx::charge_work` once per row produced by the recursive term** (before dedup, so the
  count is data-driven and env-independent — the same discipline #74 used for the correlated loop).
  This is the deterministic termination guarantee: an unbounded `UNION ALL` recursion trips the
  budget at a fixed count with the `recursive CTE "<name>"` attribution.
- MVCC/txn: the whole fixpoint runs inside one read snapshot (the CTE is statement-scoped, derived
  entirely from the snapshot's base rows) — no durability interaction, no new commit-path code.

## 6. MPEE / risk (#74 layer 1)

A recursive CTE's output cardinality is **not statically boundable** (it is data-dependent, the
halting-problem shadow). So `risk.rs` reports a recursive CTE as "unbounded unless an outer `LIMIT`
is present", and defers to the runtime work counter for the actual guard — the honest position, and
exactly the "MPEE gives a risk answer at prepare when a runaway is likely" the user asked for: a
`UNION ALL` recursive CTE with no `LIMIT` is flagged at prepare as a probable runaway.

## 7. Stage 1 deliverable + tests

Ship: `WITH RECURSIVE t(cols) AS (<anchor> UNION|UNION ALL <recursive>) <outer>` with the FIFO
fixpoint, `UNION`/`UNION ALL`, outer `LIMIT` bound, the §3 refusals, and the #74 charge. Differential
tests vs sqlite 3.45: the counting generator (`… LIMIT n`), a numbers/Fibonacci sequence, a tree/
graph transitive closure (`edges` table → reachable set), `UNION` dedup vs `UNION ALL` multiplicity,
insertion-order output, and a deliberately-unbounded `UNION ALL` with a tiny configured
`max_work_rows` asserting `Error::RuntimeBudget` with the `recursive CTE "…"` attribution — at the
same `used` count on repeat runs (determinism). Update `COMPAT.md`: `WITH RECURSIVE` ❌ → 🚧/✅.
Stage 2: depth-first via `ORDER BY` in the recursive term, multi-arm anchors.
