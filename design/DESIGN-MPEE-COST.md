# DESIGN-MPEE-COST — a self-tuning cost catalog + auto-indexing for MPEE

**Status: design (2026-07-18). Forward-looking (Phase 7+). Grows MPEE from a per-query cost model
into a self-tuning optimization subsystem. Grounded in what exists — exact transactional `row_count`,
live `CREATE INDEX` (#48), content-hashed plans, the MPEE cost broker (#12/#73). Ties #74, #76,
#80, [DESIGN-HTTP-RANGE.md](DESIGN-HTTP-RANGE.md), [DESIGN-DISTRIBUTED.md](DESIGN-DISTRIBUTED.md) §8.
Hard constraint (Morten): stats maintenance + auto-indexing are cheap / background / opt-in — NEVER
on the write hot path.**

**Update 2026-07-20 (#114):** the *consumer* of all this now exists. MPEE is a
real join-order solver ([DESIGN-MPEE-SOLVER.md](DESIGN-MPEE-SOLVER.md)), invoked
at every level of the compile recursion, and it reads its cost inputs through a
single seam (`RowCountFn`, consumed only as a magnitude bucket). Everything below
plugs into that seam without changing the solver: §5's determinism argument is
exactly the resolution the solver shipped with, and DESIGN-MPEE-SOLVER.md §9
names the four measurements this catalog should persist first.

**Update 2026-07-20 (#117) — the key is the PLAN HASH, measured.**
[FOOTPRINT-INDEX-MEASURED.md](FOOTPRINT-INDEX-MEASURED.md) censused the real corpus:
94,689 compiled statements → **81,036 distinct plans → 119 distinct footprints → 22
distinct table sets** (681 plans/footprint, 3,683 plans/table set; the footprint refines the
table set 5.41×). So a shape key pools plenty — but the plans it pools have wildly unlike
costs: the across-plan spread inside one footprint bucket is 20–52× the irreducible
within-plan spread, and the worst/best plan in a bucket differs by a median **217×**. **Key
§1's catalog on the plan hash; use the footprint only as a coarse index (which plans touch
table T) and for invalidation.** The same measurement returns don't-build verdicts for a
footprint conflict index, for memoized routing, and for delta-compressing `TableSet`.

## 0. Beyond `row_count`

MPEE prices plans today with exact `row_count` (transactionally exact, a real advantage). Richer, and
what Morten's asking for: a **persisted, continuously-updated cost catalog**, **access-method-aware**
cost functions, **per-consumer** specialization, and an **auto-indexing advisor** balancing read vs
write — all read by the planner at prepare time, none of it touching the mutator.

## 1. The cost catalog — persisted, continuously updated, cheap

A structure in the sys-keyspace (like the plan registry) holding, per table/column/index: distinct-
value count (NDV), a compact histogram, null fraction, per-index selectivity, and **access-frequency
counters** (how often a column is filtered/joined/ordered, a table scanned). MPEE reads it for
cardinality + selectivity far better than a single count gives.

- **Maintained cheaply**, respecting the constraint: running counters bumped incrementally on writes
  (O(1), no hot-path scan), and histograms refreshed by a **sampled, background/off-peak
  ANALYZE-like pass** — never a synchronous full scan on the write path. Exact `row_count` stays as
  the ground truth; the catalog adds *distribution*.
- **Survives no-downtime schema switches (#80):** a migration carries or recomputes the affected
  stats so MPEE is never blind right after a change. The catalog is versioned like everything else.

## 2. Access-method-aware cost (workload types)

Each access method registers its own cost function, so MPEE prices *unlike* operations correctly:
- PK point/range, secondary-index point/range (selectivity from §1),
- **FTS text search (#76)** — posting-list intersection cost is knowable (rarest-term length drives
  it); "always-given format on a text-search lookup" means MPEE has a deterministic cost shape for it,
- **future vector / ANN (RAG)** — approximate-nearest-neighbour has a recall-vs-cost profile
  (HNSW/IVF) unlike a B-tree; the cost model is **extensible per method**, so when a vector index
  lands it registers its cost and MPEE weighs a RAG lookup against a scan or an FTS match on the same
  footing. (Vector search is a separate feature; this is the framework that will accommodate it.)

The point: MPEE's cost function is a *registry of per-access-method cost estimators*, not a fixed
formula — text search, RAG, range scan each contribute their own, and MPEE optimizes across them.

## 3. Per-consumer specialization (the browser)

The catalog can be **weighted per consumer**. The browser/HTTP build (the two-mpedb model,
DESIGN-HTTP-RANGE) cares about *round-trips* and its *own* query set; it can carry a cost catalog
frequency-weighted to the pages/queries it actually uses (shipped with the WASM build or fetched
alongside the bootstrap span). So the browser's MPEE, with both a high fetch-cost `L` (§3 of
DESIGN-HTTP-RANGE) **and** stats tuned to its workload, "knows better" what to fetch and how much.

## 4. Auto-indexing advisor (the read↔write interplay)

Observe query shapes (which columns are filtered / joined / ordered — from the §1 access-frequency
counters) → estimate each *candidate* index's read-saving (from the catalog) against its **write-cost
+ space** → act on the net-beneficial ones. mpedb already has live `CREATE INDEX` (#48), so acting is
mechanical; the intelligence is the cost/benefit, which is MPEE's job.

- **Balances read vs write** from the observed read/write ratio — more indexes speed reads and slow
  writes; the advisor only adds an index whose estimated read-saving beats its write tax.
- **Drops the unused:** observation shows which existing indexes never help — recommend/auto-drop
  them (reversible).
- **Tunable + developer hints (the crucial knob):** an aggressiveness setting, a read-vs-write weight,
  and explicit hints — *"this table will be read-heavy on col X"*, *"future workload = text search on
  Y"* — so a developer who knows the future use makes the DB pre-adapt instead of waiting to learn it.
- **Conservative + reversible by default:** auto-indexing is risky (a bad index taxes every write), so
  it is opt-in, measured, and every auto-action is droppable. Recommend-only is the safe first mode.

## 5. The determinism fit

Plans are content-hashed. A change in the cost inputs (fresh stats, a new index) that changes the
*chosen* plan yields a **new plan hash** → re-prepare with the fresh cost — the existing re-prepare
path (`PlanInvalidated`). A cached plan stays valid until its cost inputs move enough to re-plan. So
adaptive re-optimization needs no new machinery: it is the plan-hash discipline mpedb already runs.
And because prepare-time is where the catalog is read, the mutator/write hot path never pays for any
of this.

## 6. Prior art + staging

Prior art: Oracle Automatic Indexing, SQL Server auto-tuning / Database Engine Tuning Advisor, DB2
Design Advisor, PostgreSQL `pg_statistic` + `hypopg` (hypothetical indexes for what-if costing),
learned indexes (Kraska et al.), and the self-driving DB line (CMU NoisePage/Peloton, Pavlo). The
twist mpedb brings: exact (not sampled) base cardinalities, content-hashed plans that make
re-optimization a clean re-prepare, and one cost model shared across local / HTTP / distributed.

Staging (Phase 7+, none built now):
1. **Persisted cost catalog** (NDV / histogram / selectivity / access-frequency) + MPEE reading it;
   cheap incremental + background-sampled maintenance.
2. **Access-method cost registry** (FTS first, vector when it exists).
3. **Auto-indexing advisor** — recommend-only, then opt-in auto-create + unused-drop, tunable + hints.
4. **Per-consumer / browser** catalog specialization.

Built only after the SQL-parity sprint. This is the layer that lets one MPEE cost model self-tune
across every deployment shape the other design docs describe.
