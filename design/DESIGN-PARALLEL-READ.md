# DESIGN-PARALLEL-READ — intra-query read parallelism: the substrate ceiling, measured

**Status: BUILT (partitioned parallel aggregate fold, 2026-07-22 — §8). Verdict was: BUILD
— read-only, cost-gated, nothing else.** The numbers half is `crates/mpedb/examples/par_ceiling.rs`, run on the M3
(Apple M3 Pro, 5 P-cores + 6 E-cores, 36 GB) at commit `66a0010`. Nothing here changed the
engine; every measurement went through today's public API. Method model:
[FOOTPRINT-INDEX-MEASURED.md](FOOTPRINT-INDEX-MEASURED.md).

The idea being tested: mpedb's concurrency machinery (MVCC snapshots, lock-free readers,
group commit) benefits parallel *requests*; a serial workload never touches it. But the same
substrate should let ONE job's work be split across threads — N readers on the same snapshot
coordinate on nothing, so a partitioned scan has near-zero coordination cost *if* the
executor splits the work. Before designing that executor, measure the ceiling **without
touching the engine**: hand-partition the PK range into N chunks, run the existing compiled
plan per chunk on N threads (`execute` / `stream_query` on `WHERE id >= $1 AND id < $2`),
merge accumulators in the caller. If hand-partitioned scans don't scale, no executor change
will.

## 0. What was run

- `par_ceiling <file|mem> <rows>` at 10 k / 100 k / 1 M / 4 M rows, thread sweep
  N ∈ {1, 2, 4, 8, 11}, each cell = min of 3 timed runs after 1 warmup, answers
  assert-equal to the unpartitioned statement's. Table: 5 int64 + 1 text column,
  ~64 B/row; dense PK 0..rows (the BEST-case split — a real executor splits on
  B+tree structure). Warm cache, no concurrent writers, `durability = "none"`.
- Two schedules per N: `equal` (one chunk per thread) and `morsel` (8N chunks pulled
  from a shared atomic counter — work stealing in its simplest form, because the M3's
  cores are asymmetric and the slowest partition gates the wall clock).
- Shapes: `scan` (stream + fold in the caller), `count`, `sum`, `g10`/`g10k`
  (GROUP BY, 10 and 10 k groups), `join` (1 M-row outer × 10 k-row inner, index
  nested loop, aggregated output). All partition plans compile to
  `PkRange(id >= $1, id < $2)` — verified via EXPLAIN, so partitions prune, not filter.
- Controls: bundled sqlite (rusqlite 0.31) serial on the same data, and sqlite
  hand-partitioned across N connections. DuckDB — the real incumbent for parallel
  analytics — was NOT measured: no `duckdb` CLI or Python module on the box, and
  installing one was out of scope. Stated as the gap it is: DuckDB would very likely
  win every aggregate below by a wide margin (vectorized columnar engine); the claim
  this doc makes is about mpedb's substrate, not about beating DuckDB.

## 1. The ceiling: the substrate is not the limit, the silicon is

Speedup over the unpartitioned statement (file mode; best of equal/morsel):

| shape | base ms @1M | n=2 | n=4 | n=8 | n=11 | @4M n=11 |
|---|---|---|---|---|---|---|
| scan | 158.4 | 1.78 | 3.39 | 4.85 | **5.48** | **6.44** |
| count | 157.3 | 1.79 | 3.34 | 4.63 | **5.43** | **6.20** |
| sum | 182.4 | 1.82 | 3.39 | 4.80 | **5.52** | **6.13** |
| g10 | 244.0 | 1.87 | 3.58 | 5.19 | **5.77** | **6.35** |
| g10k | 277.7 | 1.76 | 3.19 | 4.01 | **4.63** | **5.96** |
| join | 594.0 | 1.91 | 3.66 | 5.51 | **6.56** | **6.80** |

- **The curve never flattens from coordination.** 4 M rows scales *better* than 1 M
  (6.2–6.8× vs 5.4–6.6×), which is the opposite of what a lock, an allocator fight, or a
  memory-bandwidth wall would produce. 6.2–6.8× on 5 P + 6 E cores is ~85 % of the ideal
  throughput of that silicon (E ≈ half a P → ~7.5 P-equivalents, minus all-core clock
  droop). N readers on one snapshot really do coordinate on nothing.
