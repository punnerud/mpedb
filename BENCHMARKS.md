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

## Apple Silicon (M3 Pro, 11 cores) — and the durability trap it exposed

Second machine, 2026-07-14: **M3 Pro, 5 perf + 6 eff cores, 36 GB, macOS 26.6**,
mpedb and SQLite only (no PostgreSQL installed). macOS has no tmpfs, so
none-class runs on a 2 GB RAM disk (`--tmpfs /Volumes/…`) — APFS over RAM, not
tmpfs, so **none-class ratios are not directly comparable to the Linux run**
(mpedb is mmap-based and pays a filesystem layer SQLite's read/write path does
not).

| none-class, ops/s | mpedb | SQLite | ratio |
|---|--:|--:|--:|
| point-select | **1,699,066** | 327,907 | 5.2× |
| point-insert | **196,931** | 117,072 | 1.7× |
| point-update | **246,983** | 140,530 | 1.8× |
| contended-writes (4 threads) | **132,992** | 101,975 | 1.3× |
| read-while-write (reads) | **3,655,502** | 178 | — |

That last row is not a typo. SQLite's none-class journal serializes readers
against the writer, and on 11 cores the writer (86,580 writes/s) starves them
completely: **178 reads/s, p99 139 seconds.** mpedb's MVCC readers are
untouched at 3.7M/s. It is a pathological config rather than a fair fight — but
it is the failure mode mpedb's design exists to avoid, and more cores make it
worse, not better.

Note the write ratios **narrow** vs Linux (1.7× vs 3.97× on insert). Some of
that is the APFS-over-RAM medium; we did not isolate how much.

### The durability trap (why this machine was worth running)

The M3 run exposed a bug in the benchmark **and** one in mpedb — both invisible
on Linux, both in the same direction: *pretending a write was durable.*

On macOS, `fsync()` does not flush the drive's write cache; only
`fcntl(F_FULLFSYNC)` does. Two consequences we had both gotten wrong:

1. **SQLite** `synchronous=FULL` alone is not power-loss durable — its
   `unixSync` only issues `F_FULLFSYNC` when `PRAGMA fullfsync` is on, and that
   defaults to **off**. The harness never set it.
2. **mpedb** `durability=commit` was not power-loss durable either — the earlier
   macOS port routed `os::fdatasync` (the WAL path) through `F_FULLFSYNC`, but
   the commit path's barrier is `msync(MS_SYNC)`, which on macOS hands pages to
   the filesystem and stops there.

Single-client durable INSERT, before and after making both engines honest:

| | ops/s | p50 | really durable? |
|---|--:|--:|---|
| SQLite FULL (harness default) | 26,642 | 25 µs | ❌ |
| mpedb `commit` (before fix) | 7,583 | 127 µs | ❌ |
| **SQLite FULL + `fullfsync`** | **286** | 3,815 µs | ✅ |
| **mpedb `wal`** | **293** | 3,091 µs | ✅ |
| **mpedb `commit` (after fix)** | **142** | 6,999 µs | ✅ (two flushes — see Known issues) |

**~290 ops/s is simply what an Apple SSD platter flush costs.** Everything above
it was an engine skipping the flush. Honest verdict on this machine: mpedb `wal`
and SQLite-with-fullfsync are **tied** (293 vs 286) at genuinely durable
single-client inserts; the 93× and 26× "wins" either engine appeared to have
were measurement artifacts. Both fixes are committed.

## Bulk MB/s — and the number that makes it mean something

`cargo run --release -p mpedb-bench -- --io` pushes a **blob payload** (256 MiB
logical, 4 KiB values, batched 256 rows/commit) through each engine and reports
MiB/s next to a **raw-Rust baseline**: the same bytes written to a plain file
with `std::fs` on the same medium under the same durability promise (the baseline
calls `mpedb_core::durability_barrier`, i.e. the engine's own barrier — plain
`fsync()` would flatter it ~10× on Apple). An engine's MiB/s alone mostly
measures the disk; **`% of raw` is the column that means something.**

