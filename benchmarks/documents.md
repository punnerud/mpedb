# Many editors, one document

**Arm F.** A hundred people have the same Google-Docs-like document open, and
someone else's document must not care. This is the concrete form of the
question [notify.md](notify.md) answers abstractly — arm E measures what
*acting on a notification* costs; this measures what happens when everyone acts
on the same thing.

Against PostgreSQL doing the same work the natural way there: `SELECT … FOR
UPDATE` before the think time, so the row is locked across it.

```sh
cargo build --release -p mpedb-bench
./target/release/mpedb-bench --doc-load --disk /path/on/real/disk
# knobs: MPEDB_F_ACTIONS (40), MPEDB_F_DELAY_MS (10)
```

Real processes on both engines — the binary re-invokes itself as an editor.
Every measurement re-seeds its database, so no arm inherits the previous one's
row sizes.

## The shape of one edit

```text
read the document  ->  think for 10 ms  ->  write it back
```

The think time is the whole point. It stands for a person typing, an LLM
generating a paragraph, a render — and it is exactly the window a lock is held
across, or not. mpedb holds nothing: it takes a snapshot, thinks, then commits
through a guard that declares what the action may touch. A collision is refused
at commit and retried, never waited on.

## Three models of "a document"

| Arm | What it is | What it should show |
|---|---|---|
| **F-field** | The document is one field. Every editor read-modify-writes `doc.body`. | Two concurrent edits are a real lost update, so this **must** serialize. |
| **F-blocks** | The document is a table of blocks; editor `w` owns block `w` of the same document. | The model [`ordkey`](../crates/mpedb-types/src/ordkey.rs) exists for — different paragraphs are different rows. |
| **F-move** | One block, two columns: half the editors rewrite `body`, half move it by rewriting `ord`. | Column granularity (#146 K1) says a move and an edit are not a conflict. A row lock says they are. |

## Correctness first: the `lost` column

Throughput without correctness is meaningless here — an engine that silently
drops concurrent edits is infinitely fast. So in `field` mode every editor owns
a 16-byte slot of the body and writes its action counter there by
read-modify-writing the whole field. Afterwards the field is read back and every
slot checked.

| Editors | mpedb, guard **off** | mpedb, guarded | PostgreSQL |
|---:|---:|---:|---:|
| 2 | **1 lost** | 0 | 0 |
| 4 | **3 lost** | 0 | 0 |
| 8 | **7 lost** | 0 | 0 |

The unguarded control loses all but one editor's work, every time — identical
on both machines. That is the number the rest of this page is paid for.

## Linux (2 cores) and M3 (11 cores), 40 actions/editor, 10 ms think

Best per row in **bold**. The 10 ms think time is the floor for one action, so
a p50 sitting on it means the editor never waited on anyone.

### F-field — the document is one field

| Editors | Linux mpedb | Linux pg | M3 mpedb | M3 pg |
|---:|---:|---:|---:|---:|
| 2 | **73** | 69 | **56** | 51 |
| 4 | **71** | 69 | **57** | 51 |
| 8 | **73** | 70 | **59** | 51 |

Latency, where the two models actually differ:

| Editors | Linux mpedb p50 / p99 | Linux pg p50 / p99 | M3 mpedb p50 / p99 | M3 pg p50 / p99 |
|---:|---:|---:|---:|---:|
| 2 | **13** / 21 ms | 27 / **43** ms | **18** / 204 ms | 39 / **42** ms |
| 4 | **13** / 552 ms | 56 / **71** ms | **19** / 447 ms | 77 / **83** ms |
| 8 | **13** / 2197 ms | 112 / **205** ms | **19** / 648 ms | 154 / **167** ms |

Throughput is a tie, and it should be: one field everyone rewrites is one
serial chain in both engines. What differs is *where the waiting lands*, and
this is the honest trade:

- **mpedb never waits.** p50 sits on the think time at every editor count — an
  editor does its work and finds out at commit.
- **mpedb's tail is unbounded.** p99 reaches 2.2 s at 8 editors on Linux,
  because optimistic retry has **no fairness**: a refused editor rejoins the
  same race it just lost, and can lose repeatedly.
- **PostgreSQL's tail is bounded** because its lock has a *queue*. Its p50 grows
  linearly with editors — that is the queue, visible.

If you need a predictable worst case on a contended row, a lock queue is the
better instrument, and this measurement says so.

### F-blocks — the document is a table of blocks

| Editors | Linux mpedb | Linux pg | M3 mpedb | M3 pg | mpedb retries (Linux / M3) |
|---:|---:|---:|---:|---:|---:|
| 2 | **139** | 133 | **102** | 98 | 0 / 0 |
| 4 | **282** | 260 | **202** | 198 | 0 / 0 |
| 8 | 229 | **358** | 169 | **257** | 65 / 51 |

Both scale, because both are row-granular. PostgreSQL wins at 8 editors on both
machines, and mpedb's retries say why: at 8 editors × 40 actions the guard's
**64-commit ring history** wraps inside a single 10 ms think time, and a
snapshot the ring can no longer witness is refused conservatively
(`GuardVerdict::SnapshotTooOld`). That is a width limit, not a conflict — the
same saturation [notify.md](notify.md) records for arm E, and it is filed.

### F-move — a move and an edit on the same row

| Editors | Linux mpedb | Linux pg | M3 mpedb | M3 pg |
|---:|---:|---:|---:|---:|
| 2 | **145** | 68 | **103** | 51 |
| 4 | **141** | 68 | **106** | 52 |
| 8 | **141** | 70 | **109** | 52 |

**2.0× on Linux, 2.1× on the M3**, and the factor is exactly right: mpedb's
141/s is twice the 73/s of a single serial chain, because the row split into two
independent columns. p50 stays on the think time (13 / 19 ms) against
PostgreSQL's 112 / 154 ms. One person moving a paragraph and another editing it
are not in each other's way; `FOR UPDATE` takes the row and both sides queue.

This is the case that motivated [`ordkey`](../crates/mpedb-types/src/ordkey.rs):
a move rewrites one fractional order key, an edit rewrites the body, and the
edit lands on the block wherever it now sits.

## The independence claim, with its control

**4 editors per document, all fighting over that document's one field.** If
contention were a property of the *table*, adding documents would add nothing.

| Documents | Editors | Linux mpedb | Linux **coarse** | Linux pg | M3 mpedb | M3 **coarse** | M3 pg |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 4 | 73 | 73 | 49 | 57 | 56 | 51 |
| 2 | 8 | **146** | 66 | 138 | **111** | 59 | 102 |
| 4 | 16 | **238** | 72 | 270 | **211** | 60 | 210 |

Per-document rates — the number that makes the claim falsifiable:

| Documents | Linux mpedb per doc | Linux pg per doc | M3 mpedb per doc | M3 pg per doc |
|---:|---|---|---|---|
| 1 | 73/s | 49/s | 57/s | 51/s |
| 2 | 73, 73 | 69, 69 | 56, 56 | 51, 51 |
| 4 | 60 ×4 | 67 ×4 | 53 ×4 | 53 ×4 |

Each document runs at its own rate and does not notice the others. (Linux's drop
to 60/s at four documents is 16 processes on 2 cores, not locking — the 11-core
M3 holds 53/s against PostgreSQL's identical 53/s.)

**The `coarse` columns are the control, and they are the important ones.** They
run the identical work with one extra *declared* statement — a whole-table scan
— and nothing else changed. A scan names no single key, so the guard widens to
"anywhere in this table", which is exactly what the guard was before key regions
(#143). It goes **flat no matter how many documents exist**: 73 → 66 → 72 on
Linux, 56 → 59 → 60 on the M3, with 8730 refusals in the last cell.

That is the attribution: document independence comes from the declared surface
naming one row, not from anything else in the engine. Without the control this
page would be a plausible story about a number that could have had three other
causes.

## What this cost, and what it exposed

Building the control found a live correctness bug. `begin_guarded_for` took bare
SQL strings and contributed only the statements' **tables** to the declared
surface — while #143 and #146 had narrowed the conflict test to keys and
columns. So the documented pattern (snapshot, read outside, think, write inside)
silently dropped its read dependency: nothing inside the session touches the row
that was read, so nothing could re-add it, and a decision made from a stale
value committed.

The fix is that parameters are part of a declaration:
`begin_guarded_with(snap, &[(sql, params)])` resolves the point key and the
column set; the bare-SQL form widens to the whole table, because `WHERE id = $2`
with no value for `$2` genuinely names every row. Both forms are pinned against
each other in `crates/mpedb/tests/shard_guard.rs`, and the lost update has a
regression test that committed on HEAD before the fix.

## What this does not measure

- **Two machines, both single-host.** Eight editors on the Linux box's two cores
  measures the scheduler as much as the engine, which is why the 11-core M3 runs
  beside it. Both are `durability = wal` on real disk.
- **No network.** PostgreSQL pays a round trip per statement in any real
  deployment, and those round trips happen *inside* the transaction, extending
  how long its row lock is held. Arm E models that explicitly
  (`MPEDB_E_RTT_MS`); arm F does not, so PostgreSQL is measured at its most
  favourable.
- **Small documents.** A 1 KB body keeps the measurement about concurrency
  rather than about row size.
- **No client.** Delivering an edit to other editors' screens is not measured —
  only what the database will let concurrent editors do.
