# Routing: exact (mpedb + the kernel) vs the original MPEE solver

The last domain of the generic-solver program (stage M4,
[design/DESIGN-MPEE-GENERAL.md](../design/DESIGN-MPEE-GENERAL.md) §9.2): real
road-network sequencing, with the two engines this project bridges — **mpedb's
kernel grown a `(subset, last)` exact mode** (Held-Karp,
`mpedb_sql::sequence`), and **brooom**, the original MPEE vehicle-routing
solver (github.com/punnerud/mpee), run CPU-only as a subprocess with JSON I/O.

**The vecbench frame, applied to sequencing: exact is the ground truth, the
heuristic is scored by gap and time.** Below the exact cap (N ≤ 18 nodes) the
optimum is *known*, so brooom's answer gets a measured gap-to-optimum, not a
shrug. Past the cap the exact side **declines** — never a silent fallback that
stops being exact — and the heuristic's regime is reported as its own.

Harness: [`crates/mpedb-routebench`](../crates/mpedb-routebench) (std-only;
brooom invoked as a subprocess). Instance: brooom's bundled real-map San
Francisco set — `sf_s11_n80_osrm.json`, 81 locations, OSRM road durations.
Machine: the 2-core Linux dev box (gap is machine-independent; wall times are
not). Measured 2026-07-23.

**Agreement before timing, both directions:** brooom's claimed route cost is
recomputed on OUR matrix and must equal its own summary (it does, every row);
the exact solver's claimed cost is asserted equal to its route's cost inside
its own differential tests (Held-Karp vs brute-force permutation over n ≤ 8,
open and closed, asymmetric costs).

## mpedb as the platform

The instance lives in TABLES under [`models/routing.toml`](models/routing.toml)
(the model validates against this schema — dogfood): 81 stops + a 6,561-cell
`matrix(src, dst, secs)` load in 0.01 s; NDV analyzed; the exact arm reads its
submatrices *out of the database* (~1 ms for the full 81×81). The query an
application runs around a solve, measured: nearest-5 stops for 50 origins via
`ORDER BY secs LIMIT 5` — **0.97 ms total**.

## Closed tours from the depot (vehicle end = start, brooom's default)

| N (nodes) | exact optimum | exact total (read + solve) | brooom cost | brooom wall | gap |
|---:|---:|---:|---:|---:|---:|
| 9 | 3,659 | 0.2 ms | 3,659 | 564 ms | **+0.00%** |
| 11 | 6,423 | 0.5 ms | 6,423 | 878 ms | **+0.00%** |
| 13 | 6,630 | 1.0 ms | 6,630 | 1,094 ms | **+0.00%** |
| 15 | 7,988 | 4.1 ms | 7,988 | 1,709 ms | **+0.00%** |
| 17 | 7,375 | 18.5 ms | 7,375 | 2,012 ms | **+0.00%** |
| 18 | 5,098 | 42.5 ms | 5,098 | 2,331 ms | **+0.00%** |

Every row `agree: yes` (brooom's route recomputes to its claimed cost on our
matrix).

## The full instance (81 locations)

- exact: **declines** (cap 18 — beyond it the answer would stop being exact,
  and `solve_sequence` returns `None` rather than quietly becoming a
  heuristic).
- brooom: cost **15,117** in 195 s; the route recomputes to exactly 15,117 on
  our matrix. No gap is reported because no ground truth exists at this size —
  that is what the heuristic regime *means*.

## Reading it

**brooom finds the exact optimum on every instance we can check.** Six
sub-instances, 9–18 nodes, real asymmetric OSRM durations: gap +0.00% across
the board. That is the strongest statement this frame can produce about a
heuristic — *measured* optimality on everything measurable — and it is worth
more than any large-N number precisely because the ground truth is
independent.

**Where exact is available, it is not close — it is over.** 0.2–42 ms
including reading the matrix out of the database, against 0.6–2.3 s of
heuristic search. The crossover discipline writes itself: `N ≤ 18 → solve
exactly; N > 18 → heuristic, knowingly`. That decision rule is what the
`(subset, last)` kernel mode exists to make available — the same
decline-rather-than-degrade posture as every refusal in this engine.

**The streaming-N×N numbers differ by role.** Exactness costs the full
N·(N−1) matrix reads (`cells_bought`, counted by the solver); brooom's broker
exists to buy a *fraction* on problems where the matrix is not given (its own
london-scale logs: 100k×100k streamed through a 500 MB budget). With embedded
matrices both sides read everything; the fraction story belongs to the
un-embedded regime and is reported there, not claimed here.

