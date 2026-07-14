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
  Absolute numbers vary ~20-25% run-to-run by host load; treat them as *relative*
  and read the ratios, not the digits.

## Headline results (2026-07-14, Linux, single-client unless noted)

### Embedded point operations, none-class — mpedb's home turf

Zero-parse execute-by-hash + no IPC + a COW B+tree in the same address space:

| op (none-class) | mpedb ops/s | SQLite ops/s | PostgreSQL ops/s | mpedb vs SQLite / PG |
|---|--:|--:|--:|---|
| point-select (PK) | **229,969** | 38,636 | 20,284 | ~6× / ~11× |
| point-insert | **54,499** | 19,197 | 13,074 | ~2.8× / ~4.2× |
| point-update (PK) | **91,416** | 25,363 | 10,983 | ~3.6× / ~8× |

p50 latencies: mpedb select **2 µs**, insert **11 µs**; SQLite 15 µs / 29 µs;
PostgreSQL 33 µs / 51 µs.

### Lock-free reads under a concurrent writer (commit-class)

3 readers + 1 writer, durable disk config. mpedb's MVCC readers never block the
writer:

| engine | read ops/s | read p50 |
|---|--:|--:|
| mpedb | **299,514** | 2 µs |
| SQLite (WAL) | 255,134 | 2 µs |
| PostgreSQL | 20,116 | 60 µs |

### Durable single-client INSERT (durable-on-ack) — mpedb's weak spot, and the fix

One fsync per commit is a hardware floor, and mpedb's intent-ring group-commit
only engages **under contention** — so a lone durable writer is the one place
mpedb trails:

| config (durable-on-ack) | ops/s |
|---|--:|
| mpedb `wal`, single client | 1,908 |
| SQLite `synchronous=FULL`, single client | 1,379 |
| PostgreSQL `sc=on`, single client | 1,878 |
| **mpedb `wal`, batched 100/commit** | **88,565** |
| SQLite `FULL`, batched 100/commit | 69,180 |
| PostgreSQL `sc=on`, batched 100/commit | 15,661 |

Guidance: for durable bulk writes, **batch in a `WriteSession` or use `wal`** —
mpedb's batched durable insert is the fastest of the three. `durability=commit`
single-client (445-548 ops/s) is the slowest cell in the suite; prefer `wal`.

## "Have we gotten slower?" — regression check vs 2026-07-13

A prior full run on 2026-07-13 measured higher across the board (e.g. mpedb
none-class point-select 310k vs 230k here). This is **host variance, not a code
regression**, and the evidence is decisive:

- **Read-only point-select dropped the same ~24% as insert.** `SELECT` never
  touches the write path — if new code had slowed writes, insert would drop more
  than select. They dropped *equally*, which fingerprints a slower host, not a
  code change.
- **The unchanged SQLite and PostgreSQL binaries dropped a similar fraction** in
  the same run. Their code did not change between 07-13 and 07-14.
- **Co-tenancy inflated it further:** running mpedb + SQLite + PostgreSQL cells
  back-to-back on 2 cores depressed the full-run insert to 54k; an isolated
  `--only mpedb` run recovered it to 76k. Lesson: **run one engine at a time** for
  a clean number.

Net: the competitive standing is unchanged — mpedb still dominates embedded
none-class point ops and lock-free concurrent reads, and still trails on
single-client durable insert (by design, until batched).

## Known issues / improvement opportunities

1. **`newest_meta` stale-gate race (durability=commit).** A reader that loads the
   `durable_txn` gate, then is descheduled while two durable commits land, gets a
   spurious `Corrupt("no valid meta page")` — both meta slots are newer than its
   stale gate. The DB is *not* corrupt; a re-read succeeds. The bench adapter
   retries (bounded) and counts it. **Fix:** reload the monotone gate and retry in
   `mpedb-core::shm::newest_meta`. This is the top genuine bug the benchmark
   surfaces.
2. **Single-client durable-insert floor.** Group-commit engages only under
   contention, so a lone durable writer pays one serialized msync/fdatasync per
   commit. Documented workaround (batch / `wal`); a single-writer fast path could
   close the gap to SQLite/PostgreSQL.
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
