# DESIGN-PARALLEL-READ — intra-query read parallelism: the substrate ceiling, measured

**Status: BUILT (adaptive morsel scheduling for the order-independent aggregate fold,
2026-07-22 — §9). The verdict was: BUILD — read-only, cost-gated, nothing else; §8 then
replaced the cost gate with runtime scheduling and §9 measures the result.** The numbers half is `crates/mpedb/examples/par_ceiling.rs`, run on the M3
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

## 8. UPDATE 2026-07-21 — schedule ADAPTIVELY, not on a compile-time gate

§4's up-front gate ("parallel iff estimated rows ≥ threshold") is a
*prediction*, and it predicts exactly the quantity MPEE refuses to guess: the
UNKNOWN selectivity class (`LIKE '%x%'`, `f(col) > 0`, a bound-parameter range).
A gate that mispredicts either eats the full serial cost on a query it called
small, or pays the 0.56×-at-10k thread-startup tax on one it called big. Both
misses hurt, and the estimate is worst exactly where it matters.

**Replace the gate with runtime scheduling. The calling thread is worker 0.**
It begins draining morsel 0 (the lowest key range) immediately — zero added
latency, serial speed. The remaining key-ordered morsels go into a
work-stealing queue. A *small* query is finished by worker 0 before any helper
engages; a *big* one has its tail stolen. **The data decides, at run time, with
no estimate.** Helpers only ever do work worker 0 would have done anyway
(morsels are disjoint), so there is no wasted work and nothing to cancel.

Why NOT the alternative — run serial and parallel independently and race, first
to finish wins: it burns 2× CPU to one winner and must cancel the loser. In a
single-process engine (DuckDB) idle cores are free; **mpedb's whole
differentiator is many processes on one file**, so a racing query steals cores
from the other processes' requests. Racing is wrong for this architecture; the
worker-0 morsel model is not (helpers engage only when there is a queued morsel
AND a free core).

**The one cost unique to mpedb: fan-out width must be budgeted.** DuckDB never
thinks about this; we must. A greedy analytical query must not monopolise
reader slots and cores that concurrent *processes* want. Bound the helper count
by (free cores, available reader slots, a concurrency budget) and let a helper
yield back the instant contention appears. The footprint's role here is
UPSTREAM and STATIC — it proves the op is read-only and gives the disjoint key
ranges to cut (am-I-allowed + where), never the dynamic keep-going signal, which
is runtime feedback (morsels/s, queue depth).

**Assembly and combine, by result shape:**
- **Order-independent aggregates** (`count`, `min`, `max`, `i128`-summed
  integer `sum`): each worker holds a partial accumulator; merge is one scalar
  reduction — genuinely near-free, and zero-copy (shared mmap source, no row
  materialised — the §7 fusion).
- **Row-producing queries:** worker 0's output (lowest key range) is emitted
  first and can stream immediately; a helper that finishes a LATER partition
  must BUFFER until earlier ones are emitted, because byte-identity with
  sqlite binds output to key (= scan) order. So "first streams, others fill the
  remainder" is right, but into an *ordered* buffer. The assembly spine is a
  pointer concat (cheap); the row *bodies* were still built per worker — that is
  the §7 per-row constant, paid in parallel, not removed by zero-copy. Only for
  aggregates does the combine reduce to a scalar merge.

Zero-copy between cores is real but bounded in what it buys: intra-query
parallelism is THREADS in one process sharing the address space, so all workers
read the same pages with no duplication (this is why §1 scaled to 6.8×). It
makes the SOURCE free and the assembly SPINE free; it does not remove per-worker
row construction. Cut the per-row constant first (§7) and every worker — and the
serial path — gets faster; the aggregate combine is then the free case.

**Semantics unchanged from §3** — adaptive scheduling removes the gate, not the
guarantees: key-ordered morsels assembled in key order (byte-identical output),
work-meter summed deterministically at merge, a raise surfaced from the earliest
morsel, parallel OFF under RLS (join-reorder precedent) and OFF with LIMIT (v1).

