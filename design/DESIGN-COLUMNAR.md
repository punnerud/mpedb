# DESIGN-COLUMNAR — model-driven column segments, priced by MPEE (design)

**Status: design only. No code written.** This is the design document the
"columnar storage" task requires before implementation + review, in the
discipline of DESIGN-TRIGGERS and DESIGN-MPEE-GENERAL. It settles the ONE hard
question — coherence — before any storage code, because a stale column segment
returns a **wrong answer**, not a mis-price.

## 1. Why — the gap is the storage model, measured

The 2026-07-23 OLAP head-to-head (BENCHMARKS-OLAP.md, `4643aeb`, 2M-row fact):

| cell | mpedb vs SQLite | mpedb vs DuckDB |
|---|---|---|
| `scan-sum` | 1.1× slower | 86× slower |
| `scan-filter-sum` | 2.2× slower | 174× slower |
| `group-large` | **2.5× faster** | 46× slower |
| `join-star-4` | 1.5× slower | 68× slower |

**Read the two columns against each other.** mpedb and SQLite are both row
stores, so their ratio measures the ENGINE — and there mpedb is within 1–2× and
ahead on several cells after the day's executor work (covering reads, filtered
fold, selectivity-priced ranges, hash join). DuckDB's column measures the
STORAGE MODEL, and the 46–290× there is the row-vs-column gap. It has two
halves:

1. **Touched bytes.** A `sum(amount)` scan on a 6-column fact table reads every
   row's whole ~50-byte record out of the PK tree to extract 8 bytes of
   `amount`. A column store reads only the `amount` column's contiguous bytes.
2. **Block skipping.** A `WHERE day_id >= 1000` scan visits every row. A column
   store keeps a per-block min/max and skips whole blocks the predicate cannot
   satisfy — untouched, undecoded.

This document closes the storage half. The other half — DuckDB's vectorized
SIMD kernels against mpedb's per-row `Value` — is the executor axis
(DESIGN-MPEE-GENERAL §9.4) and stays out of scope here; it is the smaller half
and a different design.

## 2. The model decides column-vs-row — automatically

No user picks a storage mode per column. The workload model
(DESIGN-MODEL-LANG) already declares intent, and that declaration IS the
decision:

- **`role = "fact"`** with a `scan` / `group-by` / `filter-range` access, or
  **`archetype = "star-olap"`** — the table is read by scanning and
  aggregating. Its scanned columns are **columnar-eligible**.
- **`role = "dimension"`** with `point` / `filter-eq` access, or
  **`archetype = "oltp"`** — read by key. Stays a row store; a column segment
  would never be read and would only tax writes.

So "column vs row" is not a new knob — it is what the model already says the
data is FOR, made physical. A database with no model gets no segments and
behaves exactly as today; the feature is opt-in through the model, which is the
opt-in the model exists to be.

