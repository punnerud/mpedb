# PostgreSQL LISTEN/NOTIFY, from the source

Read in `src/backend/commands/async.c` on `master` (3299 lines, fetched
2026-07-25). Line numbers refer to that revision. This document exists because
the [notification benchmark](../benchmarks/notify.md) compares against Postgres and should
be able to defend its claims at the source level, not just against a blog post.

## What serializes

Not the queue lock. A **heavyweight lock on "database 0"** (async.c:1309):

```c
LockSharedObject(DatabaseRelationId, InvalidOid, 0, AccessExclusiveLock);
```

`InvalidOid` as the database oid makes it **cluster-wide**, not per database.
The comment right above says why, verbatim (async.c:1293–1301):

> Serialize writers by acquiring a special lock that we hold till after commit.
> This ensures that queue entries appear in commit order, and in particular that
> there are never uncommitted queue entries ahead of committed ones, so an
> uncommitted transaction can't block delivery of deliverable notifications.
>
> We use a heavyweight lock so that it'll automatically be released after either
> commit or abort. […] The lock is on "database 0", which is pretty ugly but it
> doesn't seem worth inventing a special locktag category just for this.

## How long it is held

Heavyweight locks are released at transaction end, that is, after the commit
record has been flushed. For a notifying transaction the lock therefore spans
three things:

| Step | What happens | async.c |
|---|---|---|
| 1 | `PreCommit_Notify()` takes the lock, puts the entries into the SLRU queue | 1309, `asyncQueueAddEntries` |
| 2 | `RecordTransactionCommit()` — WAL write and **fsync** | (xact.c) |
| 3 | `AtCommit_Notify()` → `SignalBackends()` | 1403, 2263 |
| 4 | Transaction end — the lock is released | |

Step 2 is the expensive one. That it sits *inside* the lock is the entire DBOS
finding, and the source confirms it: "we hold till after commit".

## The signaling: two levels, and only one of them filters

`SignalBackends()` (async.c:2263) runs **under `LWLockAcquire(NotifyQueueLock,
LW_EXCLUSIVE)`** and makes two passes:

**First pass — channel-indexed.** Modern PG has `globalChannelTable`, a
dshash on `(MyDatabaseId, channel)` with a `listenersArray` per channel. For each
channel the transaction notified, the listeners are looked up directly. That is real filtering, and
it is worth saying clearly: the claim "Postgres cannot know who cares"
is **outdated**.

**Second pass — all listeners.** Then (async.c:2337):

```c
for (ProcNumber i = QUEUE_FIRST_LISTENER; i != INVALID_PROC_NUMBER; i = QUEUE_NEXT_LISTENER(i))
```

The entire listener list in the cluster, for the direct-advance optimization —
advancing an uninterested listener's queue pointer instead of waking it. Useful in itself,
but it makes the cost per notification **O(all listeners)**, not O(interested), and
it is paid under the exclusive `NotifyQueueLock`.

## What is worth taking away

**Filtering granularity and serialization granularity are decoupled.** Postgres
can filter per channel, but serializes per cluster. You can have 10 000 channels and
still push every notifying commit through one exclusive lock. That is not a
lack of filtering — it is that the filter is not connected to what costs.

The reason is structural: the queue is **one shared, ordered structure**, and
the ordering guarantee ("never uncommitted entries ahead of committed ones") is what
requires serialization. Not the delivery — the ordering.

**The source flags it itself** (async.c:1316):

> Note: if the heavyweight lock were ever removed for scalability reasons, we
> could achieve the same guarantee by holding NotifyQueueLock in EXCLUSIVE mode
> across all our insertions, rather than releasing and reacquiring it for each
> page as we do below.

## Other limits

- **Queue:** SLRU, `max_notify_queue_pages = 1048576` → 8 GB at 8 KB pages (:584).
- **Payload:** `BLCKSZ - NAMEDATALEN - 128` ≈ 7.8 KB (:201) — one SLRU page.
- **The tail** is advanced to the minimum of all listeners' positions (`asyncQueueAdvanceTail`,
  :2870), attempted every `QUEUE_CLEANUP_DELAY = 4` pages (:282). **One slow listener
  blocks cleanup for everyone**, and the error message says so outright (:2242):
  "The NOTIFY queue cannot be emptied until that process ends its current
  transaction."

## Why mpedb does not need the same

(Our protocol in full: [DESIGN-NOTIFY.md](DESIGN-NOTIFY.md).)

Not because we are smarter about locks — because we do not have the object that requires them.

We have no queue. The notification carries "table T is at generation G", not a payload,
and the reader has an MVCC snapshot to fetch the rest itself. Then there is no shared
ordered structure to keep ordered, no uncommitted entries that can sit ahead of
committed ones, and nothing to serialize. N changes between two wakeups
coalesce into one instead of accumulating.

And where Postgres computes "who cares" at commit — it has to, the channel is a
runtime string, `pg_notify(text, text)` can be computed — our matching is a
**compile-time constant**: every statement is a precompiled plan with a
footprint, so which slots it can ring is known before it runs.

The price we pay for that: no total order across unrelated tables, and
no payload delivery. Both are deliberate, and both are stated in the [benchmark](../benchmarks/notify.md).
