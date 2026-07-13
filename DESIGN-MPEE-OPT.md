# DESIGN-MPEE-OPT — MPEE → mpedb optimization transfer (workbench synthesis)

Status: post-implementation, post-measurement. Companion to DESIGN.md §5.3 (intent
ring as-built) and §7.3 (footprints). Hypothesis under test: concepts from MPEE
(offline route-optimization engine, github.com/punnerud/mpee) transfer to mpedb's
query/batch execution. Verdict up front: **one MPEE idea transferred and shipped
(broker "buy once"), the headline idea (locality routing) was implemented, measured,
and falsified for this engine's COW model, and two ideas are staged with concrete
designs retained.** Details and verbatim numbers below.

## 1. Concept mapping

### 1.1 Streaming distance matrices under a memory budget → streaming SELECT (staged, small effort)

MPEE never materializes the N×N matrix; it streams cells under a budget. mpedb's
analog is streaming SELECT execution with LIMIT/OFFSET pushdown and bounded top-K
sort. Today `crates/mpedb/src/exec.rs` materializes the entire access-path result
before doing anything: `gather_rows` (exec.rs:387) collects every matching row into
`Vec<Vec<Value>>`, `sort_rows` (exec.rs:505) fully sorts, and only then does
`.skip(offset).take(limit)` apply (exec.rs:168). Even `SELECT * FROM t LIMIT 10`
over a FullScan clones the whole table. The engine layer already streams —
`scan`/`scan_raw` are btree-cursor based (mpedb-core/src/engine.rs) — so the fix is
facade-local: early-terminate at `offset+limit` rows when `order_by` is empty (the
planner's PK-prefix ORDER BY elision already makes the standard pagination shape
qualify), and use a bounded heap of size `offset+limit` when a sort survives.

Value: turns every LIMIT query from O(rows matched) time and memory into
O(offset+limit) — roughly 2500× fewer row materializations for LIMIT 20 over the
existing 50k-row bench table — and bounds executor memory for the first time.
Measurable via a new mpedb-bench cell; the differential sqllogictest harness
(mpedb-testkit) pins correctness against sqlite.

### 1.2 Cost-aware matrix broker ("buy only the cells the solver reads, cache them") → two analogs

**(a) Footprint-driven prefaulting (staged, medium effort, cold-cache only).**
DESIGN.md §5.2/§7.3 designed it ("writers should prefault their footprint pages
before taking the lock") but it was never built — no prefault code exists, only
MADV_HUGEPAGE (shm.rs). For `KeyAccess::Point` intents the exact PK is computable
from (plan, params) without executing (`exec::resolve_part` + `keycode::encode_key`),
so an enqueuer can probe that key in a read txn before enqueueing, faulting the
root-to-leaf path outside the lock; the ring leader then COWs already-resident
pages. Value: removes major page faults (disk reads) from inside the writer lock in
`durability = commit` on cold caches, shrinking leader lock-hold time and latency
tails. Honest caveat: warm-cache benchmarks will show approximately nothing — value
exists only for cold or larger-than-RAM working sets; measure with an evicted page
cache before believing it.

**(b) "Buy once" in the batch leader (SHIPPED — see §3).** The broker discipline —
pay for a cell exactly once, ahead of need — maps to the leader's prepare pass:
plan load and param decode now happen once per intent, outside per-intent
execution, instead of per-execution-attempt. The caching half of the broker concept
was already embodied by the content-addressed plan registry (compute once, hash,
share across processes).

### 1.3 O(1) constraint evaluation in the hot loop → already embodied; one residual

CHECK/WHERE compile once to a validated stack IR evaluated in a tight loop
(mpedb-types::expr), plans are content-hashed and cached, and footprints precompute
routing so `execute(hash, params)` parses and plans nothing. The one non-O(1) left
in the batch hot loop: `WriteTxn::savepoint` (engine.rs:1081) clones the entire
`dirty`/`freed` sets and `table_roots` map per intent, so per-intent savepoint cost
grows with the batch's accumulated dirty set. Cheap today (pages/batch ≤ ~4
measured, §4); becomes a target only if batch sizes grow — the fix is delta-logging
savepoints instead of set clones.

