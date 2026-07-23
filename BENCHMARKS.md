# mpedb benchmarks

Head-to-head throughput and latency for **mpedb vs SQLite vs PostgreSQL** — same
machine, same workloads, same measurement loop. This page is the curated
cross-machine comparison; each machine's full generated tables (every cell, both
durability classes, all latency percentiles) live in its own file.

## Machines measured

| machine | engines | full results |
|---|---|---|
| AMD EPYC-Milan, 2 cores, 7.6 GiB, Linux 6.8 | mpedb, SQLite, PostgreSQL 16, Turso 0.7 | [`RESULTS-linux-amd-epyc-milan-2c.md`](crates/mpedb-bench/RESULTS-linux-amd-epyc-milan-2c.md) |
| Apple M3 Pro, 11 cores, 36 GiB, macOS 26.6 | mpedb, SQLite, PostgreSQL 16, Turso 0.7 | [`RESULTS-macos-apple-m3-pro-11c.md`](crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md) |
| Raspberry Pi 3 B+, armv7l (32-bit), 921 MiB, Linux 6.1 | **mpedb only** | no results file — see below |

### Latest primary-cell re-measure (2026-07-21)

Linux **volume-backed** control group: `--tmpfs /dev/shm` + `--disk /mnt/xfs`
(xfs). **Primary none-class cells all win** vs SQLite and PostgreSQL (ops/s):

| cell | mpedb | SQLite | PostgreSQL |
|---|---:|---:|---:|
| point-insert | ~172k | ~42k | ~15k |
| point-select | ~444k | ~82k | ~21k |
| point-update | ~197k | ~48k | ~12k |
| contended-writes | ~140k | ~36k | ~37k |

Batched durable-on-ack (WriteSession 100/commit, `durability=wal`) also beats
both on the same volume. Attribution: existing MPEE-aligned path (content-hashed
`execute(hash)`, streaming LIMIT / `scan_rows_capped` per DESIGN-MPEE-OPT). Full
tables in the RESULTS file above.

**Graph workloads are measured separately**, against Neo4j 5.26 with an edge
table and recursive CTEs — no graph machinery at all:
[BENCHMARKS-GRAPH.md](BENCHMARKS-GRAPH.md). The crossover is at hop 3: the edge
table wins one and two probes out, native adjacency wins deeper traversals
(up to 13× on a global triangle sweep). Same per-probe execution cost the OLAP
page brackets against SQLite, measured from the other side.

**Analytics is measured separately**, against DuckDB with SQLite as the control
group: [BENCHMARKS-OLAP.md](BENCHMARKS-OLAP.md). Two findings there are mpedb's
own rather than "row store versus column store" — `count(*)` counted the PK tree
because the benchmark schema declared nothing NOT NULL — the narrow-tree path
existed and was tested all along, and engages once the schema says what the
data already was (3.0 → 1.5 ms, 2026-07-23), and MPEE's worst-case cost
model could not see a star schema's dimension filter (7.6–12× behind SQLite —
**fixed the same day** by the per-index NDV cost input, `2f4c7b7`: 5.9×
recovered, now 1.7–2.1×). The
extremum goes the other way: `min/max` over an index is 252× faster than DuckDB
and 7,675× faster than SQLite.