- `:memory:` ≡ file within noise at 1 M — same mmap substrate either way; nothing here is
  about the storage backend.
- n=1 partitioned ≈ base (0.93–1.03×): the added range predicate costs nothing
  measurable. Thread spawn+join floor: 0.16–0.22 ms for 11 threads.
- The join scales best (6.8×) because it has the most CPU per row (a `PkPoint` inner
  fetch per outer row) — and per-partition inner state dedupes: partitioning the OUTER
  side left the inner side's plan untouched.
- **Snapshot pinning works today, verified two ways.** (a) A sidecar
  `Engine::open_from_file` handle read `ReadTxn::txn_id()` before and after the 11 stream
  opens: identical, so all 11 pinned the same commit. (b) Adversarially: with 11 partition
  streams open, a commit moved one row in *every* partition by 10^12; the drained streams
  summed to the pre-commit value exactly, and a fresh read saw all 11 markers. Pins happen
  at open, never lazily, never re-taken.

## 2. The two real findings inside the sweep

**Equal vs morsel is a wash for flat folds and a REGRESSION for wide GROUP BY.** Morsel
buys 3–8 % on scan/count/sum/join (stealing absorbs the P/E straggler) but *loses* 40 % on
g10k (4.63× equal vs 2.81× morsel @1M): every per-chunk `execute` builds and returns a
fresh 10 k-entry group map, so 88 chunks build 88 maps where 11 partitions build 11. That
is an artifact of partitioning at the statement boundary — a real parallel fold keeps **one
accumulator per thread across chunks** (the probe's own thread-local `absorb` already
shows the fix). Design rule: morsel scheduling is right, but the unit a morsel feeds must
be the fold's accumulator, not a fresh plan execution.

**The pay-off threshold is ~50–100 k input rows.** At 10 k rows the best cell is 1.25×
(n=4) and n=11 is 0.56× — *slower than serial*. At 100 k: 2–3×. At 1 M: 5.4–6.6×. The
median corpus statement (1 table, small result) will never benefit; this pays on big
scans/aggregates/joins only, and the gate must be an estimated-rows threshold, which MPEE
already has at compile time (row counts + footprint, DESIGN-MPEE-SOLVER).

## 3. The honesty section: sqlite serial, and what parallelism must not paper over

Same data, same statements, bundled sqlite, one thread (1 M rows, file):

| shape | sqlite serial ms | mpedb serial ms | mpedb n=11 ms | parallel mpedb vs serial sqlite |
|---|---|---|---|---|
| count | 4.7 | 157.3 | 29.0 | sqlite 6.2× faster |
| sum | 22.6 | 182.4 | 33.1 | sqlite 1.5× faster |
| g10 | 158.1 | 244.0 | 42.3 | **mpedb 3.7× faster** |
| g10k | 142.1 | 277.7 | 60.0 | **mpedb 2.4× faster** |
| join | 41.3 | 594.0 | 90.6 | sqlite 2.2× faster |

- mpedb's serial fold costs ~160–180 ns/row on count/sum where sqlite pays 5–23 ns/row —
  a 8–34× per-row gap. **Eleven cores buy at most 6.8×; the per-row overhead on simple
  folds costs more than that.** Where mpedb's per-row work is already competitive
  (GROUP BY), parallel mpedb beats serial sqlite comfortably; where it is not (count,
  sum, the nested-loop join at 594 ns/row), threads only shrink the deficit. Serial
  per-row cost reduction is a bigger lever than parallelism for those shapes and stays
  a prerequisite for calling any of this a headline.
- sqlite CAN be hand-partitioned the same way — one connection per thread on a file DB.
  Measured: never beats its own serial (0.19–0.74×; connection opens are inside the timed
  region, but they are also the honest cost of that architecture — per-connection page
  caches, and **no cross-connection snapshot guarantee at all**). mpedb's differentiator
  is not "threads can read" — it is N range-pruned readers on ONE pinned snapshot through
  one handle, coordination-free, answer provably equal to the serial one. sqlite cannot
  express that; DuckDB is the incumbent that can.

## 4. Where the split lives (design sketch, in dependency order)