### 1.4 Bucket-based many-to-many with KNN → batch key grouping (shipped as part of §3)

The sort key buckets drained intents by written table id, then orders by encoded
key bytes within the bucket — the direct analog of MPEE's bucketing. True KNN has
no analog: keys already live in a total order and the B+tree *is* the bucket
structure, so "nearest neighbor" degenerates to "adjacent in keycode order".

### 1.5 Triangle-inequality derivation of long-range values → does not transfer

MPEE derives far cells from bought skeleton cells because road distance is a metric.
Key space has no useful metric structure over page placement: distance between two
keys bounds nothing about the distance between their pages. The actual "geometry"
is root-to-leaf path sharing in the COW tree, and the pager already exploits it
(a page dirtied once in a txn is mutated in place thereafter). Nothing to build.

### 1.6 Lossless matrix compression → does not transfer

Pages are fixed-size and msync operates at page granularity; sub-page compression
is a format change, not an optimization, and the row codec (null bitmap + fixed +
varlen) is already compact. The nearest existing analog is canonical plan bytes in
the registry, which is about identity, not size.

### 1.7 Hierarchical cluster-first decomposition with boundary re-polish → staged concept

Maps to future multi-table batch scheduling: partition a drained batch by disjoint
table footprints (cluster-first), execute partitions with independent savepoint
scopes, and handle cross-table intents at the boundary. Pointless until batches are
large and multi-table; today's measured batches average ~6 intents. Retained as a
concept only — no design written.

## 2. Three designs considered; judge's pick

- **Design 0 — ring-locality-sort** (drain the group-commit batch in key-locality
  order rather than slot order). PICKED.
- **Design 1 — streaming-limit** (§1.1): bounded SELECT execution in exec.rs.
- **Design 2 — footprint-prefault** (§1.2a): pre-lock prefaulting of Point paths.

Judge's reasoning for design 0, verified claim-by-claim against the tree:
`collect_ready` returns READY intents in slot order (mpedb-core/src/ring.rs:353-375)
while `enqueue` starts its EMPTY-slot scan at a pid-hashed offset (ring.rs:205), so
execution order was already arbitrary w.r.t. arrival — reordering is a **free choice
of linearization within one meta flip, not a semantic change**. The leader executes
the drained batch in one COW txn with per-intent savepoints
(mpedb/src/ring_exec.rs; `savepoint`/`rollback_to` are order-independent), and
`commit_with` msyncs coalesced contiguous dirty-page runs, so a `dirty_page_stats`
run count exactly equals msync calls issued — the claimed mechanism (adjacent keys
share COW root-to-leaf paths → fewer dirty pages → fewer/shorter msync runs) was
real and *directly observable*, making the design falsifiable. `resolve_part` was a
one-line visibility change. Small blast radius: commit path, locking, posting, and
recovery all untouched. Designs 1 and 2 remain live (§6).

## 3. What was implemented

Ring-locality-sort with all three judge grafts. Files changed:

- **`crates/mpedb-core/src/engine.rs`** — one read-only getter
  `WriteTxn::dirty_page_stats(&self) -> (usize, usize)` (dirty page count,
  contiguous page-id runs = the msync_range calls step 3 of `commit_with` would
  issue for the current dirty set). `commit_with`, locking, posting, recovery
  untouched.
- **`crates/mpedb/src/exec.rs`** — visibility only: `fn resolve_part` →
  `pub(crate) fn resolve_part` (exec.rs:372). No body changes.