## Mot PostgreSQL: planleggingskost og gjentatte kjøringer

A note circulating about this project claimed mpedb "handles high query
complexity more gracefully" than a traditional cost-based optimizer —
linear planning cost, a practical ceiling around 16 tables, near-zero
overhead on repeated queries. Every clause of that is measurable, so it was
measured against **PostgreSQL 16.14** on the same 2-core Linux box (7.6 GB,
ext4), same generated SQL, medians of 9. Durability is irrelevant here:
everything timed is planning/compile or warm read-only execution.

Shape: 17 tables `t1..t17`, each `(id int64 PK, a int64)`, 1000 rows,
`a = id`, joined as a 1:1 chain `t(k).a = t(k+1).id`.

**The headline claim was false when it was written — and measuring it is
what made it true.** The first run put mpedb at 3.08 ms on a 12-table chain
against PostgreSQL's 0.50, with a bizarre *cliff* down to 0.11 ms at
thirteen tables. That cliff was the finding: `DP_FULL_MAX = 12` in
`planner/mpee.rs` made the solver enumerate `univ & !mask` — every subset of
the unplaced tables, 2^n of them — up to twelve tables, and only past twelve
hand over to expansion along the join graph's **frontier**, which for a chain
admits O(n²) states instead of 2^n. The expensive branch was the one meant to
be careful. Restricting expansion to the frontier at *every* width (the
disconnected case still falls back, so cross products remain reachable where
they are forced) removes it:

| tables | mpedb before | **mpedb now** | pg plan (default) | pg plan (exhaustive) |
|---:|---:|---:|---:|---:|
| 4 | 0.04 ms | **0.03 ms** | 0.12 ms | 0.12 ms |
| 8 | 0.14 ms | **0.05 ms** | 0.35 ms | 0.36 ms |
| 10 | 0.77 ms | **0.06 ms** | — | — |
| 11 | 1.37 ms | **0.07 ms** | — | — |
| **12** | **3.08 ms** | **0.07 ms** | **0.50 ms** | **0.83 ms** |
| 13 | 0.11 ms | **0.08 ms** | — | — |
| 16 | 0.11 ms | **0.11 ms** | 0.71 ms | 1.73 ms |
| 17 | 0.11 ms | **0.11 ms** | 0.86 ms | 2.00 ms |
| 64 (self-aliases) | 0.77 ms | **0.77 ms** | 6.83 ms | — |

(`default` = PostgreSQL's shipped `join_collapse_limit = 8`; `exhaustive` =
`join_collapse_limit`/`from_collapse_limit` raised to 32 with `geqo = off`,
which is the arm the "exponential explosion" story is about.)

**41× at twelve tables, and the curve is now smooth** — no cliff, because
there is no longer a width at which the algorithm changes character. Against
PostgreSQL it is 6.8× faster at twelve tables and 6.6× at seventeen, and the
gap widens with width: PostgreSQL's growth is the honest cost of continuing
to search, ours is bounded because the search space itself is bounded by the
join graph rather than by the powerset.

Two things kept this from being a free lunch, and both were checked rather
than assumed. Seeding — whether the DP starts from every table or from an
extremal sample — is a *separate* decision from expansion, and the old
constant conflated them; it is now `DP_ALL_SEEDS_MAX`, still 12, so small
scopes keep full seeding **on top of** frontier expansion (it costs ~1 µs at
n=12 once expansion is cheap). And plan quality: all 202 planner/plan tests
assert unchanged choices, and `select4.test` — the join-heavy corpus file
this solver was built for — runs 3857/3857 correct in 35.3 s against the old
arm's 35.7 s, i.e. identical within noise. A worse join order there would
show as a blow-up, not a rounding difference.

**Repeated queries: it depends entirely on what the query does.**

| 12-table chain, 1000 repeats | mpedb | PostgreSQL |
|---|---:|---:|
| full scan-and-join (`count(*)`, answer 1000) | 5.74 ms | **2.06 ms** |
| point-anchored (`WHERE t1.id = 500`, answer 1) | **0.012 ms** | 0.074 ms |
| floor (`SELECT 1`) | 0.002 ms | 0.031 ms |

mpedb rows are `execute(hash)` (in-process, no parse); PostgreSQL rows are
`pgbench -M prepared`, one client, over a unix socket. Two different truths
sit in that table. When the statement *executes* real work over 12 tables,
**PostgreSQL is 2.8× faster** — its executor beats ours, and no amount of
plan caching hides that. When the statement is small — the ORM shape, a
point lookup through a deep join — mpedb answers in 12 µs against 74, and
the floor is 2 µs against 31: that gap is the socket round trip and the
per-statement server work, which an embedded engine simply does not have.
"Practically zero overhead on repeated queries" is the right instinct
pointed at the wrong metric: the win is in *overhead*, and it is invisible
whenever execution dominates.

