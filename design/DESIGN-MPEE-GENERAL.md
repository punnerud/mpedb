# DESIGN-MPEE-GENERAL — one solver kernel, per-domain cost plugins

**Status: design + staged plan, 2026-07-22.** Grounded in two measurements from
the same day: the OLAP head-to-head ([BENCHMARKS-OLAP.md](../BENCHMARKS-OLAP.md))
that showed the join-order solver working correctly *inside* a plan space whose
pricing cannot see a star schema, and the original MPEE engine
(github.com/punnerud/mpee), whose cost model was always a pluggable set of
dimensions rather than a formula. This document says what generalizes, what does
not, and in which order the pieces land.

Reads with: [DESIGN-MPEE-SOLVER.md](DESIGN-MPEE-SOLVER.md) (#114/#116 — the
kernel this generalizes), [DESIGN-MPEE-COST.md](DESIGN-MPEE-COST.md) (#88 — the
cost catalog these stages begin to build), [DESIGN-WORKLOAD-INDEXES.md](DESIGN-WORKLOAD-INDEXES.md)
(#118 — the advisor that becomes stage E), [DESIGN-CTE-RECURSIVE.md](DESIGN-CTE-RECURSIVE.md)
(the graph substrate), [LANDSCAPE.md](../LANDSCAPE.md) §"If mpedb ever grows a
vector index" (the storage decision stage D inherits).

---

## 0. The one-paragraph answer

The solver is already generic; the cost inputs are not. Original MPEE proves the
shape: a generic search kernel fed by **declared cost dimensions**, each carrying
an explicit **monotonicity contract**, with expensive inputs **bought on demand**
instead of materialized. mpedb's port kept the kernel discipline (lexicographic
monotone `Cost`, `UNBOUGHT` lower bounds, log2-bucketed inputs) but wired in
exactly one statistic — table `row_count` — through a seam that cannot carry
anything else (`RowCountFn(u32) -> u64`, `planner/mod.rs:17`). Every loss the
OLAP bench measured against SQLite traces to that seam, not to the solver. The
generalization is therefore a **cost-input registry** (the `CostSource` widening
DESIGN-MPEE-SOLVER §9.0 already names), populated per domain: per-index
distinct-key counts (star schemas), degree buckets (graphs), partial-distance
bounds (vectors), and workload census counts (index synthesis). One kernel, one
determinism law, five consumers.

## 1. Two MPEE regimes, and what actually transfers

Original MPEE solves 50,000-stop vehicle routing with population metaheuristics
(HGS, local-search operators as one GPU megakernel). mpedb's join-order problem
is ≤ 17 tables and demands determinism, so it uses exact branch-and-bound. The
search algorithms share nothing, on purpose. What transfers is the **contracts**:

| original MPEE | mpedb today | the shared contract |
|---|---|---|
| accumulator dimensions: `{name, transit, monotonicity}` — "monotonicity is not optional; it enables pruning" | `Cost = (worst_log, cartesian, late_unconstrained, residual_late)`, each monotone in its inputs (`mpee.rs:96-122`) | every cost dimension is **monotone**, so a partially-known cost is a valid lower bound |
| streaming N×N: never materialize the distance matrix; ~500 MB budget for a 20×-larger problem; K-NN on the hot path | `UNBOUGHT = 1` + ping-pong purchase: propose, buy only the counts the proposal depends on, re-solve (SOLVER §9.5) | expensive cost inputs are **bought on demand**, and unbought inputs price as sound lower bounds |
| hard constraints "mirrored into the O(1) insertion probe" — infeasible insertions pruned before evaluation | `known()`: full-PK / full-width-UNIQUE pinning classifies a step KNOWN=1 row before any search (`mpee.rs:242-265`) | feasibility/certainty is **precomputed into O(1) probes**, not discovered during search |
| matcodec: lossless matrix compression, 9.79×, "the index answers many cells without decompression" | — (nothing) | a compressed representation that **answers queries without decompressing** is also a statistics source — see §5 |

What deliberately does **not** transfer:

