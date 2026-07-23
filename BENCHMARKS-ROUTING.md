# Routing: exact (mpedb + the kernel) vs the original MPEE solver

The last domain of the generic-solver program (stage M4,
[design/DESIGN-MPEE-GENERAL.md](design/DESIGN-MPEE-GENERAL.md) §9.2): real
road-network sequencing, with the two engines this project bridges — **mpedb's
kernel grown a `(subset, last)` exact mode** (Held-Karp,
`mpedb_sql::sequence`), and **brooom**, the original MPEE vehicle-routing
solver (github.com/punnerud/mpee), run CPU-only as a subprocess with JSON I/O.

**The vecbench frame, applied to sequencing: exact is the ground truth, the
heuristic is scored by gap and time.** Below the exact cap (N ≤ 18 nodes) the
optimum is *known*, so brooom's answer gets a measured gap-to-optimum, not a
shrug. Past the cap the exact side **declines** — never a silent fallback that
stops being exact — and the heuristic's regime is reported as its own.

Harness: [`crates/mpedb-routebench`](crates/mpedb-routebench) (std-only;
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

## Reproducing

```sh
# brooom, CPU-only, from github.com/punnerud/mpee:
cargo build --release -p brooom --no-default-features --features cli,osrm,google

cargo run --release -p mpedb-routebench -- \
  --instance mpee/crates/brooom/benchmarks/instances_realmap/sf_s11_n80_osrm.json \
  --brooom mpee/target/release/brooom
```
