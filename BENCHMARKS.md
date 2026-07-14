# mpedb benchmarks

Head-to-head throughput and latency for **mpedb vs SQLite vs PostgreSQL** on the
same machine, same workloads, same measurement loop. The numbers below are a
curated summary; the full machine-generated tables (every cell, both durability
classes, all latency percentiles) live in
[`crates/mpedb-bench/RESULTS.md`](crates/mpedb-bench/RESULTS.md).

Reproduce:

```sh
cargo run --release -p mpedb-bench          # full run → rewrites RESULTS.md
cargo run --release -p mpedb-bench -- --only mpedb   # one engine, least noise
mpedb bench --auto --durability none|commit|wal|async   # mpedb-only, quick
```

## How to read these numbers

- **Same harness for all three engines.** One Rust timing loop calls each engine
  through its own fast path — mpedb's `execute(hash, …)` precompiled plan,
  SQLite/PostgreSQL prepared statements — so no language or driver overhead skews
  one engine. mpedb and SQLite are **embedded** (a call in-process); PostgreSQL is
  **client/server** (every op pays a unix-socket round-trip). That gap is a real
  architectural difference, not benchmark unfairness, and it dominates point-op
  latency.
- **Compare only within a durability class.** *none-class* = fastest, no fsync
  guarantee (may lose data on power loss); *commit-class / durable-on-ack* =
  power-loss-durable the instant a commit returns. Never compare a none-class
  number against a durable one — they promise different things.