- **Penalties / soft constraints.** `Penalty(x)` guides a metaheuristic toward
  feasible regions. SQL semantics are hard: a plan is correct or it is not, and
  mpedb's law is *agree or refuse, never differ*. There is no "slightly wrong,
  cheaply" in a database.
- **Expected-case costing.** Original MPEE optimizes measured travel times.
  mpedb prices the worst case (BOUNDED ≡ UNKNOWN ≡ full row count) because plan
  bytes are content-hashed and an estimate that drifts with data would churn
  plan identity. The star-schema loss is the cost of that discipline — §4 buys
  the loss back *without* giving the discipline up.
- **The GPU megakernel.** Wrong regime: mpedb's solver runs at prepare time in
  microseconds; parallelism belongs in execution (DESIGN-PARALLEL-READ), not
  planning.

## 2. Inventory: what exists, with exact seams

Established by code reading 2026-07-22 (references are load-bearing):

- **The kernel.** Lexicographic 4-tuple `Cost` (`mpee.rs:96-122`); pricing at
  `Problem::step` (`mpee.rs:271-288`); LEFT-join barriers partition the chain
  into independently-solved INNER runs (`mpee.rs:536-563`); kill switch
  `MPEDB_NO_MPEE=1` keeps both arms in one binary.
- **The bucket law.** `bucket(n) = 64 − leading_zeros(n)` (`mpee.rs:88-90`).
  SOLVER §6: bucketing buys *stability*, not safety — "a table must double
  before any cost can move". Safety is structural: a different plan is different
  bytes is a different hash.
- **The single statistic seam.** `RowCountFn<'a> = &'a dyn Fn(u32) -> u64`
  (`planner/mod.rs:17`), documented as "the ONLY statistic the planner reads",
  consumed only through `mpee::magnitude`. SOLVER §9.0 already names the
  widening: "widens to a `CostSource` … the solver code does not change."
- **No per-index statistic exists.** `IndexDef` is `{columns, unique, predicate}`
  (`schema.rs:247-257`). The solver consults indexes only in `known()`.
- **Two worst-case estimators, kept separate on purpose.** `mpee::Cost` ranks
  orders in log2 units; `risk.rs` warns about runtime budget in raw saturating
  rows (SOLVER §3 explicitly distinguishes them). This document keeps them
  separate: stages A and C feed both, merge neither.
- **FTS is priced in risk, not in MPEE.** Posting-list lengths are exact and
  feed #74's estimate (DESIGN-FTS §4); MPEE prices an FTS table like any table.
  The per-access-method registry of MPEE-COST §2 is design-only.
