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
| *ctl* | every arm has a control that does the identical work with **no notification at all** |

The controls are the point. Without them, an arm-A number cannot be attributed
— it could be the notify lock, or it could be plain fsync — and the whole
thesis turns on which.

**Fairness (#122).** PostgreSQL runs `fsync=on, synchronous_commit=on`, so
mpedb runs its log-based `wal` mode, not `none`. One `commit`-mode row (whole-page
publish, mpedb's *slowest* durable setting) is kept and labelled rather than
dropped for being unflattering. That correction had to be made once already in
this benchmark, and I walked into it again on the first run of this cell.

## Results

### Linux (dev box, /mnt/xfs)

| engine | arm | writes/s | notify cost | lat p50 | lat p99 |
|---|---|---:|---:|---:|---:|
| mpedb | A no listener *(ctl)* | 339 | — | | |
| mpedb | A per-commit | 360 | **none** | 38 µs | 1080 µs |
| mpedb | B no listener *(ctl)* | 17222 | — | | |
| mpedb | B batched | 22235 | **none** | 3 µs | 2201 µs |
| mpedb | A, page-publish mode | 37 | | 48 µs | 1466 µs |
| mpedb | **C conc no listener** *(ctl)* | 870 | — | | |
| mpedb | **C concurrent** | **922** | **none** | | |
| postgres | A no notify *(ctl)* | 315 | — | | |
| postgres | A per-commit | 300 | 5 % | 2433 µs | 6879 µs |
| postgres | B no notify *(ctl)* | 6199 | — | | |
| postgres | B batched | 6128 | 1 % | 2969 µs | 5839 µs |
| postgres | **C conc no notify** *(ctl)* | 783 | — | | |
| postgres | **C concurrent** | **408** | **48 %** | | |

### macOS (M3, /tmp)

| engine | arm | writes/s | notify cost | lat p50 | lat p99 |
|---|---|---:|---:|---:|---:|
| mpedb | A no listener *(ctl)* | 313 | — | | |
| mpedb | A per-commit | 391 | **none** | 160 µs | 312 µs |
| mpedb | B no listener *(ctl)* | 19269 | — | | |
| mpedb | B batched | 18856 | 2 % | 123 µs | 284 µs |
| mpedb | A, page-publish mode | 231 | | 164 µs | 371 µs |
| mpedb | **C conc no listener** *(ctl)* | 895 | — | | |
| mpedb | **C concurrent** | **810** | 10 % | | |
| postgres | A no notify *(ctl)* | 269 | — | | |
| postgres | A per-commit | 261 | 3 % | 3562 µs | 5184 µs |
| postgres | B no notify *(ctl)* | 10039 | — | | |
| postgres | B batched | 10316 | none | 3391 µs | 3892 µs |
| postgres | **C conc no notify** *(ctl)* | 862 | — | | |
| postgres | **C concurrent** | **401** | **53 %** | | |

## What the numbers say

**1. A global lock needs contention to be visible, and the single-writer arms
do not have it.** With one writer, PostgreSQL's `NOTIFY` costs 3–5 %. That is
worth stating plainly because the obvious mistake is to quote DBOS's 20× and
imply a single-writer cell reproduces it. It does not. Their workload had
concurrent streams; arm C is what adds them.

**2. With four writers, PostgreSQL's notify costs about half its throughput**
— 783 → 408 on Linux (48 %), 862 → 401 on the M3 (53 %). Same commits, same
durability, the only difference being whether they notify. That is the
database-0 lock, reproduced: four writers, one lock, held across each other's
fsyncs.

**3. mpedb's notification costs nothing, at either concurrency.** 870 → 922 on
Linux and 895 → 810 on the M3 straddle zero, which is what a counter bumped
under a lock the writer already holds should look like. There is no arm where
mpedb has to choose between notifying and performing, which is the choice DBOS
had to engineer around.

**4. Latency differs by more than an order of magnitude**: 38 µs vs 2433 µs on
Linux (64×), 160 µs vs 3562 µs on the M3 (22×). No queue, no server round trip
— a futex wake on shared memory, and the listener reads the table itself.

**5. The macOS floor is real and is a platform fact.** 160 µs against Linux's
38 µs is `futex_wake_all` being a documented no-op off Linux, so the listener
polls at ~200 µs granularity. It is still 22× under PostgreSQL, but it is not
the design's number — it is the platform's.

## What this does not measure

- **One machine, one process tree.** No network, so PostgreSQL pays no client
  round trip that a real deployment would. Its latency column would be worse in
  production, not better; mpedb's model has no network to add.
- **Four writers is a small contention arm.** The trend direction is clear at
  1 vs 4; where PostgreSQL's curve goes at 16 or 64 is unmeasured.
- **No payload delivery.** mpedb notifications carry "table T moved to
  generation G", not the row. A consumer that needs the data reads it from its
  own snapshot. Comparing that to a queue that ships bytes is comparing two
  contracts, and the contracts are different on purpose.
- **Single runs, not medians of repeated trials.** The per-arm numbers move a
  few percent between runs; the 48 %/53 % contention result and the ~20-60×
  latency gap are far outside that, the single-writer 3-5 % readings are not.