Turso (the Rust SQLite rewrite) joined the field 2026-07-17; its adapter's
honesty decisions and a compatibility-parity comparison live in
[design/TURSO.md](design/TURSO.md).

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
cargo run --release -p mpedb-bench -- --disk /mnt/ext4/scratch # pick the medium
cargo run --release -p mpedb-bench -- --h2h 8       # paired durable head-to-head
cargo run --release -p mpedb-bench -- --extents 800 # append+fdatasync by layout
mpedb bench --auto --durability none|commit|wal|async   # mpedb-only, quick
```

`--h2h` and `--extents` are **paired A/B instruments, not report cells**: they
walk their arms round-robin inside one loop and print ratios formed *inside* a
repetition, and they deliberately write nothing to `RESULTS-<machine>.md`.
`--disk` matters more than it looks: the default scratch sits next to the build
output, which on the dev box is the *system* disk — a different filesystem from
the one durable numbers usually want, and a durable number that does not say
which filesystem it was taken on is not a number.

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

*Re-run 2026-07-16 after #37/#39/#42: every cell reproduced within the noise
floor, so the table below stands as measured — deliberately not "freshened",
because swapping in a statistically identical run would only launder noise
into the history.*

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

*(2026-07-20: these absolutes are the 07-14 run on that day's medium and predate
#111, which halved `commit`'s flush count. The durable head-to-head has since
been re-measured with all four arms interleaved in one session, and the
`mpedb wal` row that was missing from the curated commit-class table added —
see ["The durable head-to-head,
re-measured"](#the-durable-head-to-head-re-measured-with-an-mpedb-wal-row-2026-07-20-122).
The direction of the guidance is unchanged.)*

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
| 1 | 302,284/s | 89,702/s | 3.4× |
| 2 | 186,479/s | 88,551/s | 2.1× |
| 4 | 250,992/s | 83,300/s | 3.0× |
| 8 | 270,822/s | 78,877/s | 3.4× |

(Medians of 5, both arms measured in the same session. The 2-writer cell was
160k before the writer-lock spin below; sqlite3 sags gently with more writers,
90k → 79k, while mpedb stays flat-ish.)

sqlite3 gets its best case here: WAL, and a 60 s `busy_timeout`. The timeout is
not a courtesy — without it every loser of a write race returns `SQLITE_BUSY` and
the run dies. **That asymmetry is itself the result**: mpedb's arm has no retry
path because there is nothing to retry.

#### The 2-process dip, explained and mostly fixed

The dip above (160k at two writers, between 299k at one and 274k at eight) was
real, reproducible and unexplained. It is **two writers ping-ponging on the
writer lock**: each release wakes the other, which finds the lock taken again and
sleeps. Measured, per insert:

| writers | rows/s | CPU µs/insert | **voluntary ctx-switches/insert** |
|--:|--:|--:|--:|
| 1 | 305,871 | 3.64 | ~0 |
| **2** | **164,730** | **8.30** | **0.28** |
| 4 | 260,948 | 4.60 | 0.053 |
| 8 | 275,562 | 4.11 | 0.018 |

Sleeps per insert *fall* as writers are added, which is the tell: with more
contenders, a writer that releases and immediately re-acquires wins the race and
gets a burst of work per wake-up. **Two is the worst case there is** — perfect
alternation, one sleep/wake per 3.5 inserts, and 2.3× the CPU per row.

Not group commit: the intent ring is gated on `durability = commit|wal`
(`ring_exec::ring_enabled`) and these cells run `none`, so the writers take the
lock directly. Not futex volume either — 0.012 futex calls per insert at two
writers, so the lock takes its uncontended fast path most of the time; it is the
*few* that sleep that cost, because each one is a full context switch.

The fix is a bounded `trylock` spin before blocking (`Shm::writer_lock`,
64 attempts). Measured, paired arms:

| | n | result | 95% CI | |
|---|--:|--:|---|---|
| dev box, 2 writers | 20 pairs | **+17.4%** | [+13.7, +21.1] | resolved gain |
| Pi, 1 writer (uncontended) | 15 pairs | +0.5% | [-0.2, +1.2] | **no cost** |

Voluntary context switches at two writers drop from 0.309 to 0.171 per insert.

Two things worth keeping from how this was measured. **The dip does not exist on
the Pi at all** (7,006 / 6,400 / 6,047 at 1/2/4 writers — a gentle decline), so
the magnitude is a property of that host's scheduler, not of the design; the
mechanism is real on both. And the uncontended arm first measured **-1.56%
[-2.64, -0.47]** at n=5 — a "resolved harm" that n=15 turned into +0.5% and no
effect. Five reps produce that kind of false positive; see "Measure your
instrument".

Read this table for its *shape*, not its absolutes. The dev box's run-to-run CV
is 9% — see "Measure your instrument".

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

## The durable head-to-head, re-measured with an `mpedb wal` row (2026-07-20, #122)

**The published durable cell compared mpedb's slowest durable mode against two
log-based engines' fast ones.** It ran mpedb at `durability = commit` — full
mapped-page `msync` plus meta `msync`, design/DESIGN.md §4.1's two-flush floor —
against SQLite-WAL and PostgreSQL `synchronous_commit=on`, both of which are
log-based and issue one flush. mpedb *has* a log-based mode. It was not in the
table.

It is now, **as an addition**: `commit` keeps its row, because "what does the
mapped-page barrier cost" is a real question about the default and deleting the
row would hide a real property of that mode. `wal` gets a row next to it,
because "how does mpedb's log compare to their logs" is the like-for-like
question and it was unanswerable from this page. The harness change is the same
shape — `mpedb-bench` now runs a fifth engine key, `mpedb-wal`, in the
commit-class matrix (none-class skips it: `wal` is a durable mode).

**Method.** `--h2h 8`: all four durable arms built once and then walked
round-robin, eight repetitions, so every arm is measured within seconds of
every other and each ratio is formed *inside* a repetition. **Two independent
sessions**, because this file's own rule is that a result that matters gets
reproduced in a second one. ext4 (`/dev/sdc`, `--disk /mnt/ext4/...`) — xfs was
occupied by another workload for the whole window, so **there is no xfs arm
here and the two filesystems are not mixed.** The box was NOT idle (another
agent's `cargo test` and a `powerloss` run on the other disk), which is why
every claim below is a paired ratio and the absolutes carry the spread.

| point-insert, 1 client | session A ops/s | session B | A p50 µs | B p50 µs | **barriers/commit** |
|---|--:|--:|--:|--:|--:|
| mpedb `commit` (mapped-page, §4.1 floor) | 157 [142-170] | 151 [141-173] | 5,486 | 5,576 | **2.26** |
| **mpedb `wal` (log, one flush)** | **350 [287-405]** | **371 [303-402]** | **1,875** | **1,854** | **1.11** |
| SQLite `FULL`+WAL (log) | 143 [125-168] | 150 [138-164] | 5,738 | 5,776 | **2.22** |
| PostgreSQL `sc=on` (log) | 468 [437-485] | 424 [325-465] | 1,814 | 1,938 | **1.10** |

| contended writes, 4 threads | session A ops/s | session B |
|---|--:|--:|
| mpedb `commit` | 341 [287-411] | 356 [305-405] |
| **mpedb `wal`** | **838 [756-967]** | **866 [679-931]** |
| SQLite `FULL`+WAL | 139 [126-160] | 154 [143-163] |
| PostgreSQL `sc=on` | 1,013 [995-1,182] | 999 [799-1,115] |

Medians of 8, `[min-max]` across the 8 repetitions. Paired ratios, formed per
repetition (session B, n=8):

| vs PostgreSQL, paired | ops/s | p50 |
|---|--:|--:|
| **mpedb `wal`** | **0.86× [0.76-1.08]** | **0.96× [0.79-1.04]** |
| mpedb `commit` | 0.36× [0.32-0.49] | 2.86× [2.28-3.00] |
| SQLite `FULL`+WAL | 0.36× [0.31-0.45] | 2.89× [2.51-3.17] |

**The like-for-like comparison is a tie, and the old table did not contain it.**
mpedb's log-based mode sits at 0.96× PostgreSQL's typical durable commit
latency and 0.86× its throughput, where the published cell showed mpedb losing
by 3.7×. Against the other embedded engine in its own log mode, `wal` is
**2.4-2.5× SQLite** single-client and **5.6× contended**. And `commit` really
is the slow mode: 2.2-2.9× the latency of the two log-based engines,
2.18× [1.85-2.56] / 2.45× [1.90-2.62] slower than mpedb's own `wal`
single-client and 2.5× slower contended (paired, per session).

### The barrier count, measured by the kernel rather than argued

The last column above is new and it is the load-bearing one. `/proc/diskstats`
has counted **device cache-flush requests** since Linux 5.5, so the harness
brackets each cell with them and reports flushes per committed insert. That
separates "this engine issues more barriers" from "this engine's barriers are
slower", which no engine-level timer can do:

| arm | barriers/commit | µs/barrier | p50 |
|---|--:|--:|--:|
| PostgreSQL `sc=on` | 1.10 | 132-141 | 1,814-1,938 |
| **mpedb `wal`** | **1.11** | 152-156 | **1,854-1,875** |
| SQLite `FULL`+WAL | 2.22 | 167-353 | 5,738-5,776 |
| mpedb `commit` | 2.26 | 156-164 | 5,486-5,576 |

Identical to two decimals in both sessions. Read it as: mpedb `wal` and
PostgreSQL are at the one-flush floor, mpedb `commit` is at §4.1's two-flush
floor (post-#111 — it was at 4.05), and **SQLite pays two barriers per durable
commit as well**, which was not known and is the subject of the 3b section
below.

### What this contradicts

⚠ **The 07-17 table below reports mpedb `commit` at 391 ops/s / p50 2,598 µs
and PostgreSQL at 1,457 / 649 µs. This section measures 151-157 and
424-468 on the same workload, and it is not a regression in anything.** Every
arm is ~3× slower here, PostgreSQL included; the earlier table was taken on
**xfs** on a differently loaded box, this one on **ext4** with three other
agents on the machine. That is the whole reason the rule exists: absolutes do
not cross a medium or a session, ratios inside one session do. Nothing in this
section should be read against those absolutes, and the two tables are kept
apart rather than merged.

Also superseded: the `mpedb commit` row of that table predates #111 (4.05
msyncs per commit, now 2.02), so it was stale in mpedb's *disfavour* — which is
the second reason re-measuring was required and not merely fairer.

## The one cell PostgreSQL wins: durable writes (2026-07-20, Linux, #111)

⚠ **This section's opening table is the 2026-07-17 run on xfs, kept as
measured.** It is superseded as a head-to-head by #122 above (which adds the
`mpedb wal` row and re-measures on ext4); its absolutes are not comparable to
that section's, and its `mpedb commit` row predates #111.

| commit-class, disk (2026-07-17, xfs) | mpedb `commit` | SQLite FULL+WAL | **PostgreSQL** |
|---|--:|--:|--:|
| point-insert, 1 client | 391 (p50 2598 µs) | 848 (1120 µs) | **1457 (649 µs)** |
| contended writes, 4 threads | 687 | 840 | **6370** |

Three hypotheses were put to it. **Two were wrong**, and writing down which is
the point of this section — each one was the obvious explanation.

### Hypothesis 1: "the intent ring does not amortise the flush across concurrent transactions" — REFUTED

`MPEDB_RING_STATS=1`, four writer *processes*, `durability = commit`, ext4,
796 committed batches:

| | measured |
|---|--:|
| **committed ops per batch (= per meta flip, per flush group)** | **2.82** |
| batch-size histogram (1/2/3/4 intents) | 97 / 188 / 274 / 237 |
| dirty pages per batch | 3.7 |
| **dirty RUNS per batch** | **2.84** |
| leader `exec_us` (prepare + execute the whole batch) | **67 µs** |
| leader `commit_us` (fixpoint + msyncs + flip) | **10,028 µs** |

Group commit works. At four writers the ring puts 2.82 independent transactions
behind one meta flip; batches of 3 and 4 are the mode. It is not ~1, so the
"mpedb has no group commit" story is simply false.

### Hypothesis 3: "the writer lock is held across the whole transaction, so shorten it" — REFUTED, with a number

The same two counters answer it: **the work is 0.7 % of the lock hold**
(67 µs of 10,095 µs). Everything else is the durability barrier. Moving 100 % of
the executable work off the critical section — which is what
`concurrency = "optimistic"` (#17, DESIGN-PHASE3) does — has a ceiling of
**+0.7 %** on the durable path, and it pays for that by bypassing the ring, i.e.
by giving up the 2.82× amortization above. That is the same conclusion
DESIGN-PHASE3 §5 reached by measurement (`commit` −82 %), now with the mechanism
in one ratio rather than an end-to-end number.

So the answer to "can an optimistic writer join the ring's group commit instead
of bypassing it?" is: it could, and it would be worth **at most 0.7 %** here,
because on durable media the critical section is not the work — it is the flush.
The composition is not the prize. The flush count is.

### Hypothesis 2: "msync of a mapped range is more expensive than append + fdatasync" — REFUTED as stated, but it led to the real bug

A 6-arm probe on this box, all arms interleaved **inside one loop** so host drift
cancels — 64 MiB mapping, 1,200 iterations, p50 µs. Reproduce:

```sh
cargo run --release -p mpedb --example sync_cost -- /path/on/disk 600
```

| arm | ext4 p50 | xfs p50 |
|---|--:|--:|
| A `msync(MS_SYNC)` of 2 meta pages | 1,847 | 2,480 |
| B `pwrite(200 B)` + `fdatasync` | 1,887 | 2,554 |
| C **8 scattered 1-page msyncs** + meta msync | **15,280** | **15,846** |
| D 1 msync of 8 *contiguous* pages + meta msync | 2,276 | 4,449 |
| E `pwrite(32 KiB)` + `fdatasync` | 2,181 | 3,019 |
| F 1 msync over the **span** of the 8 scattered + meta | 5,358 | 7,870 |

(Re-run on an idle box with the in-repo example: A 1,993 · B 1,963 · C 14,393 ·
D 2,405 · E 2,294 · F 5,287 — same shape, so the ratios are not a load artifact.)

**A ≈ B.** msync of a mapped range and pwrite+fdatasync of a small record cost
the same thing, because both are one device cache flush. `wal` is not cheaper
per flush — it is cheaper because it issues *one*.

**C is the finding.** Eight one-page msyncs cost **6.7×** one msync. On Linux
`msync(MS_SYNC)` IS `vfs_fsync_range`: every call ends in a jbd2/XFS-log commit
plus a `blkdev_issue_flush`. And that is exactly what the commit path did.

### The bug: `durability = commit` paid runs+1 device flushes, not 2

design/DESIGN.md §4.1 puts the floor at **two** flushes — data, then the meta that
references it. The code issued one `msync_range_nobarrier` **per contiguous run
of dirty COW pages**, then one `sync_barrier`. On macOS that is right and is what
#41 measured: msync is cheap there, `F_FULLFSYNC` is the flush, and one barrier
covers every preceding run — N cheap calls + 1 flush = 2 platter flushes. **On
Linux `sync_barrier` compiles away** (`shm.rs`: *"Linux: nothing to do"*), so
every run-msync was a full flush and the barrier added none. #41 fixed Apple and
left Linux at `runs + 1`.

With 2.84 runs per batch that is **3.84 flushes per commit group** where the
floor is 2. `strace -c -e msync`, single writer, both arms of one binary:

| arm | msync calls | commits | **per commit** |
|---|--:|--:|--:|
| per-run (old) | 1,271 | 314 | **4.05** |
| span (new) | 720 | 356 | **2.02** |

The fix is one msync over the `[min, max]` span of the dirty set (plus the
extent ranges), which makes exactly the same pages durable — writeback is driven
by the page cache's DIRTY tag, so a wider range walks *dirty pages*, not pages.
The §4.1 ordering is untouched: the span provably cannot reach the meta pages
(every id in `dirty` comes from `alloc_id`, i.e. is a data page above the reader
table), and the barrier stays exactly where it was.

**Is a wide span free?** That is the obvious objection — a commit touching page
10 and page 250,000 now msyncs a 1 GiB range. Measured directly (part 2 of
`examples/sync_cost`: 1 GiB mapping, 8 dirty pages spread evenly over the span,
arms interleaved in one loop, p50 µs):

| span | ext4 | xfs |
|--:|--:|--:|
| 4 MiB | 4,453 | 10,246 |
| 64 MiB | 2,940 | 5,524 |
| 512 MiB | 2,963 | 5,583 |
| **1,023 MiB** | **2,990** | **5,497** |

**256× wider costs nothing ON LINUX** — flat from 64 MiB to 1 GiB on both
filesystems. (The 4 MiB arm being the *slowest* is the tell that this is not a
range scan at all. Caveat on that tell: the part-2 arms run in fixed ascending
width order within an iteration, which is a plausible alternative explanation;
it does not affect the flat 64 MiB → 1 GiB result.) So on Linux the span carries
no hidden cost, which is what the change bets on.

⚠ **And that bet is Linux-only — this section was very nearly the same mistake
it diagnoses.** On Darwin `msync` is `vm_object_sync` over the range: it costs
range WIDTH, and the device flush is the separate `F_FULLFSYNC`. Measured on an
M3 Pro / APFS, same probe, msync only:

| span | 4 MiB | 64 MiB | 512 MiB | 1,023 MiB |
|---|--:|--:|--:|--:|
| Darwin | 312 µs | 493 µs | 1,544 µs | **2,403 µs** |

End-to-end, 300 durable autocommit inserts per arm, paired: the span is **+10 %
slower at ~300 MiB of live data and +63 % at ~1.2 GiB**, and it scales, because
`strace` shows the span is typically the WHOLE live data region on nearly every
commit — the allocator hands out the lowest reusable pages while the hot btree
leaf sits near the high-water mark. On macOS the per-run loop was ALREADY at
§4.1's two-flush floor, so the span is pure loss there.

**The data barrier is therefore `cfg`-gated: one span on Linux, per-run
elsewhere**, with `MPEDB_MSYNC_PER_RUN=1` / `MPEDB_MSYNC_SPAN=1` keeping both
arms measurable on both. Found by the commit-path review, not by the suite —
nothing in `cargo test` or the crash harness detects a platform-specific
slowdown.

**Paired arms, one binary, alternating, `examples/mp_writes` on ext4,
`durability = commit`. Two independent sessions**, because this file's own rule
is that a result that matters gets reproduced in a second session:

| | n | result | 95% CI | box |
|---|--:|--:|---|---|
| 1 writer process | 10 pairs | **+41.5 %** | [+32.6, +51.0] | loaded (3 other agents) |
| 4 writer processes | 10 pairs | **+55.7 %** | [+42.6, +70.0] | loaded |
| 1 writer process | 8 pairs | **+45.0 %** | [+23.5, +70.1] | **idle** |
| 4 writer processes | 8 pairs | **+63.3 %** | [+42.9, +86.6] | **idle** |

`MPEDB_MSYNC_PER_RUN=1` restores the old loop, so both arms are the same binary
— the mistake BENCHMARKS.md records under "the arms were the same binary" cuts
both ways, and one binary with a switch is the safe side of it.

⚠ **Absolutes in this section are on ext4 (`/dev/sdc`); the 07-17 tables were on
xfs (`/dev/sdb`), and `mp_writes` is not the bench harness's `point-insert`
cell.** Nothing here should be read against those tables as an absolute. Every
claim above is a paired ratio measured inside one session.

**And the same counters after the fix** (4 writers, `MPEDB_RING_STATS=1`, idle,
1,173 batches):

| | before | after |
|---|--:|--:|
| committed ops per batch | 2.82 | 2.68 |
| **msyncs per batch** | **3.84** | **2** |
| **commits per msync** | **0.73** | **1.34** |
| leader `commit_us` | 10,028 | 6,742 |
| work as a share of the writer-lock hold | 0.7 % | 1.2 % |

The batch size did not change — it is not what was broken — and the flush count
did. Note the last row: shortening the critical section is now worth **1.2 %**
instead of 0.7 %, which is the honest ceiling on "stream the work, keep the lock
small" for `durability = commit` on this hardware.

### The flush-count model, and what it says about PostgreSQL

Take PG's own single-client p50 (**649 µs**, which *includes* a unix-socket
round trip) as this box's one-flush unit, and the 07-17 latencies fall out:

| engine / mode | p50 | ÷ 649 µs | flushes per commit | agrees? |
|---|--:|--:|--:|---|
| PostgreSQL `sc=on` | 649 | 1.0 | 1 (`pwrite` + `fdatasync`) | ✅ |
| mpedb `wal` | 1,219 | 1.9 | 1 (`pwrite` + `fdatasync`) | ❌ **unexplained** |
| SQLite `FULL`+WAL | 1,120 | 1.7 | 1 | ❌ same shape |
| mpedb `commit` (before #111) | 2,598 | 4.0 | **4.05 (measured)** | ✅ |

The `commit` row closes exactly: 4.05 measured msyncs, 4.0 flush-units of
latency. That is the whole of mpedb's single-client durable deficit, and #111
takes it to 2.

The two ❌ rows were an open item — **mpedb `wal` and SQLite-WAL both sitting at
~1.8 flush-units while issuing one `fdatasync` each**. Both are now closed, and
the model was wrong in its third column rather than its second: the two engines
were not doing one barrier each. See the next section.

### Known issue 3b, settled: the flush-unit model was counting `fdatasync` calls, not barriers (2026-07-20, #122)

The recorded hypothesis was that PostgreSQL never changes a WAL segment's size
— segments are zero-filled once and then *recycled* — while mpedb grows its log
in 4 MiB `fallocate` chunks, so every `fdatasync` also journals an extent
conversion. **The mechanism is real and large. The attribution to mpedb was
wrong.**

`cargo run --release -p mpedb-bench -- --disk <dir> --extents 800`: five
log-file layouts, same 16 KiB record, all arms **interleaved inside one loop**,
with the kernel's flush counters read around each append. ext4, three sessions:

| layout | p50 µs (3 sessions) | × recycled | **barriers/append** |
|---|--:|--:|--:|
| `grow-sparse` — pwrite past EOF, i_size changes every append | 5,717 · 5,831 · 5,871 | 2.83 · 2.88 · 2.86 | **2.00** |
| `fallocate-unwritten` — 4 MiB chunks, appends into UNWRITTEN extents | 5,578 · 5,724 · 5,708 | 2.76 · 2.82 · 2.78 | **2.01** |
| **`fallocate-prezero` — 4 MiB chunks + written zeros (what mpedb does)** | **2,001 · 1,959 · 2,067** | **0.99 · 0.97 · 1.01** | **1.01** |
| `recycled` — whole file written and fsynced once, then appended into (PG) | 2,020 · 2,028 · 2,051 | 1.00 | **1.00** |
| `recycled-fsync` — identical, but `fsync` instead of `fdatasync` | — · — · 5,723 | 2.79 | **2.00** |

**The hypothesis's mechanism: confirmed, 2.8× and exactly one extra barrier.**
Appending into a file that has to change — its size, or an unwritten extent —
makes `fdatasync` commit a filesystem journal transaction, and on ext4 that
transaction is its own `blkdev_issue_flush`. Two barriers, 2.8× the latency, no
engine-level cause whatsoever. That is a trap worth having measured.

**The hypothesis as applied to mpedb: REFUTED.** `wal_ensure_alloc` already
writes zeros over each fresh 4 MiB chunk (`shm.rs: prezero`), and the probe puts
that layout at **1.01 barriers and 0.99-1.01× the recycled arm** — i.e.
byte-for-byte PostgreSQL's cost, amortized 1-in-256 growth included. There is
nothing to fix. The predicted ~1.8× win on the mode the docs recommend does not
exist, and the code comment in `wal_ensure_alloc` that claims 958 µs vs 350 µs
for the un-prezeroed case is exactly right — it is why there is no gap left.

**And the residual gap does not reproduce at all.** mpedb `wal`'s measured
barrier count is **1.11 per commit** and its p50 is **0.96× [0.79-1.04]**
PostgreSQL's, paired, n=8. The 1.9 flush-units in the table above was measured
on a different filesystem before #111; on ext4 the two engines' typical durable
commit is the same commit.

**What the model got wrong was SQLite**, and the fifth arm says why. SQLite's
WAL sync is `fsync`, not `fdatasync`: `os_unix.c`'s `full_fsync()` only takes
the `fdatasync` branch for a DATAONLY sync, and `wal.c` never requests one. An
`fsync` must persist inode metadata — mtime is enough — so on ext4 it commits a
journal transaction and buys a second barrier **even when nothing about the file
changed**: the `recycled-fsync` arm is byte-identical to `recycled` and costs
2.00 barriers and 2.79×. That is the whole of SQLite's 2.22 measured
barriers/commit, and it is not configurable short of leaving the durable class.
(The growth explanation does not fit SQLite: its `-wal` stops growing about 250
commits into a cell — measured — while the barrier count stays at 2.22 for the
whole cell.)

So the corrected model, all four rows now agreeing, all counts measured rather
than inferred:

| engine / mode | barriers per commit | why | p50 (ext4, #122) |
|---|--:|---|--:|
| PostgreSQL `sc=on` | **1.10** | `pwrite` + `fdatasync` into a recycled, fully-written segment | 1,814-1,938 |
| mpedb `wal` | **1.11** | `pwrite` + `fdatasync` into a pre-zeroed log | 1,854-1,875 |
| SQLite `FULL`+WAL | **2.22** | one `fsync` = data barrier + inode-journal barrier | 5,738-5,776 |
| mpedb `commit` | **2.26** | §4.1: data msync, then meta msync (was 4.05 before #111) | 5,486-5,576 |

Two notes on the excess over the whole number. Both one-flush engines measure
1.10, not 1.00: ~10 % of commits buy a second barrier, and for mpedb that is
the *main* file, which is `fallocate`d but deliberately not pre-zeroed (zeroing
an 800 GiB `size_mb` at create is not on) — so a commit that first touches a
fresh region pays the conversion the probe's second row measures. At 0.11 per
commit it is a 10 % effect, not a 1.8× one, and pre-zeroing the data file would
trade it against create-time cost. mpedb `commit`'s 2.26 has the same
explanation on top of its two msyncs.

**One residual, quantified and not closed:** mpedb `wal` matches PostgreSQL at
p50 but trails by ~14 % on throughput, i.e. on the *mean* — mean/p50 1.45-1.57
against PostgreSQL's 1.18-1.30. It is a tail, not the typical commit. Raising
`MPEDB_WAL_CKPT_BYTES` from its 16 MiB default to 4 GiB (a diagnostic, not a
recommendation — it trades recovery time and log space) moved mpedb `wal`'s
mean latency 2,935 → 2,393 µs and mean/p50 1.57 → 1.31 while the other three
arms did not move. n=3, so that is indicative and not resolved: the suspect is
the checkpoint's whole-mapping `msync`.

### What PostgreSQL actually does, and what mpedb has

Read from source (`postgres@d5751c33`, `src/backend/access/transam/`):

| PG mechanism | where | mpedb |
|---|---|---|
| **Flush-progress re-check before AND after the lock** — a committer whose LSN is already flushed returns with **zero I/O** | `XLogFlush`, `xlog.c:2820/2854/2886` | **Has an analog, differently shaped.** A ring waiter cannot be "already covered" — an intent must be *executed* by a leader, not merely logged — so the equivalent is the leader draining every READY intent into one txn. Measured 2.82 per flush at 4 writers. |
| **`LWLockAcquireOrWait`** — followers wait for the lock to *free* and never acquire it; woken as a batch | `lwlock.c:1378`, `xlog.c:2870` | **Has it.** Wait-or-lead: futex-wait 2 ms, then `trylock`-promote (§5.3). |
| **The leader writes/flushes everything inserted so far**, not just its own LSN | `xlog.c:2922`, `flexible=false` | **Has it.** `collect_ready()` drains the whole slot table. |
| **Work happens outside the flush lock** — WAL space reserved by atomic bump, record bytes copied into shared buffers with no global lock | `XLogInsert` / `XLogCtlInsert` | **Lacks it** — and it is worth ≤0.7 % here (see Hypothesis 3). PG needs it because its backends do real per-transaction work; mpedb's batch execution is 67 µs. |
| **One `pwrite` + one `fdatasync` per flush**, into a preallocated, zero-filled, recycled segment; **no mmap/msync anywhere in the WAL durability path** (verified: zero `msync` in `xlog.c`) | `xlog.c:2461`, `xlog.c:3300` | **`wal` has it** (`wal_commit`: one `pwrite`, one `fdatasync`, pre-zeroed log). **`commit` does not** — it is an msync-of-a-mapping design, which is why its floor is 2 flushes and `wal`'s is 1. |
| **`commit_delay` / `commit_siblings`** — the leader *sleeps inside* WALWriteLock to manufacture followers | `xlog.c:2903`, default **0** (off) | **Lacks it, and should not add it.** PG's own docs say the free "gangway effect" already does most of it at zero delay; mpedb's ring self-clocks the same way (a slower flush queues more intents). Do not add a sleep before the free mechanism is measured — mpedb's free mechanism measures 2.82. |
| **Explicit leader/follower CAS queue** (CLOG, not WAL) with the completion flag cleared before the wake | `TransactionGroupUpdateXidStatus`, `clog.c:526/578/651` | **Has it, for WAL-equivalent work** — the intent ring *is* this shape, and PG deliberately did not build it for WAL because an fsync is long enough that followers pile up on the lock by themselves (`9b38d46d9f`). |

The mapping is short: **mpedb already has PG's group-commit machinery.** What it
did not have was PG's flush *count*.

### Durability evidence for #111

The change alters *how many syscalls* make a page set durable, not *which* pages
or *in what order*. Stated so a reviewer can attack it:

- **Same pages.** `[min, max]` over `dirty ∪ extent_dirty` is a superset of every
  run the old loop msynced, and `msync(MS_SYNC)` writes back every dirty page in
  the range. Nothing that was flushed before is skipped now.
- **Same ordering.** The data barrier still precedes the meta publish; the
  `sync_barrier()` call is byte-for-byte where it was. §4.1's "data durable
  BEFORE the meta that references it" is unchanged.
- **The span cannot reach the meta.** Every id in `dirty` comes from
  `alloc_id()`, which draws from the freelist or `high_water` — always a data
  page, above the reader table, which is above the lock page, which is above
  meta A/B. (`msync_range_nobarrier` rounds the base down to the OS sync
  granularity — 4 KiB on Linux, 16 KiB on Apple — which cannot span the gap
  either. Unchanged from before, and the pre-flip meta bytes are the *previous*
  committed metas, already durable.)
- **What the span may sweep in that the run loop did not:** COW pages of an
  ABORTED transaction. Those are unreferenced garbage; writing them is wasted
  I/O at worst, never a correctness issue. Nothing else can be dirty — the
  writer lock is exclusive and readers never dirty pages.
- **Empirical:** `mpedb crash --waves 6 --children 6 --durability commit` —
  **36 SIGKILLs, 6/6 EOWNERDEAD recoveries, `verify=ok` and `index-probe=ok`
  every wave, all invariants held.** Plus `cargo test --release` on mpedb-core,
  mpedb, mpedb-types, mpedb-sql: **112 suites, 0 failures**; clippy
  `-D warnings` clean.
- **Not covered:** `mpedb powerloss` only models the WAL torn tail
  (`--durability wal|async`), so there is no power-loss simulator for `commit`
  mode to run. The argument for `commit` is the ordering argument above plus the
  SIGKILL harness, and that gap in the harness is worth recording.

### The classes that must not have moved, and did not

The diff is one hunk inside `match durability { Durability::Commit => … }`, so
`none`, `wal`, `async` and every read path are untouched by construction. Shown
anyway, same paired method, idle box:

| class | span | per-run | ratio |
|---|--:|--:|--:|
| **none-class**, 4 writer processes | 178,890/s | 179,888/s | **0.994×** (range 0.975-1.009, n=4) |

Reads were not re-measured because no read path calls `commit_inner`; the
none-class row is the control that says the shared write path did not move
either.

**Memory, 4 concurrent writer processes, commit-class** (`mp_writes` reports
the parent's peak): RssAnon 456 KiB (span) vs 392 KiB (per-run) at 1,037 vs 803
rows written — i.e. proportional to work done, not a footprint change. `wal`
costs more (908 KiB, 6.5 MB VmHWM) because it buffers a page-image record per
commit. Nothing here regresses the "~300 KB per writer process" figure above.

### Guidance, unchanged in direction and sharpened in degree

For durable writes use **`durability = wal`**. Measured post-#111, idle, paired,
with `examples/mp_writes` (multi-PROCESS writers):

| durable mode, ext4 | 1 writer | 4 writers |
|---|--:|--:|
| `commit` (2 flushes/group) | 154/s | 364/s |
| **`wal` (1 flush/group)** | **425/s** | **763/s** |
| **wal / commit** | **2.80×** [2.38, 3.29] | **2.01×** [1.52, 2.65] |

Before #111 that ratio was ~3.5× at both. So the fix closes about 40 % of the
gap between mpedb's two durable modes, and the rest is the §4.1 two-flush floor
that `commit` cannot escape.

⚠ The #122 re-measurement gets **2.18× [1.85-2.56]** and **2.45× [1.90-2.62]**
single-client and **2.51× / 2.43×** at four writers — same direction, and the
four-writer figure disagrees with the 2.01× above. The two are not the same
experiment (`mp_writes` runs writer *processes*, the head-to-head runs threads
in one process, on a different filesystem and a busier box), so neither
supersedes the other; the honest summary is that `wal` is **~2-2.5× `commit`**
on durable media and the guidance does not depend on which end of that it is.

**Recommendation for the benchmark itself — DONE (#122).** The commit-class row
ran mpedb at `durability = commit`, its *slowest* durable mode, against
SQLite's WAL and PostgreSQL's WAL: a fair reading of a default and an unfair
reading of the engine, because both other engines were measured in their
log-based durable mode and mpedb was not. An `mpedb wal` row has been **added**
— not substituted, so nothing about the default's cost is hidden — to both the
generated matrix (`mpedb-bench` engine key `mpedb-wal`) and the curated table;
see "The durable head-to-head, re-measured" above for the numbers, which
change the conclusion of that cell.

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

⚠ **These cells are 4 KiB values, and they measure a COLD database.** Two things
follow, and both were measured (2026-07-16, `examples/blob_paths`,
`examples/blob_warm`, this box):

**1. Cold, because each cell seeds a fresh file** — so every overflow page is
touched for the first time, and a `MAP_SHARED` page must be faulted in before it
can be written even when the write overwrites every byte of it. `write(2)` owes
no such fault, which is most of why the raw baseline leads:

```text
  64 MiB memcpy into a cold mapping       819 MiB/s   <- what these cells see
  64 MiB memcpy, pages already faulted 13,780 MiB/s   <- 17x, same copy
  in-engine, 16 MiB blob, round 0         244 MiB/s   <- pays the faults
  in-engine, same blob, rounds 2-5      913-981 MiB/s <- steady state, 4x
