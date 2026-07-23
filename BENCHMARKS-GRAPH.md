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

Milliseconds; cold = first run after load, warm = median of 5. Third run,
2026-07-23, after the depth sweep found and closed two holes (below). Note on
Neo4j's columns: by this run its JVM had been serving benchmarks for a while,
so its cold numbers are far better than a fresh process shows (the first-ever
run measured 151 ms cold for `degree`; a warm JVM answers the same cold query
in 3).

| workload | mpedb cold | mpedb warm | neo4j cold | neo4j warm | warm verdict |
|---|---:|---:|---:|---:|---|
| `degree` | 4.9 | 2.6 | 3.0 | 1.6 | neo4j 1.7× |
| `hop2` | 23.0 | 7.6 | 6.5 | 3.7 | neo4j 2.0× |
| `hop3` | 39.5 | 25.1 | 39.6 | 15.1 | neo4j 1.7× |
| `reach4` | 113.7 | 95.2 | 69.2 | 27.9 | neo4j 3.4× |
| `reach5` | 223.9 | 168.7 | 86.5 | 36.7 | neo4j 4.6× |
| `reach6` | 237.8 | 188.0 | 84.5 | 38.3 | neo4j 4.9× |
| `reach7` | 240.7 | 184.2 | 87.0 | 39.5 | neo4j 4.7× |
| `reach8` | 243.1 | 185.2 | 88.0 | 38.7 | neo4j 4.8× |
| `tri-hub` | 36.4 | 10.8 | 3.6 | 1.9 | neo4j 5.6× |
| `tri-global` | 1449.1 | 1401.3 | 344.0 | 301.1 | neo4j 4.7× |

## The two holes the sweep found, and their fixes

**The depth sweep (reach 4→8) exposed a linear blow-up.** First measurement:
mpedb 109 → 270 → 450 → 636 → **833 ms** while Neo4j sat flat at ~38 —
because `UNION` dedups `(node, depth)` PAIRS, a node found at depth 3 was
"new" again at depth 4, and every level re-expanded the whole reached set.
The fix (`f0b842d`) reuses the depth-guard proof the risk estimator already
owns as an execution optimization: when the counter is provably a monotone
guard, read nowhere else, and the outer statement provably cannot observe
multiplicity or the counter, the fixpoint dedups the working set on the
non-counter columns — each node expands once, at minimal depth. After:
**188/184/185 ms flat**. The curve now bends where Neo4j's bends; the
remaining ~4.8× is the same constant per-probe cost as every other row here.
Soundness is pinned differentially against sqlite from both sides — the gated
shapes at every k on a cyclic graph, and five near-misses (`count(*)`, the
counter projected, `count(DISTINCT d)`, an outer filter on `d`, `sum`) that
must decline and do.

**`tri-global` was paying an avg-degree row fetch + filter per probe.** The
triangle's closing edge pins BOTH columns (`c.src = b.dst AND c.dst = a.src`);
a `(src, dst)` composite index turns that into one tree point probe — schema
tuning through the existing #55 machinery, no engine change. 3,993 →
**1,401 ms** (2.9×), gap to Neo4j from 13.4× to 4.7×.

## Reading it

**After both fixes, every row is the same story: a constant per-probe factor.**
1.7–2× on point expansions, 3.4–4.9× on traversals and triangles — Neo4j
follows stored adjacency pointers where mpedb re-descends a B+tree per
expansion step, and that is the whole residual. No row is shaped differently
from the others any more; the two workload-specific pathologies (the linear
depth blow-up, the unindexed closing edge) are gone.

**This is the same finding as the OLAP bench, wearing a different workload.**
The join *orders* are right (MPEE + stage-A NDV statistics); what costs is the
per-probe execution price. The two benches now bracket it from both sides:
SQLite brackets it at 1.7–2.1× on star joins, Neo4j at 1.7–4.9× on
traversals, and any future work on the probe path (batch descent, sorted-run
probing) pays off in both columns at once.

**Neo4j's cold column is its own finding.** First-run Cypher pays compilation
and page-cache warm-up: 151 ms for a `degree` that answers warm in 2.8. mpedb's
cold runs pay plan compilation measured in single-digit ms — content-hashed
plans are compiled once and the hot path never parses. On a workload of
*unrepeated* queries, every Neo4j number above is its cold column.

**One proof, two consumers.** The depth-guard proof (`d` carried as `d+1`,
guarded `d < k`, anchor constant) now serves both the prepare-time risk
estimate — which stopped reporting the halting-problem default on provably
bounded recursions (`risk.rs`, `tests/risk_depth_guard.rs`) — and the
converged-frontier execution optimization above. They share one function, so
they can never disagree about what "provably bounded" means.

**Re-verified 2026-07-23** (mpedb `944ca6b`, Neo4j 5.26, same M3): every cell
lands within a few percent of the table above on BOTH sides — mpedb warm
`reach8` 185.2 → 198.0 ms against Neo4j's 38.7 → 39.3, `tri-global` 1401 → 1440
against 301 → 316 — so the ratios and the hop-3 crossover are unchanged, and
every row still agrees.

## The operator arm: the sugar is free, and it locks the fast shape

Re-run 2026-07-23 with a third arm: the same questions in the `:op:` operator
language (SQL-EXTENSIONS.md), defined by the bench itself — the model's roles
install `:->:`, and the bench adds `:deg: n`, `:reach4:`…`:reach8: n`
(statement operators), and `:tri:`.

| workload | mpedb SQL warm | mpedb `:op:` warm |
|---|---:|---:|
| `degree` | 2.6 | 1.4 |
| `reach4` | 94.0 | 93.5 |
| `reach6` | 183.7 | 183.1 |
| `reach8` | 186.3 | 184.3 |
| `tri-global` | 1,395.7 | 1,480.4 |

Every operator-arm answer equals the SQL arm's (asserted, not assumed), and
the times are identical within run noise — the macro expands at COMPILE time,
so the executed plan is the same plan. The real gain is not speed but
**shape-locking**: `:reachK: n` GENERATES the converged-frontier CTE — the
`count(DISTINCT node)` form the depth-guard optimization can prove — so a
user of the sugar cannot accidentally write the `count(*)` variant that
re-expands the reached set every level (the 833 ms hole the first sweep
found). The language is where the fast pattern lives now, not the user's
memory. (`:deg:`'s 1.4 vs 2.6 ms is a scalar-subquery plan reaching the index
count path — same answer, slightly different shape; noted for honesty.)

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
