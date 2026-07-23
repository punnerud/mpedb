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
- **C — graph vs Neo4j.** `crates/mpedb-graphbench`, std-only HTTP to the tx
  endpoint; k-hop / bounded reachability / closure / triangles, same-answer
  checked. Plus the `risk.rs` depth-guard fix: a provably monotone
  `carried < const` guard prices as bounded instead of `u64::MAX` "unbounded".
- **D — vector: exact kNN + Qdrant.** BLOB f32 embeddings, `vec_l2`/`vec_cosine`
  scalars with strict shape refusal, early-abandonment scan, MPEE-priced filter
  placement (reusing A's NDV), recall@k reported next to latency.
- **E — #118 advisor, recommend-only.** `recommend_indexes(WorkloadSource)`
  as specified in WORKLOAD-INDEXES §4, costed through the A seam. Auto-create
  stays blocked on P2/P3/P5, restated not built.

## 8. What failure looks like (so we notice)

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
