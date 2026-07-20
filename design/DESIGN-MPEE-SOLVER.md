# DESIGN-MPEE-SOLVER — MPEE as the plan solver

**Status: implemented (task #114, 2026-07-20).** Companion to
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
componentwise into a **lexicographic triple**:

```
Cost = (worst_log, cartesian, late_unconstrained)
```

1. **`worst_log`** — `0` if the step is KNOWN (full PK pinned, or a full-width
   UNIQUE index pinned, by constants or by columns of `S`), else `bucket(t)`.
   The sum is the log₂ of the worst-case product — the same quantity
   `crates/mpedb/src/risk.rs` already computes for the #74 budget, now used to
   *choose* a plan instead of only to *warn* about one.

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

Ties are broken by the **textual order**: the solver's order is adopted only if
it is *strictly* better than the order the user wrote. mpedb never reorders
without a reason it can name.

Every term is an integer, every term is a sum over steps, and every step's score
depends only on `(S, tᵢ, i)`. That last property is what makes the DP in §4
correct — the cost of a set is independent of the order the set was built in.

### 3.1 Worked example — `select5.test`'s `join-17-4`

17 tables `tN(aN INTEGER PRIMARY KEY, bN, xN)`, 10 rows each, 16 equi-join
conjuncts and one constant anchor `a38 = 9` written as the 16th of 17 conjuncts.
The join graph is a **tree** (17 nodes, 16 edges) — in fact a path.

| order | `worst_log` | `cartesian` | verdict |
|---|---|---|---|
| textual (`t9, t56, t53, …`) | 64 | **15** | 15 steps multiply by a full table with certainty |
| solver (`t38` first, then along the tree) | 64 | **0** | every step is linked; the anchor pins one row |

The worst-case product is *the same* — a non-unique equality is BOUNDED, not
KNOWN, so no order can prove a smaller bound. Term 1 ties; **term 2 decides**,
and it decides on structure alone. That is the whole robustness argument in one
table.

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

## 7. What the solver deliberately does NOT do (v1)

Eligibility is refused — the textual order is kept verbatim — whenever any of
these holds. Each is a correctness boundary, not a shortcut:

- **any non-INNER join in the chain** (LEFT / FULL; RIGHT is already rewritten
  to LEFT before planning). Outer joins are neither commutative nor associative
  in general, and the gather's NULL-extension is defined over a fixed left-deep
  shape.
- **USING / NATURAL joins.** Their desugaring picks the *leftmost* occurrence of
  a shared column as the coalesce representative; reordering would silently move
  it.
- **the recursive-CTE working table** (`CTE_TABLE`) or a non-flattened derived
  table in the chain.
- **a CORRELATED lifted subquery.** Subqueries are lifted *before* the join
  dispatch, and a correlated subplan's `outer_args` are slots of the joined
  tuple **in the textual order**; a reorder would leave them pointing at the
  wrong columns. This was caught by `crates/mpedb/tests/agg_filter.rs` as a
  genuine wrong answer during development (a `count(*) FILTER (WHERE EXISTS (…
  c.ref = t.id))` over a join returned 1 where sqlite returns 2) and is now
  refused. Remapping `outer_args` through the permutation is a ~10-line
  follow-up and the obvious next step; v1 keeps the user's order instead.
- **any RLS policy on any table in the chain.** The join path's evaluation order
  is a security contract (each table's policy runs over its own row, before any
  ON that could raise on it). The reorder preserves it — the #65 pushdown places
  every conjunct at or after the step of its last referenced table — but v1
  declines to reason about it under reordering at all.
- **a scope with more than 17 tables**, which cannot occur (`MAX_JOINS = 16`).

One behavioural consequence is worth naming rather than discovering: mpedb's
expressions **raise** on arithmetic overflow, and a reorder changes which pairs
a predicate is evaluated over (an index nested loop never visits the pairs a
scan would). A query that raised may therefore stop raising, or vice versa. This
is inherent to join reordering in every engine that does it — the *row set* is
unchanged, which is what "0 wrong" measures — and it is one more reason the
reorder is refused wherever an RLS policy is in scope, since there a raise is an
information channel and not just an error.

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
   other becomes an `INNER JOIN … ON true` in the chosen order, and **all
   original ON conjuncts move into the WHERE**. `INNER JOIN ON p` ≡ `CROSS JOIN
   … WHERE p`, and mpedb's #65 pushdown then places each conjunct at the
   earliest step where all its slots are bound — which is exactly the index
   nested-loop candidate the ON used to be. `SELECT *` is pinned to the
   **original** table order as explicit qualified items first, so output column
   order never moves.

No new plan-byte field, no `PLAN_FORMAT` bump: the chosen order *is* the plan.

`EXPLAIN` gains one line naming the order and why:

```
  join order: t38, t61, t24, … (MPEE: 0 cartesian steps, 1 keyed probe, 16 scans)
```

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

## 10. Cross-references

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
