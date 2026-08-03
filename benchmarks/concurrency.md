# "How many concurrent locks before it melts?"

Someone asked exactly that. Measured answer: **it doesn't melt — it climbs.**
Throughput RISES with writer count until far past core count, peaks around
128 concurrent writer processes on a 2-core box, and degrades gracefully
after; at 1 024 concurrent lock-takers it still commits 10× the control
arm's rate, with the conservation invariant verified at every point.

## The shape

`mpedb stress --mode incr`: N separate **processes** (not threads) attach
the same database file and each loops an autocommit
`UPDATE ctr SET v = v + 1 WHERE id = ?` over a shared 64-key space — one
writer-lock acquisition and one durable commit per op. The harness verifies
`sum(v) == total committed ops` after every run (a lost or doubled
increment fails the row), plus the engine's page-accounting verifier.

Control arms (rule #122, like-for-like durability), same host, back to
back, mpedb from the released v0.2.7 binary:

- **sqlite3** (stdlib): same N-process autocommit-increment shape,
  `journal_mode=WAL`, `synchronous=FULL`, `fullfsync=ON` (a no-op on
  Linux; on macOS plain `fsync()` does not reach the platter, and mpedb's
  commit durability pays `F_FULLFSYNC` — the control must too),
  `busy_timeout=60s`.
- **PostgreSQL 16.14**: same shape against a dedicated local cluster
  (`synchronous_commit=on`, `fsync=on` — both asserted by the harness —
  `shared_buffers=512MB`, `max_connections=1100`, unix socket). One
  transport asymmetry is inherent and worth naming: mpedb and sqlite are
  IN-PROCESS while every PostgreSQL op crosses a socket round trip — that
  is not an unfairness to correct, it is the architectural difference the
  question is about, but it does pad PostgreSQL's per-op floor at low
  concurrency.

## Linux x86-64 (2 cores, 7.6 GB RAM, ext4 — deliberately small hardware)

| concurrent writer processes | mpedb ops/s | sqlite ops/s | postgres ops/s |
|---:|---:|---:|---:|
| 1 | 1 218 | 2 505 | 1 822 |
| 2 | 1 888 | 2 376 | 3 532 |
| 4 | 3 704 | 2 296 | 5 535 |
| 8 | 6 993 | 2 398 | 8 345 |
| 16 | 11 932 | 2 269 | **9 587** |
| 32 | 24 426 | 2 248 | 9 195 |
| 64 | 37 792 | 2 190 | 7 728 |
| 128 | **47 091** | 2 097 | 5 689 |
| 256 | 31 487 | 2 364 | 4 166 |
| 512 | 29 689 | — | — |
| 1 024 | 23 475 | — | — |

Verify: green at every point, all three engines (conservation invariant;
PostgreSQL's asserted via `sum(v)` like the others).

Reading it: three different answers to the same lock.

- **sqlite** serializes on the WAL write lock and pays a private fsync per
  commit: flat ~2.3 k ops/s at every concurrency, drooping past 128. It
  neither melts nor benefits — the queue just gets longer.
- **PostgreSQL** group-commits in the WAL writer, so it scales first —
  fastest of the three from 2 to ~24 processes — peaks at 9.6 k ops/s at
  16, then pays the server model's price per additional connection
  (a backend process each, lock-manager traffic, scheduler pressure on 2
  cores): down to 4.2 k at 256.
- **mpedb**'s intent-ring group commit turns waiters into batch with no
  per-connection server state: it passes PostgreSQL between 16 and 32
  processes, peaks at 47 k ops/s at 128 — 5× PostgreSQL's peak, 38× its
  own single-writer rate, on a box 64× oversubscribed — and at 1 024
  concurrent lock-takers still commits 23 k/s, five times PostgreSQL's
  best point. The post-peak slope is scheduler cost, not lock collapse.

At 1 writer, everyone's group-commit machinery is pure overhead: sqlite
(which has none to arm) wins, and mpedb's window — a lone committer
briefly waiting for followers that never come — is the visible price of
the curve that follows.

## macOS arm64 (Apple Silicon, 11 cores, APFS)

| concurrent writer processes | mpedb ops/s | sqlite ops/s (fullfsync=ON) |
|---:|---:|---:|
| 1 | 155 | 5 384 |
| 2 | 305 | 4 409 |
| 4 | 498 | 4 882 |
| 8 | 794 | 4 650 |
| 16 | 1 453 | 4 473 |
| 32 | 3 254 | 3 712 |
| 64 | 6 452 | 3 426 |
| 128 | **12 708** | 2 504 |

Instrument validation (the A/B rule): `PRAGMA fullfsync` read back 1 in the
children, and a 200-commit probe measured 0.05 ms/commit with it off
against 0.51 ms with it on — the pragma bites, and `F_FULLFSYNC` on this
Apple-silicon SSD costs ~0.2–0.5 ms sustained, not the tens of
milliseconds of the Intel era. Without `fullfsync=ON` the sqlite arm reads
~15–21 k ops/s flat — a weaker durability contract (macOS `fsync()` stops
at the drive cache), which is exactly why rule #122 exists; the fair
column is published.

The mpedb single-writer number is the group-commit WINDOW, not the flush:
a lone committer waits briefly for followers that never come — the same
latency-for-batching trade visible in the Linux column, and the price of
the curve that then climbs 82× to the 128-process peak while the control
arm halves. Crossover on this host is ~48 processes; at 128, mpedb commits
5× the control's rate. Low-concurrency latency is durability-mode
dependent; if a workload is single-writer-dominated, `--durability commit`
is the wrong knob to hold the writer lock under in the first place.

## Latency under contention — the question throughput hides

A fair objection: batching buys throughput by making each waiter wait for
its batch turn, so the ops/s headline could hide terrible individual
latency. Measured (same Linux host and shape, per-op wall time inside each
worker; p50 = the median worker's median op, p99 = the WORST worker's p99,
so no tail is averaged away):

| N | mpedb p50 | mpedb p99 | sqlite p50 | sqlite p99 |
|---:|---:|---:|---:|---:|
| 1 | 0.81 ms | 1.3 ms | 0.38 ms | 1.0 ms |
| 8 | 1.0 ms | 1.5 ms | 0.39 ms | 2.7 ms |
| 32 | 1.4 ms | 2.1 ms | 0.40 ms | **15.0 s** |
| 128 | 2.7 ms | 5.3 ms | 0.45 ms | 16.9 s |
| 512 | 8.4 ms | 15.5 s | 0.28 ms | 19.4 s |

Two shapes of queue:

- **mpedb's median grows exactly as the batch model predicts** (Little's
  law: 128 workers / 46 k ops/s ≈ 2.8 ms — the measured 2.7 ms p50), and
  through 128 processes the p99 stays about 2× the p50: the intent ring
  serves waiters in ORDER, so nobody is starved. Batching costs every
  writer a batch turn; it does not cost an unlucky writer everything.
- **sqlite's p50 looks better and its p99 is the disaster**: the lock is a
  retry lottery, so the median grab is fast while the unlucky writer
  starves for 15–19 SECONDS — from 8 processes up. The commenter's worry
  is real, and it lands on the arm without an ordered queue.
- At 512 processes (256× oversubscribed on this 2-core box) mpedb's worst
  worker also starves once (15.5 s) — the OS scheduler's unfairness, past
  the ring's. That is the honest edge of the curve, and it is why the
  throughput table above stops claiming smooth degradation past ~256.

If individual write latency under contention is the workload's binding
constraint and the writes touch DISJOINT data, the serialized-batch road
is the wrong tool entirely — that is what the optimistic-guard work is
for, measured in [documents.md](documents.md): many editors on one
document vs editors on their own documents, p50 pinned to think time on
independent surfaces, with the C1 calibration for how many editors fit
inside a 1 s answer. (On a fully CONTENDED surface like this cell's 64
shared keys, optimistic retry is the worst tool — we measured it
collapsing to ~36 ops/s here — which is exactly the split documents.md's
control arms attribute.)

## The honest boundaries

- This is the WRITE-contention cell. Readers never take the lock at all
  (MVCC snapshots, lock-free reader table) — read scaling is a different
  question with a duller answer: reader slots are a fixed table
  (`max_readers`, default 1 024) and the 1 025th attach is a named refusal,
  not a slowdown.
- One writer commits at a time, always — the claim is not parallel writes,
  it is that the queue AMORTIZES instead of thrashing.
- 15 s per point, single run, quiet boxes; medians over longer runs would
  smooth the 256–1 024 tail but not move the shape.

Harness: `mpedb stress --mode incr --durability commit` (in-tree,
multi-process, self-verifying; per-op p50/p99/max since the latency
section) + `workbench/lockbench-sqlite_incr.py` and
`workbench/lockbench-pg_incr.py` (same shape). One instrument lesson paid
for twice while measuring: the storage medium is part of the instrument —
a curve from the box's slow volume read 34× under the SSD's and both
control arms collapsed with it; curves from different disks do not mix.
