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
    mod_gen: u64            -- the coherence tag (§6); the whole point
    n_rows: u32             -- rows in this block (PK order)
    ZONE MAP:  min, max      -- the block's value bounds (skip/aggregate from these)
    NULL bitmap
    encoding tag + payload   -- §4
```

Blocks are in PK order (the row tree's order), so block `k` covers rows
`[k·B, (k+1)·B)` of the scan — a scan reads blocks in order, and a PK-range
scan reads only the covering blocks.

## 4. Encoding — the LAYOUT is the compression. No entropy coder anywhere.

The lesson from mpee, stated precisely (matcodec `MTZU` + graph-hubs, verified
against the source): its late breakthrough is a **fixed-width, directly
index-addressable resident structure** — flat `Vec<u8>` hub ids and `Vec<i32>`
distances read as `[i*k+q]`, with **zero decompression on the lookup, bounds and
tolerance paths**. The smallness comes from the LAYOUT matching the data's
structure: `O(n·k)` directional labels replace the `O(n²)` matrix, which is why
path-aware hubs are **3.3× smaller resident** than the flat landmark table on
real London data (706 KB vs 2312 KB) while answering MORE cells, and why the
ratio *rises* with n (9.4× at 10k nodes).

Two honest corrections to the earlier draft of this section, both load-bearing:

- mpee's on-disk blob **is** deflate-based end to end, and a bit-exact lookup on
  a residual-bearing cell **does** inflate one frame. What is decompression-free
  is the *resident index* and the fast/bounds/tolerance paths. So "uncompressed"
  describes the hot structure, not the container.
- mpedb takes the hot-structure half and **drops the container half entirely**.
  There is no deflate, no zlib, no varint-then-inflate anywhere in this design.

That is not a compromise, it is the better trade here: a bit-packed
frame-of-reference block is *both* smaller than the row encoding *and* directly
scannable in place. An inflate step would buy a little more size on cold data
and cost decode on every scan — the opposite of what a scan wants.

The recipe, which is the same one mpee uses for matrices, `ch.rs` uses for
graphs and `knn.rs` uses for its `N×K` table: **find the structural anchors →
store each value as a short fixed-width label relative to them → keep a cheap
resident bound that says whether the anchor answer suffices → only what the
structure misses pays for decoding.** For a column, the separability axis is
sorting / correlation / clustering, so the anchors are per-block statistics:

- **Frame-of-reference + bit-packing** (Int64, Timestamp): store
  `value − block_min` in `ceil(log2(max − min + 1))` bits. The anchor is
  `block_min`. A fact measure whose per-block range is narrow collapses to a few
  bits per value, and the packed array is read by shift-and-mask — no decode
  pass, no intermediate buffer.
- **Delta + FOR** (sorted or slowly-varying — timestamps, monotone ids): store
  successive differences, then FOR over those. The anchor is the previous value.
- **Dictionary** (low-cardinality text/int — `category`, `store_id`): a
  per-block dictionary plus bit-packed codes. The anchor is the code table. This
  is also what makes `GROUP BY category` cheap: group on codes, translate once
  at the end.
- **Run-of-default** (the SPARSE case): a column that is mostly one value
  (0, NULL, '') stored as `(default, exceptions)` with the exceptions as
  `(row_offset, value)` pairs — mpee's default-as-prediction sparsity. "Not
  stored" means "the default".
- **Raw fixed-width** (Float64 in v1, and any block the others do not shrink):
  the plain `[f64]` array. Still a win over the row store, because it is one
  column contiguous instead of six columns strided.

Every one of these is **random-accessible by arithmetic** — block `k`, row `r`
is at a computable bit offset — which is what makes zone-map skipping (below)
and the hybrid tail (§5.1) possible at all. A deflated block would have to be
inflated whole before its first value could be read.

Best-of encoding is chosen **per block** at build time by trying the applicable
ones and keeping the smallest. This is where "you know more about the data by
the time you write it" lands: the choice is made from the block's own measured
min/max/cardinality/default-share, not from a schema declaration, so a column
whose character changes across its range gets different encodings per block. The
tag is one byte; decode dispatches on it.

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

## 5.1 Split storage: the row tree is the delta, segments are the main

The row B+tree and the column segments are not competitors — they are the two
halves of one store, and the boundary between them MOVES:

- **The row tree is the write side (delta).** Every insert, update and delete
  lands there, at full row-store speed. Nothing about the write path changes,
  ever — that is the point.
- **The segments are the mature side (main).** A compaction pass reads a
  settled range of the table and re-expresses it column-wise, choosing each
  block's encoding from what that block's data actually turned out to be.
- **A scan reads main + delta and merges.** Both halves are in PK order, so the
  merge is a concatenation, not a sort.

**v1 draws the boundary at "everything or nothing"** (§6): a `mod_gen` match
means no write has happened since the build, so the segments cover the whole
table and the delta is empty. That is exactly the bulk-load → compact → query
shape OLAP actually has, it needs no write-path change at all, and it is the
honest starting point because it makes the first measurement clean.

**Stage 5 moves the boundary continuously** — the gradual switch. A per-table
`colseg_watermark` (the highest PK covered by segments) plus a second counter
that distinguishes the two kinds of write:

- an **append above the watermark** leaves every segment valid — the new rows
  are simply delta;
- a **mutation at or below the watermark** invalidates (that block, once blocks
  are tracked individually; the table, before then).

A scan is then `segments over [min, watermark]` + `row fold over (watermark,
max]`, and the machinery for that union **already exists**: the fused fold
already splits a range and folds the remainder into the SAME accumulators on
`FoldStop::Stopped(resume_key)` (`aggregate.rs:1083-1099`), and `btree::cursor`
takes an arbitrary `pk >= watermark` lower bound as a first-class operation
(`btree.rs:1573-1585`). The accumulators are commutative over concatenated
ranges — the parallel fold already relies on that.

The write-path cost is one PK memcmp per written row against a resident
watermark. That is not free, which is why it is staged AFTER v1 is measured:
then the cost of append-invalidation is a number rather than a guess, and the
watermark can be justified against it instead of assumed.

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

**The mechanism: a per-table data-modification generation (`mod_gen`).** A
monotonic `u64` per table, bumped once per committed write transaction that
mutated that table's rows. A segment records the `mod_gen` it was built at; a
read on a snapshot trusts the segment **iff** the table's committed `mod_gen`
equals the segment's. Any write since the build bumps it → mismatch → the
segment is ignored and the row scan runs. Monotonic and per-table, so it cannot
alias: a bumped counter never returns to a prior value.

**Where it lives — the catalog directory value, not the meta.** The meta page
(`shm.rs:100-119`) is a fixed 112-byte logical record with no spare fields and a
checksum over all of it; a *per-table* counter is unbounded in count and cannot
go there. But the per-`(table, index)` tree-root directory already IS a B+tree
entry — `cat_tree_key(table_id, index_no)` (`engine/mod.rs:274-280`) mapping to
a fixed **16-byte `root u64 ‖ row_count u64`** value (`catalog_entry`,
`engine/mod.rs:1540-1559`). Widening that value to **24 bytes**
(`root ‖ row_count ‖ mod_gen`) gives the counter three properties for free:

- **Atomic publication with the root.** The commit already rewrites this entry
  for every touched `(table, index)` in one loop (`commit.rs:76-91`), inside the
  same catalog btree that the meta's `catalog_root` publishes under the existing
  fences. No new publication protocol, no new ordering to get wrong.
- **Snapshot reads for free.** `ReadTxn::row_count` (`read.rs:175-177`) already
  reads this entry through `catalog_entry(self, self.meta.catalog_root, …)`; a
  `mod_gen(table)` accessor is the same call taking the third field.
- **Free migration.** `catalog_entry`'s `len() != 16` check relaxes to accept
  16 (legacy → `mod_gen = 0`) or 24, and writes 24 from then on. An existing
  file keeps working and gains the counter on its next write — the
  no-backward-compat rule's "~free migration that saves your own files".

**Which tables get bumped.** The precise write set comes from
`WriteTxn::set_tree_root` (`write.rs:173-180`), the single choke-point every
row mutation funnels through. It already maintains `written_tables: u64`, but
that is a mod-64 bitmap and therefore lossy for `table_id >= 64`; a bump driven
by it would MISS a table and leave a stale segment live — a wrong answer. So
stage 0 adds an exact `HashSet<u32>` alongside it, populated at the same line.
Note that `table_roots`'s key set is NOT usable either: `tree_root` inserts on
read, so it includes tables the txn only looked at.

**Fail-safe by construction.** Every decline path — no segment, `mod_gen`
mismatch, unknown encoding tag, a decode that returns `Corrupt` — falls through
to the row scan. The segment can only make a query faster or be ignored; it can
never make one wrong. That is what lets it live outside the page format as a
regenerable artifact, and it is why the only commit-path change in the whole
feature is one counter increment.

It is small, but it IS commit-path, so it gets the full adversarial review the
verification-calibration rule reserves for commit-path/wire-format changes. It
also hands the derived-table machinery (DESIGN-TRIGGERS `[[model.derived]]`) a
precise staleness signal it currently lacks — worth building once, used twice.

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

**Stage 0 — the coherence primitive.** The per-table `mod_gen` counter (§6):
the 24-byte catalog directory value, the exact write set at `set_tree_root`,
the bump in the commit writeback loop, the snapshot accessor. Ships nothing
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

**Stage 5 — the moving boundary.** The `colseg_watermark` and the split
delta/main scan (§5.1): appends above the watermark keep the segments valid,
mutations below invalidate, and a scan unions segments with a row tail. Staged
last on purpose — it is the only part that touches the write path (one PK
memcmp per written row), so it is justified against a measured
append-invalidation cost from stages 1–2 rather than against a guess.

**Stage 1's honest wrinkle, recorded up front.** Summing a float column from
per-block partials changes the ORDER of additions, and float addition is not
associative, so the last digits of a `sum(FLOAT)` can differ from the row
scan's. This is not new — the parallel fold's merge already has exactly this
contract, and the OLAP bench's own re-runs move those digits — but the
differential tests must assert float sums within an epsilon while asserting
integer sums bit-for-bit, and the doc must say so rather than let a test
discover it.

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
