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
  Absolute numbers swing 20-80% run-to-run on host load alone — in both
  directions, measured. Read the **ratios**, not the digits, and see
  ["Have we gotten slower/faster?"](#have-we-gotten-slowerfaster--how-to-read-run-to-run-deltas)
  for the method that separates a code change from a noisy host.

## Headline results (2026-07-14 11:56 UTC, Linux, single-client unless noted)

### Embedded point operations, none-class — mpedb's home turf

Zero-parse execute-by-hash + no IPC + a COW B+tree in the same address space:

| op (none-class) | mpedb ops/s | SQLite ops/s | PostgreSQL ops/s | mpedb vs SQLite / PG |
|---|--:|--:|--:|---|
| point-select (PK) | **291,116** | 53,698 | 24,519 | ~5.4× / ~12× |
| point-insert | **96,519** | 26,973 | 15,874 | ~3.6× / ~6.1× |
| point-update (PK) | **112,292** | 30,311 | 13,951 | ~3.7× / ~8× |

p50 latencies: mpedb select **2 µs**, insert **8 µs**; SQLite 15 µs / 29 µs;
PostgreSQL 35 µs / 51 µs.

### Lock-free reads under a concurrent writer (commit-class)

3 readers + 1 writer, durable disk config. mpedb's MVCC readers never block the
writer:

| engine | read ops/s | read p50 |
|---|--:|--:|
| mpedb | 344,064 | 2 µs |
| SQLite (WAL) | **345,120** | 1 µs |
| PostgreSQL | 25,792 | 47 µs |

**SQLite ties mpedb here now** (345k vs 344k — inside the noise band), where the
2026-07-14 10:40 run had mpedb ahead 300k vs 255k. Both engines keep readers off
the writer's lock; on 2 cores this cell measures the reader loop, and the reader
loops are equally tight. mpedb's structural advantage is multi-*process* readers
and shared plans, which this single-process cell does not exercise. PostgreSQL
pays the socket round-trip per read, which is the whole 13× gap.

### Durable single-client INSERT (durable-on-ack) — mpedb's weak spot, and the fix

One fsync per commit is a hardware floor, and mpedb's intent-ring group-commit
only engages **under contention** — so a lone durable writer is where mpedb has
the least room to be clever:

| config (durable-on-ack) | ops/s |
|---|--:|
| **mpedb `wal`, single client** | **2,598** |
| SQLite `synchronous=FULL`, single client | 1,601 |
| PostgreSQL `sc=on`, single client | 2,232 |
| **mpedb `wal`, batched 100/commit** | **104,877** |
| SQLite `FULL`, batched 100/commit | 76,156 |
| PostgreSQL `sc=on`, batched 100/commit | 19,463 |

`wal` now leads both single-client (2,598 vs 1,601 / 2,232) and batched
(105k vs 76k / 19k) — the logical-WAL + fsync-coalescing work closed the
single-client gap that earlier runs showed. The genuinely weak cell is a
different one: **`durability=commit` single-client (525 ops/s insert, 704
update)** — the slowest in the suite, because every commit msyncs the whole
meta double-buffer with no batching partner. Guidance stands: for durable
writes use `wal`, and batch in a `WriteSession` when you can.

## "Have we gotten slower/faster?" — how to read run-to-run deltas

Three full runs now (07-13, 07-14 10:40, 07-14 11:56) and the absolute numbers
swung wildly in **both** directions — 07-13→10:40 fell ~24%, 10:40→11:56 rose
15-84%. Neither was a code change. **On this shared 2-core VM, host load
dominates the absolutes.**

The method that settles it every time: **SQLite and PostgreSQL are the control
group.** Their binaries are byte-identical across our runs, so:

- all three engines move together → **host load**;
- mpedb moves and they do not → **code signal**.

Applied to 10:40 → 11:56, after the mirror/CDC work landed in the engine:

| | mpedb | SQLite | PostgreSQL |
|---|--:|--:|--:|
| median change across point/contended cells | +20% | +19% | +35% |

All three rose together ⇒ host, not code. And the ratios — the only thing
comparable *across* runs — held or improved: point-select vs SQLite 5.5×→5.9×,
point-insert 3.4×→3.6×, point-update 4.3×→4.4×, contended-writes 6.4×→6.7×.

**This also answers the question the CDC work raised:** M1 put a change-capture
hook in all six engine mutators. If it cost anything measurable, none-class
point-insert/update (pure write-path CPU) would have lagged SQLite's
environmental gain. They tracked it instead (+84%/+18% vs SQLite's +70%/+14%).
The hook is free when no table is mirrored.

Two standing lessons: **run one engine at a time** (`--only mpedb`) for a clean
absolute — full-run co-tenancy on 2 cores depressed insert to 54k where an
isolated run gave 76k — and **never read a delta without checking the controls**.

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