## 9. BUILT 2026-07-22 — the adaptive fold, measured on both hosts

`exec/parallel.rs` (the scheduler) + `mpedb_sql::parallel_fold_shape` (the single
STATIC gate, which EXPLAIN prints) + `btree::partition_keys` /
`ReadTxn::partition_range` (structural morsel cuts) + `[runtime]
max_query_threads` (`0` = auto `min(cores, 8)`, on by default; `1` = serial).

### 9.1 The shape of the decision — no gate, no estimate

The calling thread is worker 0 and folds the statement's lowest key range
immediately, through the ordinary serial code. A scan that ends inside
`PROBE_ROWS` (32 768, `MPEDB_PAR_PROBE_ROWS` overrides) engages **nothing**: no
thread, no structural cut, no reader census, no `available_parallelism` call.
Only a scan that outlives the probe hands its REMAINING key range to a morsel
queue — cut at B+tree separators, ~4 morsels per worker — spawns helpers, and
keeps pulling morsels itself.

Engagement therefore requires: the static shape gate ∧ a pinned-snapshot read
context ∧ `max_query_threads ≠ 1` ∧ the probe outlived ∧ the tree offering ≥ 1
cut inside the remainder ∧ helper budget available. **No row estimate appears
anywhere**, which is the point: the estimate is worst exactly on the UNKNOWN
selectivity class, and by the time a thread is spawned there are 32 768 rows of
evidence instead. `par_adaptive.rs` asserts both halves at the shipped setting —
a 4 000-row scan engages nothing; a 52 768-row scan engages and answers
identically; a short PK RANGE over a long table engages nothing.

The probe is measured on rows VISITED (the #74 meter's own delta), not rows
kept, so a selective residual cannot hide a long scan.

### 9.2 Scope: the order-independent fold, ungrouped

Admitted (each because its morsel-merge is proven order-identical, values, ties,
spellings and raises alike): `count(*)`/`count(x)`, `min`/`max` over a bare
non-`any` column, `sum` over a bare `int64` column, any per-aggregate `FILTER`,
any host-free residual, over `FullScan` and `PkRange`.

Refused, with the reason: **GROUP BY** (v1 scope — per-worker maps and their
shared cell budget are a separate step; the machinery generalizes), **float
`sum`, `avg`, `total`** (f64 accumulation is non-associative — partitioned low
bits would differ from the serial oracle), **`group_concat`** (order IS the
answer), **DISTINCT** (dedup sets span morsels), **host aggregates** (opaque
state), **bare-column witnesses** (the all-NULL / all-filtered corners track the
LATEST row), **`any`-typed min/max arguments** (`sort_cmp` calls Bool/Timestamp
peers of another class — incomparability breaks first-beat associativity),
**host-called per-row programs**, **joins/windows/correlated subplans**,
**index and point access paths**. Also refused: bare `count(*)` over a filterless
range, which `try_count_only` already answers leaf-wholesale at 0.4–2.6 ns/row —
there is nothing there to parallelize.

### 9.3 Integer `sum`: the exact prefix monoid, not a raise-frequency change

The task allowed a deliberate divergence (parallel completes where serial
raises, under the §7.2 join-reorder precedent, with an RLS carve-out). **It was
not needed and was not taken.** Probing the bundled oracle first: sqlite 3.45
raises "integer overflow" on `[MAX, 1, -2]` although the total fits, and
completes on the same multiset as `[1, -2, MAX]` — the raise is
order-dependent, and mpedb's serial `Accum` has the same rule (raise iff SOME
true prefix leaves i64). Carrying `(Σ, max-prefix, min-prefix)` in i128 per
morsel reproduces that predicate exactly under ordered concatenation
(`maxp = max(A.maxp, A.Σ + B.maxp)`), for the same three words a bare total
would cost. So the parallel fold raises **iff** the serial fold raises, no
divergence exists, and no RLS carve-out is needed. `par_fold.rs` carries all
four probes, including the one that refutes per-morsel i64 accumulation (a
suffix whose LOCAL sum overflows while every TRUE prefix fits must complete).