```

A long-lived process recycling pages through the freelist pays the fault once per
page, not per blob. (Pre-zeroing the file does NOT help — it is 2.6x worse:
`fallocate`'s unwritten extents tell the kernel "this is zeros" so a fault can
zero-fill for free, and writing real zeros forces a 4 KiB read off the platter
instead. `shm.rs` gets this right.)

**2. 4 KiB is too small to show #42.** These cells did not move when the row
buffer was removed (647.9 MiB/s here vs ~650 before), and that is correct rather
than disappointing: `encode_row`'s buffer is only expensive when it is BIG. At
4 KiB the malloc is trivial; at 16 MiB it faults its own anonymous pages and cost
42% of the insert. Removing it took a 16 MiB blob from **660 to 1170 MiB/s
(+77%)** and left 4 KiB rows unchanged (+1.47% [-1.20, +4.14], n=8). What limits
THIS cell is the per-ROW engine cost (#32), not copies.

**#40 CLOSED (2026-07-16, second pass).** The remaining warm gap was the blob
being deep-cloned TWICE more on its way in — `resolve_params`' fast path did
`to_vec()` (2.49 ms of a 12.1 ms 16 MiB insert) and `build_insert_row` cloned
each param again (~2.3 ms), plus the deallocation of both 16 MiB vectors. Both
paths now BORROW (`Cow`) when nothing needs computing — which is almost every
statement — and the leakstat ledger finally closes: warm 16 MiB execute went
**12.1 → ~2.2 ms** (~7,300 MiB/s on the execute path; ~4,900 MiB/s wall
including the caller's own `params!` copy, which `Value::Blob(vec)` avoids).
What remains is structural, not copies: COLD writes pay MAP_SHARED page faults
(17× cold/warm, this section), and `copy_file_range` beats the cold path by
62% on ext4 by not faulting the destination at all — that is #50's territory
(extent blobs + reflink), not a facade fix.

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

0. ~~**macOS `durability=commit` flushes the platter once per contiguous dirty
   run.**~~ **Fixed 2026-07-16 (#41).** `msync_range` barriered with `F_FULLFSYNC`
   per call, and the commit path called it once per CONTIGUOUS RUN of dirty
   pages — so a scattered commit (a btree path plus a freelist path) paid 4-6
   platter flushes. `F_FULLFSYNC` is per-fd, so the data runs now msync without a
   barrier (`msync_range_nobarrier`) and one `sync_barrier()` covers them all:
   N+1 → 2. Measured on an M3 Pro, scattered commit-class (stress mixed, 4
   writers, paired): **+6.7% [+3.26, +10.08], n=6, resolved** — modest because at
   4 writers group commit already amortizes the meta flush across a batch, and
   the win is per-commit.

   ⚠ **This fix was Apple-only, and nobody noticed for four days (#111,
   2026-07-20).** On Linux `sync_barrier` compiles away and `msync(MS_SYNC)` *is*
   `vfs_fsync_range`, so taking the barrier out of the per-run loop removed
   nothing — every run-msync was still its own device flush and Linux went on
   paying `runs + 1`. Measured: 4.05 msyncs per durable commit. The fix is one
   msync over the dirty SPAN; see "The one cell PostgreSQL wins" above. **The
   general lesson: a per-platform durability fix must be measured on every
   platform, because "N+1 → 2" was true on the platform it was written for and
   false on the reference one.**

   ⚠ The two flushes CANNOT be merged into one, and an earlier draft of this
   entry wrongly proposed exactly that. The data barrier makes the data durable
   BEFORE the meta that references it (design/DESIGN.md §4.1); a single barrier over both
   would let a power loss land meta on the platter and not its data, leaving
   meta_T checksum-valid and pointing at COW pages that were never written. Two is
   the floor. `wal` gets one because its record is a single self-describing
   checksummed object with no ordering to preserve. Verified crash-safe on APFS:
   30 SIGKILLs under durability=commit, all invariants held.

1. ~~**UNBOUNDED HIGH-WATER GROWTH under sustained concurrent churn.**~~ **Fixed
   2026-07-15.** A 1000-key table holding ~30 KB of live rows used to fill any
   file given four or more concurrent writers doing insert/update/delete — 64 MB
   died at 10 s, 128 MB at 20 s, 256 MB at 40 s, so it was linear growth, not
   sizing. Now: **8 writers, 64 MB, 60 s, 4.9M ops, `verify: ok`**, where that
   scaling law would have demanded ~384 MB.

   `refill_reusable` used to delete the freelist entry it drew pages from, which
   made every drawn page a page the commit fixpoint had to write back — coupling
   the fixpoint's own page appetite to the pool it held. It cannot refill (it is
   mutating the tree refill reads), so it minted a high-water page whenever the
   pool ran dry: ~1 per 43 commits, forever, and pool size could not help because
   refill handed over one entry no matter how many existed. Refill is now
   read-only: draw the pages, leave the entry, let the fixpoint strike out only
   what got consumed. Cost: a measured **-7.05% [-8.71, -5.40], n=20 pairs** on
   the write path, which is a good trade for an unbounded leak. Six wrong answers
   — including two "obvious" fixes that made it 2.4× worse — are recorded in
   [`crates/mpedb-core/tests/high_water_leak.rs`](crates/mpedb-core/tests/high_water_leak.rs).

2. ~~**`newest_meta` stale-gate race (durability=commit).**~~ **Already fixed —
   this entry described code that does not exist.** The race is real: a reader
   that loads the `durable_txn` gate and is then descheduled while two durable
   commits land finds both checksum-valid slots newer than its stale gate and
   gets a spurious `Corrupt("no valid meta page")`. The window is wide on
   purpose — the commit path writes the meta slot, then `msync`s it
   (milliseconds on real disk), and only then advances the gate.

   But `shm::newest_meta` already reloads the monotone gate and retries, which
   is exactly the fix this entry asked for. Verified by experiment rather than by
   reading it (2026-07-15, same `--only mpedb` flags both arms): with that retry
   loop disabled the benchmark reports **3** spurious reader retries within
   seconds; as shipped, **0**. The `mpedb-bench` adapter's own bounded retry
   stays, re-purposed as the **tripwire** — if its counter is ever non-zero,
   `newest_meta`'s retry has regressed and the fix belongs there, not in the
   adapter.

3. ~~**`durability=commit` single-client floor.**~~ **Halved 2026-07-20 (#111);
   what remains is structural.** The entry used to say a lone durable writer
   "pays one serialized msync per commit". It paid **four** (measured: 4.05),
   because the data barrier was issued once per contiguous dirty run and on
   Linux each of those is a device flush. One msync over the span took it to
   **2.02** — the §4.1 floor — worth **+41.5 % [+32.6, +51.0]** at one writer and
   **+55.7 % [+42.6, +70.0]** at four, n=10 paired each.

   What is left is not a bug: `commit` needs *two* flushes (data, then the meta
   that references it) and `wal` needs *one* (a single self-describing
   checksummed record), so `wal` remains ~2× `commit` on durable media and the
   guidance to use it stands. A genuine single-writer fast path for `commit`
   would have to break the §4.1 ordering, which is not available.

3b. ~~**The `wal` per-flush gap.**~~ **Closed 2026-07-20 (#122) — the
   hypothesis was refuted and the gap was an artifact of counting.** The entry
   said mpedb `wal` and SQLite-WAL sit at ~1.8 flush-units while issuing one
   `fdatasync` each, and proposed that mpedb's 4 MiB `fallocate` growth (vs
   PostgreSQL's recycled, never-resized segments) made every `fdatasync`
   journal an extent conversion. Measured, with the kernel's own device-flush
   counters:

   - **The mechanism is real**: appending into a growing file or an unwritten
     extent costs **2.00 barriers and 2.8×** one into a fully-written one.
   - **mpedb is already on the right side of it**: `wal_ensure_alloc`
     pre-zeros, and that layout measures **1.01 barriers, 0.99-1.01× the
     recycled arm**. Nothing to fix; the ~1.8× win does not exist.
   - **Neither engine was doing one flush.** SQLite's WAL sync is `fsync`, not
     `fdatasync` (`os_unix.c` takes the `fdatasync` branch only for a DATAONLY
     sync), so it commits an inode-journal transaction and pays **2.22
     barriers per commit** — measured, and reproduced by an `fsync`-only
     control arm on a file where nothing changed.
   - **mpedb `wal` vs PostgreSQL is a tie**: 1.11 vs 1.10 barriers per commit,
     p50 **0.96× [0.79-1.04]** paired, n=8, two sessions.

   What is left is ~14 % of *mean* latency (mpedb `wal` mean/p50 1.45-1.57 vs
   PostgreSQL's 1.18-1.30) — a tail, with the WAL checkpoint's whole-mapping
   `msync` as the indicative (n=3) suspect. See "Known issue 3b, settled" above.
4. **~1 µs of fixed per-row cost in the write path.** The bulk write is not
   limited by copies — see "Where the bulk write actually spends its time". With
   the SQL layer removed entirely, ~1 µs/row remains: btree descent, COW page
   allocation, freelist, and the dirty-page set. The concrete next step is
   replacing that `HashSet<u64>` with a **bitset** — page ids are dense and
   bounded by `high_water`, so a shift and a mask replace the hash on every
   platform. The `contains` itself cannot go: it is the COW guard, and catching
   a violation of it in production is the point (design/DESIGN.md §3).
5. **CDC capture check on the write path (minor).** With change-capture in the
   engine (mirror foundation), each write txn does one `cdc\0tabs` sys-lookup even
   when no mirror is configured. It is *not* the cause of the run-to-run variance
   above (reads, which skip it, moved identically), but caching the config across
   txns keyed by the meta generation would shave a small constant off every
   autocommit write.

## Platforms

These numbers are **Linux (x86-64)**. macOS/Apple Silicon perf has not been
re-measured in this run; platform *correctness and crash-safety* parity (not
throughput) is covered separately — see [Platforms](README.md#platforms) and
[`design/DESIGN-MACOS-LOCK.md`](design/DESIGN-MACOS-LOCK.md).

**32-bit ARM** (Raspberry Pi 3 B+, armv7l) is a correctness platform here, not a
throughput one — it is ~11× slower than the dev box and cannot run the other two
engines. What it is good for: it is the only weakly-ordered machine anything runs
on, so the fences in the reader-pin protocol are load-bearing there rather than
theoretical; and it is the steadiest A/B instrument in the set. 318 tests and a
multi-process SIGKILL harness pass on it — see
[Platforms](README.md#platforms).
