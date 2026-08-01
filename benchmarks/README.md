# Benchmarks

Every measured comparison in one place. They were accumulating at the repo
root and there are enough of them now that a reader could not tell which was
the main event.

These are speed comparisons standing on top of a compatibility surface that is
100 % differentially verified against sqlite — zero wrong answers, zero error
mismatches across sqlite's own 7.4-million-record corpus. The summary is in
[../README.md](../README.md#differential-testing-vs-sqlite3--postgresql); the
feature-by-feature status is [../COMPAT.md](../COMPAT.md).

| Cell | Against | What it measures |
|---|---|---|
| [head-to-head.md](head-to-head.md) | sqlite3, PostgreSQL, Turso | The main campaign: inserts, selects, joins, blobs, durability classes, on Linux + M3 |
| [olap.md](olap.md) | DuckDB, sqlite3, PostgreSQL, MySQL | Star schemas, scans, aggregation, the NDV cost input |
| [vector.md](vector.md) | Qdrant | Filtered kNN and abandonment |
| [graph.md](graph.md) | Neo4j | Traversal at a converged frontier |
| [routing.md](routing.md) | the original MPEE solver | Exact sequencing against the generic solver |
| [notify.md](notify.md) | PostgreSQL LISTEN/NOTIFY | Change notification: throughput, latency, contention, fan-out — and arm E, **acting** on a notification, where PostgreSQL currently wins |
| [documents.md](documents.md) | PostgreSQL `SELECT … FOR UPDATE` | Arm F: many editors on ONE document, and another document with its own editors — with the control that attributes the scaling, plus the C1 calibration behind the 1 s feedback contract |
| [sync.md](sync.md) | — (mpedb against itself) | Arm G: many mpedb clients, each with its OWN `.mpedb`, reconciling with one authority — general sync, one contended cell, a replica offline for 10 000 edits, and the control that says the role itself is free |
| [minisqlite.md](minisqlite.md) | minisqlite, sqlite3, PostgreSQL | Two SQL engines built the same way, and the number nobody publishes |
| [turso.md](turso.md) | Turso | Side document for the fourth engine in the head-to-head |

Related but deliberately NOT here: [../LANDSCAPE.md](../LANDSCAPE.md) is a
positioning survey of what other engines do *better*, not a measurement, and
[../design/PG-NOTIFY-ANATOMY.md](../design/PG-NOTIFY-ANATOMY.md) is a source
reading rather than a run.

## The method these all share

Three rules, learned the hard way and applied everywhere:

**Controls, not just arms.** Every measurement that claims a feature costs
something runs the identical work with the feature off. Without that control an
arm's number cannot be attributed at all — in the notify cell it could have been
the notify lock or plain fsync, and the entire thesis turned on which.

**Like-for-like durability (#122).** mpedb is never compared in a weaker
durability mode than its opponent. Against PostgreSQL with `fsync=on`, mpedb
runs its log-based `wal` mode, not `none`. Where the slower `commit` mode's
number is unflattering it is kept and labelled rather than dropped. This rule
exists because the head-to-head got it wrong once and had to be corrected.

**Publish the hardware when the hardware is the answer.** The notify cell's
fan-out arm reads as a 27 % drop on a 2-core box and as flat on an 11-core one,
because 100 listener threads on 2 cores measures the scheduler. Core counts are
in the table headings for exactly that reason.

## Reproducing

```sh
cargo build --release -p mpedb-bench
./target/release/mpedb-bench --help          # the cells and their flags
./target/release/mpedb-bench --notify --disk /path/on/real/disk
```

Run isolated — nothing else on the machine. PostgreSQL cells need PG 16 on
`PATH` (`/usr/lib/postgresql/16/bin` on Debian,
`/opt/homebrew/opt/postgresql@16/bin` on macOS). On macOS raise the descriptor
limit (`ulimit -n 8192`) or the notify cell's 100-listener arm stops at 63 and
says so.

## LISTEN/NOTIFY: DBOS's numbers and ours

The notify cell exists because of DBOS's
[postgres-listen-notify-scalability](https://www.dbos.dev/blog/postgres-listen-notify-scalability),
so their published numbers belong next to ours — as the **reference point that
prompted the work**, not as a row to compare against. Different hardware,
different client, different workload size; what transfers is the *shape*.

### What DBOS published

| Arm | PostgreSQL | Note |
|---|---:|---|
| NOTIFY per committed write | **2.9K writes/s** | the commit lock is in the path |
| notifications batched off the commit path | **60K writes/s** | **20×**, at 15–100 ms delivery latency |

Their diagnosis: committing a transaction that calls `NOTIFY` takes a global
exclusive lock held until the transaction is fsync'ed, so notifying writes
cannot group-commit and serialize against each other.

**Confirmed at source level** in
[../design/PG-NOTIFY-ANATOMY.md](../design/PG-NOTIFY-ANATOMY.md), with two
refinements they did not state: the lock is
`LockSharedObject(DatabaseRelationId, InvalidOid, 0, AccessExclusiveLock)` — a
heavyweight lock on "database 0", i.e. **cluster-global, not per database** —
and it spans the queue insert, the fsync, *and* `SignalBackends()`.

### What we measured

Full tables, controls and caveats in [notify.md](notify.md). The headline:

| Question | PostgreSQL | mpedb |
|---|---|---|
| Notify cost, **1 writer** | 3–8 % | none |
| Notify cost, **4 writers** | **50–60 %** (863→347 Linux, 661→329 M3) | none (866→894, 785→799) |
| Fan-out, 1 → 100 listeners (11 cores) | **−27 %** (289→211) | **flat** (320→333) |
| Notification latency p50 | 2397 µs / 3313 µs | **36 µs / 147 µs** |

### Reading the two together honestly

DBOS's 20× and our 50–60 % are **not the same measurement**, and the gap
between them is the useful part.

- **Concurrency is what makes a global lock visible.** With a single writer we
  measured PostgreSQL's `NOTIFY` at 3–8 %, nowhere near 20×. Quoting their
  figure against a single-writer cell would be quoting a result for a workload
  that does not produce it. Their streams were concurrent; our arm C adds four
  writers and the effect appears.
- **Their 20× is arm A vs arm B, ours is arm C vs its control.** They compared
  notifying-per-commit against batching the notifications away entirely — which
  also removes per-commit overhead that has nothing to do with the lock. We
  compared identical commits differing only in whether they notify. Ours
  isolates the lock; theirs measures the whole workaround, which is the right
  number if what you want to know is "how much does the workaround buy".
- **Neither of us measured the far tail.** Four writers and 100 listeners are
  small. Where PostgreSQL's curve goes at 64 writers is unmeasured by both.

What the two together do support: the cost is real, it is a contention effect,
and it is structural rather than an implementation detail — PostgreSQL's own
source flags the lock as a scalability item and sketches what replacing it
would take.

**Why mpedb has no equivalent cost** is not cleverness about locks; it is not
having the object that requires them. There is no queue, so there is no shared
ordered structure to keep ordered, and the ordering guarantee is what forces
the serialization. A notification carries "table T moved to generation G", not
a payload, and the listener reads the rest from its own MVCC snapshot. The
price paid for that: no total order across unrelated tables, and no payload
delivery. Both are deliberate, and both are stated in [notify.md](notify.md).