1. **Gate.** MPEE prices the split at compile/solve time: estimated input rows of the
   fold ≥ threshold (~64 k, tunable) AND the plan is a read-only fold/scan the executor
   can partition (PkRange/FullScan under aggregate, the outer side of a join under
   aggregate). Everything else runs serial, unchanged. The corpus benchmark must not move.
2. **Split.** Partition the pinned snapshot's PK range engine-side — inside the executor
   where the `ReadTxn` already exists, on B+tree structure (not dense-id arithmetic:
   the probe's dense split is the best case). ~4 morsels per worker; workers pull from a
   shared counter (§2), each worker folds ONE accumulator across all morsels it pulls.
3. **Pin.** All workers read the SAME snapshot. Engine-side this is natural (share the
   already-pinned meta; reader slots are per-pin, so either N slots pinned to one txn_id
   or one slot shared by borrowing workers — the reader-pin protocol in DESIGN §4.3 needs
   no change if workers borrow the leader's pin and never outlive it). The probe had to
   *infer* same-snapshot through a sidecar engine handle because the facade exposes
   neither a snapshot handle nor a txn id — good enough for one operation's internal
   parallelism, and the missing `Database::snapshot()` / `RowStream::txn_id()` is only
   needed if hand-partitioning is ever made a public feature.
4. **Merge.** Accumulator combine already exists in spirit: count/sum/min/max add;
   GROUP BY maps merge (`exec/aggregate.rs` is a fold since `dac2ada`). The combine step
   is the probe's `Ans::absorb`, engine-typed.
5. **Threads.** A small pool owned by `Database` (spawn floor 0.2 ms is 10 % of a 2 ms
   statement), sized by config (`max_query_threads`, default = off → today's behavior).
   This is an embedded library: threads must be opt-in, capped, and never outlive the
   handle. WorkMeter budgets split per worker; `max_readers` must cover worker pins.

## 5. Out of scope, explicitly

- **Write-side intra-parallelism.** Measured dead: DESIGN-PHASE3 §2, the COW mutation is
  ~2 µs unavoidably serial, ceiling 1.28×. Not re-litigated here; read work only.
- **Small statements.** Below ~50 k input rows the gate keeps everything serial; the
  corpus's median statement stays byte-identical in behavior and timing.
- **ORDER BY / window / sorted streams** — a parallel sort-merge is its own design;
  today's sorting plans materialize anyway (DESIGN-STREAM-EXEC).
- **Serial per-row fold cost** (§3) — a separate, larger lever; parallelism must not be
  the excuse to leave 160 ns/row on the table.

## 6. What was not measured

- Parallel read UNDER a concurrent writer (reclaim pressure with 11 pinned readers).
- Cold page cache; anything larger than RAM; Linux (the 2-core box can't answer this).
- DuckDB (§0). >11 workers / oversubscription. macOS QoS pinning (default QoS spreads
  onto E-cores; a userInteractive hint might lift the 4-thread cells toward 3.9×).
- B+tree-structural splitting — the probe splits on dense ids; skew (sparse PKs, hot
  ranges) is exactly what morsel stealing is for, but it was not provoked.

## 7. UPDATE 2026-07-21 — the §3/§5 per-row constant: measured and cut

The "per-row cost FIRST" order was executed before any parallel executor work.
`examples/agg_prof.rs` (Linux dev box, 2 cores, 1M rows, min-of-5; the same
fixture as `par_ceiling`) attributes the serial fold's ~293 ns/row for
`count(*)`, then cuts the verified items. sqlite = bundled 3.45 via rusqlite,
serial, same data, same box.

Attribution at `31cb87c` (count(*) shape, 293.5 ns/row total): full-row decode
170.3 (its two per-row allocs — `Vec<Value>` spine + one text `String` — plus an
O(ncols²) offset recompute in `decode_column`-per-column), B+tree cursor walk
~74 (of which the per-row `key.to_vec` + inline-value `to_vec` pair ≈ 17),
executor fold ~47, work meter 2.0. Batch re-descent was measured NOISE:
`MPEDB_FOLD_BATCH` 256→1024 moved sum by 7 ns/row, →4096 by nothing, so
`FOLD_BATCH` stays 256 and the memory contract stays untouched.

What was cut, cumulative (ns/row, 1M rows):

```text
  shape                31cb87c   after   sqlite   change
  count(*)               293.5     0.7      0.3   leaf-wholesale key counting
  sum(a)                 347.1   150.7     30.3   1-column decode, borrowed cells
  GROUP BY 10 (c+s)      401.5   190.6    207.5   mpedb now BEATS serial sqlite
  GROUP BY 10k (c+s)     488.8   264.3    208.2   1.27x from parity
  RowCursor full drain   246.5   134.9      —     every full-row scan rides this
  decode_row (5 cols)    170.3    83.9      —     one-pass offsets
```

The cuts, in order of size:

1. **Decode only what the fold observes.** #125's `RowPrune::stage(0)` is now
   pushed INTO the scan (`TxnCtx::scan_rows_pruned` → `RowCursor::next_masked`
   → `row::decode_row_masked`): group keys, aggregate args + their FILTERs and
   the residual's own columns decode; everything else is `Null` without
   touching bytes. `count(*)` decodes an EMPTY row. The witness PK is pinned
   only when bare columns exist (the witness otherwise never reads it).
2. **`count(*)`-only + no residual = key counting.** `ReadTxn::count_range`
   counts leaf cells wholesale (`btree::Cursor::next_leaf_count`; only the
   hi-boundary leaf pays per-cell work) — sqlite's own count optimization.
   The #74 charges are the drain-scan's EXACTLY (`WorkMeter::charge_many`
   lands the refusal at `budget + 1`, same total, same label): faster, never
   cheaper.
3. **Borrowed cells.** `btree::Cursor::next_with` hands key+inline value
   borrowed from the page; the row decodes straight out of it, and the resume
   bound is the raw storage key (no per-batch PK re-encode). Kills 2
   allocs/row for every scan, not just aggregates.
4. **One-pass `decode_row`** (the O(ncols²) offset recompute hoisted).
5. **Fold hot path:** reused eval stack + group-key encode buffer (a hot
   group costs one encode + one map probe), bare-`PushCol` aggregate args
   read by reference (no interpreter, no clone). A `get_mut`/insert split was
   tried and REVERTED: two probes on a 10k-group BTreeMap cost more than the
   one small key alloc `entry()` needs.

Refused: answering `count(*)` from the catalog's O(1) `row_count` — it charges
0 work-rows where the scan charges N, which moves the #74 refusal point, and
the budget is a tested contract. The remaining `sum` gap vs sqlite (~5x) is
the row pipeline itself: a `Vec<Value>` per row plus the cursor walk, against
sqlite's decode-in-VDBE record format — cursor/fold fusion territory, not
constant-shaving.

Gates: `tests/agg_stream.rs` (incl. `MPEDB_FOLD_BATCH ∈ {1,2,7,256}`
invariance), `tests/agg_stream_mem.rs` slopes, `tests/prune_width.rs`,
`tests/runtime_budget.rs`, full `cargo test --workspace` ×3, clippy clean, and
the `select1-4` + `evidence/` corpus report byte-identical (9,489/9,689, the
same 4 flagged) vs a `31cb87c` build on the same box.

## 8. BUILT 2026-07-22 — the partitioned parallel fold, scoped by order-dependence

`exec/parallel.rs` + `mpedb_sql::parallel_fold_shape` (the single gate, EXPLAIN prints it)
+ `btree::partition_keys` / `ReadTxn::partition_range` (structural cuts) + `[runtime]
max_query_threads` (0 = auto `min(cores, 8)`, on by default; 1 = serial). Engagement:
snapshot read context ∧ shape gate ∧ est input ≥ 100k rows (`MPEDB_PAR_MIN_ROWS`
overrides for the batteries) ∧ ≥ 1 in-range tree cut.

**The semantic trap this section §4 did not cover — order-dependent aggregates — decided
the scope.** A parallel fold changes the order values meet the accumulator, and mpedb's
doctrine is bit-exact against a serial-order oracle:

- **Integer `sum` raises on INTERMEDIATE overflow, order-dependently.** Probed on the
  bundled 3.45.0: `[MAX, 1, -2]` raises "integer overflow" though the total fits; the
  same multiset as `[1, -2, MAX]` completes. Serial mpedb (`Accum`, i64 `checked_add`)
  raises iff SOME true prefix sum leaves i64 range. The parallel fold reproduces that
  raise EXACTLY: per contiguous partition an i128 monoid `(Σ, max-prefix, min-prefix)`
  (`ParSum`), composed in key order (`maxp = max(A.maxp, A.Σ + B.maxp)`); every prefix of
  the concatenation is a prefix of A or all-of-A-plus-a-prefix-of-B, i128 cannot overflow
  at any reachable row count, so raise-iff-serial-raises holds in BOTH directions — no
  §7.2-style raise divergence exists, so no RLS carve-out is needed. (The weaker
  raise-iff-the-TOTAL-escapes design — mathematically-exact map-reduce semantics, which
  would complete some statements serial raises — was considered and NOT needed; the exact
  monoid costs the same three words. If completion-on-intermediate-overflow is ever
  wanted, it is a deliberate SERIAL semantics change first, or it diverges from the
  oracle on the probe above.) A per-partition i64 fold is wrong in both directions and
  `par_fold.rs` carries the refuting probe (a suffix whose LOCAL sum overflows while
  every true prefix fits must complete).
- **Integer `avg`** parallelizes inside the f64-exactness window, checked at the merge:
  serial avg divides `fsum` (f64, scan order), which equals the exact i128 total iff
  every term and every prefix stays within ±2⁵³ — the monoid already carries the prefix
  extremes, a `big_term` bit covers the terms. Inside: `Σ/n` is bit-identical to serial
  (and to sqlite's compensated sum, whose compensation is zero on exact steps). Outside:
  the coordinator ABANDONS to serial before adoption (cost ≤ 1/k extra, rare — needs
  ~9·10¹⁵ accumulated magnitude), and serial's order-dependent low bits and i64 raise
  stand untouched.
- **Float `sum`/`avg`, `total`**: f64 accumulation is non-associative — partitioned low
  bits would differ from the serial-order oracle. Refused, permanently, per doctrine.
- **`group_concat`** (order IS the answer), **DISTINCT** (dedup sets span partitions),
  **host aggregates** (opaque state), **bare-column witnesses** (the all-NULL/
  all-filtered corners track the LATEST row; the witness carries no merge state),
  **`any`-typed min/max args** (Bool/Timestamp against another class are `sort_cmp`
  peers — incomparability breaks first-beat associativity): refused.
- **Proven in**: `count(*)`/`count(x)` (addition), `min`/`max` over bare non-`any`
  columns (total order per class; ordered merge reproduces the serial first-strict-beat
  witness, spelling included — `Accum::merge_ordered` lives beside the fold rules),
  GROUP BY structure over all of the above (per-worker maps merged per group in
  partition order; first partition's key spelling = serial first row's).

**Execution contract** (why every observable is serial-identical): workers share the
statement's own `ReadTxn` — same pin, same snapshot, ZERO extra reader slots — via
scoped threads bounded by the leader's borrow; each worker drains a contiguous piece cut
at B+tree separators through the SERIAL row body itself (`BatchScan` + `Folder`, or the
fused `fold_range_column` loop) — no second row-processing implementation; workers
charge the statement's own atomic `WorkMeter` (completed fold = exactly the serial
total); and ANY mid-flight error — work/cells budget trip (cells are a SHARED counter
across worker maps, so N maps cannot turn one budget into N), per-row program raise,
eviction — rewinds the meter and falls through to the serial paths, which then own the
authentic outcome (same deterministic `used`, same error text, same raise-vs-budget
precedence). The one merge-time error, integer-sum overflow, is returned directly on the
strength of the proof. Consequence: thread count is observable as wall time ONLY
(`par_fold.rs::thread_count_is_unobservable`), and the sole deliberate observable is
`mpedb::parallel_folds_engaged()`, which the batteries use to prove the path ran.

Gates run: `par_fold.rs` (16 differentials incl. the four overflow probes, budget
determinism vs serial, NOCASE tie spellings, snapshot-under-writes), full workspace,
clippy `-D warnings`, corpus byte-identical (§8.1), agg_stream/collate/over_index/
stream_mem/runtime_budget green. Numbers on THIS box (2 cores): §8.1.
