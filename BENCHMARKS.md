# mpedb benchmarks

Head-to-head throughput and latency for **mpedb vs SQLite vs PostgreSQL** — same
machine, same workloads, same measurement loop. This page is the curated
cross-machine comparison; each machine's full generated tables (every cell, both
durability classes, all latency percentiles) live in its own file.

## Machines measured

| machine | engines | full results |
|---|---|---|
| AMD EPYC-Milan, 2 cores, 7.6 GiB, Linux 6.8 | mpedb, SQLite, PostgreSQL 16 | [`RESULTS-linux-amd-epyc-milan-2c.md`](crates/mpedb-bench/RESULTS-linux-amd-epyc-milan-2c.md) |
| Apple M3 Pro, 11 cores, 36 GiB, macOS 26.6 | mpedb, SQLite, PostgreSQL 16 | [`RESULTS-macos-apple-m3-pro-11c.md`](crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md) |
| Raspberry Pi 3 B+, armv7l (32-bit), 921 MiB, Linux 6.1 | **mpedb only** | no results file — see below |

The Pi is not a third data point in the engine comparison and never will be:
`mpedb-bench` links SQLite (bundled C) and PostgreSQL, which needs a C
cross-compiler to reach ARM, and a PG cluster next to 921 MiB and zero swap
would OOM the box. It earns its place for two other things — it is the only
**32-bit / weakly-ordered** platform anything runs on (see
[Platforms](README.md#platforms)), and it is the **steadiest A/B instrument**
here despite being the slowest (1.6% CV vs the dev box's 9.0%).

**One file per machine, on purpose.** A generated report says "on this machine"
in its own first line — it is a single-host document, and a run on a second host
used to overwrite the first host's numbers rather than add its own. The filename
is now derived from the machine (`RESULTS-<os>-<cpu>-<cores>c.md`), so two
machines cannot collide by accident.

## How to prove a change made mpedb faster (the method, and how it fails)

`% of raw` is the column, not MiB/s. The raw `std::fs` baseline is measured in
the SAME run on the SAME medium, so it absorbs host drift; the engines' absolutes
do not. A worked example of getting this wrong, from this repo:

A change removed a redundant 4 KiB memset from the blob write path. Two `--only
mpedb` runs said 614.9 → 687.9 MiB/s and it was written up as "+11%, three runs
each, ranges do not overlap". **That was not evidence.** `--only mpedb` omits
SQLite and PostgreSQL — the control group — and the raw baseline itself had moved
+2.5% between those two runs. A later run of the *same* binary came back 639.8:
a 7.5% spread on identical code, most of the effect being claimed.

Done properly — alternating arms in one session, each run normalised by its own
raw baseline:

| arm | % of raw (3 runs) | mean |
|---|---|---|
| memset present | 22.80 · 21.65 · 22.07 | 22.17% |
| memset removed | 24.85 · 23.94 · 24.02 | **24.27%** |

Non-overlapping, so the +9.5% is real. The claim survived; the first attempt at
proving it did not, and a plausible mechanism plus two favourable runs is exactly
what a wrong result looks like.

### Measure your instrument before you measure the change

Two more ways this has gone wrong here, both worth stealing:

**The arms were the same binary.** An A/B of a hasher change came back
"+2.0 / -2.0 / +0.6%" — sane-looking noise. The two binaries were byte-identical:
`git stash` had taken the example program with it, the build failed, and `cp`
copied the same file twice. `md5sum` the arms before you believe an A/B. Noise
is what a correct null result looks like, which is exactly why it is not evidence
of one.

**Three reps cannot resolve 2%.** The same change was then measured properly —
alternating arms, real binaries — and reported as "-1.6%, a regression". Also
wrong, because nobody had asked what this box's noise floor *is*. Ten runs of an
identical workload:

| machine | mean | sd | **CV** | range |
|---|---|---|---|---|
| dev box (x86-64, 4 cores, in use) | 332 MiB/s | 30 | **9.0%** | 255–351 |
| Raspberry Pi 3 B+ (armv7, decoding ADS-B throughout) | 30 MiB/s | 0.47 | **1.6%** | 29–31 |

**The slow, busy Pi is a 6× better instrument than the fast dev box.** Steady
load beats fast-but-bursty: the Pi's three background services run at a constant
rate, while a dev box alternates between idle and a `cargo build`.

Redone as a paired design — arms back to back, enough reps to put a confidence
interval on the *difference* — the same change came out:

| platform | paired diff | 95% CI | verdict |
|---|---|---|---|
| armv7, n=15 pairs | **+3.5%** | [+2.1, +4.9] | resolved, real |
| x86-64, n=25 pairs | -0.1% | [-2.2, +1.9] | **no measurable effect** |

So it was taken, not rejected: free on the reference platform, real on a
supported one. Three reps at CV 9% had produced the opposite conclusion and a
commit message to go with it.

**The rule:** decide *whether a change helps* on the steadiest machine you have,
with paired arms and a CI on the difference. Use the fast machines for absolute
numbers. Those are different jobs and the same box is rarely good at both.

### "Can I just run it 50-100 times and average the noise out?"

Largely yes — and the caveat is the interesting part. The same change, same box,
same binaries:

| | n | result | 95% CI | |
|---|--:|--:|---|---|
| dev box | 25 pairs | -0.12% | [-2.17, +1.94] | unresolved |
| dev box | **50 pairs** | **+2.14%** | [+0.65, +3.62] | resolved |
| Pi | 15 pairs | +3.51% | [+2.13, +4.89] | resolved |

50 alternating pairs resolved it on the 9%-CV box — while that run was competing
with *another* benchmark on the same machine, which is about as adversarial as
a VPS gets. So: yes, reps work, because the standard error falls as 1/√n and the
pairing cancels drift *within* a pair.

But look at the two dev-box rows. **-0.12% and +2.14%, same box, same binaries,
CIs that barely overlap.** The confidence interval assumes independent draws from
one stable distribution; a host whose state changes between sessions violates
that, and the CI will not tell you. The Pi got a tighter answer from 15 pairs
than the dev box got from 50.

Practical version: **~50 alternating pairs on a noisy VPS ≈ ~15 on a steady box**,
and the VPS still risks a between-session shift the CI cannot see. If a result
matters, reproduce it in a second session before believing it.

## The two rules that make any of this meaningful

1. **Run on an idle machine.** Not advice — measured. A stray process holding one
   of this box's two cores did not just add noise, it *compressed* the
   parallelism results (contended-writes 6.8× → 2.4×) and made mpedb look
   closer to the others than it is. Every number taken before 2026-07-14 12:10
   was measured on half a machine. Close everything, then run.
   *Refinement (2026-07-15):* for **A/B work** what matters is not idle but
   **steady** — a Pi running a constant background load has a 1.6% CV where this
   box has 9.0%. Idle still wins for absolute numbers. See "Measure your
   instrument" above.
2. **Compare within a durability class, never across.** "none-class" gives no
   fsync guarantee; "commit-class" is durable on ack. A none-class number next
   to a commit-class one is not a comparison, it is a category error.

Reproduce:

```sh
cargo run --release -p mpedb-bench          # full run → RESULTS-<machine>.md
cargo run --release -p mpedb-bench -- --out RESULTS-mybox.md   # explicit name
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
| point-select (PK) | **493,853** | 80,458 | 22,408 | ~6.1× / ~22× |
| point-insert | **166,759** | 42,353 | 14,092 | ~3.9× / ~12× |
| point-update (PK) | **206,608** | 47,592 | 11,610 | ~4.3× / ~18× |

p50 latencies: mpedb select **1 µs**, insert **5 µs**; SQLite 11 µs / 20 µs;
PostgreSQL 43 µs / 67 µs.

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

### Concurrent writes from real PROCESSES (2026-07-15)

The `contended-writes` cell above is `std::thread::scope` — four *threads* in one
process. It measures lock contention, and it is not the claim mpedb makes.
"Many processes writing one file, no server" needs many processes, so
`examples/mp_writes.rs` forks them. Both arms native Rust, one file, none-class,
median of 3 alternating runs:

| writer processes | mpedb | sqlite3 | ratio |
|--:|--:|--:|--:|
| 1 | 298,874/s | 90,195/s | 3.3× |
| 2 | 160,098/s | 86,171/s | 1.9× |
| 4 | 253,733/s | 80,126/s | 3.2× |
| 8 | 274,023/s | 81,363/s | 3.4× |

sqlite3 gets its best case here: WAL, and a 60 s `busy_timeout`. The timeout is
not a courtesy — without it every loser of a write race returns `SQLITE_BUSY` and
the run dies. **That asymmetry is itself the result**: mpedb's arm has no retry
path because there is nothing to retry. The 2-process dip is real and
reproducible; it is not explained yet.

Read this table for its *shape*, not its absolutes: sqlite3 sags gently with more
writers (90k → 81k) while mpedb stays flat-ish. The dev box's run-to-run CV is
9% — see "Measure your instrument".

**Peak memory, 4 concurrent writers, summed across the processes:**

| | RssAnon (heap) | VmHWM (all resident pages) |
|---|--:|--:|
| mpedb | **1.2 MB** | 196 MB |
| sqlite3 | 4.4 MB | 16 MB |

Two columns because one would lie. **RssAnon is the comparable one** — what the
engine actually allocated — and mpedb uses **3.7× less**, ~300 KB per writer
process. **VmHWM goes the other way and it is an accounting artifact**: mpedb
mmaps the database, so every page it touches is resident and charged to it, while
sqlite3's identical data sits in the OS page cache charged to nobody. Quoting the
VmHWM difference as "mpedb uses 12× the memory" would be a benchmark lying with
true numbers.

**And on hardware that has no room for a server:** a Raspberry Pi 3 B+ (armv7,
921 MB, decoding ADS-B throughout) does 7,006 / 6,400 / 6,047 writes/s at 1 / 2 /
4 processes, on **72 KB of heap**. Two orders of magnitude slower than the dev
box, which is the point of including it — the interesting number is not the
throughput, it is that the whole engine is 72 KB of anonymous memory and no
daemon. PostgreSQL runs on a Pi 3 too; what it cannot do is cost nothing while
idle.

## Apple Silicon (M3 Pro, 11 cores) — and the durability trap it exposed

Second machine, 2026-07-14: **M3 Pro, 5 perf + 6 eff cores, 36 GB, macOS 26.6**,
mpedb and SQLite only (no PostgreSQL installed). macOS has no tmpfs, so
none-class runs on a 2 GB RAM disk (`--tmpfs /Volumes/…`) — APFS over RAM, not
tmpfs, so **none-class ratios are not directly comparable to the Linux run**
(mpedb is mmap-based and pays a filesystem layer SQLite's read/write path does
not).

| none-class, ops/s | mpedb | SQLite | PostgreSQL |
|---|--:|--:|--:|
| point-select | **1,679,538** | 317,810 | 40,399 |
| point-insert | **203,842** | 111,994 | 34,457 |
| read-while-write (reads) | **3,704,543** | ~180 | 112,137 |
| bulk write MiB/s (% of raw) | **2,274.3 (38%)** | 1,163.3 (19%) | 107.2 (2%) |

Four runs of this machine exist. Every cell moves ≤4% with ratios stable, so
±4% is the noise floor here — which is exactly how one apparent 5.7% bulk-write
regression was dismissed: the sequence was 2270.5 → 2141.1 → 2274.3 on a binary
that never changed between the last two. A real code signal does not un-regress.

That last row is not a typo. SQLite's none-class journal serializes readers
against the writer, and on 11 cores the writer starves them completely:
**180 reads/s, p99 ~150 seconds.** mpedb's MVCC readers are untouched at 3.7M/s. It is a pathological config rather than a fair fight — but
it is the failure mode mpedb's design exists to avoid, and more cores make it
worse, not better.

Note the write ratios **narrow** vs Linux (1.7× vs 3.97× on insert). Some of
that is the APFS-over-RAM medium; we did not isolate how much.

### The durability trap (why this machine was worth running)

Every engine here was caught pretending a write was durable on Apple. All three.
Each in a different way, each invisible on Linux, each in the same direction.

On macOS, `fsync()` does not flush the drive's write cache; only
`fcntl(F_FULLFSYNC)` does. Two consequences we had both gotten wrong:

1. **SQLite** `synchronous=FULL` alone is not power-loss durable — its
   `unixSync` only issues `F_FULLFSYNC` when `PRAGMA fullfsync` is on, and that
   defaults to **off**. The harness never set it.
2. **mpedb** `durability=commit` was not power-loss durable either — the earlier
   macOS port routed `os::fdatasync` (the WAL path) through `F_FULLFSYNC`, but
   the commit path's barrier is `msync(MS_SYNC)`, which on macOS hands pages to
   the filesystem and stops there.

3. **PostgreSQL** — found last, because until 2026-07-14 it was never *measurable*
   on a Mac at all: three files hardcoded Debian's `/usr/lib/postgresql/16/bin`,
   so Homebrew's build was invisible by construction and the cells failed
   honestly with "initdb not found". The moment it ran, it posted **23,555
   durable inserts/s at p50 38 µs** — impossible for a device flush.
   `wal_sync_method` defaults to `open_datasync`; only `fsync_writethrough`
   issues `F_FULLFSYNC`.

Single-client durable INSERT, before and after making each engine honest:

| | ops/s | p50 | really durable? |
|---|--:|--:|---|
| SQLite FULL (harness default) | 26,642 | 25 µs | ❌ `PRAGMA fullfsync` defaults OFF |
| PostgreSQL `fsync=on` (default) | 23,555 | 38 µs | ❌ `wal_sync_method=open_datasync` |
| mpedb `commit` (before fix) | 7,583 | 127 µs | ❌ `msync` stopped at the filesystem |
| **PostgreSQL + `fsync_writethrough`** | **429** | 2,125 µs | ✅ |
| **mpedb `wal`** | **318** | 3,015 µs | ✅ |
| **SQLite FULL + `fullfsync`** | **310** | 3,067 µs | ✅ |
| mpedb `commit` (after fix) | 142 | 6,997 µs | ✅ but pays two flushes — see Known issues |

**The durable-write result is that there is no result.** Three engines, three
independent implementations, agreeing within 40% — because what is being measured
stopped being the engine and became the ~3 ms an Apple SSD takes to flush. Nobody
beats it. Every apparent "win" here — SQLite's 86×, PostgreSQL's 91×, mpedb's
24× — was an engine skipping the barrier, and each one made the honest engines
look slow. A benchmark that lets one engine skip the flush is not measuring the
other two, it is slandering them. All three fixes are committed.

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
| raw `std::fs` (baseline) | 2,632 | — | 9,282 | — |
| SQLite | **998** | **38%** | **2,261** | **24%** |
| mpedb | 602 | 23% | 940 | 10% |
| PostgreSQL | 41 | 2% | 285 | 3% |

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

### Where the bulk write actually spends its time (2026-07-15)

mpedb sits at ~26% of raw `std::fs` at 4 KiB values. **The cost is per ROW, and
it is not the copies** — which is what the improvement list used to say it was.
Measured with `cargo run --release -p mpedb --example bulk_only`, which does only
the blob write, so a trace attributes to it:

**It is not I/O.** `strace -c` over the whole write at `durability=none`: 14
`write`, 5 `getpid`, and nothing else of substance. Every microsecond is
user-space, which retires the entire "it's the msync/write pattern" family of
explanations at a stroke.

**It is per-row.** The same 128 MiB at different value sizes:

| value | rows | MiB/s | µs/row |
|---|---|---|---|
| 64 B | 2,097,152 | 50 | **1.22** |
| 256 B | 524,288 | 133 | **1.84** |
| 4 KiB | 32,768 | 349 | 11.2 |
| 64 KiB | 2,048 | 726 | 86.1 |

64 B and 256 B cost nearly the same *per row* despite 4× the bytes: there is a
fixed ~1 µs per row the payload never touches. Copies would have shown a flat
MiB/s; this climbs 14×. It also explains the shape of the whole suite — 54% of
raw at 64 KiB, 26% at 4 KiB, because the fixed cost amortises.

**And it is the engine, not SQL.** A `raw` arm bypassing the SQL layer for the
typed row API leaves that ~1 µs standing (62 vs 48 MiB/s at 64 B). The SQL layer
costs 23% at 64 B and 10% at 4 KiB — real, and not the story.

**A CPU profile** (Raspberry Pi, `perf record` — the only box here with a working
profiler) puts the µs in musl's memcpy (~24%), malloc/free (~14%) and
`DefaultHasher` (~15%). That last one is now fxhash: +3.5% on armv7, nothing
measurable on x86-64.

Five hypotheses about this gap have died on contact with measurement: an
overflow-page cliff (~1%), macOS msync granularity (`F_FULLFSYNC` is per-fd),
the API-forced clone (~2%), `dirty.insert` as a *1%* item (it is ~15% of the
armv7 profile — but fixing it returns 3.5%, not 15%), and a hasher swap that
was briefly "rejected" on noise. The module docs in
`crates/mpedb-bench/src/bulk.rs` keep the corpses so nobody re-runs them.

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
3. **~1 µs of fixed per-row cost in the write path.** The bulk write is not
   limited by copies — see "Where the bulk write actually spends its time". With
   the SQL layer removed entirely, ~1 µs/row remains: btree descent, COW page
   allocation, freelist, and the dirty-page set. The concrete next step is
   replacing that `HashSet<u64>` with a **bitset** — page ids are dense and
   bounded by `high_water`, so a shift and a mask replace the hash on every
   platform. The `contains` itself cannot go: it is the COW guard, and catching
   a violation of it in production is the point (DESIGN.md §3).
4. **CDC capture check on the write path (minor).** With change-capture in the
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

**32-bit ARM** (Raspberry Pi 3 B+, armv7l) is a correctness platform here, not a
throughput one — it is ~11× slower than the dev box and cannot run the other two
engines. What it is good for: it is the only weakly-ordered machine anything runs
on, so the fences in the reader-pin protocol are load-bearing there rather than
theoretical; and it is the steadiest A/B instrument in the set. 318 tests and a
multi-process SIGKILL harness pass on it — see
[Platforms](README.md#platforms).
