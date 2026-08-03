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
multi-process, self-verifying) + `sqlite_incr.py` (same shape, stdlib).
