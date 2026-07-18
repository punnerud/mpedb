# DESIGN-HTTP-RANGE — serving a live mpedb over HTTP range requests (layout study)

**Status: exploration (2026-07-18, #22). A *light* think, not a build commitment. Scope refined by
Morten: the file need not be frozen — it is served live over HTTP and may be mutating; the whole
question is **data layout**, because each HTTP call is a slow round-trip. Hard constraint: **nothing
here may cost the local (mmap) hot path** — every lever below is opt-in or lives in the client
fetcher.**

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

## 2. Levers (each additive / client-side — zero local hot-path cost)

1. **Contiguous bootstrap prefix (the big one).** Today the meta + catalog + plan-registry pages are
   spread across the file (322 scattered pages → up to 322 range requests on first touch). If they
   were **clustered into a contiguous region at a known offset**, the client fetches the whole
   bootstrap in *one* coalesced range request. This is a *write-side layout* choice (where the
   allocator places catalog/registry pages) or an **opt-in repack** that produces an
   HTTP-optimized copy — neither touches the read hot path. Biggest single win.
2. **The client fetcher is a `PageStore` over `Range: bytes=`** with a page cache + read-ahead
   coalescing (fetch a small window around the needed page; adjacent B-tree pages ride along). The
   bootstrap is cached once; thereafter each query adds 3–6 small fetches. A WASM build gives the
   browser case (sql.js-httpvfs). All of this is *new client code*, not an engine change.
3. **Mutation fragmentation.** COW scatters a table's leaves as it churns, so a range scan that reads
   contiguous leaves on a fresh file reads scattered ones on a churned file → more round-trips. Fix
   with an **opt-in repack/compaction** that restores leaf locality (like `VACUUM` → contiguous). It
   is a maintenance op, off the hot path — respects the constraint.
4. **Freshness for a live file.** mpedb's meta is a double-buffered A/B pair at pages 0/1; a client
   re-fetches the meta page to observe the newest committed version, and MVCC gives it a consistent
   snapshot for the rest of the query. HTTP `ETag`/`If-Range` validate the cached bootstrap; a
   changed meta invalidates just the pages a bumped `schema_gen`/root touches. So "mutable over HTTP"
   is: re-read the tiny meta, keep the rest of the cache, re-descend only what moved.

## 3. Honest limits

- **Full scans / large aggregates are not range-friendly** (773 pages here, and unbounded on a big
  table). Those belong server-side — run them where the file is local and return the *result*, not
  the pages. HTTP-range is for point/range/index access, which is exactly the cheap case above.
- **Writes over HTTP are a different problem** — a write needs the writer lock + the commit path,
  which is a protocol, not a range request. This study is about *read-serving* a file that some
  other process may be writing; the reader just re-reads the meta to move to the new snapshot.
- Prior art to lean on: **sql.js-httpvfs** (phiresky — SQLite over HTTP range), DuckDB `httpfs`,
  S3-range / Parquet footer-then-range reads.

## 4. If we ever build it (not now)

FrozenDb/pack (#22's original framing) becomes just the *repack* of lever 1/3 producing an
HTTP-optimized copy; the native + WASM `Range` fetcher is lever 2. Sequence: (a) opt-in repack that
clusters the bootstrap prefix + defragments leaves; (b) a `PageStore` HTTP fetcher with cache +
coalescing; (c) WASM. **None of it changes the local engine** — that is the whole point, and the
measurement says it is viable: after a one-request bootstrap, a point query is ~3 tiny fetches.

*(M3 network-latency numbers deferred — the M3 SSH key from a prior session is gone, so real
round-trip timing wasn't measured here; the page-count/round-trip analysis above is
platform-independent.)*
