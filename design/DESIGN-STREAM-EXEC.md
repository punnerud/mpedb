# DESIGN-STREAM-EXEC (#123) — the budget sizes the chunk instead of aborting

MPEE's fifth memory technique is the one that never transferred: it does not
materialise the distance matrix, it **streams it through a byte budget and lets
the budget size the chunk** (chunk 97 → 296 → 694 → 1489 as the budget grows).
mpedb has the same number — `max_join_cells`, `RuntimeBudget { kind: JoinCells }`
— and uses it only to **refuse**. It knows exactly how much it is holding and
does nothing with that but give up.

This document is steps 1–3 of #123: what the peak actually is (measured, §1–2),
whether the plan-time safety condition is answerable (§3), and the design for
the shapes the measurement puts on top (§4–7). **Step 4 — building it — is not
started**; it touches `exec/` and waits on the prerequisites in §7.

---

## 1. Method

`crates/mpedb/examples/mem_shapes.rs`. Two numbers per run:

- **`held`** — peak of a live-bytes counter in a wrapping `GlobalAlloc`: every
  `alloc` adds, every `dealloc` subtracts, peak by `fetch_max`. This is "how
  many heap bytes did the engine hold *simultaneously*", which is exactly the
  quantity a streaming design moves. Deterministic: no allocator quantisation,
  no sampling race, no mmap'd file pages.
- **`rss`** — peak `RssAnon` from `/proc/self/status`, sampled by a thread that
  never allocates (held fd + `pread` into a stack buffer + a hand-rolled digit
  scan). `RssAnon` and not `VmHWM`: on a database that mmaps its file `VmHWM`
  charges the whole touched mapping and measures the *file* (`README.md` §
  writer-process comparison makes the same distinction).

Both counters are **reset after the fixture is loaded and immediately before the
measured statement**, so the figure is the statement's marginal hold.

`rss / held` came out **0.95–1.11 on every shape above 10 MB**. The two
independent instruments agree to within noise, which is the strongest evidence
available that neither is measuring itself.

Fixture: `src(id int64 PK, g int64, g10 int64, a int64, b int64, t text)` — five
`int64` and one 21-character text, ~61 bytes of user payload per row. Row counts
10 000 / 40 000 / 160 000; `B/row` below is the **slope** between 40 k and 160 k,
so every fixed cost cancels. Box: `ulimit -v 3000000`, scratch on `/mnt/xfs`,
`/dev/shm/mpedb-*` cleared first.

### 1.1 Two harness corrections that changed the answer

Both are recorded because both produced a confident, wrong, plausible number
first — the `INNOVATIONS.md` §9.2 failure mode.

1. **Measure `execute(hash, params)`, not `query(sql, params)`.** `query`
   recompiles and re-registers the plan on every call. Through `query`, a bare
   `SELECT … WHERE id = 1` PK point lookup measured **149 bytes per row of the
   whole table**. Through `execute` it is a flat **618 bytes**, independent of
   table size. Left in, that would have put a false linear floor under every
   shape in this file.

2. **The bulk loader must not publish plans.** The first loader used
   `Database::query` per 500-row chunk. Every chunk is a distinct statement text,
   so every chunk published a distinct plan — 200 plans of ~74 KB for a 100 k-row
   load, because a 500-row literal `VALUES` plan carries 3000 constants. That is
   what produced the "149 bytes per table row" above, and it is **not**
   proportional to table rows at all (see §2.3). The loader now runs inside a
   `WriteSession`, which compiles into the local cache and never publishes.

---

## 2. What the peak actually is

### 2.1 The ranked table

At 160 000 rows, `held` in bytes; `B/row` is the 40 k→160 k slope.
`amp` = `B/row ÷ 61`, the amplification over the user payload.

