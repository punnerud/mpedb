# DESIGN-MPEE-SOLVER — MPEE as the plan solver

**Status: implemented (task #114, 2026-07-20; extended by #116 the same day —
§7.1 turns three of v1's refusals into constraints, §9.5 makes the cost side
demand-driven, §9.6 designs the execution-time half).** Companion to
[DESIGN-MPEE-OPT.md](DESIGN-MPEE-OPT.md) (what transferred from the offline
route engine and what was falsified) and [DESIGN-MPEE-COST.md](DESIGN-MPEE-COST.md)
(the self-tuning cost catalog, still Phase 7+ / #88 and still deferred).

Before this, mpedb had **no join-order solver at all**. Join order was the
user's textual order, left-deep, always; the only reorder was the semantic
RIGHT→LEFT rewrite. Access path per join was a local rule (plain equality in
the ON ⇒ probe, PK → unique → non-unique; else hold the inner, O(n·m)). The
"MPEE-style pruning" in `planner/subquery.rs` was a consumer cap, not a solver.

This document is the design of the solver: **one mechanism, invoked at every
level of the compile recursion**, that chooses the join order and therefore the
access path per position for that scope, and composes upward.

---

## 1. What MPEE is, restated for the query graph

MPEE (the offline route optimizer) never materializes the N×N distance matrix.
It **streams** the cells, it **collapses** regions that attach to the rest of
the network through only two or three connections into a single abstract node
with an interface (a roundabout with twenty internal points is one point with
an entry and an exit), and it prunes branches whose partial cost already loses.

The query graph is the same object:

| MPEE | mpedb planner |
|---|---|
| node = place | node = a table instance in one FROM scope |
| edge = road | edge = a predicate connecting two tables |
| N×N matrix | the join-order search space for that scope |
| region behind a cut vertex | a sparsely-attached subgraph of the join graph |
| roundabout collapsed to a point | a subquery / derived body / compound arm, collapsed to its interface |
| streaming the matrix | enumerating only the reachable partial orders |

DESIGN-MPEE-OPT.md §1.7 staged "hierarchical cluster-first decomposition with
boundary re-polish" as a concept with no design written, and pointed it at
**batch scheduling** (partition a drained group-commit batch by disjoint table
footprints). **That was the wrong target.** §4 of the same document measured why:
the pages copied by one COW transaction are a property of the key *set*, not of
the visit *order*, so nothing in the commit path is path-dependent and there is
nothing for a decomposition to buy. The query graph *is* path-dependent — the
cost of a left-deep plan depends on the order, not just the set — so this is
where the decomposition belongs. §1.7 is hereby promoted from concept to
implemented, retargeted from the batch to the query graph.

---

## 2. Cost input: three honest classes, and no invented constants

Morten's constraint: `row_count` is the simple input, "smart cost analysis is
the central input but that part can be kept simpler", and — the sharpening that
shaped the whole model — *"row_count er bare en simpel måte, eks LIKE søk osv
vil variere basert på type data og kan ikke alltid vite slikt før man har kjørt
det."* `row_count` says how many rows **exist**, never how many a predicate
**lets through**. So predicates are classified, and the class decides what the
solver is allowed to assume:

| class | example | what the solver knows |
|---|---|---|
| **KNOWN** | full PK equality; full-width probe of a UNIQUE index | exactly 1 row |
| **BOUNDED** | non-unique index equality; a constant anchor on a non-key column; any equality linking this table to an already-placed one | an upper bound: `row_count(t)` — and nothing tighter |
| **UNKNOWN** | `LIKE '%x%'`, `f(col) > 0`, an `any`-column comparison, a bound-parameter range | **nothing**. No constant is invented. |

BOUNDED and UNKNOWN are priced **identically** — at the full `row_count(t)`.
That is deliberate. The alternative is a magic selectivity factor, which is
exactly the thing that produces a plan that is great on the estimate and
catastrophic on the data.

**The consequence, and the point:** the solver optimises the *worst case*, not
the *expected case*. For the UNKNOWN class this is the only defensible
objective — a solver that maximises expected speed can still choose a plan that
explodes; one that bounds the worst case cannot. `join-17-4` is precisely that
situation: the problem is not that an estimate was slightly off, it is that the
textual order makes the worst case astronomical while another order makes it
finite **regardless of the data**.

### 2.1 `row_count` enters only as a magnitude bucket

The only statistic consulted is the catalog's transactionally-exact
`row_count` per table, and it is consulted only through

```
bucket(n) = 64 - n.leading_zeros()     // 0 → 0, 1 → 1, 2..3 → 2, 4..7 → 3, …
```

i.e. `⌈log2⌉`-ish magnitude. Costs are therefore **sums of logs**, and a table
has to double in size before any cost can move. This is §6's determinism
argument as much as it is a cost decision.

No histograms. No NDV. No sampling. No persisted stats catalog. If a future
change wants those, that is DESIGN-MPEE-COST.md / #88 and it stays deferred.

---

## 3. The cost vector

For a candidate left-deep order `t₀, t₁, …, t_{n−1}`, each position `i` is
scored against the set `S` of tables already placed, and the scores are summed
componentwise into a **lexicographic tuple** (a triple in v1; #116 appended the
fourth term, §7.1):

```
Cost = (worst_log, cartesian, late_unconstrained, residual_late)
```

1. **`worst_log`** — `0` if the step is KNOWN (full PK pinned, or a full-width
   UNIQUE index pinned, by constants or by columns of `S`), else `bucket(t)`.
   The sum is the log₂ of the worst-case product — the same *idea*
   `crates/mpedb/src/risk.rs` computes for the #74 budget, now used to *choose*
   a plan instead of only to *warn* about one.

   ⚠ The same idea, **not** the same code and not the same quantity. `risk.rs`
   walks a *decoded plan* and accumulates a saturating raw product of exact
   catalog row counts, in rows, to compare against `max_work_rows`. `worst_log`
   sums magnitude buckets (§2.1) over *candidate orders* that no plan exists for
   yet, in log₂, purely to rank them against each other. Two independent
   implementations in two different units, and they are meant to stay that way:
   the solver runs before there is a plan to decode, and a ranking needs only
   ordinality where a budget needs a number.

2. **`cartesian`** — `1` when `i > 0` and **no** predicate connects `tᵢ` to any
   table in `S`. A cartesian step multiplies the intermediate by the whole
   table *with certainty*; a linked step multiplies it by *at most* the whole
   table. Same upper bound, categorically different risk — and this term is
   purely structural, so it needs no statistics at all.

3. **`late_unconstrained`** — `(n − i)` when `tᵢ` has no predicate constraining
   it at all at that point (no link to `S`, no single-table filter, not KNOWN),
   else `0`. An unconstrained table inflates its own stage and every stage after
   it, so it is charged once per remaining step. This is what pushes
   certainly-full scans to the end.

4. **`residual_late`** (#116) — the position `i` at which a conjunct becomes
   evaluable, charged once per conjunct resolved at that step. #65 evaluates a
   conjunct at the step that places its LAST table, so a residual's position is
   a *choice*, and one that shrinks everything downstream when it is early.
   Purely structural. Last in the tuple, so it decides only among candidates the
   first three rate identically — the population v1 handed back to the textual
   order. See §7.1.

Ties are broken by the **textual order**: the solver's order is adopted only if
it is *strictly* better than the order the user wrote. mpedb never reorders
without a reason it can name.

Every term is an integer, every term is a sum over steps, and every step's score
depends only on `(S, tᵢ, i)`. That last property is what makes the DP in §4
correct — the cost of a set is independent of the order the set was built in.

### 3.1 Worked example — `select5.test`'s `join-17-4`

17 tables `tN(aN INTEGER PRIMARY KEY, bN, xN)`, 10 rows each, 16 equi-join
conjuncts and one constant anchor `a38 = 9` written as the 16th of 17 conjuncts.
Every conjunct has the shape `aP = bQ`: one side is a PRIMARY KEY, the other is
not. The join graph is a **tree** — in fact a **path** — and it is *directed for
free*: entering `tP` from `tQ` is a PK probe (KNOWN, 1 row), entering `tQ` from
`tP` goes through the non-key `bQ` (BOUNDED, `row_count`).

The variant that used to die (`FROM t9,t56,t53,t61,t54,t1,t27,t4,t38,…`):

| order | `worst_log` | `cartesian` | what it does |
|---|---|---|---|
| textual | **32** (8 positions un-probed × bucket 4) | **6** | six steps have no predicate linking them to anything already read; the intermediate reaches 10⁷ rows × 51 columns |
| solver | **4** (one scan, then nothing) | **0** | starts at `t27` — the one end of the path whose own PK is pinned by nobody — and walks it; all 16 following steps are PK probes |

Term 1 decides this one outright: 2⁴ against 2³². (Term 2 is what decides when
the equalities are *non-unique* on both sides, so no order can prove a smaller
bound and the worst cases tie — the case §2's UNKNOWN class describes, and the
reason the structural term exists at all.)

Note what the solver did NOT need: the anchor. `a38 = 9` makes `t38` KNOWN with
nothing placed, so it is one of the extremal seeds (§4.1) and it is what the
first refinement round finds — but the winning order starts at the *other* end
of the path and reaches `t38` last. The anchor's real contribution is that the
whole 17-way join returns one row; the ordering win is structural.

`crates/mpedb/tests/mpee_solver.rs::join_17_4_answers` pins exactly this: the
join-order line must end `(0 cartesian steps)` and carry 16 `[pk]` positions,
and the query must answer — one row. It deliberately does **not** pin the
identity of the seed: the tuple is what the anchor determines, the *ordering*
win is structural, and pinning `t27` by name would make an equally good tie-break
look like a regression.

---

## 4. Search: collapse, stream, cap

`MAX_JOINS = 16` ⇒ n ≤ 17 tables per scope (the solver's own ceiling,
`MAX_SOLVE = 24`, sits above the format cap so the solver is never the thing
that refuses a statement; `select5.test` carries comma joins up to **64** tables
wide and those are declined here and refused by plan validation). 17! is not a
search space.

The solver is a **dynamic program over subsets**, processed level by level in
increasing population count, with the state set *restricted by connectivity* —
which is the collapse and the streaming, implemented as one thing:

- **State** = the bitmask of placed tables. (Legal because the cost of a set is
  order-independent, §3.) `BTreeMap<u32, (Cost, last)>` per level — an ordered
  map, because iteration order must be identical in every process.
- **Expansion** from state `S`:
  - `n ≤ 12` (`DP_FULL_MAX`): expand to **every** unplaced table. 2¹²·12 ≈ 49 k
    transitions — exact over all left-deep orders, cartesian steps included.
  - `n > 12`: expand only to tables **adjacent** to `S` in the join graph; if
    `S` has no unplaced neighbour (a disconnected graph, or a genuine cross
    join), fall back to every unplaced table for that state only.
- **Cap**: `MAX_STATES = 20_000` live states. Exceeding it falls back to an
  extremal-seeded greedy pass using the identical scoring function (§4.2). The
  cap and the threshold are constants, not heuristics on the data, so the choice
  of algorithm is a function of the *statement*, never of the catalog — which is
  what keeps two processes on the same algorithm as well as the same cost.

**Why the connectivity restriction is the collapse.** A subgraph that attaches
to the rest of the graph through only a few edges can only ever appear in a
reachable state as a connected prefix — so the enumerated states are
proportional to the graph's decomposition rather than to 2ⁿ. For `join-17-4`'s
path graph the connected subsets number 153 instead of 131 072, and the DP is
*exact*. For a star, a chain, or the snowflake shapes real schemas produce, the
same collapse happens for free. No separate biconnected-component pass is
needed: restricting expansion to the frontier **is** cluster-first decomposition
with the boundary handled by the DP itself.

**Why it is streaming.** States are generated level by level and a dominated
state is dropped the moment a better cost for the same mask appears; the search
is bounded by what survives, never by the full product. Nothing resembling an
N×N matrix is materialized.

### 4.1 Extremal sampling and progressive refinement — how far the road analogy carries

Morten's search strategy from the route engine: *"ta helt sør, så helt nord, så
helt vest og øst — sannsynligheten er stor for at den 4×4-matrisen finner
hovedveier/knutepunkter. Legg til én til mellom alle … og da trenger man ofte
ikke kalkulere hele N×N."* Sample the extremes, solve the tiny matrix among
them, insert the points between, repeat; stop when a round stops changing the
decision.

**The query-graph analogues** (`Problem::extremes`), all read straight off the
problem, all deterministic:

| road | query graph | why it is extremal |
|---|---|---|
| compass extreme | a table already **KNOWN** with nothing placed — a CONSTANT anchor pinning its whole PK or a whole UNIQUE index | the strongest restriction a table can carry, and the round-one find that solves `join-17-4` |
| — | the **smallest** table (min bucket) | you want it early |
| — | the **largest** table (max bucket) | you want it late, or probed rather than scanned |
| main junction | the **highest-degree** node in the join graph | every path tends to pass through it |

**The refinement rounds.** Seeds₀ = the extremes. Seeds₁ = the extremes plus
their graph frontier — literally "the node between each pair". Seeds₂ =
everything. Each round re-runs the frontier DP restricted to those seeds and
keeps the best order found. **Stopping rule:** the first round that does not
improve the decision ends the refinement, because widening further can only buy
more search for the same answer. **Worst-case bound:** three rounds, each capped
at `MAX_STATES`; a round that blows the cap contributes nothing, and an
extremal-seeded greedy completion runs from every extreme as the floor. That
floor is always a *valid* plan — reordering an INNER chain never changes the
answer, only possibly its optimality.

**Where the analogy stops, and why (the honest part).** A road solution is a
**route between endpoints**: the extremes bracket it and interior points refine
what happens in between. A left-deep join order has a **start but no end** — it
is a permutation whose cost compounds from position 0 outward, so the first
choice dominates and there is no far endpoint to bracket against. Extremal
sampling therefore does *not* transfer as "solve the 4×4 and interpolate"; it
transfers as **seed selection plus hub identification**, which is the half that
carries the value. That is what is implemented, and it is why the exhaustive DP
is still preferred whenever `n ≤ 12`: when you can afford to look at everything,
sampling is only a way of not doing so.

Measured compile cost for the 17-table `join-17-4` shape: below a millisecond,
and paid once — the plan is content-hashed and cached.

---

## 5. The recursion is the same mechanism

The solver is invoked from `plan_join_select`, which is the single entry every
join chain in every scope passes through: the top-level SELECT, each lifted
subquery body, each derived-table body, each compound arm, each recursive-CTE
component. So "run MPEE after every sub-compilation, as each N in the N×N" is
not extra plumbing — it is where the code already converges.

And it is the *same idea* as §4's collapse, seen one level up. A subquery is a
bounded scope attached to its parent through a narrow interface: its correlation
arguments in, its result slot out. By the time the parent's chain is solved, the
subquery has already been compiled to a single `SubPlan` occupying one parameter
slot — it **is** a collapsed node. The consumer-cap pruning already shipped in
`planner/subquery.rs` (EXISTS → 1 row, scalar → 2, IN uncapped) is the cost
*interface* of that collapsed node. Decomposition and recursion are one
mechanism, not two.

This also helps §6: a collapsed subgraph's internal ordering depends only on its
own local facts, so a change in one scope's inputs cannot perturb another's.

---

## 6. Determinism and the content-hash contract — resolved

**The constraint.** Plan bytes are content-hashed and published to a registry
shared across processes (`plan/<hash>` in the sys-keyspace). If the same hash
could name two different plans, that is a correctness disaster.

**Investigated, and the answer is structural.** The hash is

```rust
// crates/mpedb-sql/src/plan/mod.rs
blake3(canonical_plan_bytes ‖ schema_hash ‖ FORMAT_VERSION)
```

and the registry has **no** other key — SQL text is stored inside the record but
is never an index (`crates/mpedb/src/registry.rs`; the lookup path is
`sys_get(plan/<hash>)` everywhere). Load re-validates the format byte, every
bound, `schema_hash == live schema hash`, the recomputed footprint, and finally
`plan.hash() == requested hash`.

Therefore: **a different chosen plan is different bytes, and different bytes are
a different hash, by construction.** The failure mode the task warned about —
one hash naming two plans — is not merely avoided, it is unreachable. Two
processes that compile the same SQL against slightly different row counts and
choose differently simply publish two entries, each self-describing and each
valid.

**Fail-closed is automatic, and it is the "still valid" branch.** A plan whose
cost inputs have since moved is *stale*, not *wrong*: reordering an INNER join
chain preserves the result set exactly, so an old order remains a correct
answer, only possibly a slower one. Nothing has to detect the drift, because
nothing breaks. (The mechanisms that *do* fail closed — `PlanInvalidated` on
schema-hash drift, on `PLAN_FORMAT` drift, on a stale RLS policy stamp — are
untouched.)

**What bucketing buys, then, is not safety but stability.** Without it a plan's
identity could move on every commit, churning a 4096-entry registry with LRU
eviction. With `bucket(n) = 64 - leading_zeros(n)`, a table must **double** before
any cost can move, and even then the comparison usually does not flip. Two
processes reading snapshots a few thousand commits apart land in the same bucket
and produce the same bytes and the same hash — which is agreement in practice,
on top of safety by construction.

**And the UNKNOWN class makes this easier, exactly as suspected.** The primary
decision is `worst_log` over KNOWN/BOUNDED classes, the tiebreaker is
`cartesian`, which is *purely structural and reads no statistics at all*. A
robustness-first solver depends on volatile counts far less than an
estimate-tuned one would: for `join-17-4` the decisive term does not consult
`row_count` even once.

**Where counts enter.** `Database::compile_maybe_explain` opens a read snapshot
and passes `&|tid| row_count(tid)` down through
`mpedb_sql::prepare_maybe_explain_with_views` → `plan_statement` → … →
`plan_join_select`, the same `&dyn Fn(u32) -> u64` shape `risk.rs` already uses.
The lighter `mpedb_sql::prepare(sql, schema)` entry points pass a zero source —
all buckets 0 — so the crate stays standalone and its existing callers keep
their exact previous hashes for every non-join statement.

---

## 7. Refusals — and which of them became constraints (#116)

Morten's framing for v2: *"Left join, where etc er som constraints i
vehicle-optimalisering, og samme gjelder her og inngår da i N×N-kost-analysene
FØR solver kjører."* In vehicle routing a time window, a capacity or a
forbidden turn is not a reason to abandon the route — it is a constraint the
solver prices and searches the feasible region under. v1's eligibility list was
a list of situations where the solver gave up. v2 converts the three that
carried the value and keeps two, each with a stated reason.

### 7.1 Converted (v2, #116)

**A LEFT JOIN is a BARRIER, not a veto.** An outer-join step pins its own
position: the set of tables placed *before* it is exactly the set the user
wrote, so what it preserves and what it NULL-extends is untouched. Every
maximal INNER run *between* barriers is then an independent sub-problem the
same solver orders, on the one-line argument

```
(A ⋈ B) ⟕ C  ≡  (B ⋈ A) ⟕ C
```

— reordering inside the run cannot change what the following outer join sees,
because the run's row set is identical either way. Three consequences fall out
for free: the barriers are *cut points* in the query graph, so each segment is a
smaller `N` (§4's collapse, arrived at from the other side); segment-local
optimisation is globally optimal, because a segment's internal order cannot
change the SET any later segment sees and `step`'s cost depends on the set, not
on how it was built (§3); and the emitted statement keeps each barrier's `ON`
**on its join**, since moving a LEFT join's ON into the WHERE turns "does this
row match" into "does this row survive".

Two costing rules follow from #65's pushdown and are load-bearing:

- a **barrier's own ON** constrains only its NULL-extended table. Under a LEFT
  join every preserved-side row survives whatever the ON says, so crediting the
  preserved side with it would be a false constraint;
- a conjunct whose **last** table is a barrier lands in `joined_filter` (it
  filters the already-NULL-extended row), so it restricts no step and is priced
  at nothing.

**A CORRELATED lifted subplan is remapped.** Subqueries are lifted *before* the
join dispatch and a correlated subplan's `outer_args` are base-row slots of the
joined tuple **in the textual order**, so a reorder left them pointing at the
wrong columns. That was a genuine wrong answer during v1's development — a
`count(*) FILTER (WHERE EXISTS (… c.ref = t.id))` over a join returned 1 where
sqlite returns 2 — and v1 refused the whole scope whenever any correlated
subplan existed. v2 has the solver report the permutation as a slot map
(`Solved::slot_map`) and applies it to every top-level subplan's `outer_args`
(`Solved::remap`). Only the top level is ours: a nested lift's args name slots
of *its* parent's row, which this reorder does not touch. The registry path
re-validates every `outer_arg` against the reordered base row on decode, so a
stale slot would surface as `Corrupt` rather than as an answer.

**Residual PLACEMENT is priced.** #65 evaluates a conjunct at the step that
places its LAST table — so *when* a filter runs is a consequence of the order,
not a fixed property of the text. v1 had no term for it and fell back to
"wherever the textual order put it" whenever the first three terms tied. v2 adds
a fourth, last-place lexicographic term, `residual_late`: every conjunct is
charged the position at which it becomes evaluable. It is purely structural (it
reads no statistics), it is a sum over steps that depends only on `(S, tᵢ, i)`
so the DP stays correct, and — sitting last — it can only decide among
candidates the first three terms rate identically. That is exactly the
population v1 left to the text.

### 7.2 Still refused, and why

- **FULL JOIN.** #65 disables WHERE pushdown *entirely* when any FULL is in the
  chain, because both sides NULL-extend and every conjunct would filter rows
  that do not exist yet. The rewrite is built on `INNER JOIN … ON p ≡ CROSS JOIN
  … WHERE p` plus that pushdown putting each conjunct back at the earliest step
  where its slots are bound; under FULL there is no way back to a per-step ON,
  so the move has no counterpart. Named, not overlooked.
- **USING / NATURAL joins.** Their desugaring picks the *leftmost* occurrence of
  a shared column as the coalesce representative; reordering would silently move
  it.
- **the recursive-CTE working table** (`CTE_TABLE`) or a non-flattened derived
  table in the chain.
- **`DUAL_TABLE`** — the synthetic one-row table a `FROM`-less SELECT binds
  against. It has no rows to bucket and no PK to pin, so every cost term reads
  0 and the solver would be free to move it anywhere; refusing keeps it where
  the binder put it. Grouped with the eligibility refusals above, so it
  surfaces as `Skip::Ineligible`.
- **any RLS policy on any table in the chain.** This one is kept deliberately,
  and the argument is the overflow channel below: a reorder changes which pairs
  a predicate is evaluated over, mpedb **raises** on arithmetic overflow, and
  under a policy scope a raise is an information channel and not just an error.
  Pricing that would mean proving that no reachable reorder changes the set of
  *raises* a policy-scoped query can produce — a much stronger claim than
  preserving the row set, and not one this solver can make. A named refusal is
  the honest answer.
- **a scope with more than 17 tables**, which cannot occur (`MAX_JOINS = 16`) —
  and, at the other end, fewer than two, since there is nothing to order
  (`Skip::Size` covers both).
- **a failed probe bind** (`Skip::Unbindable`). This one is not an eligibility
  judgement at all: §8 step 1 binds each `ON` over its own left-deep prefix
  scope, and if that fails the statement is very likely invalid. The solver
  abandons **silently** rather than reporting, so the normal compile path
  produces the real, well-worded error instead of a reorder-flavoured one. A
  refusal that must not become a diagnostic.

The four refusal outcomes are `Skip::{Size, Ineligible, Unbindable, NoGain}`
(`planner/mpee.rs`); `NoGain` — nothing strictly better than what the user
wrote — is the common one and not a limitation.

One behavioural consequence is worth naming rather than discovering: mpedb's
expressions **raise** on arithmetic overflow, and a reorder changes which pairs
a predicate is evaluated over (an index nested loop never visits the pairs a
scan would). A query that raised may therefore stop raising, or vice versa. This
is inherent to join reordering in every engine that does it — the *row set* is
unchanged, which is what "0 wrong" measures.

Beyond eligibility, the solver does not yet consider: bushy (non-left-deep)
plans; semi-join / hash-join alternatives; the cost of ORDER BY or GROUP BY
(an order that happens to deliver sorted output is not credited); index-only
scans; the number of *columns* carried through each stage (cells, not rows, are
what `max_join_cells` counts); correlated-subquery placement relative to join
steps; or any statistic finer than a magnitude bucket.

---

## 8. Mechanics of the rewrite

The reorder happens at the **AST** level, before binding, following the pattern
`rewrite_right_join` already established:

1. Probe-bind each `ON` over its own left-deep prefix scope (preserving the
   refusal of a forward reference) and the `WHERE` over the full joined scope.
   Left-deep prefixes share slot numbering, so the bound slots are directly
   comparable. Any error at this stage abandons the reorder silently — the
   normal path re-runs and reports it properly.
2. Split into conjuncts; derive per table its *pins* (equality to a constant or
   to a column of another table, with the mask of tables that pin requires) and
   its *links* (any multi-table conjunct).
3. Solve (§4). Adopt only if strictly better than textual (§3).
4. Emit a new `SelectStmt`: the chosen first table becomes the outer, every
   other becomes an `INNER JOIN … ON true` in the chosen order, and **every
   INNER ON conjunct moves into the WHERE**. (#116: a BARRIER position keeps its
   table, its `LEFT` kind and its `ON` verbatim — moving a LEFT join's ON into
   the WHERE would turn "does this row match" into "does this row survive".)
   `INNER JOIN ON p` ≡ `CROSS JOIN
   … WHERE p`, and mpedb's #65 pushdown then places each conjunct at the
   earliest step where all its slots are bound — which is exactly the index
   nested-loop candidate the ON used to be. `SELECT *` is pinned to the
   **original** table order as explicit qualified items first, so output column
   order never moves.

No new plan-byte field, no `PLAN_FORMAT` bump: the chosen order *is* the plan.

`EXPLAIN` gains one line naming the order and the access class each position was
entered with. As built (`plan/explain.rs`), for §3.1's `join-17-4`:

```
  join order: t27 [scan] -> t9 [pk] -> t56 [pk] -> … -> t38 [pk] (0 cartesian steps)
```

Three things about that line, each deliberate:

- **Arrow-separated, class per position** (`pk` | `index` | `range` | `fts` |
  `scan` | `cartesian`), not a bare comma list — the *why* is the class, and it
  is per position or it says nothing.
- **Only the cartesian count is summarised.** An earlier draft of this doc
  printed keyed-probe and scan counts too; they are redundant with the
  per-position classes, and the cartesian count is the one term the reader
  cannot recover by eye on a 17-way chain.
- **No `(MPEE: …)` prefix** — removed at `36155ae`. The line is RECONSTRUCTED
  post hoc from the finished plan by classifying each `AccessPath`; nothing in
  the plan bytes records whether `mpee::reorder` ran or returned `Err(Skip::*)`.
  A FULL JOIN chain the solver explicitly declines to look at (§7.2) would
  otherwise print an attribution it had not earned. What the line states is
  true of the plan either way.

---

## 9. The loop — hooked, not built (#88 / DESIGN-MPEE-COST.md)

Morten: *"PLUSS at vi kan lagre ned query-historikk, slik at gjentatte LIKE kan
ta vare på historisk kost og gradvis optimalisere queries"* → *"loop med kost vs
MPEE-iterasjon for raskere queries."*

The loop is **solve → execute → measure → feed back → re-solve on the next
compile**, and it is the only thing that can ever price the **UNKNOWN** class of
§2. Static analysis cannot know what `LIKE '%x%'` lets through; one execution
can. This solver does not implement the loop. It is built so that the loop can
be added without touching it, and the rest of this section is the contract #88
implements against.

### 9.0 The seam

The solver reads its cost inputs through exactly one channel:
`RowCountFn<'a> = &'a dyn Fn(u32) -> u64` (`planner/mod.rs`), consumed only via
`mpee::magnitude`. Nothing in `mpee.rs` knows where a number came from. When
measured history arrives it widens to a `CostSource` whose today-implementation
answers from `row_count` + structural facts and whose tomorrow-implementation
answers from a persisted history record — **the solver code does not change**.
The classification in §2 already names the three answers a `CostSource` can
give, and today's source simply never returns anything better than BOUNDED.

### 9.1 The measurements worth persisting

`#74`'s work meter already counts actual rows processed, `max_join_cells`
already counts actual live joined cells, and both already attribute to a named
node. What is missing is only that these are discarded at the end of the
statement. The numbers that, persisted per plan hash, would let the solver
re-decide — in priority order:

1. **Actual rows out of each join position**, against what the solver assumed
   (`KNOWN ⇒ 1`, `BOUNDED/UNKNOWN ⇒ bucket(row_count)`). The ratio is the
   selectivity the solver could not compute. This single vector is most of the
   value: it converts every BOUNDED position into a measured one and every
   UNKNOWN position into a *bounded* one.
2. **Peak live joined cells**, and at which position it occurred — the direct
   observation of whether `late_unconstrained` put the right table late.
3. **Actual rows out of a residual filter** (per compiled `ExprProgram`), which
   is what prices a repeated `LIKE '%x%'` on the same column.
4. **Execution count and total work per plan hash** — so the loop spends its
   re-optimization budget on statements that actually run often.

All four are aggregate and coarse. None of them is a per-value histogram, and
that is deliberate: DESIGN-MPEE-OPT.md's sharing section makes the shared layer
a privacy boundary, and value-level frequency data must not enter shared memory
in queryable form.

### 9.2 The identity rule — quoted, because it is already law

DESIGN-MPEE-OPT.md, "Cross-query sharing of optimization artifacts":

> **Share by plan shape, never by values.** Cache keys are canonical plan bytes
> … plus a stats epoch … This is also what keeps content-addressed plan hashes
> stable: **statistics inform *costing*, never the plan identity.**

So: measured history lives **beside** the plan — a sys-record keyed by
`(plan hash, stats epoch)` — never inside it. A plan's bytes are immutable under
its hash, forever. When history says a better plan exists, the compiler emits a
**new** plan with a **new** hash and the caller's SQL→hash mapping moves; the old
hash keeps naming exactly what it always named. Two processes can therefore never
disagree about what a hash means, and the loop changes *which plan a statement
uses*, not *what a hash means* — which is the same resolution §6 reaches, arrived
at from the other end.

### 9.3 `max_join_cells` as the cheapest first turn of the loop

A plan that trips `max_join_cells` is a **measured fact that the ordering was
wrong**, not merely an error to return. mpedb already has the counter, the
attribution string (`nested-loop join with "b"`) and a clean typed error. The
adaptive loop attaches there:

- on `RuntimeBudget { kind: JoinCells, .. }`, the facade knows the plan hash and
  the offending step; re-plan with that step's table forced *not* to be a
  BOUNDED-priced position (equivalently: raise its bucket to the observed
  blow-up magnitude) and retry once;
- the re-plan produces **different bytes and therefore a different hash** — the
  §6 contract holds unchanged, and the old plan stays valid for whoever holds
  it;
- a per-process "this hash exploded" set, consulted at `prepare`, would stop the
  re-derivation cost without touching shared state.

Not built at v1: it needs a retry policy (a partially-executed statement is
already rolled back, but the caller's expectations about a single `execute`
doing two scans need stating) and it changes the observable error behaviour of a
budget trip. Designed here so the attachment point is fixed.

### 9.4 Recursive window functions, and mid-execution re-decision

Morten's point stands: for a recursive window function you genuinely cannot know
enough up front, so the plan should be allowed to adjust *during* execution.
What such a hook may and may not do is fixed by the content-hash contract:

- **May not** change the plan bytes. The bytes are the identity; a plan that
  rewrites itself under its own hash is exactly the disaster §6 rules out.
- **May** change anything the executor already treats as a runtime choice made
  *within* a fixed plan: which side of a nested loop is held versus re-probed,
  whether a correlated subplan's memo is built eagerly, the working-set
  materialization strategy of the recursive term, the order the drained frontier
  is visited. These are executor-local and already invisible to the plan bytes.
- The clean split is therefore: **plan = what must be agreed across processes;
  strategy = what one execution may decide for itself from what it has already
  seen.** A mid-execution re-decision belongs entirely on the strategy side, and
  the recursive-CTE fixpoint loop (`crates/mpedb/src/exec/recursive.rs`) is
  where the first one should attach — it is the only place that already
  discovers its own cardinality as it runs.

Not built. Written down so that when it is, it does not reach for the plan bytes.

---

### 9.5 The compile-time ping-pong — BUILT (#116)

Morten: *"ved å slå sammen N×N/kost og solver, kan den dynamisk teste og løse
bedre ruter … gir ekstra verdi til N×N-streaming, for valg av N kan da gjøres
med MPEE-styring."*

v1 streamed the *step costs* on demand inside the DP but bought every table's
`row_count` **eagerly, up front**, before the search started. Fusing cost and
solver means the opposite: the solver asks, and only then does the cost side
pay. That is what turns "enumerate then prune" into **branch-and-bound with an
MPEE-chosen exploration order**.

**The mechanism.** `Node::bucket` is a memoizing `Cell<Option<u32>>`. Nothing
but `Problem::buy` ever calls `row_count`, and an unbought table prices at
`UNBOUGHT = 1`. The solve is then a loop:

```
propose  = solve_chain()                       // under the current beliefs
owed     = the non-KNOWN positions of `propose` whose count is unbought
if owed is empty  → `propose` is optimal, stop
buy(owed); repeat                              // bounded: PING_PONG_ROUNDS = 3
```

**Why the stopping rule is a proof, not a heuristic.** Every cost term is
monotone non-decreasing in a table's bucket, so an unbought table priced at a
lower bound makes *every* candidate's cost a lower bound. When the winner `O`
has all of its own contributors bought, `cost(O)` is exact while every rejected
`P` satisfied `cost_est(P) ≥ cost_est(O) = cost_true(O)` and `cost_true(P) ≥
cost_est(P)`. So `O` is optimal over everything the search explored — the same
guarantee eager v1 gave, for fewer cost reads. A chain that has not settled
within `PING_PONG_ROUNDS` buys everything and solves once, which *is* v1's eager
solve, so the fallback is bit-identical to the pre-#116 behaviour and the whole
mechanism is bounded at four searches.

**Why the lower bound is `1` and not `0`.** `magnitude(n) = 1` for a one-row
table, so `1` is a genuine lower bound for every table holding at least one row.
`0` is unconditionally safe and also useless: with every unbought table at `0`
the *leading* cost term is `0` for every candidate, the first round decides on
tie-breakers alone, and the solver goes off buying counts for an order it is
about to discard — measured, on the 6-table chain, as 5 of 6 counts bought
instead of 1. At `1` the leading term of an all-unbought round *is* the count of
un-probed steps, so the first proposal is the one whose cost the solver can most
cheaply certify. The one exception is an **empty** table (`magnitude(0) = 0`),
where the bound is one too high and the search could in principle prune the true
optimum; a query joining an empty table returns no rows and terminates
immediately whatever the order, so nothing is at stake, and the row SET is never
affected either way.

**The adoption test is the same bound, used the same way.** A reorder is
adopted only if it is strictly better than the textual order. The textual
order's cost starts as a lower bound and is refined **one count at a time**,
stopping the instant `chosen < textual` appears — because buying can only raise
the textual cost, the first such comparison is final. That is literally the
solver's current best bound deciding which `N` is worth examining next.

**Measured** (`planner::mpee::tests`, a counting `RowCountFn`): the scrambled
chain with a late constant anchor — the `join-17-4` shape — costs **exactly one
`row_count`, at n = 6, 10 and 17 alike**, against `n` under v1. Every position
but the first is a PK probe, and a probe's cardinality is a *proof*, not a
statistic, so its magnitude is never worth buying. The honest other half is
pinned by a second test: a chain joined entirely on non-key columns is decided
by size alone and pays for its sizes. Laziness is a property of the *question*.

**What this does NOT do.** A `row_count` is a catalog B-tree lookup, so one
probe versus seventeen is not a wall-clock story today — `select4`'s time is
unchanged within noise. The value is that the seam is now demand-driven, which
is the precondition for §9.0's `CostSource`: the moment an answer can come from
*persisted measured history* instead of a catalog counter, "which N to examine
next" stops being free and this loop is what makes it affordable.

### 9.6 Execution-time ping-pong — designed, not built

The compile-time loop above is free because it happens *before the plan exists*.
Execution-time re-decision is a different contract, and the identity rule fixes
it (§9.2, quoted because it is already law):

> **Plan bytes are immutable under their hash.** Statistics inform *costing*,
> never plan identity. A persisted better plan is a NEW hash.

So an execution-time ping-pong may change **strategy**, never **bytes**. The
shape, for whoever builds it:

1. after position 0 has drained, the gather knows the ACTUAL rows out of it —
   the one number the solver had to bound rather than know;
2. if that contradicts the assumption by more than a magnitude bucket, re-solve
   the remaining **suffix** with position 0's bucket replaced by the observed
   one. The prefix already emitted stays valid **for INNER joins**: an inner
   chain's row set is order-independent, so re-ordering the tail cannot change
   which tuples the whole chain produces;
3. that safety argument is exactly why LEFT/FULL need care here. A barrier's
   position is part of what the outer join preserves; a suffix re-solve must
   treat every barrier as immovable, i.e. it may only permute inside the
   *current* free run and the runs after it — the same segmentation §7.1
   already defines;
4. the re-decision is executor-local (which side of a nested loop is held versus
   re-probed, whether a correlated memo is built eagerly, the working-set
   materialization of a recursive term) and therefore invisible to the plan
   bytes. If the better order is worth keeping, the compiler emits a NEW plan
   with a NEW hash and the caller's SQL→hash mapping moves; the old hash keeps
   naming exactly what it always named.

Not built at v2: it needs the gather to expose a per-position row counter to the
planner mid-statement and a re-entry point into `plan_join_select` for a suffix,
neither of which exists. Written down so that when it is built it does not reach
for the plan bytes.

## 10. Measured

Release build, x86-64 Linux, the gregrahn sqllogictest corpus, runner
`crates/mpedb-testkit/src/bin/sqlite_corpus.rs`, `ulimit -v 3000000`. "before" =
`f00856c` built in a separate worktree; "after" = this branch. Same binary
procedure both sides.

**The acceptance test — `select5.test`'s `join-17-4`:**

| | before | after |
|---|---|---|
| the failing variant (`FROM t9,t56,t53,t61,…`) | **out of memory: allocation failed while materializing a nested-loop join's intermediate rows** | **answers**, md5-verified against sqlite |
| the three `join-17-4` blocks isolated | 2 / 3 pass, 7.8 s | **3 / 3 pass, 0.2 s** |
| whole `select5.test` | 871 / 1436 pass, 0 wrong, **186.7 s** | **872 / 1436 pass, 0 wrong, 1.0 s** |

The 564 records still unsupported in `select5.test` are comma joins of 18–64
tables, refused by the plan format's `MAX_JOINS = 16` — unrelated to ordering.

**Regression — `select1-4` + `evidence/` (9,689 records):**

| | before | after |
|---|---|---|
| passed | 9,489 (98.9 %) | 9,489 (98.9 %) |
| unsupported | 101 | 101 |
| **wrong answers** | 4 | **4** — the same four `slt_lang_replace` shim artifacts (CORPUS-STATUS §3), byte-identical list |
| error mismatches | 0 | 0 |
| `select4.test` wall clock | **447.2 s** | **22.8 s** (19.6×) |

The whole report body — per-file table, category attribution, the wrong list —
diffs **byte-identical** except for the timings. `select4.test` is the milder
instance of the same shape and is where the 19.6× comes from.

There is **no join cell in `crates/mpedb-bench`** to compare against; the join
battery above is the measurement.

### 10.1 v2 (#116) — measured against v1 in the same worktree

Control group: v1 is `2fe36f7`'s `planner/mpee.rs` + `planner/join.rs` checked
out into this worktree and rebuilt, so both sides are the same machine, the same
compiler and the same everything else. `crates/mpedb/tests/mpee_solver.rs` is
the harness; every "v2" line below is an assertion in it.

**Each converted refusal, before and after.** The `join order:` line is
EXPLAIN's, verbatim.

| shape | v1 | v2 |
|---|---|---|
| correlated `FILTER (WHERE EXISTS (… k.ref = a.id))` over `FROM a, b WHERE b.aref = a.id AND b.id = 3` | `a [scan] -> b [pk]` — refused, textual order kept, because a correlated subplan existed | `b [pk] -> a [pk]`, **0 cartesian steps**, answer differential-identical to bundled sqlite 3.45 (and through the plan registry) |
| `FROM a, b LEFT JOIN c ON c.id = b.y WHERE b.aref = a.id AND b.id = 3` | `a [scan] -> b [pk] -> c [pk]` — refused, LEFT anywhere in the chain | `b [pk] -> a [pk] -> c [pk]` — the preserved run reorders, the barrier stays put; answer differential-identical |
| the `join-17-4` chain (10 tables, scrambled) + `LEFT JOIN t11`, 200 k-cell budget | `t1 [scan] -> t3 [cartesian] -> t5 [cartesian] -> t7 [cartesian] -> t9 [cartesian] -> …` — **`runtime budget exceeded: 200010 live joined cells > limit 200000 while evaluating nested-loop join with "t9"`** | `t1 [scan] -> t2 [pk] -> … -> t11 [pk]`, **0 cartesian steps**, **1 row** |
| `FROM c, b, a` joined on non-key columns, three single-table filters on `a` and one on `c` | `c [scan] -> b [scan] -> a [scan]` — first three cost terms tie, so the textual order stands | `a [scan] -> b [scan] -> c [scan]` — `residual_late` places the most-restricted table first; answer differential-identical |
| `FULL JOIN` | `a [scan] -> c [scan]` | `a [scan] -> c [scan]` — **unchanged, by design** (§7.2) |

**The ping-pong, measured** (`planner::mpee::tests`, a counting `RowCountFn`):

| chain width `n` | v1 `row_count` reads | v2 |
|---|---|---|
| 6 | 6 | **1** |
| 10 | 10 | **1** |
| 17 | 17 | **1** |

and the honest converse: a 4-table chain joined entirely on non-key columns is
decided by size alone and buys ≥ 3 of its 4 counts. Laziness is a property of
the question, not a trick that always wins. With `UNBOUGHT = 0` instead of `1`
the 6-table case buys **5** — see §9.5 for why the lower bound matters.

**Regression — `select1-4` + `evidence/` (9,689 records):**

| | v1 | v2 |
|---|---|---|
| passed | 9,489 (98.9 %) | 9,489 (98.9 %) |
| unsupported | 101 | 101 |
| **wrong answers** | 4 | **4** — the same four `slt_lang_replace` shim artifacts |
| error mismatches | 0 | 0 |
| whole report body | — | **diffs BYTE-IDENTICAL** (timings excluded) |
| `select4.test` | 39.7 s | 38.6 s |
| whole run, wall | 44.75 s | 42.96 s |

**`select5.test`:** 872 / 1436, 0 wrong, report body **byte-identical**;
1.14 s → 1.02 s.

(The 22.8 s / 447.2 s figures in §10 above were measured on a different machine;
39.7 s is this worktree's v1 control for the same file.)

Also green: `cargo test --workspace --exclude mpedb-bench`,
`crates/mpedb-testkit/tests/slt_files.rs` (the `join order:` expectations in
`slt/joins.test` and `slt/left_join.test` are unchanged), and
`cargo clippy --workspace --exclude mpedb-bench --all-targets -- -D warnings`.

Also green: `cargo test --workspace`, `crates/mpedb-testkit/tests/slt_files.rs`
(two EXPLAIN expectations gained the new `join order:` line), and
`cargo clippy --workspace --all-targets -- -D warnings`.

### 10.2 Memory — the paired A/B, one binary

Everything above is wall clock. The memory case was an anecdote — "it used to
OOM and now it doesn't" — until this section. That IS a memory result, but a
qualitative one, and the whole structural claim of the solver is about memory:
a bad order carries an intermediate that is the PRODUCT of everything placed so
far, a good one carries one step's worth.

Machine: **Apple M3 Pro** (11 cores, 36 GB), macOS, release build, commit
`192ba29` plus the probe. Both arms are the **same binary** — `MPEDB_NO_MPEE=1`
selects the pre-#114 textual order (§ the switch's own doc comment). Probe:
`crates/mpedb/examples/mpee_memory.rs`. Shape: `open_chain(n)` — n tables of 10
rows chained by PK equalities, FROM scrambled odds-then-evens, the only
constant anchor written LAST.

**The gate came first.** `mpee_solver.rs::the_mpee_kill_switch_selects_the_textual_order`
passes in BOTH arms on this machine; without that, every number below would be
worthless. So does `reordering_never_changes_the_answer`. The rest of that file
is the solver's ACCEPTANCE suite, and 8 of its 16 tests necessarily FAIL with
the solver off — asserting a chosen join order is asserting that the solver ran.
"16/16 in both arms" would therefore be the wrong gate, and is not the one used:
the two arm-agnostic tests are.

#### Headline — peak live join cells, exact and deterministic

`max_join_cells` counts the `Value` cells a join HOLDS and `charge` trips on
`live > budget`, so **the smallest budget under which a statement completes IS
its peak**. The probe recovers it exactly by bisection. It is a pure function
of data and plan — not of the machine, the allocator or the timer — so unlike
RSS it reproduces everywhere, which is why the ON column now lives in a test
(`the_solved_order_holds_a_linear_number_of_cells`) rather than only here. Two
independent passes returned identical numbers, and the `40n − 60` peak that test
pins reproduces byte-for-byte on x86-64 Linux and on Apple Silicon — which is
the determinism claim, checked rather than assumed.

| shape | solver ON | solver OFF | ratio |
|---|---|---|---|
| chain, n=4 | **100** | 460 | 4.6× |
| chain, n=6 | **180** | 6,800 | 37.8× |
| chain, n=8 | **260** | 90,000 | 346× |
| chain, n=10 | **340** | 1,120,000 | 3,294× |
| chain, n=12 | **420** | 13,400,000 | **31,905×** |
| chain, n=14 | **500** | **> 64,000,000** | > 128,000× |
| `join-17-4`, 17 tables | **930** | **> 64,000,000** | > 68,800× |
| ordinary 3-table join, 2,000 rows | 686 | 686 | **1.00× — no effect** |

ON is exactly `40n − 60`: **linear** in the width. OFF gains **a factor of ten
per table added**. Linear versus exponential is the result; every other number
in this section is a consequence of it. The last two rows are **thresholds, not
ratios** — their textual-order peak is above the probe's 64 M-cell cap (≈2.5 GB
at the 40 B/cell calibration) and was not chased further. The two are different
kinds of result and are not averaged together.

Every row's answer digest is **identical across arms**. Reordering preserves
the result set exactly; that is the licence for doing it at all.

#### Corroboration — peak RSS

`getrusage(RUSAGE_SELF).ru_maxrss`, **bytes** on Darwin (not KiB as on Linux).
Arms interleaved inside one loop with the order flipped on odd reps so host
drift cannot masquerade as an effect; 9 reps, p50. "marginal" subtracts the
fixture floor — the same database built and populated with no join run,
4.1–6.1 MB, arm-independent to within 0.4 %.

| shape | ON p50 | OFF p50 | total | marginal | wall p50 |
|---|---|---|---|---|---|
| chain, n=6 | 4.72 MB | 4.90 MB | 1.0× | 1.3× | 85 µs → 428 µs |
| chain, n=8 | 4.82 MB | 8.21 MB | 1.7× | 6.4× | 138 µs → 4.1 ms |
| chain, n=10 | 5.00 MB | 48.74 MB | 9.8× | 63× | 0.50 ms → 39 ms |
| chain, n=12 | 5.29 MB | **512.0 MB** | **96.7×** | **534×** | 2.5 ms → 406 ms |
| ordinary 3-table | 6.62 MB | 6.57 MB | 1.0× | 0.9× | 103 µs → 93 µs |

Run-to-run spread (max−min over 9 reps) is ≤ 2.4 % on every chain row and
≤ 5.7 % on the ordinary join, so from n=8 up the effect swamps the noise by
orders of magnitude. **At n=6 it does not**: 1.0× total sits inside the spread
and only the marginal figure shows anything at all. Read n=6 as *no measurable
effect*, not as 1.3×.

**The real corpus files**, both arms under an identical explicit
`--join-cells 64000000` so neither can run away:

| | `select5.test` ON | OFF | `select4.test` ON | OFF |
|---|---|---|---|---|
| peak RSS | **9.98 MB** | **4.92 GB** — **493×** | 626.6 MB | **3.23 GB** — **5.2×** |
| wall | 0.26 s | 41.3 s | 7.6 s | 94.8 s |
| pass | 872 / 1436 | 868 / 1436 | **3857 / 3857** | **3857 / 3857** |
| **wrong answers** | **0** | **0** | **0** | **0** |

`select5`'s OFF arm loses four records — the `join-17-4` blocks, which exceed
the shared cap and are re-attributed to `comma-join`. It loses records; it never
gets one wrong. `select4` is the milder instance of the same shape and is a
clean RATIO rather than a threshold: both arms answer all 3,857 records
identically, at 5.2× the peak RSS and 12.5× the wall. Its 626 MB ON floor is
largely the corpus runner's own retained data, so 5.2× understates the join's
share — which is exactly why the synthetic chain, with its 4 MB floor measured
separately, is the primary evidence and this is corroboration.

#### Where the effect is ABSENT

The ordinary 3-table join — a fact table filtered to a narrow range joined to
two dimensions by their primary keys, the corpus-median shape — is **686 cells
in both arms**, with the **same join order** in both, at 1.0× RSS and 0.9× wall
(inside the noise, i.e. no difference). The solver neither saves nor costs
anything there, because the textual order was already the one it would have
chosen. This is a technique that pays on adversarial shapes and is free on
ordinary ones. It does not pay everywhere, and this paragraph is what saying so
looks like.

#### The solver's own cost

`mode=compile`, 200 DISTINCT statements per rep so the content-hashed plan
cache never answers, p50 of 3 reps:

| shape | ON | OFF | delta |
|---|---|---|---|
| ordinary 3-table | 63 µs | 60 µs | +5 % — inside the noise |
| chain, n=6 | 33 µs | 20 µs | +13 µs |
| `join-17-4`, 17 tables | **106 µs** | 57 µs | +49 µs |
| chain, n=12 | **2.26 ms** | 36 µs | **+2.23 ms** |

The sub-millisecond claim for the 17-table shape **holds on Apple Silicon**
(106 µs). But the worst case in this family is **not the widest shape** —
compile cost is NON-MONOTONE in the chain width and peaks exactly at
`DP_FULL_MAX = 12`:

| n | 4 | 6 | 8 | 9 | 10 | 11 | **12** | 13 | 14 | 16 | 17 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| p50, µs | 43 | 44 | 66 | 114 | 377 | 898 | **2,207** | **67** | 75 | 84 | 90 |

Below the boundary the exact DP runs and roughly doubles per table added; at 13
the extremal-sampling path (§4.1) takes over and the cost drops **33×**. Paid
once per distinct statement because plans are content-hashed and cached — but a
2.26 ms one-off sitting at exactly n=12, and a solver that plans 17 tables **21×
faster** than it plans 12, is the one number here a reader would have guessed
wrong.

## 11. Cross-references

- [DESIGN-MPEE-OPT.md](DESIGN-MPEE-OPT.md) — §1.7 (cluster-first decomposition,
  now implemented here against the query graph rather than the commit batch),
  §4 (why the batch framing was falsified), §5 roadmap item 5 ("cost broker for
  the planner when joins/multi-index selection land" — this is that item),
  and the consumer-cap pruning that is this solver's collapsed-node interface.
- [DESIGN-MPEE-COST.md](DESIGN-MPEE-COST.md) — the persisted, self-tuning cost
  catalog (NDV, histograms, access-frequency counters, auto-indexing). Still
  Phase 7+, still #88. §5 of that document already anticipated exactly the
  determinism resolution §6 reaches: "a change in the cost inputs that changes
  the chosen plan yields a new plan hash".
- [DESIGN-RUNTIME-BUDGET.md](DESIGN-RUNTIME-BUDGET.md) — `max_work_rows` /
  `max_join_cells`, the worst-case estimator this solver reuses, and the
  feedback channel §9.1 would attach to.
