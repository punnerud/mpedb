# DESIGN-HTTP-RANGE — serving a live mpedb over HTTP range requests (layout study)

**Status: exploration (2026-07-18, #22). A *light* think, not a build commitment. Scope refined by
Morten: the file need not be frozen — it is served live over HTTP and may be mutating; the whole
question is **data layout**, because each HTTP call is a slow round-trip. Hard constraints: **nothing
here may cost the local (mmap) hot path, and the on-disk format does not change** — the browser is a
separate WASM build (feature-flagged) that swaps the `PageStore`; every optimization lives in its
fetch strategy (see §2).**

## 0. The question

A client (browser WASM, or a native fetcher) reads an mpedb file hosted on a plain HTTP server via
`Range: bytes=` requests, fetching only the 4 KB pages a query needs — phiresky's sql.js-httpvfs
model, but for a **live, possibly-mutating** mpedb, and asking what *layout* keeps the round-trip
count low.

## 1. Measured (this session): where the bytes go

A 20k-row table (id PK, a secondary index), 4 KB pages, page reads traced (env-gated trace, since
reverted so the hot path stays clean). Isolating a query's own pages from the one-time process open:

| access | query-only pages | ≈ range round-trips |
|---|---|---|
| **open / bootstrap** (meta + catalog + plan-registry) | **322**, scattered | the dominant *cold* cost |
| point (`id = k`) | **3** (root→internal→leaf) | 3 |
| PK range, 100 rows | 5 | 5 |
| index point | 6 | 6 |
| full scan | 773 | not range-friendly (as expected) |

**The finding:** the per-query B-tree descent is already tiny (3–6 pages); the expensive, *scattered*
part is the **one-time bootstrap** (reading the catalog + plan-registry to learn the schema and the
table's tree root). So HTTP-range viability is almost entirely a **bootstrap-layout** question, not a
per-query one.

## 2. The two-mpedb model — a WASM read build, no format change

Morten's framing sharpens it: this is essentially **two mpedbs sharing one on-disk format**. One is
the normal disk engine (writes, mmap `PageStore` — unchanged). The other is a **WASM/browser build**
(a feature flag, e.g. `http-range`) whose *only* difference is a **different `PageStore`
implementation** — an HTTP-`Range` fetcher with a page cache — under the *same* btree/row/plan/exec
code reading the *same* bytes. `PageStore` is already the seam (mmap `Shm` is one impl, `TestStore`
another; the HTTP fetcher is a third, returning borrows into its own cache). **So the disk format
never changes**, and the browser build is free to fetch *more, or differently* than the disk engine
would — its cost model is round-trips, not page faults. Every lever below lives in that build's fetch
strategy; none touches the format or the local hot path:

1. **Over-fetch a span instead of clustering the format (the big win).** The 322 bootstrap pages span
   ~1000 pages here (`[69..1106]` ≈ 4 MB). The browser fetches that **whole span in ONE range
   request** and serves the 322 it needs from cache — one round-trip instead of 322, **no format
   change**. One ~4 MB fetch beats 322 × latency by orders of magnitude. (*Optional* later: an opt-in
   repack that clusters the bootstrap shrinks the useful span from ~4 MB to ~1.3 MB — less
   over-fetch — but it is never required, and it does not touch the read path.)
2. **Adaptive, predictive fetching** — all client-side. The fetcher caches fetched pages, coalesces a
   read-ahead window around each needed page (adjacent B-tree nodes ride along), and can prefetch the
   *predictable* descent path (a point query is root→internal→leaf — knowable ahead of the fetch). It
   tunes the window from observed access. A point query is then ~3 small fetches (or a single
   windowed one) after the cached bootstrap. sql.js-httpvfs is the reference.
3. **Mutation fragmentation.** COW scatters a table's leaves as it churns, so a range scan that reads
   contiguous leaves on a fresh file reads scattered ones on a churned file → more round-trips. Fix
   with an **opt-in repack/compaction** that restores leaf locality (like `VACUUM` → contiguous). It
   is a maintenance op, off the hot path — respects the constraint.
4. **Freshness for a live file.** mpedb's meta is a double-buffered A/B pair at pages 0/1; a client
   re-fetches the meta page to observe the newest committed version, and MVCC gives it a consistent
   snapshot for the rest of the query. HTTP `ETag`/`If-Range` validate the cached bootstrap; a
   changed meta invalidates just the pages a bumped `schema_gen`/root touches. So "mutable over HTTP"
   is: re-read the tiny meta, keep the rest of the cache, re-descend only what moved.

## 3. MPEE turns round-trip cost into automatic optimization

Morten's key move: **feed the round-trip cost into MPEE's cost model, and the plan optimizes itself.**
MPEE is already the cost-based solver (the N×N cost matrix, exact catalog `row_count`s — #12/#73). Its
cost function is parameterized by the storage's **per-fetch cost**: ≈0 for local mmap (optimize CPU +
cardinality, as today), a high latency `L` **per non-contiguous range request** for HTTP. The WASM
build just injects its `L`; the *same* solver then produces a round-trip-minimizing plan. Concretely,
over HTTP MPEE would automatically:

- **Cost an access path by coalesced round-trips, not raw pages** — so a 100-leaf *contiguous* range
  scan (1 request) can beat a 6-*scattered*-page index lookup (up to 6 requests). MPEE flips the
  access-path choice for the HTTP cost model without any manual tuning — the plan is content-hashed
  per cost model, so the local and HTTP builds get different, each-optimal plans from one solver.
- **Value memoization far more** — the "buy once, collapse by correlation key" N×N move (a correlated
  subquery re-fetching the same pages) is a *round-trip* saved per collapse, so MPEE leans harder into
  fetch-once-reuse-many when `L` is high.
- **Size the prefetch / over-fetch window** from the round-trip-vs-bandwidth balance in the cost model
  (fetch a bigger span when a round-trip costs more than the extra bytes).
- **Route heavy work server-side** — when a full scan's client round-trip cost exceeds shipping the
  query and returning the result, the cost model says so, and MPEE runs it where the file is local.

This is the same "cost includes where the bytes live" principle as the distributed shard/sync cost
(DESIGN-DISTRIBUTED §8) and the #74 runtime-budget estimate — one cost model, parameterized by the
storage. See [design/DESIGN-MPEE-OPT.md](DESIGN-MPEE-OPT.md).

## 4. Honest limits

- **Full scans / large aggregates are not range-friendly** (773 pages here, and unbounded on a big
  table). Those belong server-side — run them where the file is local and return the *result*, not
  the pages. HTTP-range is for point/range/index access, which is exactly the cheap case above.
- **Writes over HTTP are a different problem** — a write needs the writer lock + the commit path,
  which is a protocol, not a range request. This study is about *read-serving* a file that some
  other process may be writing; the reader just re-reads the meta to move to the new snapshot.
- Prior art to lean on: **sql.js-httpvfs** (phiresky — SQLite over HTTP range), DuckDB `httpfs`,
  S3-range / Parquet footer-then-range reads.

## 5. If we ever build it (not now)

FrozenDb/pack (#22's original framing) becomes just the *repack* of lever 1/3 producing an
HTTP-optimized copy; the native + WASM `Range` fetcher is lever 2. Sequence: (a) opt-in repack that
clusters the bootstrap prefix + defragments leaves; (b) a `PageStore` HTTP fetcher with cache +
coalescing; (c) WASM. **None of it changes the local engine** — that is the whole point, and the
measurement says it is viable: after a one-request bootstrap, a point query is ~3 tiny fetches.

*(M3 network-latency numbers deferred — the M3 SSH key from a prior session is gone, so real
round-trip timing wasn't measured here; the page-count/round-trip analysis above is
platform-independent.)*