The leader's probe prefix is an ordinary serial `Accum`; it seeds the monoid
through `Accum::int_sum_prefix`, which is sound precisely because that prefix
COMPLETED — its own prefixes provably never escaped i64.

### 9.4 Execution contract

Workers share the statement's own `ReadTxn` through scoped threads: same
`txn_id`, same meta, **zero extra reader slots** (the design's §4.3 alternative,
N slots pinned to one txn, is unnecessary — sharing is both cheaper and a
structural guarantee rather than a protocol one). Each worker drains its morsel
through the SERIAL row body itself (`BatchScan` + the one `Folder`, or the fused
`fold_range_column` loop) — no second row-processing implementation. Morsels
merge by index, i.e. in key order, into the leader's prefix.

Any mid-flight error — budget trip, per-row raise, eviction, a defensive
invariant break — rewinds the meter to the pre-hand-off checkpoint and returns
"not handed off"; the caller's own scan then keeps folding the remainder
**serially, from exactly where it stood**, into accumulators it never gave up.
The statement therefore charges the serial total in the serial order and trips at
the serial row: `runtime_budget.rs`'s "same `used` every run" contract holds
under threads, and `par_fold.rs` asserts parallel-vs-serial refusal equality
directly. The one error returned from the parallel side is the integer-sum
overflow, on the strength of §9.3's proof.

Peak input residency during an engaged fold is O(workers × batch) rows rather
than O(batch) — each worker holds one `BatchScan` batch (the fused body holds
none). Still kilobytes, and still O(1) in table size, which is what
DESIGN-STREAM-EXEC §5.1 promises; `agg_stream_mem.rs`'s slopes are unmoved
because its fixtures are below the probe.