| # | shape | held @160 k | B/row | amp | out rows | what is held |
|---|---|---:|---:|---:|---:|---|
| 1 | `agg_many` GROUP BY, n groups | 167 875 378 | **1049.4** | 17.2× | 160 000 | input set **+** group map **+** 3 more full spines |
| 2 | `window` row_number() OVER | 78 340 330 | **489.8** | 8.0× | 160 000 | base rows + projected rows + 5 per-row side vectors |
| 3 | `join_rows` | 65 482 467 | **409.6** | 6.7× | 160 000 | held inner + accumulated product |
| 4 | `insert_select` | 57 111 901 | **357.1** | 5.9× | 160 000 | source set + built target set |
| 4= | `update` | 57 111 901 | **357.1** | 5.9× | 160 000 | full old-row set |
| 4= | `delete` | 57 112 029 | **357.1** | 5.9× | 160 000 | full old-row set |
| 7 | `select_sorted` ORDER BY, no LIMIT | 54 660 353 | **341.8** | 5.6× | 160 000 | full set, sorted in place |
| 8 | `select` | 50 820 659 | **317.8** | 5.2× | 160 000 | `ExecResult::Rows` |
| 8= | `count` `SELECT count(*)` | 50 822 241 | **317.8** | 5.2× | **1** | the whole input, to produce one integer |
| 8= | `agg_few` GROUP BY, 10 groups | 50 826 842 | **317.8** | 5.2× | **10** | the whole input, to produce ten rows |
| 11 | `join_held` (inner held, 0 out) | 30 182 303 | **188.8** | — | 0 | the inner relation alone |
| 12 | `rcte` recursive CTE | 29 114 511 | **182.0** | — | 160 000 | fixpoint result (1 column) |
| — | `blob` 64 MiB `INSERT … VALUES ($1)` | 67 110 007 | 1.00 × payload | 1.0× | 1 | one encoded row image |
| — | `blob_streamed` `insert_streaming` | **4 537** | **0** | 0 | 1 | nothing |
| — | `stream` `stream_query`, same SQL as `select` | **61 801** | **0.002** | 0 | 160 000 | nothing |
| — | `select_limit` `LIMIT 10` | **3 437** | **0** | 0 | 10 | nothing |
| — | `pkpoint` `WHERE id = 1` | **618** | **0** | 0 | 1 | nothing |

The four O(1) rows at the bottom are flat to the byte across a 16× change in
table size — `pkpoint` is 618 at 10 k, 40 k and 160 k. The harness can see O(1),
so the linearity above it is real and not an artefact.

Unit calibration: `select` holds 317.8 B for a 6-cell row = **53 bytes per
`Value` cell**. `design/DESIGN-RUNTIME-BUDGET.md` §2b calibrates
`max_join_cells` at "~40 B resident per cell"; 53 is the same constant measured
on a shape with a text column. The default `max_join_cells = 268 435 456` is
therefore **~14 GB**, not 11 — which is why `budget_fits_in_memory` has to exist.

### 2.2 Where the assumption in the brief was wrong

- **`INSERT … SELECT` does not cost 2×. It costs 12%.** The double
  materialisation is real in the code — `exec/mod.rs:1644-1679` holds `src` and
  `built` at once — but the measured slope is 357.1 vs `select`'s 317.8, a
  **39 B/row** delta, not 318. The reason is that `for srow in src` consumes the
  source by value, so each source row's payload is freed as the target row is
  cloned out of it; only the two *spines* (`src`'s `Vec` and `built`'s
  `Vec::with_capacity(src.len())`) are simultaneously full-length. The statement
  still holds **one** full row set, which is what streaming removes — but the
  premise "materialises twice" overstates the prize by ~8×. `update` and
  `delete` land on the identical number for the same structural reason.

