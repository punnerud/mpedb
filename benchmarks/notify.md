# Change notification: mpedb vs PostgreSQL LISTEN/NOTIFY

Measured 2026-07-25 with `mpedb-bench --notify`, on both machines, isolated.
The shape is DBOS's ([postgres-listen-notify-scalability](https://www.dbos.dev/blog/postgres-listen-notify-scalability)),
because their measurement is the reason this cell exists.

**Their finding.** Committing a transaction that calls `NOTIFY` takes a global
exclusive lock, held across the commit fsync, so notifications are delivered in
commit order. They measured 2.9K writes/s that way, and 60K (20×) once
notifications were batched off the commit path.

**Verified at source level** in [design/PG-NOTIFY-ANATOMY.md](design/PG-NOTIFY-ANATOMY.md),
which is worth reading before trusting anything below. Two things it corrects:

- The lock is `LockSharedObject(DatabaseRelationId, InvalidOid, 0, AccessExclusiveLock)`
  — a heavyweight lock on "database 0", i.e. **cluster-global, not per database**,
  and it spans the queue insert, the fsync, *and* `SignalBackends()`.
- **Modern PostgreSQL does filter by channel.** `master` indexes listeners in a
  dshash keyed `(database, channel)`. The claim "it can't know who cares" is
  out of date. What remains true is sharper: its *filtering* granularity
  (channel) and its *serialization* granularity (cluster) are decoupled, and
  `SignalBackends` still walks **every** listener in the cluster under an
  exclusive `NotifyQueueLock` for the direct-advance optimization.

The protocol itself is specified in
[design/DESIGN-NOTIFY.md](../design/DESIGN-NOTIFY.md).

**The question this cell answers.** mpedb has no queue, so it has no ordering to
protect and nothing to serialize: the writer lock already orders commits per
table, and a listener reads what changed from its own MVCC snapshot instead of
from a queued payload. Does that actually show up in a measurement?

## Method

Four arms, each run against both engines, `ROWS = 3000`:

| Arm | Shape |
|---|---|
| **A** | one row per transaction, notify per commit |
| **B** | 100 rows per transaction, one notify at the end (DBOS's workaround) |
| **C** | **4 concurrent writers**, one row per transaction, notify per commit |
| **D** | one writer, **1 / 10 / 100 listeners** — the fan-out axis |
| *ctl* | arms A–C have a control that does the identical work with **no notification at all** |

The controls are the point. Without them, an arm-A number cannot be attributed
— it could be the notify lock, or it could be plain fsync — and the whole
thesis turns on which.

**Fairness (#122).** PostgreSQL runs `fsync=on, synchronous_commit=on`, so
mpedb runs its log-based `wal` mode, not `none`. One `commit`-mode row (whole-page
publish, mpedb's *slowest* durable setting) is kept and labelled rather than
dropped for being unflattering. That correction had to be made once already in
this benchmark, and I walked into it again on the first run of this cell.

## Results

One row per arm, one column group per engine, so a comparison is read across a
row rather than by counting rows in two separate blocks. The control sits
directly above the arm it controls, because that pair is the only thing that
makes an arm's number mean anything.

**Bold marks the better engine in each pair** — higher throughput, lower
latency — but only when they differ by **more than 5 %**. Anything closer is
left unmarked, because highlighting a tie as a win is the easiest way to make a
table lie. (That rule is why the 4-writer controls, 866 vs 863 and 785 vs 661,
are treated differently: the first is a tie, the second is not.)

### Linux (dev box, /mnt/xfs — **2 cores**)

| Arm | writes/s mpedb | writes/s pg | notify cost mpedb | notify cost pg | p50 mpedb | p50 pg | p99 mpedb | p99 pg |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| A control — no notification | **395** | 341 | — | — |  |  |  |  |
| A per-commit | **392** | 314 | **none** | 8 % | **36 µs** | 2397 µs | **1004 µs** | 7068 µs |
| B control — no notification | **18371** | 5780 | — | — |  |  |  |  |
| B batched (100/txn) | **18753** | 5693 | **none** | 2 % | **39 µs** | 3140 µs | **93 µs** | 16136 µs |
| C control — 4 writers, no notification | 866 | 863 | — | — |  |  |  |  |
| C concurrent — 4 writers | **894** | 347 | **none** | **60 %** |  |  |  |  |
| D — 1 listener | **383** | 303 |  |  |  |  |  |  |
| D — 10 listeners | **369** | 268 |  |  |  |  |  |  |
| D — 100 listeners | **285** | 217 |  |  |  |  |  |  |

### macOS (M3, /tmp — **11 cores**)

| Arm | writes/s mpedb | writes/s pg | notify cost mpedb | notify cost pg | p50 mpedb | p50 pg | p99 mpedb | p99 pg |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| A control — no notification | **315** | 292 | — | — |  |  |  |  |
| A per-commit | **317** | 284 | **none** | 3 % | **147 µs** | 3313 µs | **304 µs** | 3920 µs |
| B control — no notification | **20472** | 11346 | — | — |  |  |  |  |
| B batched (100/txn) | **22776** | 10229 | **none** | 10 % | **143 µs** | 3168 µs | **289 µs** | 3994 µs |
| C control — 4 writers, no notification | **785** | 661 | — | — |  |  |  |  |
| C concurrent — 4 writers | **799** | 329 | **none** | **50 %** |  |  |  |  |
| D — 1 listener | **320** | 289 |  |  |  |  |  |  |
| D — 10 listeners | **316** | 280 |  |  |  |  |  |  |
| D — 100 listeners | **333** | 211 | **flat** | **−27 %** |  |  |  |  |

**mpedb-only row, with no counterpart to sit beside.** `durability = commit` —
whole-page publish, mpedb's *slowest* durable mode — run through arm A so the
unflattering number is on the record rather than omitted: **38 writes/s** at
46 µs p50 on Linux, **143 writes/s** at 154 µs p50 on the M3. PostgreSQL has no
equivalent mode, so it is a footnote and not a column.

## What the numbers say

**1. A global lock needs contention to be visible, and the single-writer arms
do not have it.** With one writer, PostgreSQL's `NOTIFY` costs 3–8 %. That is
worth stating plainly because the obvious mistake is to quote DBOS's 20× and
imply a single-writer cell reproduces it. It does not. Their workload had
concurrent streams; arms C and D are what add them.

**2. With four writers, PostgreSQL's notify costs half its throughput** — 863 →
347 on Linux (60 %), 661 → 329 on the M3 (50 %). Same commits, same durability,
the only difference being whether they notify. That is the database-0 lock,
reproduced: four writers, one lock, held across each other's fsyncs.

**3. mpedb's notification costs nothing, at either concurrency.** 866 → 894 on
Linux and 785 → 799 on the M3 straddle zero, which is what a counter bumped
under a lock the writer already holds should look like. There is no arm where
mpedb has to choose between notifying and performing, which is the choice DBOS
had to engineer around.

**4. Fan-out (arm D) separates the engines — but only on a machine with cores
to spare, and that caveat is the finding's own footnote.** On the 11-core M3,
mpedb is **flat** across 1 / 10 / 100 listeners (320 → 316 → 333) while
PostgreSQL falls 27 % (289 → 280 → 211). That is `SignalBackends`' walk over
every listener in the cluster, under an exclusive `NotifyQueueLock`, against
one `futex_wake_all` that costs the same whatever is parked on the word.

On the **2-core** Linux box both engines fall about the same (mpedb 383 → 285,
PostgreSQL 303 → 217). That measurement says nothing about either design: 100
listener threads on 2 cores is scheduler oversubscription, and it hits both.
I predicted a flat mpedb line there and did not get one; the M3 is what shows
the prediction was right about the mechanism and wrong about the hardware.

Delivery was exact on both engines and both machines — 1.00 wakeups per
listener per commit, no amplification.

**5. Latency differs by more than an order of magnitude**: 36 µs vs 2397 µs on
Linux (67×), 147 µs vs 3313 µs on the M3 (23×). No queue, no server round trip
— a futex wake on shared memory, and the listener reads the table itself.

**6. The macOS floor is real and is a platform fact.** 147 µs against Linux's
36 µs is `futex_wake_all` being a documented no-op off Linux, so the listener
polls at ~200 µs granularity. It is still 23× under PostgreSQL, but it is not
the design's number — it is the platform's.

## Arm E — acting on a notification, measured

Arms A–D measure what a *notification* costs. This measures what **acting on
one** costs, which is the half that decides whether the feature is usable. One
action is: read a row, think for 20 ms, then write that row and an audit row in
a second table. **Real processes on both engines** — the bench binary
re-invokes itself as a worker.

The think time is the point. It stands for whatever a handler does between
deciding and committing, and it is exactly the window a lock is held across, or
not. PostgreSQL takes `SELECT … FOR UPDATE` before thinking, which is the
ordinary correct way to write it there. mpedb holds nothing and guards at
commit (`begin_guarded_with`, declaring the statements **with their parameter
values** — bare SQL names every row of each table and would measure the
pre-#143 engine; see [documents.md](documents.md) for why that stopped being a
detail). Re-measured after that change: the numbers below did not move, because
this arm's read and its write name the same key either way.

Two sub-arms: **E-shard** gives each worker a disjoint slice of users, so
nothing can collide; **E-hot** puts every worker on four users, so everything
does. Run `mpedb-bench --notify-load`.

Two profiles are published. **local** is engine-to-engine with no network
modelled; **networked** adds a 1 ms client round trip per statement **to
PostgreSQL only**, five read/write pairs per action, and a 40 ms think time.
That asymmetry is a deliberate model and needs saying plainly rather than
burying: mpedb is embedded and has no hop to pay, PostgreSQL in any real
deployment pays one per statement, and those round trips sit *inside* the
transaction where they extend how long its row lock is held. The RTT is a knob
(`MPEDB_E_RTT_MS`) so a reader who disagrees with 1 ms can pick another.

### actions/s — local profile (20 ms think, **1** read/write pair)

| workers | Linux *unguarded (ctl)* | Linux guarded | Linux pg | M3 *unguarded (ctl)* | M3 guarded | M3 pg |
|---:|---:|---:|---:|---:|---:|---:|
| 2 | 81 | 69 | **78** | 63 | 61 | 61 |
| 4 | 164 | 120 | **155** | 120 | 95 | **116** |
| 8 | 304 | 183 | **296** | 235 | 136 | **226** |

### actions/s — networked profile (40 ms think, **5** pairs, 1 ms RTT to pg)

| workers | Linux *unguarded (ctl)* | Linux guarded | Linux pg | M3 *unguarded (ctl)* | M3 guarded | M3 pg |
|---:|---:|---:|---:|---:|---:|---:|
| 2 | 44 | 26 | **29** | 40 | 23 | **26** |
| 4 | 87 | 36 | **57** | 76 | 35 | **52** |
| 8 | 175 | 44 | **115** | 148 | 40 | **93** |

### What this says

**#143 works, and it works only for small actions.**

With **one** read/write pair the guarded column went from a flat 42, 42, 42 to
69, 120, 183 on Linux and 61, 95, 136 on the M3 — from "does not scale at all"
to within reach of PostgreSQL. Recording *which keys* a commit landed on,
rather than only which tables, is what did that.

With **five** pairs it collapses: 26, 36, 44 against an unguarded control of
44, 87, 175. The arithmetic is not subtle. The ring entry has 8 bytes, so the
region set is a 64-bit Bloom filter with one bit per key. An action touching
six keys sets six bits; two such actions share a bit with probability roughly
6 × 6 / 64 ≈ **56 %**. The filter stops filtering exactly when actions become
realistic, and the retry counts show it — 785 refusals for 320 actions at 8
workers.

**So this is a width problem, not an algorithm problem.** The mechanism is
right and is now proven by the single-pair column; 64 bits is simply too narrow
to summarise a multi-statement action. Widening it is a shared-memory format
change and therefore a deliberate decision, not a tweak.

**Under real contention, waiting still beats retrying.** PostgreSQL leads E-hot
on both machines and both profiles. A waiter is handed the row and does its
work once; a retry throws away everything it already did — here a whole think
time — and starts over. Optimistic concurrency is the wrong tool when conflicts
are common *and* the work between read and commit is expensive, which is
precisely the case a lease (#142 G2) exists for.

**Where the guard is unambiguously right:** two workers, local profile, both
engines at the think-time floor with a handful of retries. Nothing waits,
nothing is held across the work, and a killed worker costs nothing.

**Note on the networked profile.** PostgreSQL's p50 rises to 67–80 ms there
because it pays 15 modelled round trips inside each transaction. It still wins
on throughput, which is the honest result: the network cost we modelled in its
favour is real, and it is not what decides this arm.

## What this does not measure

- **One machine, one process tree.** No network, so PostgreSQL pays no client
  round trip that a real deployment would. Its latency column would be worse in
  production, not better; mpedb's model has no network to add.
- **Four writers is a small contention arm.** The trend direction is clear at
  1 vs 4; where PostgreSQL's curve goes at 16 or 64 is unmeasured.
- **Arm D is fan-out, not slot collision.** Every listener watches the written
  table, so all 100 wakeups per commit are legitimate. What it establishes is
  that a wakeup is free at 100× on adequate hardware — which is why #141 N4
  (a cost-model-chosen table→slot assignment, to remove *false* wakeups) closes
  as "measured, not worth it": you cannot save anything by removing a fraction
  of a cost that is already zero.
- **No payload delivery.** mpedb notifications carry "table T moved to
  generation G", not the row. A consumer that needs the data reads it from its
  own snapshot. Comparing that to a queue that ships bytes is comparing two
  contracts, and the contracts are different on purpose.
- **Single runs, not medians of repeated trials.** The per-arm numbers move a
  few percent between runs; the 50–60 % contention result and the ~20–70×
  latency gap are far outside that, the single-writer 3–8 % readings are not.