- **This is a shared 2-core cloud VM** (AMD EPYC-Milan, 7.6 GiB, ext4 + tmpfs).
  Every run before 2026-07-14 12:10 was measured with **one of the two cores
  pinned at 99% by an unrelated stray process** — see
  ["Reading run-to-run deltas"](#reading-run-to-run-deltas--the-control-group-method).
  The numbers below are the first on a genuinely idle box, and two back-to-back
  runs agree within ~4%.

## Headline results (2026-07-14 12:14 UTC, Linux, 2 idle cores, single-client unless noted)

### Embedded point operations, none-class — mpedb's home turf

Zero-parse execute-by-hash + no IPC + a COW B+tree in the same address space:

| op (none-class) | mpedb ops/s | SQLite ops/s | PostgreSQL ops/s | mpedb vs SQLite / PG |
|---|--:|--:|--:|---|
| point-select (PK) | **469,777** | 80,145 | 21,638 | ~5.9× / ~22× |
| point-insert | **165,142** | 41,555 | 13,749 | ~4.0× / ~12× |
| point-update (PK) | **201,638** | 46,214 | 11,058 | ~4.4× / ~18× |

p50 latencies: mpedb select **1 µs**, insert **5 µs**; SQLite 11 µs / 21 µs;
PostgreSQL 44 µs / 68 µs.

### Lock-free reads under a concurrent writer (commit-class)

3 readers + 1 writer, durable disk config. mpedb's MVCC readers never block the
writer:

| engine | read ops/s (none-class) | read p50 | read p99 |
|---|--:|--:|--:|
| mpedb | **466,464** | 2 µs | 3 µs |
| SQLite (journal=MEMORY) | 4,145 | 11 µs | **18,313 µs** |
| PostgreSQL | 35,226 | 70 µs | 330 µs |

**This is the cell mpedb was built for, and it only became visible once the box
was idle: 112× SQLite.** SQLite's none-class journal serializes readers against
the writer, so with two real cores the writer simply runs harder and starves the
readers — p99 read latency 18 ms. mpedb's MVCC readers never take the writer's
lock, so they are untouched (p99 3 µs).

Give SQLite its WAL (commit-class), though, and it wins this cell:

| engine | read ops/s (commit-class) | read p50 |
|---|--:|--:|
| mpedb | 561,117 | 2 µs |
| SQLite (WAL) | **657,662** | 1 µs |
| PostgreSQL | 41,231 | 59 µs |

Honest read: **against SQLite-in-WAL, mpedb has no read-throughput edge in one
process** (−15%). mpedb's structural advantages here — multi-*process* readers
and cross-process shared plans — are not exercised by this single-process cell,
so it under-sells mpedb and over-sells SQLite relative to the multi-process case
mpedb targets. PostgreSQL pays a socket round-trip per read: that is the 13-16×
gap.

### Durable single-client INSERT (durable-on-ack) — mpedb's weak spot, and the fix

One fsync per commit is a hardware floor, and mpedb's intent-ring group-commit
only engages **under contention** — so a lone durable writer is where mpedb has
the least room to be clever:

| config (durable-on-ack) | ops/s |
|---|--:|
| **mpedb `wal`, single client** | **1,900** |
| SQLite `synchronous=FULL`, single client | 846 |
| PostgreSQL `sc=on`, single client | 1,679 |
| **mpedb `wal`, batched 100/commit** | **129,087** |
| SQLite `FULL`, batched 100/commit | 61,923 |
| PostgreSQL `sc=on`, batched 100/commit | 17,679 |

`wal` leads both single-client (1,900 vs 846 / 1,679) and batched (**129k** vs
62k / 18k, 2.1×) — the logical-WAL + fsync-coalescing work closed the
single-client gap earlier runs showed. The genuinely weak cell is a different
one: **`durability=commit` single-client (~560 ops/s insert)** — the slowest in
the suite, because every commit msyncs the meta double-buffer with no batching
partner. Guidance: for durable writes use `wal`, and batch in a `WriteSession`
when you can.

### Contended writes — where mpedb's single writer lock costs it

4 threads × autocommit inserts, none-class:

| engine | ops/s |
|---|--:|
| **mpedb** | **79,243** |
| PostgreSQL | 33,727 |
| SQLite | 29,747 |

mpedb still leads, but this is the cell that **shrank most when the box got a
second real core: 6.8× → 2.7× vs SQLite**, reproducibly. That is the honest
shape of the design — mpedb serializes writers behind one lock and amortizes
with group commit, so extra cores buy it comparatively little, while SQLite's
and PostgreSQL's contended writes scale with them. mpedb's write-parallelism
answer is architectural, not per-lock: separate files (Workspaces / ShardSet),
which this single-file cell does not measure.

## Reading run-to-run deltas — the control-group method

**A stray process ate half this machine for five days.** An unrelated orphaned
python script (PPID 1, a websocket test that spun forever on `recv()` after EOF)
sat at 99% of one of the two cores from 2026-07-09 until it was killed on 07-14
at 12:09, having burned ~120 hours of CPU. **Every mpedb benchmark before
12:10 — including three "full" runs — was therefore measured on ~1 core.**

That is a cautionary tale with a method attached, because we caught it the right
way. **SQLite and PostgreSQL are the control group**: their binaries are
byte-identical across our runs, so

- all three engines move together → **the host**;
- mpedb moves and they do not → **a code signal**.

That test is what proved the 07-14 10:40→11:56 swing (every cell +15-84%) was
host load and not the mirror/CDC work — SQLite (+19% median) and PostgreSQL
(+35%) rose with it. It also answers the question the CDC work raised: **M1's
change-capture hook in all six engine mutators costs nothing measurable** when
no table is mirrored — the none-class insert/update cells (pure write-path CPU)
tracked SQLite's environmental gain instead of lagging it.

**But the correction matters more than the confirmation.** Freeing the core
showed that ratios are only portable for cells that do not need the missing
core:

| ratio (none-class, mpedb vs SQLite) | ~1 core | 2 cores (×2 runs) |
|---|--:|--:|
| point-select | 5.4× | 6.1× / 5.9× |
| point-insert | 3.6× | 4.1× / 4.0× |
| point-update | 3.7× | 4.3× / 4.4× |
| **contended-writes** | **6.8×** | **2.4× / 2.7×** |

Single-client cells held (all engines were equally starved). The **contended**
cell did not: with ~1 core nothing can actually contend in parallel, which
flattered mpedb's single-writer-lock design by 2.5×. Same for read-while-write,
where the starved box hid a 112× mpedb win in none-class *and* a 15% SQLite win
in commit-class. **A starved host does not just scale numbers down — it silently
compresses exactly the cells that measure parallelism.**

Standing rules: **check `ps aux` before believing a number**; **run one engine at
a time** (`--only mpedb`) for a clean absolute; **never read a delta without the
controls**; and treat multi-threaded ratios from a loaded host as unusable, not
merely noisy.

## Known issues / improvement opportunities

1. **`newest_meta` stale-gate race (durability=commit).** A reader that loads the
   `durable_txn` gate, then is descheduled while two durable commits land, gets a
   spurious `Corrupt("no valid meta page")` — both meta slots are newer than its
   stale gate. The DB is *not* corrupt; a re-read succeeds. The bench adapter
   retries (bounded) and counts it. **Fix:** reload the monotone gate and retry in
   `mpedb-core::shm::newest_meta`. This is the top genuine bug the benchmark
   surfaces.
2. **`durability=commit` single-client floor.** Group-commit engages only under
   contention, so a lone durable writer pays one serialized msync per commit —
   525 ops/s insert, the slowest cell in the suite (SQLite FULL does 1,632, and
   mpedb's own `wal` does 2,598). `wal` already closed this gap for itself; the
   remaining opportunity is a single-writer fast path for `commit` mode, or
   simply steering users to `wal` (which the docs do).
3. **CDC capture check on the write path (minor).** With change-capture in the
   engine (mirror foundation), each write txn does one `cdc\0tabs` sys-lookup even
   when no mirror is configured. It is *not* the cause of the run-to-run variance
   above (reads, which skip it, moved identically), but caching the config across
   txns keyed by the meta generation would shave a small constant off every
   autocommit write.

## Platforms

These numbers are **Linux (x86-64)**. macOS/Apple Silicon perf has not been
re-measured in this run; platform *correctness and crash-safety* parity (not
throughput) is covered separately — see [Platforms](README.md#platforms) and
[`DESIGN-MACOS-LOCK.md`](DESIGN-MACOS-LOCK.md).
