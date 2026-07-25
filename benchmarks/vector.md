# Vector: mpedb exact vs Qdrant HNSW

mpedb has no vector index. What it has, as of stage D
([design/DESIGN-MPEE-GENERAL.md](design/DESIGN-MPEE-GENERAL.md)): embeddings
as BLOBs of little-endian f32 (no schema-format change), `vec_l2` /
`vec_cosine` scalars with strict shape refusals, and an exact-kNN executor
path — `ORDER BY vec_l2(emb, $q) LIMIT k` runs a k-sized heap with
**per-dimension early abandonment**: squared-difference terms are
non-negative, so a candidate's partial sum is a lower bound on its distance,
and it is dropped the moment it exceeds the current k-th best. Exactness and
errors are both preserved; only arithmetic is skipped.

**The comparison is exact-vs-approximate on purpose.** mpedb's answer is the
ground truth Qdrant's recall@10 is scored against; its latency is the price of
exactness. A bare "X× faster" in either direction would be a category error —
HNSW answers *probably-nearest*, a scan answers *nearest*. The honest frame is
the pair (latency, recall), side by side.

Harness: [`crates/mpedb-vecbench`](crates/mpedb-vecbench) (std-only Qdrant
REST client — no new dependencies). Machine: Apple M3 Pro. Qdrant 1.18,
default HNSW parameters. Data: 100,000 × 128-dim f32, **clustered** (64
centroids + noise — uniform noise would be an adversarial dataset for HNSW,
the same manufacturing-a-loss rule the OLAP bench applied to DuckDB's
indexes). 100 queries, k = 10. Measured 2026-07-23.

## Results

| side | median | p99 | recall@10 |
|---|---:|---:|---:|
| mpedb exact, heap + early abandonment | 17.9 ms | 38.3 ms | **1.000** (ground truth) |
| mpedb exact, generic sort (the A/B arm) | 52.3 ms | 54.6 ms | 1.000 |
| qdrant HNSW, default params | **3.6 ms** | 10.0 ms | 0.992 |

Filtered — `WHERE cat = 'c3'` (1/8 of the data) then nearest 10:

| side | median | p99 | recall@10 |
|---|---:|---:|---:|
| mpedb exact, filter **before** heap | **5.6 ms** | 32.0 ms | **1.000** (ground truth) |
| qdrant HNSW, payload filter | 75.2 ms | 104.8 ms | 1.000 |

## Reading it

**Unfiltered, the trade is what theory says it is.** Qdrant's graph answers in
3.6 ms at 0.992 recall; the exact scan pays 17.9 ms for the last 0.8%. If
probably-nearest is acceptable, a vector index is 5× faster, and nothing here
argues otherwise — that is what the structure exists for.

**Filtered, the result inverts — 13× — and this is the finding.** A selective
predicate is poison for a graph traversal: the HNSW walk keeps arriving at
neighbours the filter rejects, and at default settings Qdrant pays 75 ms to
claw back full recall. mpedb runs the filter FIRST — the `cat` index serves
12,500 candidate rows — and scans exactly those with the abandoning heap:
5.6 ms, exact by construction. This is the pgvector post-index-filtering
problem LANDSCAPE.md called out in prose, now measured: *the index is an
accelerator over candidates, never an authority, and the predicate belongs
inside the traversal — or before it.* Filtered vector search is what RAG
inside a database actually looks like (`WHERE tenant = $1 AND kind = $2
ORDER BY distance LIMIT k`), which makes this the cell that matters.

**Early abandonment bought 2.9×** (52.3 → 17.9 ms median), measured as an A/B
inside one binary: the same prepared query in a shape the heap path declines
(a second sort key) runs the generic materialize-and-sort. Same answers,
bit-for-bit — the differential tests in `crates/mpedb/tests/knn.rs` pin ties,
NULL embeddings, OFFSET paging and the raise-on-malformed-row behaviour to the
generic path's exactly. The mechanism is DESIGN-MPEE-GENERAL §3's monotone
lower bound, applied per dimension instead of per table.

**Re-verified 2026-07-23** (mpedb `944ca6b`, same Qdrant, same M3): exact
unfiltered 17.9 → 17.7 ms, the generic-sort A/B arm 52.3 → 51.8 (abandonment
still 2.9×), filtered exact 5.6 ms against Qdrant's 75.2 → 76.1 — the 13×
filtered inversion reproduces. One difference worth stating: Qdrant's
unfiltered recall@10 came back **1.000** this run against 0.992 before, on a
collection rebuilt from scratch by the harness; HNSW recall is
construction-order dependent, so treat 0.992–1.000 as its band on this data
rather than either number as the figure.

## The operator spelling is provably free

`mpedb op install-model` (the rag model's `embedding` role) installs `:~:`,
and `ORDER BY emb :~: $q LIMIT 10` compiles to the **identical plan hash** as
the `vec_l2(emb, $q)` spelling — asserted in the harness, reproduced in the
2026-07-23 re-run (which also reproduced the headline: filtered exact 5.5 ms
vs Qdrant's 75.9). The macro expands at compile time; the hash proves the
sugar costs nothing by construction, not measured-to-be-close.

## What is deliberately absent

- **An approximate index in mpedb.** The storage position is decided
  (LANDSCAPE.md: ordinary mpedb trees, pgvector's visibility model, predicate
  into traversal from day one) and the ground-truth harness now exists — any
  future HNSW/IVF lands with its recall measured against the engine's own
  exact answers from day one.
- **Tuned Qdrant.** Default HNSW parameters, deliberately: this is the
  out-of-the-box comparison. Raising `ef` buys recall for latency on the
  unfiltered cell and would not repair the filtered one, whose cost is
  structural.

## Reproducing

```sh
# Qdrant on localhost:6333 (a bare binary works: ./qdrant), then:
cargo run --release -p mpedb-vecbench
```