- **The blob control is not clean, and the reason matters.** A 64 MiB
  `INSERT … VALUES ($1)` holds 67 110 007 bytes — exactly 1.00× the payload,
  with `churn/held = 1.00`, i.e. precisely one extra allocation. But
  `insert_streaming` over a `ReaderBlobSource` holds **4 537 bytes flat** at
  4 / 16 / 64 MiB, and is **2.2× faster** (38.9 ms vs 85.6 ms at 64 MiB). So the
  resident copy is the encoded row image built from the caller's own
  `Value::Blob`, the parameter path is `Cow::Borrowed` and clones nothing
  (#40 holds), and mpedb already ships the O(1) alternative. The blob path is
  where the brief said it would be: not the problem.

- **The biggest ratio in the table is not the biggest number.**
  `SELECT count(*) FROM src` holds **50.8 MB to produce one integer**, and
  `GROUP BY g10` holds 50.8 MB to produce ten rows. `exec/aggregate.rs:172-177`
  gathers the base rows in full and says so ("Unbounded on purpose"). Ranked by
  bytes held, aggregation is joint 8th; ranked by *bytes held per byte of
  answer*, it is first by several orders of magnitude, and it is the shape
  streaming fixes most completely because an aggregate is a fold.

### 2.3 An incidental finding, not part of #123

**Plan compilation is O(bytes ever registered in the sys keyspace.)**
`compile_maybe_explain` runs two full `sys_scan`s (`load_policy_catalog`,
`load_view_catalog`, both `crates/mpedb/src/…` — `policy_store.rs:192`,
`ddl_apply.rs:22`), each materialising the entire sys keyspace into
`Vec<(Vec<u8>, Vec<u8>)>` — and the shared plan registry lives in that same
keyspace. Measured with the `registry` shape (`MEM_VIA=compile`): **298 bytes
held per previously-registered plan**, dead linear over 100 / 400 / 1600 plans,
with the compile itself flat at 3 123 bytes when the registry is empty. Every
`Database::query` and every `prepare` pays it, for the life of the file. It is
not proportional to table rows and it is not an execution cost, so it is out of
scope here — recorded so the next person does not rediscover it as a query bug.

### 2.4 mpedb already streams — twice

`stream_query` is flat at 61.8 KB over a 160 000-row scan, and
`insert_streaming` is flat at 4 537 bytes over a 64 MiB blob. The engine has the
pattern, the discipline and the tests for it. What it does not have is streaming
*through the row pipeline*: `stream.rs:152-157` refuses to stream anything with
a subplan, an `ORDER BY`, a join, `DISTINCT` or an aggregate, and falls back to
running the whole materialising executor and draining the result out of a
`VecDeque`. This design is about widening that gate, not about inventing it.

---

## 3. Safety: is the streaming condition answerable at plan time?

Streaming a DML statement is unsafe when the statement reads its own writes (the
Halloween problem), and MVCC does not help: a `WriteTxn`'s reads see its own
uncommitted writes. Today `exec/mod.rs:1644-1648` buys safety by materialising
— `INSERT INTO t SELECT … FROM t` reads the pre-insert state because the source
is fully drained before the first write.

The condition is `tables_read ∩ tables_written ≠ ∅`. **The footprint answers it
— and the footprint alone is not sufficient.** Both halves were checked against
`crates/mpedb-sql/src/planner/footprint.rs` and measured through the public
`Database::plan_footprint`:

```
read=[]     written=[1]  intersect=false   INSERT INTO dst (id,a) VALUES (1,2)
read=[2]    written=[1]  intersect=false   INSERT INTO dst SELECT … FROM src
read=[1]    written=[1]  intersect=true    INSERT INTO dst SELECT … FROM dst
read=[2]    written=[2]  intersect=true    INSERT INTO src SELECT … FROM src
read=[0,2]  written=[2]  intersect=true    INSERT INTO src SELECT … FROM aux
                                             WHERE a IN (SELECT a FROM src)
read=[2]    written=[2]  intersect=true    UPDATE src SET a = a + 1
read=[2]    written=[2]  intersect=true    DELETE FROM src WHERE a > 0
```

**Precise enough, and not degenerate.** A plain `INSERT … VALUES` has an *empty*
read set (`footprint.rs:273-295`; asserted by `planner/tests.rs:399-407`), so the
test is not trivially true for every write plan. `INSERT … SELECT` unions the
source table, every join table of the source, and — recursively — every lifted
subplan, compound arm and derived body (`footprint.rs:99-151, 213-244`). Views
are inlined before planning, so a view's base tables appear as ordinary FROM
entries. `KeyAccess` degrading to `Full` on a join is irrelevant: the test is at
table granularity, and `TableSet::intersects` is a sorted merge with no
allocation. The check is one existing call:
`fp.tables_written.intersects(&fp.tables_read)`.

**Three ways it is not sufficient.** Each is a correctness hazard, not a
performance one:

1. **Triggers are invisible to the footprint.** `compute_footprint` takes no
   trigger argument and has no access to the trigger catalog; the firing set is
   resolved at execute time from a gen-gated cache. A trigger body may write any
   table, and none of them enter the statement's sets. Verified: with an
   `AFTER INSERT ON dst` trigger that inserts into `aux`, the footprint of
   `INSERT INTO dst … SELECT … FROM src` is still `read=[src] written=[dst]` —
   `aux` appears nowhere. The precedent is already in the tree: the optimistic
   blind-apply path does not trust the footprint either, it asks
   `db.table_has_trigger(table)` and that helper fails **closed**
   (`trigger.rs:326-337`, `.unwrap_or(true)`; used at `ring_exec.rs:478-483`).
   Streaming must gate the same way.

2. **`ON CONFLICT DO UPDATE` and `INSERT OR REPLACE` under-claim.** The INSERT
   arm ignores `on_conflict` entirely, yet `DoUpdate` probes the target and
   evaluates `set` over the *existing* row and `Replace` proactively deletes
   conflicting rows. Both are genuine reads of the target with `tables_read`
   empty. Restrict streaming to `PlanOnConflict::Error | DoNothing`.

3. **`UPDATE` and `DELETE` are unconditionally `true`.** `footprint.rs:320-342`
   builds `tables_read: one.clone(), tables_written: one`. The test can never
   distinguish a streamable UPDATE from a non-streamable one; for those two
   statement kinds it carries no information at all. They need a different
   argument (§5.2), not this one.

Foreign keys are safe today only because they are parsed and discarded
(`parser/ddl.rs:353-356`) — if enforcement is ever added the footprint will
silently under-claim, and this design's gate is one of the places that would
break. RLS, CHECK and generated columns are all validated to reference only the
one table, so none of them widen the read set.

**The gate, then, is:**

```rust
fp.read_only == false
  && !fp.tables_written.intersects(&fp.tables_read)
  && !db.table_has_trigger(target)                     // fails closed
  && matches!(on_conflict, PlanOnConflict::Error | PlanOnConflict::DoNothing)
```

Nothing here needs new plan bytes: the footprint is already encoded, already
recomputed-and-compared on decode (`plan/validate.rs:39`), and `table_has_trigger`
already exists. There is **no test anywhere** asserting the `tables_read` of an
`INSERT … SELECT` — the nearest is `django_parse_gaps.rs:327`, which compares
rows against sqlite and never looks at the footprint. Step 4 must add that
assertion first; the gate's safety rests entirely on it.

---

## 4. The design: the budget sizes the chunk

**One sentence.** `max_join_cells` stops being only a tripwire and becomes the
divisor that sets a batch size: instead of "hold `n` rows or fail", the executor
holds `C` rows at a time where `C` is derived from the budget, and a statement
that would have been refused now succeeds in bounded memory.

### 4.1 What the chunk is

A **row batch**: `C` tuples of the pipeline stage's tuple, in the stage's own
width. Not pages, not bytes, not a fraction of the result — the same unit
`stream.rs` already batches in (`BATCH = 256`) and the same unit
`JoinCells::live` already counts in (cells = `Value`s).

The chunk is a chunk *of the pipeline*, so it flows: a source batch of `C` rows
is drained, transformed, consumed by the sink, and released before the next
batch is drawn. Peak hold is `C × W × 53 B` plus whatever the stage's own state
requires, instead of `n × W × 53 B`.

### 4.2 How the size is derived from the budget

```
C = clamp(C_MIN, budget_cells / (W · S), C_MAX)
```

- `budget_cells` — `TxnCtx::join_cells_budget()`, which already exists, is
  already engine-seeded from `[runtime] max_join_cells`, and already reaches the
  executor. No new config knob, no new plumbing.
- `W` — cells per tuple at this stage. Known from the plan's projection width;
  no estimate, no statistic.
- `S` — stages simultaneously live. `S = 2` for `INSERT … SELECT` (the drained
  source batch and the built target batch); `S = 1` for a streaming aggregate.
- `C_MIN = 1` — one row must always fit, or the statement cannot make progress.
  A budget so small that `C` would be 0 is clamped to 1 and the statement still
  completes, just slowly; it does **not** become a new refusal.
- `C_MAX = 65 536` — past this the per-batch tree re-descent is already noise
  and the only thing a larger batch buys is a larger peak.
- `budget_cells == 0` (the unlimited sentinel) → `C = 256`, matching
  `stream.rs::BATCH`. "Unlimited" must mean "do not refuse", not "materialise
  everything"; today it means the latter, which is backwards.

This is the MPEE shape exactly: MPEE's chunk moves 97 → 296 → 694 → 1489 as the
budget grows, i.e. chunk ∝ budget with a fixed per-unit cost in the divisor.
Here the per-unit cost is `W · 53 B` and it is *exact* rather than calibrated,
because mpedb knows its tuple width where MPEE has to estimate a matrix cell.

**`C` must not be observable.** Same rows, same order, same errors, same
`charge_work` count, for every `C`. `C` is a config-derived number and a
statement's result may not depend on config. The one thing allowed to differ is
whether the memory abort fires. This is a test obligation, not a comment:
`tests/stream_correctness.rs` is the existing precedent — it exists because the
streaming path once silently returned outer rows for a join, an adversarial
review find — and the `C`-invariance suite belongs beside it, run at
`C ∈ {1, 2, 7, 256, unlimited}`.

### 4.3 What the abort semantics become

Today: `live > budget` → `Error::RuntimeBudget { kind: JoinCells }`. The budget
is a tripwire and its only outcome is a refusal.

After: the budget is consulted **twice, for two different purposes**.

1. **As a divisor**, at the start of a pipeline stage that can stream: it sets
   `C` and the stage then never trips, because the stage's hold is `C · W` by
   construction. A statement that today refuses on a 268 M-cell budget completes.
2. **As a tripwire**, unchanged, for every piece of state that is *not* the
   streaming buffer — the group map, the dedup set, the sort buffer, the held
   inner side, the recursive-CTE `seen` set. These are O(answer) or O(distinct),
   not O(input), and no chunking makes them smaller. They keep the existing
   error, the existing `BudgetKind`, and the existing knob name.

So the error does not disappear; it changes meaning from "this statement read
too much" to "this statement's *irreducible* state is too large", which is
strictly more informative. The `which` attribution string should say which,
since a user who raises `max_join_cells` after a group-map trip is doing the
right thing and one who raises it after an input trip should not have needed to.

### 4.4 Which shapes stream, which still refuse

From the §2.1 measurement, honestly split. "Recovers" is the fraction of the
160 k `held` figure that a chunked pipeline removes.

| shape | recovers | why |
|---|---:|---|
| `count`, `agg_few` — aggregate, few groups | **~100%** | an aggregate is a fold; the input needs no residency at all |
| `insert_select` | **~100%** | subject to the §3 gate |
| `select` (materialising API) | **~100%** | already proven — `stream` is 61.8 KB; needs the API of §7.2 |
| `join_rows` | **~54%** | output side chunks; the held inner (30.2 MB) does not |
| `agg_many` — n groups | **~30%** | input goes (50.8 MB); the group map (117 MB) is O(groups) and stays |
| `update`, `delete` | **conditional** | see §5.2 — the footprint test is uninformative here |
| `window` | **0–100%** | bounded by the largest *partition*, not by the result; one unbounded partition recovers nothing |
| `select_sorted` | **0%** | a full sort is a materialisation by definition. `ORDER BY … LIMIT` is already O(k) via `scan_rows_topk` |
| `rcte` with `UNION` | **0%** | the `seen` dedup set is O(result) |
| `rcte` with `UNION ALL` | ~100% | no dedup set; needs a 1:1 outer, which `outer_iteration_cap` already detects |
| `DISTINCT`, `EXCEPT`, `INTERSECT` | **0%** | the key set is O(distinct output) |

**Plainly: five of eleven shapes must still refuse**, and no chunk size changes
that. The three that recover fully are worth the work; the doc should not
pretend the rest come along.

---

## 5. Scope: build two, not eleven

### 5.1 Streaming aggregate (build first)

`SELECT count(*) FROM t` holding 50.8 MB is the worst held-to-answer ratio in
the table, and the fix is the smallest: `exec/aggregate.rs:172-177` gathers the
base rows in full only so it can fold over them. Replace the gather with a
batched drain — pull `C` rows, fold them into the accumulators, drop them.

Bounded by construction for the no-`GROUP BY` case and for the small-cardinality
case. The group map stays and stays budgeted (§4.3 tripwire), as do
`DISTINCT`-aggregate `BTreeSet`s (`aggregate.rs:20, 81-84, 98`), which are
O(distinct arg values) *per aggregate per group* and are the one place where an
aggregate can still be O(input). `HAVING`'s second full copy
(`aggregate.rs:476-482`) and the third and fourth spines at `:494-495` and `:527`
are O(groups) and are a separate, cheaper cleanup.

This shape has no Halloween exposure at all — it is read-only — so it needs none
of §3 and can land before the gate exists.

### 5.2 Chunked `INSERT … SELECT` (build second)

Drain the source `C` rows at a time; build and apply `C` target rows; release.
Peak becomes `O(C · W)` plus the write transaction's own COW page set, which is
a different budget and is not addressed here.

Gated by §3. When the gate fails the statement takes today's full-materialise
path, unchanged — so the fallback is the current behaviour, not an error, and
`INSERT INTO t SELECT … FROM t` keeps its sqlite semantics exactly.

`UPDATE` and `DELETE` look identical in the measurement (357.1 B/row, the same
`old_rows` structure) and are tempting to fold in. **They should not be, in this
stage.** Their footprint test is unconditionally `true` (§3.3), so the gate that
makes `INSERT … SELECT` safe says nothing about them, and they genuinely read
what they write: a chunked `UPDATE t SET a = a+1` whose scan is over the same
tree it is rewriting can revisit a row it already updated. That needs a
snapshot-scan argument — the scan must be pinned to the pre-statement state
while the writes land in the txn — which is an engine change, not an executor
one. Named, not designed.

---

## 6. What it costs

- **Per-batch re-descent.** Each batch resumes the B+tree scan from the last
  row's encoded PK. `stream.rs` already pays this at `BATCH = 256` and documents
  it as amortising "to noise"; the `stream` shape measures 90.9 ms against
  `select`'s 125.8 ms at 160 k rows, i.e. streaming is currently **faster**, not
  slower. That is one shape and it should not be generalised, but there is no
  evidence of a throughput penalty to pay for.
- **A second code path per shape.** `stream.rs` exists because of the first one,
  and `tests/stream_correctness.rs` exists because that path silently returned
  wrong answers for join/DISTINCT/aggregate plans. Every shape added here adds
  the same risk and owes the same differential test against the bundled sqlite
  oracle. This is the real cost and it is not small.
- **`C`-invariance is a new global invariant** that every future executor change
  must preserve (§4.2).
- **The error message changes meaning** for the shapes that still trip (§4.3).
  No format change, no plan bytes, no migration — the repo's standing
  no-backward-compat rule makes the knob's widened meaning free.

---

## 7. Prerequisites (named, not designed around)

1. **A pull cursor for index access paths.** `stream.rs`'s resume-by-encoded-PK
   works for `PkRange` and `FullScan` only. `gather.rs:699-703` says a streaming
   index cursor is deliberately deferred to **#48**. Until #48, chunking an
   `IndexPoint`/`IndexRange` plan is not possible and those shapes take the
   materialising fallback.
2. **A row sink at the API boundary.** `ExecResult::Rows` is a
   `Vec<Vec<Value>>`; a statement that streams internally and then hands back a
   fully-built `Vec` has saved nothing at the boundary. Either `ExecResult` gains
   a streaming variant or the streaming shapes route through a `RowSink` the
   executor writes into. This is an API break and, under the repo's
   no-backward-compat rule, cheap.
3. **A read cursor on a `WriteTxn`.** This is the largest prerequisite and it
   blocks §5.2. The default `TxnCtx::scan_rows_capped` / `scan_rows_topk`
   (`exec/mod.rs:167-192`, `:200-226`) — which is what *every* write context uses
   — fully materialises via `scan_rows_raw` and only then filters and truncates.
   So the same `LIMIT 10` is O(k) on a read txn and O(table) inside a write
   session, and there is no cursor to batch. It lives in
   `crates/mpedb-core/src/engine/write.rs`.
4. **A footprint assertion for `INSERT … SELECT`** (§3). None exists. The gate's
   safety rests on it.
5. **Nothing in the executor spills to disk**, and this design does not add
   spilling. The five shapes in §4.4 that recover 0% recover 0% *because* there
   is no spill; an external merge sort would move `select_sorted` and `DISTINCT`,
   and is a strictly larger project.

---

## 8. Honest estimate of what this buys

**Not MPEE's 20×, and the comparison should not be made.** MPEE streams a dense
numeric matrix whose every cell is a `f32` it can recompute on demand; mpedb
streams heterogeneous rows it must read off a B+tree. The two techniques share a
formula (`chunk = budget / per-unit-cost`) and nothing else.

What the measurement supports:

- On the shapes that stream, the reduction is **O(n) → O(C)**, i.e. unbounded in
  the ratio rather than 20×. `stream_query` already demonstrates the endpoint:
  61.8 KB where the materialising path holds 50.8 MB, an **822× reduction at
  160 k rows and growing linearly with n**. That is the honest headline, and it
  is a fact about code already in the tree, not a projection.
- But it applies to **three of eleven** measured shapes fully, two partially, and
  **five not at all**. A workload that is mostly `ORDER BY`, `DISTINCT` or
  `GROUP BY` with high cardinality gets little or nothing.
- The single most defensible win is the smallest piece of work:
  `SELECT count(*)` and low-cardinality `GROUP BY` currently hold the entire
  input to produce a handful of rows, and that is pure waste with no semantic
  content — no Halloween exposure, no API change, no #48 dependency.
- The `INSERT … SELECT` prize is real but **~8× smaller than the brief assumed**
  (§2.2): 357 B/row held, not ~636.

Stated as one number: for a 160 000-row table on this fixture, the shapes this
design would fix hold **50.8 MB (aggregate) and 57.1 MB (INSERT … SELECT)**
today, and would hold **tens of kilobytes**. The shapes it would not fix hold
**168 MB (`agg_many`), 78 MB (`window`) and 55 MB (`select_sorted`)** today and
would still hold most of that.

---

## 9. Step 4, built: the streaming aggregate (measured)

§5.1 shipped. `exec/aggregate.rs` no longer gathers its input for the shapes
that are a fold over a PK-ordered scan: `gather::BatchScan` drains the scan in
batches and `Folder::push` folds each batch into the accumulators before the
next is drawn. One row-processing body serves both the streaming and the
materialising input, so the §6 cost ("a second code path per shape") was not
paid — there is no second implementation of grouping, of `FILTER`, or of the
bare-column witness to drift.

### 9.1 What it moved

Same harness as §1 (`examples/mem_shapes.rs`, `MEM_VIA=execute`, 160 000 rows,
`ulimit -v 3000000`, scratch on `/mnt/xfs`). `B/row` is the 40 k→160 k slope.

| shape | held before | held after | ratio | B/row before | B/row after | ms before | ms after |
|---|---:|---:|---:|---:|---:|---:|---:|
| `count` `SELECT count(*)` | 50 822 265 | **79 526** | **639×** | 317.8 | **0.002** | 87.7 | **59.8** |
| `agg_few` GROUP BY, 10 groups | 50 826 866 | **84 110** | **604×** | 317.8 | **0.003** | 145.0 | **84.7** |
| `agg_many` GROUP BY, n groups | 167 875 402 | 117 132 646 | 1.43× | 1049.2 | 732.1 | 352.2 | **307.2** |

The 79.5 KB that remains is *flat*: 79 012 at 10 k, 79 245 at 40 k, 79 526 at
160 k. It is the read's fixed cost, not a per-row residue — the same order as
`stream_query`'s 61.8 KB, which is the endpoint §8 named.

Four more shapes, measured at 1 000 → 16 000 rows in `tests/agg_stream_mem.rs`
(before = the same test against HEAD~):

| shape | B/row before | B/row after | |
|---|---:|---:|---|
| `count(DISTINCT a)`, n distinct | 362.9 | **60.9** | 6.0× — the rows go, the dedup set stays |
| `GROUP BY g10 ORDER BY count(*)` | 302.0 | **0.03** | ORDER BY over the GROUPED tuple is O(groups) |
| `count(*)` over a join | 753.4 | 753.4 | unchanged, by design (§9.3) |
| `count(*) FILTER (correlated EXISTS)` | 566.6 | 566.6 | unchanged, by design (§9.3) |

**§4.4's predictions held.** It said `count`/`agg_few` recover ~100% and
`agg_many` ~30%; the measured recovery for `agg_many` is **30.2%**
(1049.2 → 732.1 B/row), which is the input's own residency and nothing else.

**There is no throughput penalty; there is a throughput gain.** §6 predicted
the per-batch re-descent would amortise "to noise" and offered `stream_query`
being faster as the only evidence. It is faster here too, on all three shapes,
by 13–42%. The likely reason is the one §6 did not name: the materialising path
allocated and held ~50 MB, and not doing that is worth more than a tree
re-descent per 256 rows costs.

### 9.2 Three things the design got wrong

1. **§4.2's chunk formula is wrong for a fold.** `C = clamp(1, budget/(W·S),
   65 536)` is right for a stage whose throughput improves with a bigger chunk.
   A fold is not one: its hold is O(groups) however the input arrives, so every
   cell above the re-descent amortisation point buys nothing but peak. With the
   default `max_join_cells = 268 435 456` that formula clamps to `C_MAX`,
   i.e. 65 536 rows ≈ **20 MB** held for `SELECT count(*)` — 250× worse than the
   79 KB actually delivered. The implementation keeps the budget as a *divisor*
   (a small budget still shrinks the batch) but caps at `FOLD_BATCH = 256`, the
   same constant `stream.rs` uses.

2. **§4.2's `C`-invariance suite is untestable through `max_join_cells`.**
   The same knob is both the divisor and the §4.3 group-map tripwire, so every
   budget low enough to force `C = 1` *refuses* the grouped statements instead
   of chunking them. The two roles are only jointly satisfiable in a narrow
   band. `MPEDB_FOLD_BATCH=<n>` (read once per process, `MPEDB_NO_SUBPLAN_MEMO`
   is the precedent) forces the batch size independently, and
   `tests/agg_stream.rs::c_invariance` re-runs the whole differential battery
   under `C ∈ {1, 2, 7, 256}` — each against the bundled oracle, which is a
   stronger claim than "the four agree with each other".

   Fault-injected to prove the battery is not vacuous: changing the
   "short batch means the cursor is exhausted" test from `<` to `<=` (so the
   scan stops after one batch) fails on the first query, `count(*)` returning 7
   against the oracle's 24. Making the resume bound INCLUSIVE instead hangs —
   the batch re-reads its own last row forever — which is detection, but of the
   worse kind; it is the one fault mode in this code that is not a wrong answer.

3. **§7 lists five prerequisites without saying which shape needs which. The
   aggregate needs none of them.** Not §7.2's row sink (an aggregate's output is
   already O(groups)), not §7.4's footprint assertion (read-only, no Halloween
   exposure), not §7.1's index cursor (those access paths simply keep the
   materialising path). Reading §7 as a gate on §5.1 would have blocked the
   cheapest item in the document behind the most expensive one.

### 9.3 What still materialises, and why that is not a hedge

`BatchScan::open` answers `None` — and the aggregate takes its previous path,
byte for byte — for four cases. Each was measured (§9.1) rather than assumed:

- **an aggregate over a JOIN.** The tuple being folded is the join's
  accumulated product, which `gather_joined` holds anyway and which its own
  `max_join_cells` tripwire already governs. 753.4 B/row before and after.
- **a correlated subplan or a correlated WHERE residual** (#73 §1). Those run
  `correlated_survivors` over the gathered set and keep a per-row scratch
  beside each row; folding them would need the scratch stream, not just the row
  stream. 566.6 B/row before and after.
- **a non-PK-ordered access path** — `IndexPoint`/`IndexRange`/`FtsScan` have no
  resume key until #48 (§7.1); `PkPoint` is one row.
- **every WRITE context.** `TxnCtx::scans_incrementally` is `false` unless the
  context's `scan_rows_capped` is a real cursor that stops at the cap, and only
  `ReadCtx`'s is. Batching a materialise-then-truncate would be O(n) *per
  batch*. So `SELECT count(*)` inside a `WriteSession` is unchanged. §7.3 named
  the missing `WriteTxn` read cursor as blocking §5.2; it caps §5.1 too, and
  that is the single largest remaining piece of this shape.

### 9.4 The budget's second role, wired

§4.3 said the budget stops being only a tripwire on the input and becomes a
tripwire on the *irreducible* state. That is now true for the aggregate: the
group map is charged `1 key + 1 accumulator + 1 bare column` per cell per group
against `max_join_cells`, once per group created, with the attribution string
`the group map of an aggregate over "<table>"`. Before this change an unbounded
`GROUP BY` was governed by nothing at all. The distinction §4.3 asked for is
visible in the message: a user who raises the knob after a group-map trip is
doing the right thing, and `tests/agg_stream.rs::group_map_is_governed_by_the_budget`
asserts that the *same* budget that refuses 24 groups runs a scalar aggregate
over the same 24 input rows — which is the whole change of meaning in one
assertion.

The per-aggregate `DISTINCT` `BTreeSet`s are deliberately NOT charged: they are
O(distinct arg values) per aggregate per group, charging them costs a branch per
*value* rather than per group, and the measurement (§9.1: 60.9 B/row, down from
362.9) shows the larger half of that shape's hold was the rows, which are gone.