- **`crates/mpedb/src/ring_exec.rs`** — the substance:
  - Graft 1 ("buy once", §1.2b): a `prepare_intent` pre-pass loads the plan
    (`cached_or_load`) and decodes params once per intent into
    `PreparedIntent { intent, prepared: Result<(Arc<CompiledPlan>, Vec<Value>)> }`;
    execution reuses them via `execute_prepared` — no double decode, no second
    plan-cache lock. Error checks mirror the old prelude in order, so per-intent
    slot errors are byte-identical; pre-pass errors are staged directly.
  - Graft 2 (total, deterministic sort key): `(written table id via
    tables_written.trailing_zeros(), rank, key bytes, slot idx)`. `Point` resolves
    all PK parts to `keycode::encode_key` bytes; `Range` uses its lo bound
    (unbounded-below = empty bytes); `Full`/unresolvable keys rank last within
    their table; slot idx is the final tiebreak so same-key intents keep relative
    slot order and duplicate-PK races resolve exactly as before.
  - Graft 3 (falsifiability): `MPEDB_NO_BATCH_ROUTING=1` (alias
    `MPEDB_RING_NO_SORT=1`) restores slot-order drain for A/B;
    `MPEDB_RING_STATS=1` prints per-batch pages/runs/intents lines backed by
    `dirty_page_stats`.

DESIGN.md §5.3 was updated to describe the as-built drain order and the two env
switches. All gates passed at implementation time.

## 4. Measured results (verbatim)

MEASUREMENT REPORT — ring-locality-sort A/B (build: release mpedb-cli, host disk =
ext4 on /dev/sda1; scratch dirs cleaned)

THROUGHPUT (mpedb stress, 8 workers, 6 s, disk dir, --durability commit; ON =
default sort, OFF = MPEDB_NO_BATCH_ROUTING=1; runs interleaved ON/OFF; all runs
verify: ok)

| config              | arm | r1 (ops/s) | r2    | r3    | median | Δ median   |
|---------------------|-----|-----------|-------|-------|--------|------------|
| disk commit, unique | ON  | 15427     | 15189 | 14362 | 15189  | +1.4%      |
| disk commit, unique | OFF | 14980     | 15220 | 14454 | 14980  |            |
| disk commit, mixed  | ON  | 5546      | 5566  | 5446  | 5546   | -2.0%      |
| disk commit, mixed  | OFF | 5661      | 5516  | 5672  | 5661   |            |
| tmpfs none, mixed   | ON  | 139554    | —     | —     | —      | +2.2% (1 run/side; ring inactive both arms → direct-path noise, zero regression confirmed) |
| tmpfs none, mixed   | OFF | 136542    | —     | —     | —      |            |

Run-to-run spread within a single arm reaches 7.4% (unique ON: 14362–15427), so
both disk deltas are inside noise.

PAGE INSTRUMENTATION (MPEDB_RING_STATS=1, separate runs — stats writes perturb
timing so these are excluded from the throughput arms; disk commit, multi-intent
batches only)

| mode   | arm | batches | intents/batch | pages/batch | msync runs/batch | pages/intent |
|--------|-----|---------|---------------|-------------|------------------|--------------|
| unique | ON  | 10108   | 6.53          | 0.17        | 0.13             | 0.025        |
| unique | OFF | 10687   | 6.34          | 0.15        | 0.11             | 0.024        |
| mixed  | ON  | 3128    | 6.14          | 4.23        | 3.15             | 0.689        |
| mixed  | OFF | 3263    | 6.20          | 4.26        | 3.18             | 0.686        |

nokey/batch = 0.00 in all arms (every intent resolved to a Point key — no pressure
yet to build the Range-aware slice).

VERDICT: Locality routing did not help — throughput deltas (+1.4% unique, −2.0%
mixed) are inside the ~7% run-to-run spread, and direct instrumentation shows
pages-per-batch (4.23 vs 4.26) and msync-run counts (3.15 vs 3.18) identical to
within 1%. The page counters show why: the pages copied in one COW transaction are
the UNION of root-to-leaf paths over the batch's key set — a set property that is
order-independent, because an already-dirty page is mutated in place no matter when
it is revisited.

## 5. Honest verdict on the hypothesis

- **Transfers NOW, shipped:** the broker's "buy once" discipline (§1.2b prepare
  pass — order-independent, one plan-cache lock and one param decode per intent)
  and instrumentation-as-a-first-class-output (`dirty_page_stats`, ring stats).