| none-class (tmpfs) | write MiB/s | % of raw | scan MiB/s | % of raw |
|---|--:|--:|--:|--:|
| raw `std::fs` (baseline) | 2,603 | — | 7,722 | — |
| SQLite | **1,041** | **40%** | **2,274** | **29%** |
| mpedb | 598 | 23% | 1,012 | 13% |
| PostgreSQL | 41 | 2% | 292 | 4% |

| commit-class (disk) | write MiB/s | % of raw | scan MiB/s | % of raw |
|---|--:|--:|--:|--:|
| raw `std::fs` (baseline) | 869 | — | 8,086 | — |
| SQLite | **167** | **19%** | 2,377 | 29% |
| mpedb | 121 | 14% | **2,912** | **36%** |
| PostgreSQL | 33 | 4% | 336 | 4% |

**SQLite wins bulk blob writes — 1.7× mpedb.** That is the mirror image of the
point-op cells (where mpedb leads 4-6×), and it is the honest shape of the
design: mpedb is built for small keyed rows through a zero-parse plan, and a
4 KiB blob is the case it is worst at — the value exceeds the 4 KiB page, so it
takes an overflow chain, and every touched page is copied (COW) before the meta
flip. That is crash-safety being paid for in bandwidth. mpedb does take the
commit-class scan (36% vs 29%). PostgreSQL is 2-4% of raw in every bulk cell:
a socket round-trip per row is simply the wrong shape for bulk.

**Nobody is close to the medium.** The best engine uses 40% of the raw write
ceiling and 29% of the read. If you need bytes moved, a file is still the fastest
database.

### What the >100% cells taught us

The first version of this section reported SQLite writing at **103% of raw** and
mpedb scanning at **266%** — impossible numbers that were pure methodology bugs,
and worth recording because both are easy to ship without noticing:

1. **The baseline wrote one 4 KiB syscall per row.** That measures syscall
   overhead, not the medium — and every engine batches internally, so they "beat
   the raw file". Fixed: the baseline writes 1 MiB chunks, like any real writer.
2. **The baseline read was cache-dropped; the engine scans were warm.** The
   engines scan data they just wrote, out of the page cache, so `posix_fadvise
   (DONTNEED)` on the baseline rigged a cold-vs-warm race. Fixed: both sides warm.
   Neither scan column is a disk-read benchmark; both measure the software path.

A number above 100% of the hardware is a gift — it is the measurement telling you
it is broken. The subtler version of the same bug is the one that lands at 85%
and gets published.

### Not measured: write amplification

The obvious proxy — physical file bytes per logical byte — is meaningless for
mpedb, whose file is **preallocated to a fixed `size_mb` and never grows**: the
ratio would report our own provisioning choice. It printed a suspiciously exact
`4.00×` (the harness sizes the file at 4× the payload) before we cut it. Doing it
honestly needs per-process block-layer accounting (`/proc/self/io` `write_bytes`),
which is Linux-only and cannot see PostgreSQL's server-side writes at all. Left
out rather than shipped wrong.

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

0. **macOS `durability=commit` pays two platter flushes per commit.** The commit
   path msyncs the data range and then the meta page, and each `msync_range` now
   follows with `F_FULLFSYNC` (required — `msync(MS_SYNC)` does not flush the
   drive cache on macOS). But `F_FULLFSYNC` is per-*fd*, not per-range, so one
   barrier before the ack would cover both: measured 142 ops/s (~7 ms = 2×3.5 ms)
   where `wal` gets 293 ops/s with a single flush. Fixing it means moving the
   barrier out of `msync_range` and into the commit path — reviewed protocol code
   (DESIGN.md §5), so it needs the design-review treatment, not a quick edit.
   Linux is unaffected (its `msync(MS_SYNC)` already runs `vfs_fsync`).

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
