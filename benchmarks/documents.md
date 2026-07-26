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
| 2 | **73** | 70 | **57** | 51 |
| 4 | **73** | 69 | **58** | 51 |
| 8 | **74** | 69 | **59** | 52 |

Latency, where the two models actually differ:

| Editors | Linux mpedb p50 / p99 | Linux pg p50 / p99 | M3 mpedb p50 / p99 | M3 pg p50 / p99 |
|---:|---:|---:|---:|---:|
| 2 | **13** / 117 ms | 27 / **41** ms | **19** / 219 ms | 39 / **43** ms |
| 4 | **13** / 562 ms | 56 / **71** ms | **19** / 470 ms | 77 / **84** ms |
| 8 | **13** / 2171 ms | 111 / **201** ms | **19** / 690 ms | 153 / **166** ms |

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

| Editors | Linux mpedb | Linux pg | M3 mpedb | M3 pg | mpedb refusals (Linux / M3) |
|---:|---:|---:|---:|---:|---:|
| 2 | **145** | 131 | **106** | 104 | 0 / 0 |
| 4 | **287** | 253 | **204** | 199 | 0 / 0 |
| 8 | 314 | **352** | 255 | **306** | 0 / 0 |

Both scale, because both are row-granular, and after #149 mpedb refuses nothing
at all here — every editor owns a row, no two editors are related, and the guard
now says so.

**This row is where a benchmark earned its keep.** Before #149 it read 229 a/s
on Linux and 169 on the M3, with 65 and 51 refusals, and the page said the
guard's 64-commit ring history had wrapped inside the think time. That was wrong, and the fix for it would have
fixed nothing. `GuardStats` separates a real conflict from each of the ring's
limits, and it reported `snapshot_too_old = 0`, `ring_gap = 0`, `overlap = 65` —
the guard believed those were genuine conflicts.

They were an artefact of the comparison's width. Both sides held an exact key
and then folded it through `region_bit`, a 64-bit Bloom with one bit per key.
Editors 2 and 5 hash to bit 19; they conflicted on every commit for forty
actions while editing rows that have nothing to do with each other. Eight keys
collide on a bit about 35 % of the time, so this was not bad luck — it was the
expected outcome at eight editors.

The ring entry now carries up to eight **exact** keys beside the summary, and a
comparison where both sides can name their keys is a set intersection with no
false positives. Beyond eight keys it falls back to the Bloom, which is coarser
and still correct.

### F-move — a move and an edit on the same row

| Editors | Linux mpedb | Linux pg | M3 mpedb | M3 pg |
|---:|---:|---:|---:|---:|
| 2 | **149** | 69 | **104** | 51 |
| 4 | **145** | 69 | **107** | 52 |
| 8 | **148** | 69 | **110** | 52 |

**2.1× on both machines**, and the factor is exactly right: mpedb's 148/s is
twice the 74/s of a single serial chain, because the row split into two
independent columns. p50 stays on the think time (13 / 19 ms) against
PostgreSQL's 112 / 153 ms. One person moving a paragraph and another editing it
are not in each other's way; `FOR UPDATE` takes the row and both sides queue.

This is the case that motivated [`ordkey`](../crates/mpedb-types/src/ordkey.rs):
a move rewrites one fractional order key, an edit rewrites the body, and the
edit lands on the block wherever it now sits.

## The independence claim, with its control

**4 editors per document, all fighting over that document's one field.** If
contention were a property of the *table*, adding documents would add nothing.

| Documents | Editors | Linux mpedb | Linux **coarse** | Linux pg | M3 mpedb | M3 **coarse** | M3 pg |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 4 | **74** | 73 | 69 | **57** | 58 | 53 |
| 2 | 8 | **148** | 73 | 98 | **112** | 59 | 104 |
| 4 | 16 | 264 | 73 | **267** | **210** | 59 | 210 |

Per-document rates — the number that makes the claim falsifiable:

| Documents | Linux mpedb per doc | Linux pg per doc | M3 mpedb per doc | M3 pg per doc |
|---:|---|---|---|---|
| 1 | 74/s | 69/s | 57/s | 53/s |
| 2 | 74, 74 | 49, 49 | 56, 56 | 52, 52 |
| 4 | 66 ×4 | 67 ×4 | 53 ×4 | 52 ×4 |