**Sparse / dynamic (the user's ask), via MPEE.** MPEE does not build a segment
for every fact column — it builds them for the columns the workload actually
scans, drawn from the model's `access` declarations and the level-2 statement
list (the same corpus the advisor reads). A fact column never aggregated gets
no segment; a default-heavy column gets the sparse encoding (§4). The set is
**dynamic**: segments are regenerable (§5), so the advisor can recommend adding
or dropping them as the observed workload moves — `mpedb model sync-columnar`,
the twin of `sync-derived`.

## 3. What a column segment is (NOT a new page format)

A segment is a **regenerable, read-optimized artifact**, keyed in the
sys-keyspace exactly like a stats record or a derived table — NOT a change to
the page format, the row codec, or canonical bytes. This is deliberate and
load-bearing: it keeps the whole feature out of the commit-path / wire-format
review surface (DESIGN.md's 37-finding perimeter). The row B+tree stays the
source of truth; a segment is an accelerator over it, the covering-index
philosophy applied to scans. If a segment is missing or stale, the query runs
the row scan — never wrong, only slower.

Layout, per `(table_id, column)`, blocked (default 65 536 rows/block so a block
fits well under the 1 MiB `SYS_RECORD_MAX_VALUE`, one sys-record per block):

```
colseg/<table_id>/<column>/<block_no>  →
    MAGIC "MCOL" | u16 COLSEG_FORMAT
    table_gen: u64          -- the coherence tag (§6); the whole point
    n_rows: u32             -- rows in this block (PK order)
    ZONE MAP:  min, max      -- the block's value bounds (skip/aggregate from these)
    NULL bitmap
    encoding tag + payload   -- §4
```

Blocks are in PK order (the row tree's order), so block `k` covers rows
`[k·B, (k+1)·B)` of the scan — a scan reads blocks in order, and a PK-range
scan reads only the covering blocks.

## 4. Encoding — "smarter than zip" is the predictor, not the coder

The mpee codec's central lesson (matcodec, verified): the win over gzip is a
**model that makes the residual near-zero**, then a stock entropy coder — it
literally calls zlib. The transferable stack is **predict → residual →
zigzag+varint → deflate**, chosen per block from the column's own structure:

- **Frame-of-reference** (numerics): store `value − block_min`; the residuals
  are small non-negative integers, bit-packed to `ceil(log2(max−min+1))` bits.
  A fact measure in a narrow per-block range collapses to a few bits/value.
- **Delta** (sorted or slowly-varying columns — timestamps, monotone keys):
  store successive differences, then FOR over those.
- **Dictionary** (low-cardinality text/int — `category`, `store_id`): a
  per-block (or per-segment) dictionary + bit-packed codes. This is also what
  makes a `GROUP BY category` cheap: group on codes, translate once.
- **Run-of-default** (the SPARSE case): a column that is mostly one value
  (0, NULL, ''), stored as `(default, exceptions)` — the exceptions as
  `(row_offset, value)` pairs. "No data stored" means "the default", mpee's
  default-as-prediction sparsity. This is the sparse/dynamic encoding the user
  named.
- **zigzag + varint before deflate** on any residual stream — mpee measured
  26–36 % smaller than deflate-over-raw, and it is cheap.

Best-of encoding is chosen per block at build time by trying the applicable
ones and keeping the smallest (mpee's best-of-two, generalized). The tag is one
byte; decode dispatches on it.

**Aggregates read the summary, not the values** — the columnar payoff:

- `min` / `max` → the zone map, no payload touched at all.
- `count(*)` → block `n_rows`; `count(col)` → `n_rows − popcount(null bitmap)`.
- `sum` over a frame-of-reference block → `block_min × n + Σ residuals`, and the
  residual sum can itself be precomputed and stored, making the block's `sum`
  O(1). (mpee stores per-block summaries for exactly this reason.)

**Filtered scans skip blocks** — the block-skipping half of §1. A
`WHERE day_id >= 1000` scan checks each block's `[min, max]`: a block wholly
below 1000 is skipped whole (never decoded); a block wholly at or above is
taken whole (predicate needs no per-row test); only a straddling block decodes
and tests. This is the DuckDB advantage a row scan structurally cannot have,
and it composes with the filtered fused fold already shipped (`dca70b1`).

## 5. Built by a pass, regenerable — not on the write hot path

Segments are built by an explicit pass, `Database::compact_columns()` (folded
into `analyze` or run as a `vacuum`-like step), matching how OLAP actually runs:
bulk load → analyze → query. The pass streams the column a block at a time
(mpee's `RowSource` model, peak memory one block, not the table), encodes,
writes the sys-records, and stamps each with the table's current generation
(§6). Nothing here touches the write path; a heavy write workload simply leaves
segments stale (and thus unused) until the next pass, which is correct.

## 6. Coherence — the one thing that must be exact

A stats record may be stale: it only mis-prices, and a mis-price is slow, not
wrong. **A column segment may not be stale**: reading a value the table no
longer holds is a wrong answer. So the reuse test must be EXACT, and this is
the design's load-bearing decision.

Heuristic guards are rejected outright. `(row_count, pk_root_page)` looks
tempting but is not bulletproof: a delete+insert can restore the count, and the
freelist can hand a committed write a root page id equal to a freed old root —
either aliases two different table states as "unchanged". A wrong answer is not
acceptable at any probability.

**The mechanism: a per-table data-modification generation.** A monotonic
`u64` per table, bumped in the commit path on every committed write that
touches the table's rows (insert / update / delete), stored in the meta's
table directory beside the tree root. A segment records the `table_gen` it was
built at; a read on a snapshot trusts the segment **iff** the table's committed
`table_gen` equals the segment's. Any write since the build bumps the gen →
mismatch → the segment is ignored and the row scan runs. Monotonic and
per-table, so it cannot alias: a bumped counter never returns to a prior value.

This is the only commit-path touch in the whole feature — one counter
increment per write txn per touched table, published in the same meta the tree
roots already are, under the same fences. It is small, but it IS commit-path,
so it gets the full adversarial review the verification-calibration rule
reserves for commit-path/wire-format changes. It also hands the derived-table
machinery (DESIGN-TRIGGERS `[[model.derived]]`) a precise staleness signal it
currently lacks — worth building once, used twice.

**Fail-safe by construction.** Every decline path — no segment, stale gen,
unsupported encoding, a decode that returns `Corrupt` — falls through to the
row scan. The segment can only ever make a query faster or be ignored; it can
never make one wrong. That is what lets it live outside the page format as a
regenerable artifact.

## 7. MPEE prices column-scan vs row-scan

The access-path choice becomes a real cost decision the solver makes, extending
the CostSource seam (DESIGN-MPEE-GENERAL §9.3):

- **Row scan** cost ≈ `rows × row_width` bytes touched.
- **Column scan** cost ≈ `Σ blocks (not pruned by the zone map) × block_bytes`
  for the referenced columns only — and under a range predicate the pruned
  fraction is estimable from the NDV/zone-map statistics the analyze pass
  already gathers.

So a `sum(amount)` over a fact table prices the column scan far below the row
scan and takes it; a `SELECT *` prices the row scan below (a column store would
have to stitch every column back into a row) and keeps it. The solver picks per
query from measured widths, not a hardcoded rule — which is the whole "automatic
via MPEE" ask, and the reason column-vs-row belongs in the cost layer rather
than in a per-table flag.

## 8. Staged plan (each stage ships, measurable, fail-safe)

**Stage 0 — the coherence primitive.** The per-table `table_gen` counter (§6),
its commit-path bump, its meta publication, its snapshot read. Ships nothing
user-visible; gets the full commit-path review. Everything else depends on it
and nothing else does, so it is the isolated, reviewable base.

**Stage 1 — numeric column segments + scan aggregates.** `compact_columns` for
Int64/Float64/Timestamp columns (frame-of-reference + zone map), read by the
fused fold for whole-table `sum`/`count`/`min`/`max`. The first measurable win:
`scan-sum` should fall toward the touched-bytes floor (one column, not six).
Differential-tested against the row scan, bit-for-bit, on every corpus shape.

**Stage 2 — zone-map block pruning for filtered scans.** The block-skip path
for range and equality predicates (the larger DuckDB-shaped win, `scan-range`
and `scan-filter`). Composes with the filtered fold.

**Stage 3 — dictionary + sparse + group-by.** Dictionary encoding, run-of-
default, and `GROUP BY` on dictionary codes — where the group cells already ahead
of SQLite widen further.

**Stage 4 — MPEE pricing + model-driven build.** The cost decision (§7) and
`mpedb model sync-columnar` building the segments the model+advisor call for
(§2). This is where "automatic + sparse + dynamic via MPEE" lands in full.

## 9. What this is NOT

- **Not a full column store.** Rows stay the source of truth; point lookups,
  updates, PK joins, and `SELECT *` stay on the row tree. Segments accelerate
  scans and are ignored otherwise — the covering-index posture, at column
  granularity.
- **Not vectorized execution.** Reducing touched bytes is the storage half;
  the per-row `Value` materialization is the executor half and a separate
  document. Even fully staged, mpedb will not match DuckDB's SIMD kernels — the
  goal is to close the storage half (the larger one) and stay ahead of SQLite,
  which is a row store with no columnar answer at all.
- **Not a new on-disk format.** Sys-keyspace records, regenerable, fail-safe;
  no canonical-bytes or PLAN_FORMAT change. The only format addition is the
  versioned `colseg/…` record (bounds-checked, truncation-tested,
  `Corrupt`-never-panic) and the one `table_gen` field in the meta directory.

## 10. Open questions for review

1. **Block size.** 65 536 rows balances zone-map granularity (skip resolution)
   against per-block overhead; measure against 8 K and 256 K on the OLAP fixture.
2. **`table_gen` placement.** In the meta table directory (published atomically
   with the roots) vs a sys-record bumped in the same txn. The former is one
   more `u64` per table in a fixed-size structure; the latter avoids touching
   the meta layout. Recommendation pending the commit-path review.
3. **Rebuild trigger.** Explicit pass only (v1) vs an advisor that flags a
   segment as "stale enough to rebuild" from the gen delta. v1: explicit.
4. **Interaction with `[[model.derived]]`.** A derived reverse-edge or counter
   is itself a table and could carry segments; confirm the gen counter composes
   with trigger-maintained tables (it should — a trigger write is a write).
