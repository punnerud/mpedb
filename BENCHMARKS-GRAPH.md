# Graph: mpedb vs Neo4j

mpedb has no graph type. A graph here is an edge *table* — `(id PK, src
indexed, dst indexed)` — answered with joins and recursive CTEs, against Neo4j
5.26 answering the same questions in Cypher over a property graph. That is the
point of the comparison: how far does a relational engine get on graph
questions **before** any graph-native machinery exists, and where exactly does
the native representation start to win?

Harness: [`crates/mpedb-graphbench`](crates/mpedb-graphbench) — a workspace
member with **zero new dependencies** (the Neo4j client is ~300 lines of std:
HTTP/1.0 so the server never chunks, a minimal JSON parser, base64 by hand).
Machine: Apple M3 Pro, macOS 26.6. Neo4j 5.26.0, community, 4 GB heap + 4 GB
page cache, measured over its HTTP transactional endpoint on localhost.
Measured 2026-07-23.

Graph: 50,000 nodes, 250,000 directed edges, sources drawn from a squared
distribution so node 0 is a hub (~1,100 out-edges) — the shape a follower
graph has. Deterministic generator, identical edges to both engines.

Every workload runs on both engines and the canonically-rendered results are
compared **before** any timing is believed; every row below agrees.
Deliberately excluded: unbounded `[*]` traversal — Cypher walks *trails* (no
repeated relationship per path), a different mathematical object than a
recursive CTE's reachable set, and timing two different questions teaches
nothing.

Two taxes to keep in mind, one per side: Neo4j pays HTTP + JSON per query
(a real client's path, but the in-process side pays nothing); mpedb pays its
per-probe execution cost (the same one the OLAP bench measured against SQLite).

## Load

| engine | load | note |
|---|---:|---|
| mpedb | 0.4 s | in-process, edge table, src+dst indexes, NDV analyzed (stage A) |
| neo4j | 3.3 s | HTTP tx endpoint, UNWIND batches of 10k, id index |

## Results

Milliseconds; cold = first run after load, warm = median of 5.

| workload | mpedb cold | mpedb warm | neo4j cold | neo4j warm | warm verdict |
|---|---:|---:|---:|---:|---|
| `degree` | 4.9 | 2.2 | 151.2 | 2.8 | **mpedb 1.3×** |
| `hop2` | 25.7 | 8.0 | 133.4 | 15.3 | **mpedb 1.9×** |
| `hop3` | 73.0 | 25.0 | 119.8 | 15.5 | neo4j 1.6× |
| `reach4` | 160.1 | 109.2 | 129.8 | 26.7 | neo4j 4.1× |
| `tri-hub` | 63.4 | 19.8 | 72.6 | 2.7 | neo4j 7.3× |
| `tri-global` | 4005.9 | 3992.7 | 461.4 | 298.7 | neo4j 13.4× |

## Reading it

**The crossover is at hop 3, and it is the honest headline.** One or two index
probes out from a node, the edge table wins: `degree` is an index range count,
`hop2` one self-join, and the HTTP round-trip is a bigger share of Neo4j's
2.8 ms than the traversal is. From three hops on, the native representation
takes over — Neo4j follows stored adjacency pointers where mpedb re-descends a
B+tree per expansion step, and by `tri-global` (a full 3-cycle sweep: 250k
scan × two probe levels) that per-probe tax compounds to 13.4×.

**This is the same finding as the OLAP bench, wearing a different workload.**
The join *orders* are right (MPEE + stage-A NDV statistics); what costs is the
per-probe execution price. The two benches now bracket it from both sides:
SQLite brackets it at 1.7–2.1× on star joins, Neo4j at 1.6–13× on traversals,
and any future work on the probe path (batch descent, sorted-run probing)
pays off in both columns at once.

**Neo4j's cold column is its own finding.** First-run Cypher pays compilation
and page-cache warm-up: 151 ms for a `degree` that answers warm in 2.8. mpedb's
cold runs pay plan compilation measured in single-digit ms — content-hashed
plans are compiled once and the hot path never parses. On a workload of
*unrepeated* queries, every Neo4j number above is its cold column.

**The recursive CTE holds up better than expected.** `reach4` — semi-naive
fixpoint over a working table, no graph machinery at all — lands 4.1× behind a
native traversal engine on its home question. The prepare-time risk estimator
also stopped crying wolf on this shape: a depth guard the engine can *prove*
monotone (`d` carried as `d+1`, guarded `d < 4`) now bounds the estimate
statically instead of reporting the halting-problem default
(`risk.rs`, tested in `tests/risk_depth_guard.rs`).

## What each workload is

- **`degree`** — hub out-degree.
  `SELECT count(*) FROM edge WHERE src = 0` vs `MATCH (:N {id: 0})-[:E]->() RETURN count(*)`
- **`hop2` / `hop3`** — distinct nodes exactly 2/3 hops out (self-joins vs
  chained pattern).
- **`reach4`** — distinct nodes within 4 hops, start included: depth-guarded
  `WITH RECURSIVE … WHERE r.d < 4` vs `[:E*0..4]`.
- **`tri-hub`** — directed triangles through the hub (3-cycle join anchored at
  both ends).
- **`tri-global`** — every directed 3-cycle (each counted 3× by both engines,
  so the counts agree).

## Reproducing

```sh
# Neo4j 5.26 on localhost:7474, password set:
cargo run --release -p mpedb-graphbench -- --pass <password> --reps 5
```

The harness wipes and reloads the Neo4j database (`MATCH (x) DETACH DELETE x`)
on every run, builds the id index, and loads both engines from the same
deterministic edge list.
