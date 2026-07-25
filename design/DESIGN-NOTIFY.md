# Change notification, and acting on it

The protocol behind `Database::listen` / `mpedb listen` (#139, #141) and the
shard guard that makes a notification actionable (#142). Measurements against
PostgreSQL are in [benchmarks/notify.md](../benchmarks/notify.md); the source
reading that grounds the comparison is in
[PG-NOTIFY-ANATOMY.md](PG-NOTIFY-ANATOMY.md).

## 1. What a notification is

**"Table T is at generation G."** Not a payload, not a queue entry. The listener
reads whatever it needs from its own MVCC snapshot.

That single decision is what removes the global lock. PostgreSQL serializes
notifying commits on a cluster-wide exclusive lock because its queue is one
shared ordered structure and the ordering guarantee — never an uncommitted entry
ahead of a committed one — is what forces serialization. With no queue there is
no order to protect, and N changes between two wakeups coalesce into one instead
of accumulating.

The price, stated so nobody has to discover it: **no total order across
unrelated tables**, and **no payload delivery**.

## 2. Shared-memory layout (lock area, page 2)

| Offset | Field | Purpose |
|---|---|---|
| `LA_NOTIFY` | 64 × 24 B slots | `gen` u64, `seq` u32 (futex word), `table` u32 (exact id), `key` u64 (region) |
| `LA_NOTIFY_WAITERS` | u32 | parked listeners; a commit with none pays one relaxed load |
| `LA_NOTIFY_ANY` | u32 | the "any table changed" futex word |
| `LA_NOTIFY_EPOCH` | u64 | incarnation stamp (§5) |

Slot = `table_id % 64`, with the **exact** id stored alongside. A collision is a
false wakeup, never a missed one. The exact id matters: the committed-footprint
ring folds ids `& 63` and is sound doing so because a false conflict costs only
a retry, but the same fold used as *identity* would wake the wrong listener.

## 3. Publication and the ordering that carries it

Under the writer lock, after the meta flip, for every mutated table: store the
id and key region (Release), bump `gen`, bump the slot's `seq`, then bump the
"any" word. A parked futex re-checks `seq`, so it must move last; the "any" word
moves after the per-table one because a multi-table listener released by it
re-reads every generation it watches.

## 4. Waiting, and why the park is chosen by arity

A futex waits on **one** word. A listener on one table parks on that table's
slot word and keeps the exact filter. A listener on several parks on the "any"
word and re-checks its generations on wakeup.

The alternative — parking on `tables[0]` — was the shipped behaviour until #141
N3, and it meant a change to any other watched table was noticed only when the
timeout expired: a latency defect of *orders of magnitude*, not percent. Parking
on a shared word trades that for false wakeups, which is the direction
everything here errs in.

**The lost-wakeup window is closed by sampling the futex word BEFORE testing
the generations.** The other order loses wakeups: a publish landing between the
test and the sample makes the expected value the post-publish one, and the park
then sleeps through a change that already happened.

**Platform:** a real futex exists only on Linux. Elsewhere the wake is a
documented no-op and the wait is a bounded sleep, so this degrades to ~200 µs
polling — correct, and measured rather than hidden.

## 5. Key regions, and the epoch

**Regions.** `keycode` is memcmp-ordered, so the common byte prefix of a range's
`lo` and `hi` contains every key between them. That prefix, fingerprinted into
the slot's 8 key bytes as (length, hash), *is* the region; a point write is the
case where it is the whole key. A listener hashes the same prefix of its own key
to compare. Anything unresolvable publishes 0, which matches everything.

Granularity is worth being exact about: an int64 encodes as a tag plus 8
big-endian bytes, so a range `10..12` names the 256-aligned block. It excludes
essentially all of an int64 keyspace and never excludes a neighbour. Text keys
fare better — distinct prefixes separate from the first differing byte.

**The epoch** exists because generations are counters inside *one incarnation*
of the file, and `notify_reset` zeroes them on reboot, on reformat, and on
delete-and-recreate. A live listener cannot notice; it dies with the reboot. A
client that *persists* its position and returns very much can — holding 900,
meeting a counter reset to 3, concluding "unchanged" for the next 900 commits.
Resetting alone never fixed that; it moved the stale number from the file to the
client. So a position is `(epoch, generations)`, and an unrecognised epoch reads
as "everything, go look".

## 6. Acting on it: the shard guard (#142 G1)

A notification alone leaves a race in the next step. The guard closes it without
holding anything:

1. Take the token: `Listener::snapshot()` — the committed txn id the listener is
   caught up to, captured with its generations. A *fresh* snapshot after waking
   would include the very commit you woke for and guard against nothing.
2. Work, outside any lock, for as long as you like.
3. `Database::begin_guarded_for(snap, &[…sql…])` — the declared surface is the
   union of those statements' footprints, reads included.
4. Commit. The check runs first in `commit_inner`, under the writer lock the
   commit already holds, against the committed-footprint ring. Overlap ⇒
   `Error::WriteConflict`, cheaply: no catalog writeback, no freelist fixpoint.

**Declared, not accumulated.** A branch not taken would shrink an accumulated
surface, so two workers running the same logical action would guard different
things — and a shard needs an identity that exists before anything runs, which
is what a lease (G2) and shard-aware queue dispatch (G3) key on. Execution still
widens, so a wrong declaration makes the guard bigger, never wrong.

**Guarantee:** no lost updates on your surface.
**Not a guarantee:** exclusivity of the work. Two actors may compute, one
commits. In an engine whose premise is that processes may be SIGKILLed at any
instant, that is worth more than mutual exclusion — nothing to lease, nothing to
reap, and a dead process simply did not commit. The task queue's claim was
already written this way.

The ring's limits (64 commits of history, `& 63` table folding, point-only key
precision) all fail toward a retry and are pinned as tests, with `GuardStats`
counting which one actually bites.

## 7. Invariants that bite

- The exact table id in the slot is **identity**; the ring's `& 63` fold is
  **conflict detection**. Do not swap them (`cdc.rs` documents that bug class
  having been fixed once already).
- `mutated_tables` is monotone across savepoint rollback, so a notification may
  fire for a change that was rolled back. False wakeup, in contract.
- The notify region must be reset on boot-epoch change **and** stamped with a
  fresh epoch, or a persisted cursor outlives the counters it refers to.
- `opt_record` must run on **every** commit, not only optimistic ones. It was
  gated once, and the guard — a second reader of that history — then refused
  every action because the ring looked full of gaps.