- **Implemented but falsified:** batch routing = route optimization. The mechanism
  was real and the implementation correct, but the payoff assumed pages-copied
  depends on visit *order*; in a single COW transaction it depends only on the key
  *set*. MPEE's routing wins because travel cost is path-dependent; a COW dirty set
  is not. The sort is retained anyway: it costs nothing measurable, makes batch
  linearization deterministic (reproducibility, stable duplicate-PK resolution),
  and the kill switch preserves the A/B. Ordering *could* matter across
  transactions (tree-shape evolution from sorted inserts) — unmeasured, out of
  scope.
- **Staged for later, designs retained:** streaming-limit (§1.1, next; largest
  expected measurable win) and footprint-prefault (§1.2a, cold-cache harness
  first). The cost-matrix/broker idea proper — buy selectivity/cardinality cells
  on demand and cache them — becomes relevant when joins and multi-index selection
  arrive; the catalog's exact transactional `row_count` per table/index is the
  already-paid skeleton cell.
- **Does not transfer:** triangle-inequality derivation (§1.5, no metric), matrix
  compression (§1.6, page-granular durability), KNN proper (§1.4, total order
  already exists).

## 6. Roadmap

1. **streaming-limit** (small): early-terminate + bounded heap in exec.rs; bench
   cell in mpedb-bench; differential correctness via mpedb-testkit vs sqlite.
2. **Cold-cache harness, then footprint-prefault** (medium): evicted-page-cache
   benchmark mode first — without it the result is unmeasurable (§1.2a caveat).
3. **Savepoint delta-logging** (small-medium): only if batch sizes grow past the
   current ~6 intents / ≤4 pages; §1.3 residual.
4. **Re-measure locality sort on a Range-heavy workload** before deciding whether
   to keep `MPEDB_NO_BATCH_ROUTING` long-term (nokey/batch = 0.00 today means the
   Range path is untested by real load).
5. **Cost broker for the planner** when joins/multi-index selection land: exact
   `row_count` + on-demand cardinality probes cached in the plan registry (§5).


## Cross-query sharing of optimization artifacts — and the privacy boundary

(Added from the 2026-07-13 discussion.) When cost/statistics machinery arrives (joins,
non-unique indexes), optimization work must be shared ACROSS queries so similar ones
never re-derive it — MPEE's broker cache applied at the workload level. Two rules govern
that shared cache, the second being a hard privacy requirement:

1. **Share by plan shape, never by values.** Cache keys are canonical plan bytes (the
   structure the caller already possesses) plus a stats epoch — `WHERE id = $1` shares
   one entry for every parameter value. This is also what keeps content-addressed plan
   hashes stable: statistics inform *costing*, never the plan identity.
2. **The shared cache must not be a side channel onto neighbors' data.** Shared
   optimizer state famously leaks (timing + value-level statistics let one client infer
   another's data). Therefore: the shared layer holds only aggregate, coarse statistics
   (row counts — already transactionally exact in the catalog — and bucketed key ranges);
   parameter values, sampled keys, and per-value frequency histograms never enter shared
   memory in queryable form. If value-level histograms are ever needed, they stay
   per-process or are quantized to buckets wide enough to be non-identifying, and that
   trade-off is documented at the feature, not discovered after it.

## Streaming LIMIT/top-K — shipped 2026-07-13

The mapping's top opportunity was implemented immediately after this workbench run:
`TxnCtx::scan_rows_capped` pushes the residual filter and an `offset+limit` cap into the
B+tree cursor for autocommit SELECTs (collect-then-mutate paths keep the uncapped
gather). Measured on a 50k-row table (release, Python bindings): `LIMIT 10` went from
8,712 µs to **9.2 µs per query (947x)**; filtered `LIMIT 10` 12.5 µs; semantics pinned by
the sqllogictest LIMIT/OFFSET corpus and the sqlite differential harness. Deep OFFSET
remains O(offset) by nature (keyset pagination is the eventual answer). The bounded
top-K heap for surviving ORDER BY is the staged follow-up. The planner
precedence defect found by the same analysis (PkRange chosen over IndexPoint) was also
fixed, with a regression test.