**Fan-out is budgeted** (§8's mpedb-specific point): helpers are bounded by the
knob, by free cores, by this process's in-flight helpers, and by the file's
reader census — other live pins are other requests, possibly other PROCESSES.
A helper that sees another engagement denied budget finishes its morsel and
yields. Correctness never depends on a helper running: the leader drains the
queue.

### 9.5 The measured wall: the work meter's cache line

The #74 meter is one atomic cell, and a per-row read-modify-write on it from N
cores is a hard scaling limit — not a constant factor. Measured on the M3, 1 M
rows, `count(*) … WHERE` (the general row body):

```text
  meter charging   4 threads    11 threads
  per row            40.4          61.0    ns/row   (serial: 70.4)
  batched (64)       25.2          17.1    ns/row
```

Per-row charging caps the 11-core speedup at 1.15×; batching it (worker-only —
it may fold up to 63 rows past a serial abort point, which is legal exactly
because any error abandons to a serial re-run) restores 4.6×. `RowCursor::
batch_charges` / `FoldOpts::worker` carry the rule and the reason.

A second measurement trap, recorded so it is not re-hit: `examples/agg_prof.rs`
counts allocations through a global allocator with two shared atomic counters.
Invisible on one thread, they dominate on eleven — they made the allocating
body look unparallelizable (0.64× at 8 threads) when what could not be
parallelized was the instrument. `MPEDB_PROF_ALLOC_COUNT=0` turns them off;
every number below is measured with them off.

### 9.6 Numbers

ns/row, min-of-5 (1 M) / min-of-3 (4 M), warm, `durability = "none"`, the
`agg_prof` fixture. `[base]` = the unindexed columns, i.e. the table fold;
`WHERE` = the general row body under a residual. Engagement was asserted per
cell by the harness (`[par n/n]`).

**M3 Pro, 11 cores, `max_query_threads = 11`:**

| shape | 1 M serial | 1 M par | × | 4 M serial | 4 M par | × |
|---|---|---|---|---|---|---|
| `sum(g10)` (fused) | 28.6 | 5.2 | **5.5** | 30.3 | 4.7 | **6.4** |
| `min(gk), max(gk)` (fused) | 34.1 | 6.7 | **5.1** | 35.9 | 6.1 | **5.9** |
| `count(*), sum(g10)` (fused) | 30.7 | 5.7 | **5.4** | 32.5 | 5.1 | **6.4** |
| `sum(g10) … WHERE` | 81.0 | 16.6 | **4.9** | 83.1 | 14.2 | **5.9** |
| `count(*) … WHERE` | 72.7 | 14.4 | **5.0** | 75.0 | 12.8 | **5.9** |
| `min/max(gk) … WHERE` | 87.9 | 17.8 | **4.9** | 89.5 | 15.1 | **5.9** |

§1's hand-partitioned ceiling on this machine was 5.4–6.6× at 1 M and 6.2–6.8×
at 4 M. The executor-integrated fold reaches **90–95 % of it** while adding no
reader slot, no second row body, and no answer change — the missing few percent
is the serial probe prefix (3.3 % of 1 M, 0.8 % of 4 M) plus the merge.

At 11 threads mpedb's parallel fold also passes bundled serial sqlite on these
shapes for the first time: `sum` 5.2 vs 19.3 ns/row, `min/max` 6.7 vs 29.5.

**Linux dev box, 2 cores (1 helper), `max_query_threads = 0` (auto):**

| shape | 1 M serial | 1 M par | × | 4 M serial | 4 M par | × |
|---|---|---|---|---|---|---|
| `sum(g10)` (fused) | 63.2 | 40.9 | **1.55** | 61.8 | 39.3 | **1.57** |
| `min(gk), max(gk)` (fused) | 68.4 | 53.6 | **1.28** | 67.8 | 52.1 | **1.30** |
| `count(*), sum(g10)` (fused) | 68.0 | 44.4 | **1.53** | 66.2 | 43.9 | **1.51** |
| `sum(g10) … WHERE` | 166.3 | 119.4 | **1.39** | 169.9 | 120.0 | **1.42** |
| `count(*) … WHERE` | 146.0 | 115.8 | **1.26** | 161.9 | 111.8 | **1.45** |
| `min/max(gk) … WHERE` | 172.9 | 133.3 | **1.30** | 167.9 | 132.7 | **1.27** |

Two cores buy one helper, so ~1.8× is the ceiling and 1.26–1.57× is what
scheduling, the probe prefix and the merge leave of it.

**The 10 k-row latency check — the whole point of adaptive scheduling.** At
10 000 rows (below the probe) the harness reports `[par 0/9]` on every shape,
both hosts: the parallel-enabled handle engages nothing and runs the serial code
path. Timings are equal within noise on both boxes (M3 33.1→19.8 and
84.2→73.6 ns/row; Linux 48.1→46.6 and 148.6→142.8 — the parallel handle
measured marginally FASTER, i.e. noise at 0.2–1.6 ms durations). There is no
small-query tax to trade against, which is what a compile-time gate would have
had to predict its way out of.

### 9.7 Gates

`par_fold.rs` (12 differentials against the bundled 3.45 oracle AND serial
mpedb, incl. the four overflow probes, budget-refusal determinism, NOCASE tie
spellings, snapshot identity under a concurrent writer, thread-count
invariance at {2,3,5,8}) + `par_adaptive.rs` (4, the adaptive claims at shipped
settings); `agg_stream`, `agg_collate`, `agg_over_index`, `agg_stream_mem`,
`prune_width`, `runtime_budget`, `mpee_solver` green; `cargo test --workspace
--no-fail-fast` green (166 binaries, 0 failures); clippy `-D warnings` clean.

**Corpus: byte-identical.** `select1-4` + `evidence/` reproduces §7's
9,489 / 9,689 with the same 4 flagged; the full 621-file sweep (7,419,202
records, 5,936,882 passed, 0 genuine wrong, 1,391 refused) `diff`s **byte for
byte** against the same sweep from a `bc45e69` control build on the same box,
same corpus copy, same file list. (That control was necessary: the stored
`mpedb-corpus-bd420e8-r1.log` counts 75 more records than this box's corpus
copy yields, a difference the base build reproduces exactly and this change does
not touch.)