- **The workload is enumerable, not sampled.** Every compiled statement is a
  plan-registry record with full SQL and `CompiledPlan` blob (#118 §1).
  Census over the real corpus: 99,279 statements → 112 whole-table index
  candidates, top 32 covering 94% (#118 §3).

## 3. The generalization contract

Three rules, none new — each is stated in an existing doc and restated here as
the contract every stage must satisfy:

1. **Cost inputs enter ONLY through the `CostSource` registry.** One trait, per
   access method / per statistic; today's `RowCountFn` becomes its first
   implementation, answering identically (no plan changes, no hash movement).
   Nothing in `mpee.rs` may know where a number came from (SOLVER §9.0).
2. **Every input is deterministic and bucketed; statistics never enter
   identity.** The three-doc law (SOLVER §9.2, MPEE-OPT, WORKLOAD-INDEXES
   §2.2): a statistic informs costing; a changed choice is a *new plan hash*
   via the existing `PlanInvalidated` re-prepare, never a mutated plan.
3. **Every cost dimension is monotone.** This is what makes `UNBOUGHT` lower
   bounds sound in the solver, and it is the *same* argument in every new
   domain:
   - **join order**: cost terms monotone in a table's bucket → unbought counts
     are lower bounds (SOLVER §9.5, shipped);
   - **vector kNN**: squared-L2 terms are non-negative → a partial-dimension
     sum is a lower bound on the full distance → abandon a candidate the moment
     the partial sum exceeds the current k-th best. Exactness is preserved; only
     work is skipped;
   - **graph expansion**: frontier size × degree bound is monotone in depth →
     a depth-guarded recursion has a static bound (stage C);
   - **index synthesis**: adding an index never makes a priced plan slower
     under worst-case costing → greedy-with-bound subset selection is sound
     (stage E).

## 4. Per-domain instantiation

| domain | decision variable | new cost input | bucketing | stage |
|---|---|---|---|---|
| join order | permutation within barrier segments | table `row_count` | log2 | **shipped** (#114/#116) |
| star schema | same permutation | **per-index distinct-key count (NDV)** | log2 | A |
| `count(*)` | which tree to count | NOT-NULL narrow-tree guard (correctness gate, not a statistic) | — | B |
| graph | expansion order; prepare-time risk bound | avg degree = entries/NDV of the edge index; depth-guard-aware CTE bound | log2 | C |
| vector | filter-before vs filter-after similarity; scan order | dimensions summed before abandonment; candidate-set size | log2 | D |
| index synthesis | subset of candidate indexes | #118 census + per-plan-hash execution counts (P5) | log2 | E |

The star-schema row is the exemplar for all of them. The measured failure
(BENCHMARKS-OLAP): MPEE chose `fact [scan] → product [pk]`, scanning 2M fact
rows and discarding 89% after the join, because `product.category = 'tools'`
prices as BOUNDED ≡ full row count. SQLite entered the dimension first and was
11× faster. With a per-index NDV bucket, equality on an indexed column prices at
`bucket(rows) − bucket(ndv)` — 5,000 products across 8 categories gives
`bucket(5000) − bucket(8) ≈ 13 − 4 = 9` instead of 13 — and the dimension-first
order wins *without any change to the solver or to the worst-case philosophy*:
the bound is still a worst case, just a correct one for an indexed equality
(no key can match more rows than the largest key group, and NDV bounds the
largest group from below deterministically… conservatively: we use
`rows/ndv`-style magnitude, which is exact for uniform keys and an
underestimate only for skew — the doc for stage A must state the skew caveat
and why log2 granularity absorbs most of it).

## 5. Compression as a statistics source (the matcodec analog)

matcodec's insight was not "smaller"; it was that the compressed form *answers
queries without decompression*. The database analog is **dictionary encoding**
of low-cardinality TEXT columns: the dictionary is simultaneously

- a smaller thing to scan (the columnar `TableKind`'s encoding of choice —
  separate doc, DESIGN-COLUMNAR, agreed but unwritten), and
- an **exact NDV by construction** — the statistic stage A otherwise has to
  estimate with a background pass, delivered free and transactionally current
  for every dictionary-encoded column.

Stage A therefore designs the NDV record so a future dictionary can *become*
its provider without a format change: the sys-keyspace stats record stores
`(source: Analyzed | Exact, bucket)`, and a dictionary-backed column upgrades
the source tag.

## 6. The vector storage position (inherited, not re-litigated)

LANDSCAPE.md §"If mpedb ever grows a vector index" is the standing decision,
quoted so no stage reopens it:

> **Copy sqlite-vec's storage decision without reservation.** Everything —
> including its new DiskANN graph — lives in ordinary SQLite shadow tables, so
> durability, atomicity and rollback are inherited entirely. There is no bespoke
> durability path to get wrong. Put the vector index in ordinary mpedb trees and
> you inherit COW, MVCC, group commit, multi-process writers and SIGKILL
> survival for free.

Plus its two riders: pgvector's over-inclusive visibility model (the index is an
accelerator over candidate row ids, never an authority), and predicate-into-
traversal from day one (do not copy pgvector's post-index filtering — which is,
not coincidentally, exactly what MPEE-priced filter placement in stage D does).
Stage D builds **exact brute-force kNN with early abandonment first** — no index
structure at all — because it establishes ground truth for every later
approximate structure's recall, and because the OLAP bench's lesson is that the
honest baseline is the thing you learn from.

## 7. Stages

Committed plan, each stage green (`cargo test --workspace`, clippy `-D
warnings`) before the next; measured results are appended to this doc per stage.

- **A — `CostSource` seam + per-index NDV bucket.** The star-schema fix.
  **SHIPPED + MEASURED 2026-07-22 (`2f4c7b7`):** `join-star-2` 1198 → 203 ms,
  `join-star-4` 1325 → 336, `join-bad-order` 1099 → 196 on the M3 2M-row star —
  5.9× recovered, plans read `product [index] -> fact [index]`, every engine
  still agrees, `analyze()` cost 0.18 s for nine indexes. The predicted pricing
  from §4 (17 vs 21) is the pricing observed.
- **B — `count(*)` narrow tree, NOT-NULL-guarded.** **CLOSED 2026-07-23 — the
  engine half already existed** (PLAN_FORMAT 59; `agg_over_index.rs` asserts the
  narrowest all-NOT-NULL tree serves `count(*)`, and the `aggregate.rs:989`
  decline this plan cited is `try_fused_fold`, a different helper). What was
  missing was the SCHEMA: the benchmark declared nothing NOT NULL, so no
  index's entry count provably equalled the row count. With NOT NULL declared:
  3.0 → 1.5 ms. The residual 10.6× against SQLite is the per-leaf walk — both
  engines now count narrow trees; answering from interior-node subtree counts
  would be a page-format change and is deliberately not planned.
- **C — graph vs Neo4j.** **SHIPPED + MEASURED 2026-07-23** (`600ece0`, then
  the depth sweep `a53d427`/`f0b842d`; [BENCHMARKS-GRAPH.md](../BENCHMARKS-GRAPH.md)).
  The sweep found the hole: reach-k grew LINEARLY in k (UNION dedups
  (node, depth) pairs → every level re-expanded the reached set; 833 ms at
  k=8 vs Neo4j's flat 38). Closed by **converged-frontier dedup** — the
  depth-guard proof reused as an execution optimization, one shared function
  with the risk estimate, observability-gated and differentially pinned
  against sqlite from both sides. After: 185 ms flat, every row a constant
  1.7–4.9× per-probe factor. tri-global 3,993 → 1,401 via a (src,dst)
  composite (#55 machinery, schema only). The §3 monotonicity contract now has
  a THIRD consumer.
- **D — vector: exact kNN + Qdrant.** **SHIPPED + MEASURED 2026-07-23**
  (`03626bc`, [BENCHMARKS-VECTOR.md](../BENCHMARKS-VECTOR.md)). Early
  abandonment bought **2.9×** (52.3 → 17.9 ms median on 100k × 128d), measured
  as an in-binary A/B, answers bit-identical. Unfiltered: Qdrant HNSW 3.6 ms @
  0.992 recall vs exact 17.9 @ 1.000 — the expected trade. **Filtered
  (1/8 selectivity), the result inverts 13×**: filter-before-heap 5.6 ms exact
  vs Qdrant's payload-filtered 75.2 — the pgvector post-filtering problem
  LANDSCAPE.md described, now measured. `ORDER BY <expr> LIMIT k` needed no
  new SQL surface (the sort-only column machinery already existed).
- **E — #118 advisor, recommend-only.** **SHIPPED 2026-07-23.**
  `Database::recommend_indexes(WorkloadSource::{Registry, Statements})` +
  `mpedb advise <target> [statements.sql]`. The candidate extraction is the
  `--index-census` harness's measured rules carried into the engine verbatim
  (equalities sorted-canonical, one range column, ORDER BY tail, opaque-filter
  refusal on jumps, served-by-prefix filter), identity is the §2.2 content
  hash, ranking is statements-desc then the stage-A row bucket, and the report
  counts everything it skipped — no silent caps. Auto-create stays blocked on
  P2/P3/P5, restated not built.

## 8. The probe path: batch descent and the MPEE split (stages B–D's residual)

After stages A–D every measured gap is the same number wearing three costumes:
a **constant per-probe factor** — SQLite brackets it at 1.7–2.1× on star
joins, Neo4j at 1.7–4.9× on traversals, and the exact kNN scan pays it per
row. One residual, three benches; work on it pays in all three columns at
once. Two moves, in order:

1. **Batch descent (sorted-run probing), serial first.** An index nested loop
   probes the inner tree once per outer row, in outer order — random descents.
   Sorting each morsel's probe keys first makes consecutive descents share
   their upper-tree path (the leaf-locality argument B+trees exist for), and
   an entire sorted run can be resolved in one leaf walk when keys are dense.
   No concurrency, no snapshot questions — measurable on `join-star-*`,
   `reach-k` (each fixpoint level is a batch of probes by construction: the
   frontier IS a sorted-deduped key set), and `tri-global`, before any
   parallelism is spent.

2. **The MPEE split: morsel-parallel probing.** Original MPEE never hands a
   worker "half the stops" — it streams work in morsels against one shared
   cost oracle. mpedb already has exactly this machinery for order-independent
   folds: DESIGN-PARALLEL-READ §8's adaptive morsel scheduler (a scan that
   proves long at run time splits its REMAINING key range across workers on
   the same snapshot — same answer, engaged only when worth it). The
   generalization is to let a morsel carry **probe work, not just scan work**:
   partition the outer rows, each worker runs batch descent against the shared
   snapshot, results merge under the same order restored discipline the
   parallel fold already has. Candidates, cheapest first:
   - **parallel exact kNN** — embarrassingly parallel (per-worker abandoning
     heap over a key sub-range, merge k best at the end; the bound even
     TIGHTENS during merge). The adaptive scheduler fits unchanged;
   - **parallel fixpoint levels** — each reach-k level's frontier expansion is
     an independent batch of probes;
   - **parallel join morsels** — the general case, and the one that needs the
     most care with the work meter (#74 charges must stay deterministic:
     charge per morsel result, merge in morsel order — the discipline
     par_fold already established).

   The kill-switch rule applies as everywhere: serial remains the semantics of
   record, parallel must produce bit-identical results, and the scheduler
   engages only past a proven-long threshold.

## 9. The generic solver, and benchmarking against the original MPEE

The ask this section answers: one solving engine for OLAP, OLTP, vector and
graph workloads, built on the streaming-N×N discipline, with **everything —
memory, time, quantities — as cost dimensions fed to the kernel**.

### 9.1 What "generic" already means here, measured

The kernel's three contracts (§3) now have measured consumers in every domain
this document set out to cover:

| contract | join order | count/agg | graph | vector | index synthesis |
|---|---|---|---|---|---|
| monotone dimensions | worst_log lower bounds (§9.5 solver) | — | depth guard ⇒ frontier bound | partial-distance abandonment, 2.9× | additive coverage counts |
| streaming purchase | UNBOUGHT ping-pong (1 probe on a 17-chain) | — | — | heap bound tightens as bought | registry scanned, not sampled |
| feasibility → O(1) probes | `known()` | NOT-NULL admission | guard proof, 2 consumers | shape checks before summing | served-by-prefix filter |

The missing piece is not a new idea; it is **naming the dimensions**. Original
MPEE declares `{name, transit, monotonicity}` accumulators (fuel, load, time)
and prunes on their bounds. mpedb's equivalents exist but are implicit:
*rows* (worst_log), *bytes* (the columnar argument, §5), *memory* (the
morsel/heap budgets — `max_join_cells` is literally a memory dimension with a
hard bound), *wall time* (the #74 work meter is its deterministic proxy),
*recall* (the exact/approx axis vecbench measures). A future `CostSource`
entry is one more accumulator with a monotonicity declaration — that is the
whole registration contract, and §3's rules are its validity conditions.

### 9.2 The honest benchmark against github.com/punnerud/mpee

The two engines share contracts, not objectives: brooom minimizes an
**additive pairwise** path cost with a metaheuristic (HGS/local search, 50k
stops); mpedb's port minimizes a **lexicographic worst-case** join cost
exactly (≤ 17 tables, subset DP whose step cost is order-independent given
the placed SET — deliberately a *stronger* property than routing's, which
needs last-element state). Running either on the other's native objective
unmodified would be the category error the vector bench refused.

The honest shared instance class is **path sequencing with additive pairwise
cost** (open TSP), and the honest frame is the one vecbench established:
**exact as ground truth, heuristic scored by gap and time.**

- mpedb's kernel grows a `(subset, last)` DP mode — Held-Karp, exact to
  N ≈ 20, the natural extension of the existing subset DP (`mpee.rs` already
  owns the placed-set recursion; the mode adds last-element state and an
  additive step cost read through the same CostSource seam).
- brooom runs CPU-only (`--no-default-features` — verified to build clean,
  17 s on the dev box) on the same instances, deadline-matched.
- Small N (10–20): exact optimum known ⇒ brooom's **gap-to-optimum** and
  time-to-within-x% are measurable, not guessed. Large N (1k–50k): exact
  declines honestly; brooom's regime, reported as such — the crossover chart
  is the deliverable, exactly like the vector bench's exact/approx pair.
- Purchase counts on both sides: brooom's streamed matrix entries vs the
  kernel's bought cost inputs — the streaming-N×N discipline compared as a
  NUMBER (fraction of the N×N matrix ever materialized), which is the claim
  both engines actually share.

**Built and measured 2026-07-23** ([BENCHMARKS-ROUTING.md](../BENCHMARKS-ROUTING.md)):
the kernel's `(subset, last)` mode is `mpedb_sql::sequence` (Held-Karp, exact
to N = 18, brute-force-differential-tested, DECLINES past the cap), the
harness is `crates/mpedb-routebench`, and the instances are brooom's bundled
real-map San Francisco set. The result: brooom finds the exact optimum on
every checkable sub-instance (gap +0.00% at N = 9…18 on real asymmetric OSRM
durations) — measured optimality, which only an exact ground truth can grant —
while exact answers in 0.2–42 ms against the heuristic's 0.6–2.3 s. The
crossover rule falls out: N ≤ 18 solve exactly, N > 18 heuristic, knowingly.

## 9.3 The cost layer, stored (stage M5, SHIPPED 2026-07-23)

The plan drafted a `[mpee]` config section; what shipped is strictly better on
the axis that matters, and answers the user's requirement directly ("cost
analysis and adjustment must not be locked to config"): **the whole cost layer
is stored state in the file**, coherent across processes by construction and
schema-gen-gated so every change re-prepares everywhere.

- **Tunables** (`mpedb tune set ndv_discount=false`): named switches on the
  calculator, v1 shipping the stage-A NDV channel. Proven by the star flip
  moving on BOTH handles when either sets it.
- **The cost-policy spell** (`mpedb cost-policy set policy.py`): a stored
  PySpell `def policy(kind, table, index_no, bucket, rows_bucket, archetype)`
  running at prepare inside the CostSource seam — statistics AND the model's
  level-0 claim in, the bucket to use out. Programmable adjustment with the
  same determinism as a switch: stored, content-hashed, budgeted, identical
  in every process. A policy that cannot run fails the prepare by name (a
  probe call per compile); a per-input error degrades that one decision to
  no-discount, deterministically.
- **The read side** (`mpedb stats`): rows, buckets, NDV/analyze state per
  index — what the engine believes, as API + CLI. (SQL-queryable `mpedb_stats`
  waits for the synthetic-table seam with `mpedb_operators`.)

One real bug fell out and is worth the ink: stats records were guarded by the
schema GENERATION, and the generation also bumps for every cost-layer change —
so setting a tunable killed every NDV record until the next analyze. The guard
is now an index-identity FINGERPRINT (table name ‖ column names, the #118
names-not-ordinals rule in miniature): stats survive unrelated changes and die
exactly when `(table_id, index_no)` could mean a different index.

## 9.4 Measured 2026-07-23: what MPEE can still fix, and what it cannot

Three optimizations landed this day (covering index reads `8eeec0d`, filtered
fused fold `dca70b1`, selectivity-priced index range `daacf20`). What they left
behind was then MEASURED rather than guessed, and the split matters because it
says which residual belongs to the solver and which does not.

**The graph frontier, instrumented per level** (reach-8, 250k rows produced
through 8 levels, 2-core Linux, 251 ms total):

| level | queue in | rows out | `select_rows` |
|---:|---:|---:|---:|
| 1 | 1 | 1,123 | 0.6 ms |
| 2 | 1,113 | 5,657 | 4.0 ms |
| 3 | 5,237 | 26,084 | 18.5 ms |
| 4 | 17,708 | 87,961 | 61.9 ms |
| 5 | 21,378 | 105,863 | 77.6 ms |
| 6 | 3,984 | 20,069 | 17.1 ms |
| 7 | 196 | 994 | 1.1 ms |
| 8 | 7 | 38 | 0.1 ms |

**181 of 251 ms — 72 % — is inside the join**, and only 28 % is the fixpoint's
own bookkeeping (dedup, clones, accumulation). Confirmed from the other side:
making the dedup allocation-free bought 3 %, which is the useful part of that
result — it proved where the time is NOT.

**The plan is already the right plan.** The recursive term drives from the
frontier and probes `edge` through the covering `(src, dst)` index; there is no
better ORDER and no better ACCESS PATH to choose. The residual is ~0.73 µs per
produced row spent materializing it: `scan_by_index` allocates a `Vec<Value>`
per inner row, the join builds the joined tuple, the projection allocates the
output row — three allocations to carry two integers.

### What that means for the solver

**MPEE cannot price its way out of this one.** A cost model chooses among the
strategies that EXIST, and mpedb has exactly one join strategy: the nested
loop, index-driven when the inner side has a usable index and held otherwise
(`plan/mod.rs`, `JoinStep`). When the shape is already optimal, a better cost
model changes nothing — which is precisely the case here.

**The generic lever is therefore a new STRATEGY for it to choose, not a better
price for the old one.** A hash join is the one the measurements keep pointing
at, from two unrelated directions:

- the graph frontier at levels 4–5 probes `edge` 17k–21k times per level, each
  a fresh descent, where ONE ordered pass over the index and a hash probe per
  frontier node would touch each entry once;
- the OLAP star (`join-star-2`, 192 ms against SQLite's 111) drives 2M fact
  rows against a 20k-row dimension — the textbook hash-join shape, and the one
  DuckDB answers in 1.8 ms.

Its price is exactly the kind MPEE already computes: build ≈ |inner|, probe ≈
|outer| × 1 lookup, against nested-loop ≈ |outer| × (descent + fetch), with the
existing `max_join_cells` budget bounding what the build side may hold. That is
a strategy the solver picks BETWEEN, so the win generalizes to every shape with
a big outer and a small inner instead of being special-cased per workload.

**What stays outside the solver either way:** the per-row `Vec<Value>`. A hash
join reduces how MANY rows are materialized, not what one costs; closing the
last constant needs a streaming pipeline where a join step hands values to the
next operator without building a row. That is an executor-architecture change,
it is what both the Neo4j residual (2.9×) and the DuckDB scan residual are made
of, and it should be designed as its own document rather than inferred from a
benchmark.

## 10. What failure looks like (so we notice)

- A CostSource input that moves a plan hash on unchanged data — violates rule 2;
  the regression is a flapping hash in the registry, and the existing
  `plan/`-prefix census is the detector.
- An NDV bucket that makes a *wrong* plan (not a slow one): impossible by
  construction if pricing stays worst-case-shaped, which is why stage A changes
  the bound's *tightness*, never the semantics of BOUNDED.
- A vector scalar that guesses on malformed blobs: the widening rule
  ([[widening-can-create-wrong-answers]] discipline) — length mismatch is a
  refusal, tested at every truncation offset like every other decoder.
- Skew: `rows/ndv` under-prices a heavy key group. Log2 absorbs skew up to 2×
  per bucket; beyond that the plan is still *correct*, only its ranking was
  optimistic — and measurement 1 of SOLVER §9.1 (actual rows per join position)
  is the designed detector when the cost catalog starts persisting.