**Nested subqueries: mpedb compiles them faster and runs them slower.**

| depth | mpedb compile | mpedb wall | pg plan | pg exec |
|---:|---:|---:|---:|---:|
| 3 | 0.023 ms | 2.49 ms | 0.101 ms | 0.434 ms |
| 4 | 0.029 ms | 3.68 ms | 0.138 ms | 0.633 ms |
| 5 | 0.034 ms | 4.91 ms | 0.162 ms | 0.812 ms |

(uncorrelated `IN`-nesting; correlated `EXISTS` at the same depths is within
noise of these rows on both engines, and both engines answer 500 everywhere.)

**The limits, quoted verbatim.** The note's "practical ceiling around 16
tables" is stale by a wide margin — the real cap is 64 join operands
(`MAX_JOINS = 63`, `plan/mod.rs`), and the 16 that survives is a *subquery*
cap. PostgreSQL accepts every probe below; mpedb refuses four, each by name,
each identically from the Python module and the release CLI:

| probe | mpedb |
|---|---|
| 17-table chain | answers 1000 |
| 64 self-join aliases | answers 1000 |
| 65 self-join aliases | `SQL parse error at byte 1921: at most 64 tables in a join` |
| `RIGHT JOIN` first | answers 1000 |
| `RIGHT JOIN` mid-chain | `RIGHT JOIN in a multi-join chain is only supported as the FIRST join — for a RIGHT that follows another join, swap the tables and write LEFT JOIN` |
| `FULL JOIN` mid-chain | answers 1000 |
| `FULL` after a leading `RIGHT` | `FULL JOIN following a leading RIGHT JOIN is not supported — swap the RIGHT's tables and write LEFT JOIN so the FULL sits in a plain left-deep chain` |
| 17-deep `IN`-nesting | `too many subqueries in one statement (max 16, including nested)` |

### Reading it

**Planning: mpedb wins, and now for a defensible reason.** 6.8× at twelve
tables, 6.6× at seventeen, bounded by the join graph rather than the
powerset — not by abandoning the search, which is what the number looked
like before this cell was run. That the fix was one branch does not make the
old number less real; it makes the measurement the reason it is gone.

**Execution: PostgreSQL wins, and it is not the join algorithm.** The same
12-table chain costs 5.7 ms per call against their 2.06. The obvious story —
index-nested-loop against hash join — is wrong, and `workbench/joinstep-profile.py`
is what says so. Decomposing the 520 ns each of the ~11 000 join steps costs:

| | ns |
|---|---:|
| `count(*)` over one table — traversal, no row materialised | **4 /row** |
| full scan projecting one column — traversal + materialise | **187 /row** |
| the probe itself (slope − scan) | **332** |

Materialising a row costs **45× walking past it**. A hash join would
materialise those same rows, so it cannot be the answer; PostgreSQL's entire
per-step budget is 187 ns, and we spend that much before the probe even
starts. The lever is decode-time column pruning on the join path — and the
machinery already exists: `RowCursor::next_masked` (#125's observable-column
analysis) decodes only the slots a statement reads, `scan_rows_pruned` uses
it, and the join gather calls `scan_rows_capped`, which does not. Threading
the analysis through join gather is the next concrete piece of work; it is
scoped, guarded by the corpus, and deliberately not started at the tail of
the session that measured it.

**Overhead: mpedb wins by a distance**, on the axis an embedded engine owns —
12 µs against 74 for a point-anchored join through those same twelve tables,
2 µs against 31 on the floor. Compile times an order of magnitude under
PostgreSQL's planning on nested subqueries belong here too: they matter for
SQL minted fresh rather than prepared.

**Limits: mpedb declines where PostgreSQL bends** — four named refusals here
against zero. Worth defending on its own terms, and not the same claim as
being better at complex queries.

Reproduce: `workbench/complexq-{gen,mpedb,pg}.py` (the PostgreSQL arm builds
and tears down its own throwaway cluster).

## Reproducing

```sh
# brooom, CPU-only, from github.com/punnerud/mpee:
cargo build --release -p brooom --no-default-features --features cli,osrm,google

cargo run --release -p mpedb-routebench -- \
  --instance mpee/crates/brooom/benchmarks/instances_realmap/sf_s11_n80_osrm.json \
  --brooom mpee/target/release/brooom
```