Each document runs at its own rate and does not notice the others. (Linux's drop
to 66/s at four documents is 16 processes on 2 cores, not locking — the 11-core
M3 holds 53/s against PostgreSQL's 52/s.)

**The `coarse` columns are the control, and they are the important ones.** They
run the identical work with one extra *declared* statement — a whole-table scan
— and nothing else changed. A scan names no single key, so the guard widens to
"anywhere in this table", which is exactly what the guard was before key regions
(#143). It goes **flat no matter how many documents exist**: 73 → 73 → 73 on
Linux, 58 → 59 → 59 on the M3, with 8269 refusals in the last cell against 1381
for the precise declaration doing the same work.

That is the attribution: document independence comes from the declared surface
naming one row, not from anything else in the engine. Without the control this
page would be a plausible story about a number that could have had three other
causes.

## C1: how many editors fit inside a 1 s answer

The contract (#150, [design/DESIGN-COLLAB.md](../design/DESIGN-COLLAB.md)) is
that everyone who submits an edit learns within a second whether it landed. That
is a **fraction**, not a percentile, so every arm above reports one — and two
arms exist to calibrate it. Both run with **no artificial think time**: they
measure the engine, not this benchmark's sleep.

### F-cap — editors piled onto ONE block

| Editors | Linux ≤1 s | Linux engine p50 | M3 ≤1 s | M3 engine p50 |
|---:|---:|---:|---:|---:|
| 2 | 100.0 % | 2589 µs | 100.0 % | 3003 µs |
| 4 | 100.0 % | 2833 µs | 100.0 % | 3812 µs |
| 8 | 100.0 % | 3876 µs | 100.0 % | 3927 µs |
| 16 | 100.0 % | 4750 µs | **100.0 %** | 4051 µs |
| 32 | **99.4 %** | 8716 µs | 95.6 % | 5178 µs |
| 64 | 92.8 % | 10054 µs | 90.5 % | 7131 µs |

**Measured cap: 32 editors per block on Linux, 16 on the M3.**

The arithmetic anyone would reach for first — `deadline ÷ service time` — says
386 and 333. It is wrong by more than an order of magnitude and wrong in the
dangerous direction, because a refused editor **re-does its whole action**
instead of queueing behind the winner. Optimistic concurrency does not conserve
work the way a lock queue does, so the cap has to be measured.

### F-words — does splitting multiply capacity?

A block can be as small as you like, so a paragraph of 20 words could be 20
blocks. The intuition is that capacity becomes blocks × cap. **It does not.**

| Blocks | Editors | Linux a/s | Linux *no guard* | Linux ≤1 s | M3 a/s | M3 *no guard* |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 50 | 211 | 365 | 97.0 % | 173 | 369 |
| 4 | 200 | 200 | 356 | 74.9 % | 201 | 484 |
| 20 | 1000 | 182 | 341 | 52.9 % | 209 | 394 |

Throughput is flat as the document is split — **and so is the unguarded
control**, which settles the attribution: the guard is not what is in the way.

### F-batch — the ceiling is per COMMIT, not per edit

The obvious reading of the table above is that the write path is saturated.
That reading is wrong, and this arm is what catches it. Eight editors, K edits
folded into a single commit:

| K | Linux commits/s | Linux **edits/s** | M3 commits/s | M3 **edits/s** |
|---:|---:|---:|---:|---:|
| 1 | 2024 | 2024 | 1183 | 1183 |
| 8 | 1365 | **10921** | 960 | **7682** |
| 64 | 508 | **32506** | 514 | **32907** |
| 256 | 377 | **96390** | 462 | **118234** |

**46× the edit rate on the two-core box and 100× on the M3 — while commits run
*slower* on both.** The engine's limit is commits, and an edit never had to be
one. A thousand concurrent editors is a thousand edits: thirty milliseconds of
capacity at K = 64 on either box.

> **The Linux column was re-measured on 2026-07-26 and every cell moved up 7–10×**
> (it read 197 / 1370 / 4525 / 14988). The M3 column reproduced to within run
> variance — 1183 against the 1178 published — and that pairing is what says the
> *first Linux run* was wrong rather than the arm. The likeliest cause is that it
> shared the two cores with other work; `benchmarks/README.md` already says to run
> isolated, and this is what that rule is worth. The conclusion is unchanged and
> the multiplier is smaller.

So the earlier draft of this page was wrong to conclude that a thousand editors
needs a machine committing a thousand times a second. What it needs is that an
edit is not a commit of its own — and note that the M3 clears a thousand commits
a second anyway, which is how thin that conclusion was. The rule that survives
is one line:

```text
editors on one block  ≤  cap        (16–32 at a 1 s deadline)
```

`F-words` measured a benchmark that gives every edit its own commit, which is
the shape a naive client would have. It is a real limit *for that shape*, and
the fix is the shape, not the machine.

**What batching does not do:** it does not make an edit durable sooner. The
answer an editor waits for is "did my edit win" — a question about **conflict**
— and that is decided the moment the surface is claimed. Durability arrives with
the batch. Splitting those two answers apart is filed, with byte-range conflict
units, in [design/DESIGN-COLLAB.md](../design/DESIGN-COLLAB.md) §3.

### F-quorum — flush on a majority, with time as the upper bound

`F-batch` says a commit can carry K edits, but it is a *stated* K: the arm is
told to fold 64 edits and folds 64. A real service does not know K. It knows who
is editing, and it can wait — and the question is what it should wait **for**.

The etcd answer is: not for the clock. Flush when a majority of the editors with
outstanding work have delivered, and keep a deadline only as the bound that
should never be reached. `F-quorum` builds that on `submit_batch` — editors are
threads on disjoint 16-byte slices, a channel stands in for transport, quorum is
`n/2 + 1` of those with work outstanding, and the upper bound is 5 ms.

| | | Linux | | | | M3 | | |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **editors** | | on quorum | on timeout | avg K | edits/s | on quorum | on timeout | avg K | edits/s |
| 8 | all healthy | 15 | **2** | 4.7 | 1744 | 16 | **0** | 5.0 | 1870 |
| 32 | all healthy | 18 | **1** | 16.8 | 4090 | 18 | **1** | 16.8 | 4057 |
| 64 | all healthy | 19 | **1** | 32.0 | 7085 | 19 | **1** | 32.0 | 6935 |
| 8 | ¼ are 10× slow | 13 | 8 | 3.8 | 468 | 12 | 16 | 2.9 | 334 |
| 32 | ¼ are 10× slow | 16 | 12 | 11.4 | 1745 | 16 | 6 | 14.5 | 1375 |
| 64 | ¼ are 10× slow | 17 | 6 | 27.8 | 1612 | 16 | 13 | 22.1 | 2745 |

**The batch fills.** Average K reaches 32 — where #152 measured the intent ring
carrying exactly 1, every time, because a guarded session never reaches it. The
K in `F-batch` was declared; this one was discovered from arrivals.

**The deadline is a bound, not the mechanism.** With everyone healthy it fires
1–2 times in ~20 flushes on Linux and 0–1 on the M3. It is what fires when a
quarter of the editors are ten times slower — 6–16 times in ~25. That is the
result the design asked for: time is the thing that should never happen, and it
happens exactly when something is wrong.

**Quorum does not insulate the fast from the slow, though.** Healthy → quartered
costs 1744 → 468, 4090 → 1745, 7085 → 1612. The reason is arithmetic: a majority
of *active* editors usually cannot be assembled out of the fast ones alone, so
the fast editors wait for slow ones anyway — just via the majority rather than
via the clock. Quorum bought the right *trigger*, not immunity.

**Against the ceiling:** at avg K = 32 the arm gets 7085 edits/s where `F-batch`
at K = 64 gets 32506. Most of that gap is not the engine — `F-batch` children
loop without pausing, while a `F-quorum` editor waits for its own verdict before
submitting again, so offered load is capped by the round trip. The arm is a floor
on what the flush path sustains, not a competitor to the ceiling.

#### The 40% loss, and what the control proved

Between 27 and 285 edits lose per run, on **disjoint** offsets that should never
collide. Two hypotheses died: stale snapshots (losses are unchanged with every
editor fast) and whole-batch refusal (a `wiped` counter for batches where every
member lost came back far too small).

What killed it was a third counter. `behind` marks a batch whose *oldest* member
snapshot predates the previous batch's commit, and `b.wiped` the ones of those
that lost everybody:

| | Linux 8 / 32 / 64 | M3 8 / 32 / 64 |
|---|---|---|
| `wiped` | 2 / 4 / 8 | 3 / 5 / 7 |
| `b.wiped` | 2 / 4 / 8 | 3 / 5 / 7 |

**Equal in all eighteen cells measured, across both machines and both health
settings.** Not one batch of fresh members was ever wiped, and not one wipe
happened without a lagging member. The cause is two conservative choices
multiplying:

- `submit_batch` walks the committed ring from `min(snap)` over its members, so
  **one lagging editor drags every other member's rebase walk back** across
  commits those members had already seen.
- `record_written_range` publishes `(min lo, max hi)` — the union over the whole
  transaction (`engine/write.rs`). So the previous batch declared *one* span
  covering all K of its members, which for 32 editors is most of the block.

Either alone is a rounding error. Together, one lagging member makes its whole
batch walk over a range that covers everyone, and everyone loses. Both are
fail-safe in the correct direction — a spurious conflict, never a missed one —
which is exactly why this cost throughput and not correctness, and why it took a
counter rather than a failing test to find.

The fix is per-member snapshots on the rebase walk, and it is filed rather than
built here: this arm exists to find out whether the quorum idea works, and that
answer does not depend on it.

## What the verdict counter is for

Every mpedb arm reports `(cleared, overlap, snapshot_too_old, ring_gap)` beside
its refusal count, because a refusal alone cannot say whether the guard caught a
real conflict or ran out of machinery — and those call for opposite fixes. The
counter is what turned "widen the ring" from a plausible plan into the right
one: it said `snapshot_too_old = 0` everywhere, which killed the history
hypothesis, and left the width of the *comparison* as the only candidate.

`OPT_RING_SLOTS` went 64 → 256 anyway, and that is worth being honest about: no
measurement here needed it. It bounds how long a guarded action may think, in
commits, and arm F thinks for 10 ms. The workload arm F stands for thinks for
seconds, so the limit is real even though this page did not hit it — and it is
pinned from both sides, at 100 commits (must be witnessed) and 300 (must still
refuse).

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
