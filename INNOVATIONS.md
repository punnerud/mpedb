# Innovations

What mpedb invented, what it borrowed, and what it moved somewhere it is not
normally used. Written for an engineer with no database background: each entry
states the *problem* before the mechanism, and the mechanism in enough detail to
reimplement.

Every entry carries a provenance tag, because an innovation list without one is a
sales brochure:

| tag | meaning |
|---|---|
| **[I]** | invented here |
| **[T]** | textbook / standard database practice — listed for completeness, not credit |
| **[X]** | transferred from a different field into a database |
| **[C]** | standard parts, unusual combination |

A copy-on-write B-tree is [T] and gets no entry. A *specific discipline around*
one may be [I]. Where a claim rests on an assumption we have not measured, the
entry says so. Section 9 is the negative results — things built, measured, and
rejected — and it is deliberately as long as the successes.

---

## 1. Surviving sudden death

mpedb's operating model is sqlite's: no server process, any number of processes
map the same file directly, and **any of them may be `SIGKILL`ed at any single
instruction**. Everything in this section exists because of that last clause.

### 1.1 Reader slots: packed `{pid, seq}` generation-CAS + process start-time identity [C]

**Problem.** A process that is reading must publish "I am looking at version 47,
do not recycle its pages" into shared memory. Three things must not happen.
(a) It is killed while registered — someone must *prove* it is dead, or the file
eventually fills up because nothing can ever be recycled. (b) The OS reuses its
PID for an unrelated new process, so "is PID 1234 alive?" answers *yes* forever
and a ghost pins version 47 until the database is full. (c) A sweeper frees a
slot in the microsecond between a new reader reserving it and publishing its
identity — and now two processes believe they own it, and the second one's data
is recycled underneath it.

**Mechanism.** A 64-byte slot per reader: one word `{pid: high u32, seq: low
u32}`, a pinned-version word, and the process's start time read from
`/proc/<pid>/stat` field 22. Every state transition is a CAS of the whole word,
bumping `seq` — so an interrupted-and-resumed transition can never be mistaken
for a fresh one. `CLAIMING = 1<<31` is set *in the pid half*, which is safe
because Linux's `PID_MAX_LIMIT` is 2²², so bit 31 is never a real PID.

Claim is three steps and the *order is the whole point*:

1. `CAS {0, s} → {pid|CLAIMING, s+1}` — reservation and identity publication are
   one atomic step.
2. As sole owner, store the pin, then the start time.
3. `CAS {pid|CLAIMING, s+1} → {pid, s+1}`. **If this fails, the slot was
   reclaimed from us — walk away and never touch it again.**

A sweeper frees a `CLAIMING` slot only on a definite `ESRCH`, never on a
start-time mismatch, because that slot's start time is not yet trustworthy.
`EPERM` counts as *alive*. Every free is a CAS of the exact word observed dead,
so racing a re-claim is harmless.

**Why start time.** PID + start time is a pair the OS will not reissue: a
recycled PID belonging to a different process has a different start time. This
is standard in process supervisors and `pidfd`-era tooling; using it as a
database's reader-liveness identity is the transfer.

**Provenance.** The reader table itself is LMDB's, [T]. The CLAIMING bit, the
start-time identity, and the "every side-field store is owner-only, sandwiched
between two CASes" discipline are the hardening, and the claim-order bug was
found *by adversarial review*, not by testing.

### 1.2 The paired sequentially-consistent fence [X]

**Problem.** The reader stores "I pin version 47". The writer reads the reader
table to compute "the oldest version anyone still needs" so it can recycle
everything older. On real CPUs — x86 *and* ARM — a store can sit in the storing
core's buffer while that core races ahead, and a load on another core can be
satisfied before an earlier store drains. So the reader's pin can be invisible to
the writer's scan *and* the writer's commit invisible to the reader's re-check,
**simultaneously**. The writer then concludes nobody needs version 47, recycles
it, and the reader reads garbage as if it were rows.

This is the store-buffering (Dekker) litmus test. Release/Acquire alone does not
forbid it. The intuitive-and-wrong design — "Release store, then re-check" — was
mpedb's v1.0 protocol.

**Mechanism.** Reader: publish pin → `fence(SeqCst)` → re-read the version and
retry if it moved. Writer: `fence(SeqCst)` → scan the table. The soundness
argument is that *any pin published after a writer's scan is at least as new as
the newest commit at publish time, and pins only ever leave* — so a computed
minimum is a permanently valid conservative bound, which is what lets the
expensive scan sit off the per-commit hot path behind a monotone cache.

**Provenance.** The SC-fence pair is [T] in lock-free programming. Using it
*across processes* in a mapped file, and knowing you need it here, is the
transfer. Weakening it reintroduces the race silently — no test fails.

### 1.3 Boot-id recovery [X]

**Problem.** A robust mutex's owner-death detection lives in the **kernel**, not
the file. Power-cycle the machine and the file still says "locked by PID 1234",
but the kernel has no record and will never report the owner's death. Every
process then blocks forever on a database that is otherwise perfectly intact.

**Mechanism.** The lock page stores the boot id. On the first attach after it
changes, under an exclusive `flock`: replay the write-ahead log **first** (after
power loss the mapping is whatever the kernel happened to write back; the log is
the source of truth), *then* reinitialize the mutex, wipe the reader table (every
pre-reboot pin is meaningless), then write the new boot id **last**. Dying
anywhere inside makes the next attacher redo the whole sequence, because the boot
id is only updated at the end.

Reading the boot id returns `Result`, not `Option`: a process that cannot
determine it must refuse to attach, because a zeroed boot id would trigger
spurious boot recovery — mutex re-init plus reader-table wipe — on a **live**
database.

**Provenance.** Boot-id epoch detection is standard in init systems and
stale-lockfile cleanup. Using it as the validity epoch for shared-memory control
state in an embedded database is the transfer.

### 1.4 macOS FLD-2: synthesizing a robust mutex from three ordinary primitives [I]

**Problem.** macOS has no robust mutex. Without owner-death detection, a writer
killed mid-commit wedges the database permanently. Every obvious substitute
fails: PID+start-time is necessary but not sufficient, and `__ulock_wait/wake` —
the natural futex analogue — keys its wait queue on the caller's *(task, virtual
address)*, not the physical page, so a wake never meets a waiter in another
process.

**Mechanism.** Three primitives and one invariant:

> **`flock` free AND `DIRTY == 1` ⟺ the previous holder died inside its critical
> section.**

A sidecar-inode `flock` gives kernel-guaranteed release on death (keyed on the
open *file description*, so immune to PID reuse, `EPERM`, and clock changes). A
process-private errorcheck mutex catches re-entrancy — and must be taken *before*
the `flock`, because `flock` would happily re-grant a lock the process already
holds. One shared tri-state word `DIRTY` is set to 1 after acquiring and before
any write, and cleared by CAS *while still holding the `flock`*, so a clean
release is always observed as 0.

There is no takeover routine, which is the point of using a kernel file lock. A
survivor blocked on `flock` is simply granted it, reads `DIRTY == 1`, and
recovers under exclusion before writing.

`DIRTY = 2` is a poison state if recovery itself fails — a deliberate improvement
over the Linux path, which makes consistent and unlocks, losing the signal.

Uses `kern.bootsessionuuid`, **not** `KERN_BOOTTIME`: the latter is `now −
uptime` and is re-derived on any clock step, so an NTP correction would
masquerade as a reboot and wipe control state on a live database.

**Honest scope.** A live-but-hung or `SIGSTOP`ped holder wedges on *both*
platforms. Listed as a limitation, not papered over.

### 1.5 Everything else in the file, briefly

- **Meta double-buffer** [T] shape, [C] discipline. Two pages hold alternating
  commit records; the checksum is written **last with Release** after an explicit
  Release fence, and the slot index is part of the checksum preimage so a
  whole-page copy of A into B cannot validate as B. The doc's load-bearing note:
  *"checksum written last in source order guarantees nothing"* — it is the fence
  and the ordering annotations that make it true, not the source line order.
- **Initialization via `flock` + `fallocate`** [C] — never `O_EXCL` (a dead
  creator's file must be adoptable), never `ftruncate` (an `ftruncate`d file is
  one big hole, and the first store into a hole on a full filesystem kills the
  writer with `SIGBUS` mid-commit; `fallocate` surfaces `ENOSPC` at open time as
  a clean error), never a bare futex (no owner-death semantics).
- **No Rust references into shared mutable memory** [C] — all cross-process
  structs are `#[repr(C)]`, reached only through raw pointers and atomics. No
  `&`/`&mut` (aliasing UB) and no `volatile` (*"it neither removes data races nor
  orders"*).

---

## 2. Storage discipline

### 2.1 Read-only freelist draw, and why the fallback *is* the termination proof [I]

**Problem.** Freed pages are recorded in a B-tree, which is itself made of pages.
So recording "these pages are now free" allocates and frees pages, which changes
what must be recorded. That is a fixpoint loop and it must terminate. The naive
design — take pages out of the freelist, delete the entry — makes the loop
*hungrier the more you feed it*: every page drawn is now recorded nowhere, so the
fixpoint must write it back, which costs pages, which need drawing.

This produced a real, unbounded leak. Measured: doubling the file exactly doubled
survival time (64 MB died at 10 s, 128 MB at 20 s, 256 MB at 40 s) at a rate of
~1 leaked page per 43 commits, forever, with 12,000 free pages sitting unused.

**Mechanism.** A writer *draws pages out of an entry without deleting it*,
recording provenance. The commit-time fixpoint strikes out only what was actually
consumed and **never rewrites an entry nothing came out of**. Decoupling the draw
from any write-back obligation is the fix.

Three rules that are not obvious:

- An entry freed by transaction `T` is drawable when **`T ≤ oldest-pinned`**, not
  `T <`. Pages freed *by* commit T are referenced only by snapshots older than T
  — commit T is what replaced them — so a pin exactly at T cannot see them. The
  off-by-one leaks without bound.
- **The sticky rule**: once a pass has written an entry it stays in the plan
  forever, even if a later pass frees every page back and it looks untouched.
  Dropping it strands the entry's pages. *"The page-accounting verifier catches
  that as `page N leaked`, which is exactly how this was found."*
- **The termination argument is the `high_water` fallback**: once the drawn pool
  is consumed, allocation falls back to extending the file, which frees nothing,
  so the sets stop growing. That is precisely why the "no refill inside the
  fixpoint" guard must stay even though refill no longer mutates anything — a
  refill inside the loop lets the pool grow on the fly and the monotone-bounded
  argument stops closing.

**Free bonus.** An uncommitted writer's draw is invisible — the entry still lists
the pages — so `SIGKILL` mid-transaction loses nothing and needs no undo.

**Evidence.** After the fix: 8 writers, 64 MB, 60 s, 4.9 M operations, page
accounting exact, where the scaling law demanded ~384 MB. Cost: −7.05 % on the
write path [−8.71, −5.40], n=20 paired. Seven hypotheses died along the way; see
§9.

### 2.2 `page_mut` refuses non-dirty pages, in release builds [C]

**Problem.** Copy-on-write correctness is a *discipline*: if any code path ever
writes a page a reader's snapshot still points at, that reader sees a row half
old and half new, with no error anywhere. It is the easiest way to corrupt an
MVCC engine and it is invisible in single-process testing.

**Mechanism.** The mutable-page accessor returns an error unless the page is in
the current transaction's dirty set. Always on, in production, on the hot path —
not a debug assertion, not a test harness.

Paired discipline: `alloc` zero-fills, and a separate `alloc_raw` exists rather
than making `alloc` lazy, because *"every other caller relies on the zeroed
contract, and quietly weakening it for all of them to speed up one path is how a
subtle corruption gets introduced."*

### 2.3 Write-ahead log: offset-in-checksum, lean records, monotone punch-hole [C]

**Problem.** Replay must distinguish "a real record written here" from "old bytes
that happen to look like a record". Old bytes get there two ways: a page image
inside a record can *contain* a copy of an earlier record, and after
truncate-and-restart an offset gets a second life with stale-but-valid content.

**Mechanism.**

- **The checksum preimage includes the file offset the record sits at**, so a
  copy of a record embedded in page bytes can never validate. Recovery
  additionally requires consecutive records to carry consecutive transaction ids.
- **Lean records**: a B-tree node's two *used* regions are logged and the free
  middle elided, zero-filled on replay. This is safe only because it was
  *audited*: no reader touches those bytes, and there is no per-page checksum
  anywhere in the engine, so a replayed page is observationally identical even
  though the elided span is zeroed rather than restored. Worth ~1.15–1.2×, modest
  exactly because one `fdatasync`'s fixed cost dominates a few KiB of payload.
- **Checkpoint punches holes instead of truncating** — a deliberate deviation.
  Space cost is identical, but punching keeps the log length **strictly
  monotone**, so an offset is never reused in the file's lifetime. That closes a
  whole hazard class: no stale-but-valid record can sit where a scan will look,
  and no mixed-epoch `(checkpoint, length)` pair is observable (a writer dying
  between the two zero-stores of a truncate-reset leaves exactly such a pair).
- A checkpoint sync failure is **swallowed** — the checkpoint offset simply does
  not advance, recovery replays more, the next commit retries. *"Failing a commit
  that is already durable and acknowledged would be a lie."*

**A measured trap worth stealing.** Appending into `fallocate`d-but-unwritten
extents makes every `fdatasync` journal an extent conversion: **958 µs vs 350 µs
per append+sync (2.7×)** on ext4. New log chunks are pre-zeroed.

### 2.4 The `async` class, and refusing to call it durable [C]

The tempting fast mode is "don't sync", which done naively returns a **torn**
database after power loss — half a transaction applied. The honest version
guarantees crash-consistency always (whole records vanish, never partially apply)
while admitting durability is deferred.

Two watermarks: `appended` (write cursor) and `len` (advanced only after a sync
returns). The gap is the loss window. Copy-on-write does the rest: an abandoned
tail's freshly-allocated pages came from a snapshot the recovered meta never
committed — the same argument that makes an uncommitted writer safe.

The technique worth naming is the **naming discipline**: the design says outright
that this mode *"must never be described as durable-on-ack"*, and repeats it. The
class is [T] (sqlite `synchronous=NORMAL`, PostgreSQL `synchronous_commit=off`);
refusing to let marketing language attach to it is the contribution.

Measured, single client on ext4: `wal` ≈ 2.2–2.7k inserts/s (durable on ack),
`async` ≈ 22–32k, batched `wal` ≈ 100k rows/s.

### 2.5 Two flushes, not `runs+1` [I]

**Problem.** On Linux, `msync(MS_SYNC)` *is* `fsync` on that range — it is a
device flush, not a memory operation. A commit path that loops "one `msync` per
contiguous dirty run" therefore issues `runs + 1` device flushes where the design
floor is 2. The bug survived because an earlier fix removed a barrier from that
loop: correct on macOS, a no-op on Linux, and nobody re-measured on Linux.

**Mechanism.** One `msync` over the `[min, max]` span of the dirty set, plus the
meta flush. Span *width* is free — 4 MiB to 1023 MiB over a 1 GiB mapping
measures flat on ext4 and xfs, because writeback is driven by the dirty tag, not
the range; the 4 MiB arm is the *slowest*, which is the tell.

**Evidence.** 4.05 → 2.02 `msync`s per durable commit (`strace`). Commits per
`msync` under 4 contended writers: 0.73 → 1.34. Throughput +41.5 % (1 writer)
and +55.7 % (4 writers) on a loaded box, +45.0 % / +63.3 % idle; the
non-durable class is the control at 0.994×.

**The diagnosis that was wrong is the interesting part.** The hypothesis was that
group commit was not forming. It was: 2.82 independent transactions per batch at
4 writers. What was broken was the *flush count per group*. And a paired probe
showed `msync` and `fdatasync` cost the same — one device flush — so the
write-ahead log is not cheaper *per* flush, it is cheaper because it issues one.

---

## 3. Plans as content-addressed objects

### 3.1 The plan hash is the only key [C]

**Problem.** Several processes share one file and hand each other compiled query
plans through it. If the shared name were the *question* ("the plan for this SQL
text"), two processes holding slightly different statistics would both claim that
name for different plans, and whoever loaded it would get something other than
what they published.

**Mechanism.** `PlanHash = blake3(canonical plan bytes ‖ schema hash ‖ format
version)`, and the shared registry has **no other key** — the SQL text is stored
inside the record but is never an index. A different plan is different bytes is a
different hash, *by construction*. The failure mode "one hash naming two plans"
is not avoided, it is unreachable.

Everything else follows from that one property:

- **Stale cost input is safe, and nothing has to detect it.** A plan whose
  statistics have since moved is *stale*, not *wrong* — reordering an inner join
  chain preserves the result set exactly — so an old plan remains a correct
  answer, only possibly a slower one.
- **Statistics inform costing, never identity.** Measured history lives *beside*
  the plan, keyed by `(plan hash, statistics epoch)`. A better plan is a **new**
  hash; the old hash keeps naming exactly what it always named. This is what lets
  adaptive re-optimization be added later without a new invalidation mechanism.
- **Shared memory is hostile.** The load path re-validates the blob against the
  schema — including **recomputing the footprint rather than trusting stored
  flags** — then checks that the bytes still hash to the key they were found
  under, degrading anything else to "unknown plan" so the caller re-prepares.

**Provenance.** [C]. Content-addressed storage is old; server databases key plan
caches by SQL text plus session settings and hold them per process. The
combination — cross-process, in a mapped file, with the fingerprint as the *only*
key, and the staleness argument that falls out — is the unusual part.

### 3.2 Footprints: pre-computed locks in an embedded database [X]

**Problem.** A database normally discovers what a statement touches *while
running it*, taking locks as it goes. That is why it can deadlock, and why it
cannot schedule.

**Mechanism.** Every compiled plan carries, computed at prepare time and
*recomputed on every decode*: which tables it reads, which it writes, which
indexes it uses, what key access it performs, and whether it is read-only. A key
access is `Point` / `Range` / `Full`, where a `Point` is expressed in terms of
parameters and constants — **computable from (plan, parameters) alone, without
executing**.

**Provenance.** This is Calvin-style deterministic-database thinking (Thomson et
al. 2012, and the H-Store/VoltDB line): determinism comes precisely from knowing
each transaction's read/write set up front. That is a *distributed, sharded,
high-contention server* technique. mpedb applies it inside a **single-file
embedded database**, where nobody normally bothers because there is no scheduler
to feed. That is the transfer, and it is the entry in this document with the
clearest lineage to a named body of work.

**The honesty rule, which is a design principle here and not a caveat.** Exact
key-level write sets exist only for primary-key point operations on tables
without read-dependent index maintenance. Everything else — `UPDATE … WHERE email
= $1`, multi-row inserts, anything with a subquery — degrades to a table-level
`Full`. Enforced in code: the moment a second table joins, key access degrades,
because *"a claim narrower than the truth is a claim that rows this statement
does read are rows it does not. `Full` is the only honest answer the type can
express — it costs conflict precision, never correctness."*

**What it feeds today.** Read-only statements route to a snapshot and never touch
the writer lock; the batch drain's deterministic sort key; multi-file routing.
Conflict-based grouping is designed but has no production caller (§9.7), and
prefaulting is described in the design as a consumer but is **not implemented** —
both stated here so the idea is credited as *partly built*.

**A rule discovered by widening the table-id space.** The same `x & (N-1)` fold
was a latent corruption in one place and provably correct forever in another. The
discriminator: **a conflict signal may alias (it costs a false positive); an
identity map may not (it changes whose data moves).** That rule let the project
keep code that looked wrong and fix code that looked fine.

### 3.3 Group commit through an intent ring [C]

**Problem.** Two hard ones. Making a commit survive power loss costs one disk
flush — hundreds of microseconds to milliseconds — whether you wrote one row or
fifty, so serialized writers pay N flushes for N transactions. And the mailbox
you use to hand results back is a *reusable* resource: a process posts a result
into slot 7 while slot 7's original owner has timed out and a different process
has re-reserved it, so the answer lands in the wrong process's mailbox.

**Mechanism.** Writes are published as `(plan hash, parameters)` into a 256-slot
shared table; whichever process holds the writer lock becomes leader and executes
every pending intent in **one** transaction under per-intent savepoints, with one
meta flip and one flush for the whole batch.

Four ordering rules make posting incarnation-safe, and reordering any of them
reintroduces a stress-reproducible phantom-result race:

1. Posts happen under the writer lock.
2. The result store precedes the READY→DONE transition — so a leader dying
   mid-post leaves a slot whose waiter already has its result.
3. Owners may release from READY as well as DONE.
4. Recovery never acts on DONE slots. *"Acting on DONE slots here was a confirmed
   TOCTOU: a stale header plus the new incarnation's zeroed result state let a
   recovery poison a fresh intent."*

**The clever piece.** Batch consumption is made atomic with the meta flip by
stamping each drained slot with the transaction id the batch *will* commit as,
and comparing that stamp against the committed meta afterwards: `≤` means the
batch landed, `>` means the flip never happened and nothing was visible. Reusing
the meta's own counter as the batch's commit oracle needs no batch-sequence
counter and imposes no contiguity requirement, so a slow enqueuer never blocks
intents behind it. This replaced a sketched sequence counter during review.

**Self-clocking.** The ring engages only for durable modes. Slower media → longer
flush → more intents queue → bigger batches, with no tuning knob.

**Measured.** 2.9× durable write throughput at 10 contended writers (5.4k vs 1.9k
ops/s); 2.65× on the mixed workload. Batches per second 280 → 709.

### 3.4 A deterministic work counter instead of a timeout [X]

**Problem.** One bad query — an accidental cross join, an unbounded correlated
subquery — can exhaust memory or wedge a process others are waiting on. The
obvious guard is a timeout. **A timeout measures the machine, not the query**: it
passes on the laptop and fires on the loaded CI box, and the abort point is
non-reproducible, so you cannot write a regression test for it.

**Mechanism.** Count data-driven work units — rows yielded, join candidates
considered, correlated re-evaluations — and abort with an attributed error. Every
increment is data-driven: never time, never random, never dependent on an
environment flag. The counter is charged per outer row of a correlated subquery
*before* the memo lookup, specifically so that disabling the memo does not change
the count.

A second, memory-proportional budget counts *live* join cells, because *"the work
counter bounds what a query READS; it cannot see what a join HOLDS."* The
motivating case materialized gigabytes while still far under a billion work rows
— the process died on an allocation failure before the counter ever tripped.
Cells are released when a stage is superseded, so a legitimate multi-step join is
charged for its *peak*, not its history.

**Provenance.** Statement timeouts are [T]. Deterministic work-unit budgets are
borrowed from smart-contract gas metering and lockstep simulation. The repo
applies the same policy consistently: bounded view depth, subplan count,
expression depth, trigger depth.

---

## 4. MPEE: a route optimizer pointed at query plans

MPEE is an offline vehicle-routing engine from another repository by the same
author ([github.com/punnerud/mpee](https://github.com/punnerud/mpee)). Its
concepts were carried into query planning. This is the clearest cross-domain
transfer in the project, and — importantly — it includes a **falsification**: the
headline idea was implemented, measured, and killed before the right target was
found.

Before this, mpedb had no join-order solver at all. Join order was the user's
textual order, left-deep, always.

### 4.0 The mapping

| routing | query planning |
|---|---|
| node = place | node = one table in one `FROM` scope |
| edge = road | edge = a predicate connecting two tables |
| the N×N cost matrix | the join-order search space for that scope |
| a region behind a cut vertex | a sparsely-attached subgraph |
| a roundabout collapsed to a point | a subquery collapsed to its interface |
| streaming the matrix | enumerating only reachable partial orders |
| a time window or forbidden turn | a `LEFT JOIN`, a security policy |

**Problem, stated without database vocabulary.** A query names N tables and some
pairwise constraints. The engine must visit them in *some* order, carrying the
partial answer forward. Visiting in the order the user typed can mean carrying
the *product* of everything seen so far — millions of rows — before any
constraint applies. A different order makes every step a one-row lookup. Same
answer, same data, the difference between 0.2 seconds and the process dying.

**The retargeting, which is the honest part.** The decomposition was first staged
against *batch scheduling*, and the measurement said no: **the pages copied by
one copy-on-write transaction are a property of the key *set*, not the visit
*order*, so nothing in the commit path is path-dependent.** The query graph *is*
path-dependent. Same mechanism, right target.

### 4.1 Three honest predicate classes, and optimizing the worst case [C]

To choose an order you need to know how many rows survive each step. For `id = 5`
on a unique key you know: one. For `name LIKE '%x%'` you cannot know without
running it. Every mainstream optimizer plugs in a made-up selectivity constant
here, and when the guess is wrong the plan is catastrophic.

mpedb refuses to guess. Three classes: **KNOWN** (full primary-key or unique
equality → exactly 1), **BOUNDED** (upper bound = the table's row count, nothing
tighter), **UNKNOWN** (nothing). BOUNDED and UNKNOWN are priced *identically*, at
the full row count.

> The solver optimizes the **worst case**, not the expected case. A solver that
> maximizes expected speed can still choose a plan that explodes; one that bounds
> the worst case cannot.

Robust plan selection is a known minority position in the literature; the
refusal to invent a constant *anywhere* is the sharp commitment.

### 4.2 Magnitude bucketing buys stability, not safety [I]

The only statistic consulted is the catalog's transactionally-exact row count,
and only through `bucket(n) = 64 - leading_zeros(n)` — a power-of-two magnitude.
Costs are therefore sums of logarithms.

The reason is **not** accuracy. It is that the chosen plan is content-addressed,
so if the cost input moved on every insert, the plan's *name* would churn and
thrash a shared registry. Quantizing means a table must **double** before any
comparison can flip. Safety comes from §3.1; bucketing only buys agreement
between two processes reading snapshots thousands of commits apart.

No histograms, no distinct-value counts, no sampling, no statistics catalog.

### 4.3 A lexicographic cost vector whose tie-breakers read no statistics [C]

Four integers compared in order:

1. **`worst_log`** — 0 if KNOWN, else the magnitude bucket. Sums to log₂ of the
   worst-case product.
2. **`cartesian`** — 1 when a step has no predicate linking it to anything
   already read. *"A cartesian step multiplies the intermediate by the whole
   table with certainty; a linked step multiplies it by at most the whole table.
   Same upper bound, categorically different risk — and this term is purely
   structural, so it needs no statistics at all."*
3. **`late_unconstrained`** — pushes wholly unconstrained scans to the end.
4. **`residual_late`** — charges filters that run late. Because predicate
   pushdown evaluates a condition at the step placing its last table, *when* a
   filter runs is a **choice the order makes**. Sits last, so it only decides
   among candidates the first three rate identically — exactly the population
   that previously fell back to the text.

Every term is a sum over steps depending only on `(placed set, table, position)`,
which is what makes the dynamic program legal: the cost of a set is independent
of the order the set was built in.

**Worked example.** 17 tables, 10 rows each, joined in a path where one direction
of each edge is a primary-key probe. Textual order: `worst_log` 32, six cartesian
steps, intermediate reaching 10⁷ rows. Solver: `worst_log` 4, zero cartesian
steps — start at one end of the path and walk it. 2⁴ against 2³².

### 4.4 Collapse = connectivity restriction [T mechanism, [X] derivation]

A dynamic program over subsets, level by level, whose **state set is restricted
to the join graph's frontier**. A district hanging off a city by two roads can
only be entered and left through those two roads, so you never consider tours
that dip in and out of it.

For the 17-table path graph, the connected subsets number **153 instead of
131,072**, and the search is *exact*. No separate decomposition pass is needed:
restricting expansion to the frontier **is** cluster-first decomposition, with
the boundary handled by the dynamic program itself.

State is held in an ordered map, not a hash map, **because iteration order is
part of tie-breaking and must be byte-identical in every process**. The bounds
(exhaustive below 12 tables, capped state count, a ceiling deliberately set
*above* the plan format's join limit so the solver is never what refuses a
statement) are functions of the *statement*, never of the catalog — so two
processes run the same algorithm as well as the same cost model.

**Provenance.** Connectivity-restricted enumeration is established practice
(DPccp/DPhyp, Moerkotte & Neumann 2006; PostgreSQL does it). **Do not claim it as
invented.** What is repo-specific is arriving at it from the routing analogy, and
the observation that the frontier restriction, the roundabout collapse, and the
matrix streaming are *one mechanism with three descriptions*.

The same mechanism handles recursion: a subquery is a bounded scope attached
through a narrow interface, so by the time the parent's chain is solved it is
already one collapsed node. *"Decomposition and recursion are one mechanism, not
two."*

### 4.5 Extremal sampling — and where the analogy stops [I]

> *Take the far south, then the far north, then west and east — the 4×4 among
> those will probably find the arterial roads and the junctions. Add one more
> between each pair, and often you never need the full N×N.*

The query-graph analogues: a table already KNOWN with nothing placed (the
strongest restriction a table can carry), the smallest table, the largest table,
and the **highest-degree node** — the junction every path tends to cross. Then
widen the seed set to those nodes' frontier ("literally the node between each
pair"), then to everything. **Stop the first round that does not move the
decision**, because widening further can only buy more search for the same
answer.

**Where it stops, which is the honest half:**

> A road solution is a **route between endpoints**: the extremes bracket it and
> interior points refine what happens between. A left-deep join order has a
> **start but no end** — it is a permutation whose cost compounds from position 0
> outward, so the first choice dominates and there is no far endpoint to bracket
> against. Extremal sampling therefore does not transfer as "solve the 4×4 and
> interpolate"; it transfers as **seed selection plus hub identification**, which
> is the half that carries the value.

And the deflation: it only runs above 12 tables or when the exhaustive search
blows its cap, so on the test corpus — maximum 3 tables per statement — it
essentially never fires. This is the most plausible genuinely-not-textbook item
in the cluster, and also the one with the least exercise.

### 4.6 Ping-pong: the solver steers which cost input gets bought [C]

Every distance cell costs something to look up. Rather than buying the whole
matrix and searching it, guess optimistically low, find the best route under that
guess, then buy only the cells *that route's cost rests on*. If the winner's own
cells are all paid for, its cost is exact while every rejected route was scored
with an underestimate — so no rejected route can secretly be better. Stop.

Soundness: every cost term is monotone non-decreasing in a table's bucket, so an
unbought table priced at a lower bound makes *every* candidate a lower bound.
Termination: each non-stopping round buys at least one count; capped at three
rounds, after which it buys everything and solves once — which is bit-identical
to the old eager behaviour, so the whole mechanism is bounded at four searches.

**The lower bound must be 1, not 0.** Zero is unconditionally safe and also
useless: with every unbought table at 0, the leading cost term is 0 for *every*
candidate, the first round decides on tie-breakers alone, and the solver buys
counts for an order it is about to discard — **measured as 5 of 6 counts instead
of 1**. At 1, the leading term of an all-unbought round *is* the count of
un-probed steps, so the first proposal is the one the solver can most cheaply
certify.

| chain width | eager reads | ping-pong |
|---|---|---|
| 6 | 6 | **1** |
| 10 | 10 | **1** |
| 17 | 17 | **1** |

The honest converse is a second test: a 4-table chain joined entirely on non-key
columns is decided by size alone and buys 3 of its 4 counts. **Laziness is a
property of the question, not a trick that always wins.**

And what it does *not* buy: a row count is one B-tree lookup, so one probe versus
seventeen is not a wall-clock story today. The value is that the seam is now
demand-driven, which is the precondition for a measured cost catalog.

### 4.7 Constraints price, they do not veto [I]

The first version *refused* to reorder any scope containing a `LEFT JOIN`. The
routing framing says that is the wrong shape of answer:

> In vehicle routing a time window, a capacity or a forbidden turn is not a
> reason to abandon the route — it is a constraint the solver prices and searches
> the feasible region under. The old eligibility list was a list of situations
> where the solver gave up.

So an outer join becomes a **barrier**: pinned at its own position, with every
maximal inner run between barriers freely reordered. Sound because
`(A ⋈ B) ⟕ C ≡ (B ⋈ A) ⟕ C` — reordering inside a run cannot change what the
following outer join sees, because the run's row set is identical either way.

Three consequences fall out free: barriers are **cut points**, so each segment is
a smaller problem — §4.4's collapse arrived at from the other side;
segment-local optimization is **globally optimal**, because a segment's internal
order cannot change the set any later segment sees; and each barrier keeps its
condition *on its join*, because moving it into the `WHERE` turns "does this row
match" into "does this row survive".

**Measured:** a 10-table chain plus a `LEFT JOIN` went from `runtime budget
exceeded: 200010 live joined cells` to **1 row, 0 cartesian steps**.

**What still refuses, and why — each named, not overlooked.** `FULL JOIN` (both
sides null-extend, so predicate pushdown is disabled entirely and the rewrite has
no counterpart). `USING`/`NATURAL` (desugaring picks the leftmost occurrence of a
shared column, which reordering would silently move). And **row-level security**,
which is the sharpest:

> A reorder changes which pairs a predicate is evaluated over, and mpedb *raises*
> on arithmetic overflow — under a policy scope a raise is an information
> channel, not just an error. Pricing that would mean proving no reachable
> reorder changes the set of raises a policy-scoped query can produce: a much
> stronger claim than preserving the row set, and not one this solver can make. A
> named refusal is the honest answer.

The general consequence is stated rather than discovered: a query that raised may
stop raising, or vice versa. This is inherent to join reordering in *every*
engine that does it. The **row set** is unchanged, which is what "0 wrong
answers" measures.

### 4.8 Evidence

| | before | after |
|---|---|---|
| `select4.test` | 447.2 s | **22.8 s** (19.6×) |
| `select5.test` | 186.7 s, 871/1436 | **1.0 s**, 872/1436 |
| the 17-way join | **out of memory** | answers, hash-verified |
| corpus regression (9,689 records) | 9,489 pass, 0 genuine wrong | **byte-identical report**, timings excluded |

That last row is the licence for the whole thing. A join-order solver that
changes one answer is not worth any speedup.

**Memory, measured rather than asserted.** "It used to OOM" is a memory result,
but a qualitative one; the numbers above are all wall clock. `MPEDB_NO_MPEE=1`
puts both arms in one binary, so the memory case is now a paired A/B on one
machine (Apple M3 Pro, `design/DESIGN-MPEE-SOLVER.md` §10.2 for method, spread
and the full tables). The honest headline is not RSS but **peak live join
cells** — `max_join_cells` trips on `live > budget`, so bisecting the budget
recovers the peak *exactly*, and it is deterministic: a property of the engine,
not of the machine, the allocator or the timer.

| chain width | solver ON | solver OFF | |
|---|---|---|---|
| 4 tables | 100 cells | 460 | 4.6× |
| 8 tables | 260 cells | 90,000 | 346× |
| 12 tables | 420 cells | 13,400,000 | **31,905×** |
| 17 tables (`join-17-4`) | 930 cells | > 64,000,000 (cap) | > 68,800× |
| `select5.test`, peak RSS | **9.98 MB** | **4.92 GB** | **493×** |
| `select4.test`, peak RSS | 626 MB | 3.23 GB | 5.2× |
| ordinary 3-table join | 686 cells | 686 cells | **1.00× — no effect** |

The solved order is `40n − 60` cells: **linear** in the width. The textual order
gains **a factor of ten per table added**. Linear versus exponential is the
result, and it is the largest single effect measured anywhere in this document.

The last row is the negative result, and it is the one worth reading twice (§9):
on the corpus-median shape — a filtered fact table joined to two dimensions by
their primary keys — both arms pick the *same order*, hold the *same* 686 cells,
and finish within each other's noise. This technique pays on adversarial shapes
and is free on ordinary ones. It does not pay everywhere. The surprise was on
the cost side: compile time is **non-monotone** in the chain width, peaking at
2.26 ms for 12 tables and falling to 106 µs for 17, because `DP_FULL_MAX = 12`
is where the exact DP hands over to extremal sampling. The solver plans a
17-table join **21× faster** than a 12-table one.

### 4.9 The bug that became a constraint [I]

Correlated subqueries are lifted *before* join dispatch, so their arguments name
row slots in **textual** order — and a reorder left them pointing at the wrong
columns. This was a genuine wrong answer during development: an aggregate with a
correlated filter returned 1 where sqlite returns 2. Version 1 responded by
refusing the whole scope; version 2 **applies the permutation** to them, with the
registry's decode path re-validating every argument so a stale slot surfaces as
corruption rather than as an answer.

The pattern is the point: a wrong answer found in development became first a
refusal, then a priced constraint. Both are better than a silent wrong answer,
and the ordering between them is a roadmap.

---

## 5. Interoperability

### 5.1 The inversion: a sqlite `.db` as the base, a `.mpedb` as its write-ahead log [I]

**Problem.** You have a file format the whole world can read, and a faster engine
that speaks a different one. Normally you pick: import everything (and the
world's tools go blind) or stay slow.

**Mechanism.** The stock `.db` stays the canonical, durable, every-tool-readable
home. mpedb writes only into a sidecar overlay holding *deltas since the last
checkpoint*, merged per primary key on read. Checkpoint pushes the deltas into
the base **through the sqlite library's own transaction** — mpedb never executes
sqlite's write protocol.

**Why it is an inversion.** Base-plus-delta is standard (a log-structured tree
has a memtable and sorted files; sqlite has a database and a `-wal`). What is
inverted is *which format is which*: normally the fast engine owns the durable
format and a log is the delta. Here the **foreign** format is the durable home
and the **native** engine's whole file is demoted to the delta log.

**A non-obvious consequence.** Truncating pushed deltas runs in bounded batches,
because deleting delta rows is copy-on-write — it *allocates before it frees* —
so a one-shot truncate deadlocks against the very out-of-space condition the
checkpoint exists to relieve.

### 5.2 Persist the *conversion rule*, not the type [I]

**Problem.** A sqlite column declared `int` may genuinely contain the text
`'abc'`. Copy the declared type into a rigidly-typed engine and you build a
database that refuses to read rows the source happily holds. Drop the type
entirely and you write `'1.50'`-as-text where sqlite would write `1.5`-as-real —
a wrong answer, and your reconciliation then reports a divergence you caused
yourself.

**Mechanism.** Every non-key overlay column is typed as *per-value* plus the
base's declared **affinity** — sqlite's conversion rule — and mpedb applies that
rule itself. A value written through the overlay is *already* in the storage
class sqlite would have chosen, so the push is a no-op for affinity and the
predicted divergence cannot arise.

**The review got the failure right and the cure wrong.** It proposed sniffing a
concrete type from the data. The shipped answer is a third thing: **the invariant
sqlite guarantees is the conversion, so persist the conversion.**

Constraints that cannot be represented take the table **out of the attach by
name**, rather than being silently dropped — a constraint enforced nowhere lets
in a row the base itself rejects, and fails the checkpoint later on an unrelated
statement.

### 5.3 A captured base image makes conflicts provable [C]

**Problem.** Two writers touched the file. You have your value and theirs.
Without a record of what the row looked like *when you started*, you cannot
distinguish "they changed it" from "I changed it" — so every merge tool guesses.

**Mechanism.** At the *first* delta write to a key, the base's row image is
captured atomically with the delta, as a hidden column of the delta row itself. A
delta whose captured image still equals the current base row is *provably*
conflict-free. Anything else is a per-key conflict resolved by a named policy —
**counted and reported, never silently merged**.

Three states, and the third matters: image present, "no base row existed", and
"captured offline, unknown".

### 5.4 The optimistic read bracket, and the shortcut it replaced [T mechanism, [I] counterexample]

You want to read someone else's file without holding a lock all day. The obvious
cheap trick is "check a version counter before and after; if it did not move, the
read was clean". **That trick is wrong, and the counterexample is written down:**
a foreign writer can begin a transaction, spill dirty pages into the base
mid-transaction (documented cache-spill behaviour), then roll back — journal
playback restores every page *including the counter's pre-image*, all locks
release, and both checks plus the counter equality pass around a read of garbage.
Independently, the counter under-counts by its own specification: it increments
when the file is *unlocked after modification*, not per commit.

The shipped answer takes a real transient shared lock per statement — sqlite's
own reader ladder, [T] — and two rules fall out worth stealing verbatim:

- **A lock refusal is never divergence.** It means a writer is active, not that
  the data moved. Conflating them turns contention into a spurious
  reconciliation storm.
- **No output before the bracket closes.** Results buffer until the closing
  validation passes; nothing is streamed out of an unvalidated read.

### 5.5 The checkpoint marker lives in the *foreign* file [C]

You copy from A to B and then record "done" in A. Crash between and you cannot
tell whether the copy happened. Everyone knows to make this idempotent — but
**idempotence here is false**. Our lock dies with the crash, so the base is
unlocked all through the crash window and a foreign writer may commit. Re-pushing
would then overwrite their newer value and resurrect deleted rows.

> **Idempotence holds against ourselves, not against third parties.**

So the marker moved *into the base*, written atomically with the push, and
recovery validates the file stamp **before any re-push**; a moved stamp routes to
full reconciliation, never to a blind replay.

**A POSIX trap worth knowing.** Classic POSIX locks are per (process, inode), and
sqlite's own `close()` cancels *all* of the process's locks on that file. So the
checkpoint's own library use silently destroys a naive raw shared lock — every
checkpoint would open an unnoticed unlocked window.

### 5.6 The freshness stamp, settled against the filesystem's clock [I]

"Has this file been touched?" is answered by one `stat()` after minutes or days —
but file timestamps have coarse resolution, and if a mutation lands in the same
tick as your stamp it is invisible forever. So at capture time, while still
holding the lock, the process **touches a scratch file in a loop until its
timestamp is strictly greater than the candidate stamp**. The settle-until-
strictly-greater family exists in build systems; specifying it in the *file
clock* domain rather than the wall-clock domain is the part most implementations
get wrong.

The stamp itself is a tuple: timestamp, size, change counter, schema cookie, two
header bytes, and — if a write-ahead log exists — **its salt pair**, which is the
monotone witness the counter cannot be, because a log reset reuses the file from
offset 0 with new salts and unchanged size. Network filesystems are refused by
default: attribute caching defeats every part of this.

### 5.7 ABI-level drop-in, with the interposition itself under test [T]

`mpedb-capi` exports sqlite3's C symbols, so `LD_PRELOAD=… python app.py` runs
CPython's `sqlite3` module, Django, and their test suites against mpedb
unchanged. Prior art named: libSQL, DuckDB's sqlite3 shim.

Non-obvious constraints:

- CPython's `_sqlite3` resolves ~50 symbols **at load time**, and any one missing
  is a hard load failure — so every symbol must exist even when its behaviour is
  a refusal.
- The version string is a bare `X.Y.Z` because consumers parse each dotted field
  as an integer; the identity is moved to the source-id field nobody parses.
- `sqlite_master` and `PRAGMA` are emulated as a **pure function of the live
  schema**, and the reconstructed `CREATE TABLE` elides the hidden implicit key
  column, because emitting it makes a dump **replay as a different table**.
- The authorizer is driven by the *compiled plan's own footprint*, refined to
  columns by the same compile the statement will run — so an authorized statement
  and the executed one can never be two different plans. `SQLITE_IGNORE` is
  **refused by name**, because it means "read this column as NULL" and returning
  the real value would leak exactly what the callback hid. Joins widen to every
  column of every table read: **over-report, never under-report.**

### 5.8 Type provenance and a pre-flight that never touches the target [C]

**Problem.** Moving data between two systems, the destination is stricter than
the source in ways invisible until row 40,000 fails with the target half
populated. Worse: your intermediate store is *looser than both*, so it accepted
values neither end will take.

**Mechanism.** The import records the **source's** schema and a per-column
mapping policy, and pre-flight walks the local data against that recorded schema
— **entirely locally, with no connection to the target at all** — producing one
complete rejection report before the first insert.

Four policies, and the distinction is what the record exists for: `Exact`;
`ViaText` (preserved through a canonical text form); **`Widened`** (mpedb's type
is *wider* than the source's — import was lossless, but a local write can now
hold a value the source column cannot take, which is exactly what fails at the
target); and `LossyAtImport` (reported once at column level and **never per
row**, because the information is already gone and a per-row report would imply a
per-row fix that does not exist).

The `any`-typed column is **deliberately not resolved by sniffing the data**:
deriving `bigint` because today's rows happen to be integers would make the
target's schema depend on its content.

**The dialect rule, learned the hard way.** A recorded declared type only means
something *in its own dialect*, and the vocabularies collide: sqlite's `INTEGER`
is 64-bit where PostgreSQL's `integer` is 32-bit; sqlite's `REAL` is a double
where PostgreSQL's `real` is single. Reading a sqlite mirror with PostgreSQL's
rules rejected `5000000000` out of a column sqlite stores natively — a false
rejection that then blocked the export.

**The fidelity rule.** A read-write column requires `store ∘ push == identity` on
**all** representable values, *not just proven round-trip of source-origin
values*. Otherwise: pushing `'3.1'` into a `numeric(10,2)` makes the source store
`'3.10'`, echo suppression blocks the normalized value from returning, and you
get permanent divergence plus a re-flag storm.

### 5.9 Cross-file queries layered strictly above the reviewed core [C]

`ATTACH` is sqlite's feature, [T]. The technique is the **containment rule**:
name resolution happens *before* the parser, compilation runs against a
disposable merged schema, and cross-file plans are connection-local — never
published to the shared registry, never encoded. **So the plan wire format and
the footprint's per-file table domain are untouched: no format bump.**

The limit is stated by name: snapshots are **per-file consistent, not globally
serializable**. Justified by observing that sqlite's attached databases behave
the same way. Eleven v1 refusals are listed by name; the shape battery against
the bundled oracle found zero divergent answers.

### 5.10 A refusal standing in for a format [I]

Cold-data tiering moves rows to a second file with a strict ordering: select
under the hot lock → copy → **the cold side commits** → every copied row is
**re-read from a fresh cold snapshot and compared bit-exactly** (floats by bit
pattern) → only then does the delete commit. Killed anywhere, every row is in
hot, cold, or both — never neither. 40 of 40 kill waves converged.

The sharp part is what replaced a marker format. A re-drain finding a cold row
under the same key with **identical** content reconciles; with **different**
content it **errors and touches nothing**:

> After a crashed drain the hot side may have re-used the key, and without
> content-hash lineage "overwrite cold" would destroy the archive while "keep
> cold" would destroy the live row. Fail loud is the only markerless answer.

No on-disk marker format was added.

---

## 6. Method as technique

The verification doctrine is itself a set of transferable techniques, and the
three strongest are all **refusal-shaped** — each converts a judgement call into
a mechanical gate.

### 6.1 The oracle is bundled, not ambient [C]

Differential testing against sqlite is [T]; sqlite itself does it. The sharper
claim is that the oracle is **compiled into the test binary** and version-pinned,
so the ambient binary cannot silently change the answer:

> Every differential test used to shell out to whatever `sqlite3` the machine
> happened to have — so the same commit could pass on one box (3.45.1) and fail
> on another (3.51.0, different rounding), and **every such failure cost a human
> judgement: "real bug, or version wobble?"**

Pinning a test dependency is ordinary hygiene. Treating *the reference
implementation* as a pinned, reviewable artifact of the repo is not. Reproducing
the reference CLI's stdout byte-for-byte — including routing floats through
sqlite's *own* value-to-text conversion, and truncating at an embedded NUL
because the CLI prints C strings — is what let 53 test files convert from
subprocess to in-process without touching their parsing.

And the oracle's own semantics are a **golden file**: a contract test pins the
version-wobble cases to the pinned version's answers, so a dependency bump
produces a failing test that *is* the behavioural changelog.

### 6.2 ERROR versus FAIL: a refusal and a wrong answer are different measurements [C]

You borrow someone's 831-test suite and score 800. What does the 31 mean? Two
completely different things hide in that number: "we don't support that yet" (a
roadmap item; the user got a clear error and did not act on bad data) and "we
returned the wrong rows" (a data-corruption incident). One number makes the
second invisible.

Test frameworks have separated errors from assertion failures since JUnit — that
part is free. Nobody normally *uses* the split as the primary reporting axis.
Here it is the whole spine, and it is a **structural guard** because the two move
in opposite directions under the same pressure:

- Adding a feature converts ERRORs into passes — monotone, cheap to accept.
- Adding a feature can *also* convert an ERROR into a **FAIL**, if the new
  support is subtly wrong. Under a single pass count that looks like a small win.
  Under the split it is a stop-the-line event.

The budget for wrong answers is **zero**. Current standing, measured at one
commit on one machine: Django 826/831 with **0 FAIL / 5 ERROR**, `queries`
488/493 with 0 FAIL, CPython's own suite 450/474 whose 6 FAILs each reduce to a
refusal or a metadata string. The only deliberate FAILs anywhere are three
honesty positions where passing would mean claiming foreign-key enforcement that
does not exist.

### 6.3 Widening can create wrong answers — probe on VALUE *and* type [I]

Being more permissive feels like a strict improvement. It is not. **Every input
you newly accept is an input you must now handle correctly, and the previous
behaviour — a clear error — was a safe answer you just gave up.**

This has fired three times in this project, each time producing a real wrong
answer from a change that only *added* support. The best-documented case shows
why the procedure has two halves. Three designs were considered for making
sqlite's blob affinity work through the shim, and the oracle was probed on both
the stored value and its type *before* choosing:

- **(a) convert at bind** — **rejected: it is the wrong answer.** sqlite's blob
  affinity applies *no* conversion, so after `UPDATE t SET b='aaaa'` the column
  holds text. Under (a), `typeof(b)` answers `'blob'` where sqlite answers
  `'text'`, and `b = 'aaaa'` flips from true to false. **It agrees with sqlite on
  `length` and `hex` and disagrees on the two things a consumer branches on** —
  so a value-only probe would have *passed* this design. Only the type half
  killed it.
- **(b) per-value types** — chosen, but scoped to where sqlite's affinity is
  genuinely not a storage class. Applying it more widely would have been faithful
  *and* catastrophic: the planner never probes a per-value column, so every
  `varchar` key and every `bigint` index in a real application would have become
  a full scan. Noted honestly: *the blast radius is why (b) is scoped, not why it
  loses.*
- **(c) stay rigid and document the deviation** — rejected for these types, kept
  for the rest, because a fourth way exists that (c) misses: **apply the
  conversion and stay rigid about its result.** `'12'` into an integer column
  stores 12 exactly as sqlite does; `'abc'` is still refused, because sqlite
  keeps the text and a rigid integer cannot.

The general law:

> **Agree or refuse, never differ.**

Its assertion form is two-sided: on acceptance the value *and* the type must
equal the oracle's; on refusal the oracle must have stored something the rigid
column structurally cannot hold. That is what makes compliance a *number* (zero
wrong answers) rather than an aspiration, and it is what makes the ERROR/FAIL
split meaningful — under this law, ERROR count is a roadmap and FAIL count is a
defect count.

The same reasoning one level up decided that `typeof()` reports **exactly** one
of sqlite's five storage classes, never a sixth: it is a *borrowed function* and
it borrows the oracle's **range**, not just its name. A consumer switching on it
takes the wrong branch on an answer that does not exist upstream.

### 6.4 Refuse to measure a harness you cannot prove ran [I]

Your A/B compares "with my library" against "without". If the "with" arm silently
fails to load it, you get a perfect score that means nothing — **and a perfect
score is exactly the result nobody investigates.**

This happened. A macOS run reported 831/831 with timings identical to stock,
because `DYLD_*` variables are stripped when executing a system-protected binary,
and the wrapper chain went through `/usr/bin/perl` (macOS has no `timeout(1)`).
The shim was never loaded; stock sqlite answered every query. The pre-flight
check passed **because it used a shorter path than the measurement did.**

Two fixes, both general: re-add the variable *after* the stripping point, and
require the self-check to run through the **identical wrapper chain** the
measurement uses. The gate now aborts the whole run on a match, and the published
proof is a version pair plus content evidence — the shim arm's errors carry
wording no sqlite can emit.

> **A shim arm that scores identically to stock is a broken harness, not a better
> engine.**

### 6.5 One commit, one machine, one control build [T, rarely done]

Numbers accumulated over months at different commits on different machines cannot
be subtracted from each other, and a measurement-tool improvement looks exactly
like a product improvement.

The procedure: re-take every headline at a single commit on a single machine, and
attribute any delta by **building a control of the prior commit** and diffing per
file. When a corpus sweep improved by 3 records, a control sweep at the ancestor
commit reproduced the old number byte-for-byte, the per-file diff moved exactly
two files, and the only change to the runner in that range was one commit.
**Verdict: measurement artifact.** The engine was unchanged.

The corollary is stated as loudly as any success: the join reorder, the compound
rewrite, generated columns and the rounding fixes all landed in that range and
moved **zero** records — and, more importantly, produced **no new wrong answer**.

### 6.6 Ablate your own workarounds, and freeze the comparability set [C]

If you patched the *consumer* to get its suite running, your score measures your
patch. So every adaptation is periodically **disabled and re-measured**, and the
result is published even when unflattering — including one recorded as *"NOT
root-caused; the direct probe is still owed."* The label groups are frozen: new
coverage is added as a *new* group, never by editing the frozen ones, so the
trend line stays meaningful.

Two arm-asymmetric skips are recorded purely for honesty: four tests self-skip
under the shim because mpedb enforces no foreign keys, and one is gated on a
sqlite version the shim does not claim. They do not change the scores, but they
mean a handful of tests the reference passes are not actually exercised.

### 6.7 A cost that scales with the substrate is invisible to the obvious metric [I]

Not a technique so much as a failure mode worth naming, because it cost this
project a wrong number before anyone knew the cost existed.

Compiling a statement was **O(bytes ever registered in the shared key space)** —
two full scans, 297 bytes held and 0.24 µs per plan that had ever been
published. Not per plan *live*: the registry evicts at 4,096 entries, and
eviction dropped the cost by exactly the 6 % it dropped entries, which is what
proved the cost was over the *substrate* and not over the working set.

Nothing pointed at it, and here is why. Every per-operation benchmark measures
one operation against an empty or fresh database, where the term is zero. Every
correctness test does the same. The cost only appears in a database with
*history*, and by then it looks like the database being "slower when full",
which is a thing databases are expected to be.

**It then corrupted an unrelated measurement.** A memory probe reported a bare
primary-key point lookup holding *149 bytes per row of the whole table*, which
is an absurd shape for a point lookup and was accepted anyway because the number
came out of an instrument. Through `execute(hash, params)` the same lookup holds
**618 bytes flat**. The 149 B/row was this scan, and it entered the report as a
property of the lookup.

Two rules fall out, and they generalise past this instance:

- **A per-operation benchmark cannot see a per-substrate cost.** If a cost is
  shared, vary the *substrate* — here, registry size at 0 / 1k / 4k /
  post-eviction — and the post-eviction point is the one that distinguishes
  "grows with what is live" from "grows with what has ever existed". A ratchet
  and a working-set cost look identical until you shrink the working set.
- **A slope that is absurd for the operation is a slope from somewhere else.**
  149 bytes per row of a table a point lookup never reads is not a surprising
  result about point lookups; it is an instrument reading through to something
  it did not mean to include. The reflex should be to doubt the attribution
  before publishing the number, not after.

The fix itself is not interesting — prefix-bounded ranges over a B+tree, which
is [T] and gets no credit here. What the entry records is that the cost was
structurally invisible to every instrument pointed at it.

### 6.8 Smaller standing rules

- **Truncation at every offset** [T, under-practised]. Every decoder is fed its
  own valid output truncated at *every* byte offset, asserting a clean corruption
  error and never a panic. Files get cut short — a crash mid-write, a partial
  read, a bad backup — and a parser that panics turns a recoverable data problem
  into a crash. Testing "a few malformed inputs" tests your imagination; testing
  every prefix tests the parser. It is a `for cut in 0..len` loop: deterministic,
  no fuzzing infrastructure.
- **Model tests against standard collections** [T]. The B-tree is checked against
  `BTreeMap` under a random operation stream. Generalized twice: the mirror's
  switch tests use the *source database* as the model, and the multi-process
  collision fuzzer uses whichever side is authoritative as the model.
- **Deterministic seeded RNG, no dependency** [T]. Every failure reproduces from
  its seed alone. A randomized test that fails once and never again is not a
  test, it is a rumour.
- **Multi-process behaviour is tested through the product's own CLI** [C]. Crash
  recovery, writer collisions, power loss, mirror convergence, tier drains — real
  forked processes, real `SIGKILL`s. The bugs that matter here cannot be
  reproduced in one process, and a unit test that mocks the second process tests
  the mock. These harnesses ship as *subcommands*, not test scaffolding, so they
  run against any build on any box; and each asserts **convergence against a
  model**, not merely "did not crash". Power-loss testing applies the
  truncate-at-every-offset idea to a whole file.
- **Numbered adversarial findings, permanently marked in the design** [I]. Major
  designs carry `[R#n]` / `CONF#n` markers at each fix, with the standing rule
  that *building anything contradicting them re-opens a named finding*. One
  design absorbed 58 confirmed defects; another 20; the core protocol 37. The
  review being **wrong** is recorded too: *"[R#17] named the right failure and the
  wrong cure."*

---

## 7. The shape that recurs

Four of the mechanisms above are the same idea stated four times:

| | stores | instead of |
|---|---|---|
| sqlite overlay | the base's **affinity** (conversion rule) | a sniffed concrete type |
| mirror | a **mapping policy** | a source type name |
| delta rows | the **base row image** | a conflict flag |
| test harness | a **pinned** oracle | whichever one is installed |

**Persist the invariant, not the value.** A type name is only meaningful in its
dialect; a conversion rule is meaningful anywhere. A conflict flag is someone's
conclusion; a base image lets anyone re-derive it. The version installed today is
an accident; the version pinned is a decision.

The second recurring shape is **fail loud instead of adding a format**: the tier
drain refuses an ambiguous re-drain rather than introducing a lineage marker; a
plan-format bump fails closed with a re-prepare rather than attempting migration;
an unknown durability tag refuses the attach; a stale record decodes as corrupt
rather than as a wrong capture set. Every format break is engineered to be loud
at the earliest possible point.

---

## 8. Designed, not built

Everything below is a proposal. Each entry states **what becomes possible that is
not possible now**, because an idea list without that is a wish list. Where the
value rests on an unmeasured assumption, it says so.

### 8.1 Workload-derived indexes — the one with a measurement

**Claim.** The index set an application needs is *derivable* from the plans the
database has already compiled, because the plan registry keeps the SQL text **and
the full compiled plan** — so "which columns does this app filter on" is a query,
not a sampling guess.

**What becomes possible.** Today a developer hand-designs indexes and re-issues
them each migration. After this: run the app's test suite, pipe the statement
stream in, and get the exact index DDL for the whole schema, **costed, before a
single production query has run.** The alternative — learn indexes from live
usage — makes the first N queries pay; this removes the cold start, and adaptive
refinement only ever corrects what the offline pass got wrong.

**Why it is cheap.** `prepare` is a pure function of (SQL, schema, row counts), so
the advisor clones the schema, adds a hypothetical index, re-prepares, and reads
the cost both ways. **No file is touched.** That is `hypopg` without the
extension, and it is the strongest argument for doing this *inside* the database
rather than as an external tool.

**Measured** (115,612 corpus records, 0 wrong, run twice byte-identical): 99,279
compiled statements collapse to **112 distinct index candidates over 6 tables**;
32 candidates cover 94.2 % of occurrences; key widths are 1–3. That is an
**enumeration, not a search** — small enough to cost exhaustively, with no
pruning heuristic and no risk of missing the good one, and it grows *linearly* in
tables, so a 200-table schema is a few thousand candidates.

The count is also **parameterization-robust**, which is what lets it be quoted at
a real application: admitting literal equalities as predicates added *zero*
candidates, because an equality atom becomes a **key column, never a predicate**
(`WHERE status = 'active'` is better served by an index keyed on `status`, which
serves every value, than one restricted to one value).

**Identity.** `blake3(version ‖ table NAME ‖ unique ‖ per key: name, collation,
direction ‖ predicate)` — names, not positions. `CREATE INDEX` becomes:
canonicalize, hash, look up; present means built, so **an app re-declaring its
whole index set costs one hash and one lookup per index**, with no rebuild across
version bumps. mpedb is already most of the way there by accident: the index name
is not persisted, so creation is already idempotent *by shape*.

**The open risk, in the design's own words.** The *partial*-index half rests on a
predicate class the test corpus structurally cannot exhibit — its schema is
random numerics with no soft-delete, no status enum, no tenant key. The candidate
count transfers; whether partial indexes are the main event or a footnote does
not. **Capture a real application's statement stream before building.**

**Prerequisites named rather than designed around**, and one is a wrong-answer
blocker: the planner picks an index *the instant it appears*, so a background
build needs a state bit or a half-built index returns too few rows. Also: `DROP
INDEX` does not exist, which makes auto-create a **ratchet** — so auto-create is
gated on it.

### 8.2 Delta-compressed index keys — the polyline

**Claim.** Store a run of sorted keys as one anchor plus relative offsets,
exactly as a route is stored as relative points.

**What it would buy.** Three things, and they are not equally solid:

- **Compression without a decompression step.** A general-purpose compressor
  requires materializing the page before a binary search can run, trading CPU on
  every descent. A delta run can be *searched in its compressed form* by walking
  from the anchor. This is the structurally interesting claim.
- **Denser pages, which compound here specifically**: fewer pages means fewer
  copy-on-write copies per mutation (every mutation copies the root-to-leaf
  path), and a shallower tree means fewer page touches per descent — which
  becomes fewer *network round-trips* in the HTTP build below.
- **An insert rewrites only the anchor, not the 999 that follow.** ⚠ **Recast
  this for mpedb.** In a copy-on-write tree an insert already copies the whole
  page regardless, so the value is *not* fewer bytes written — it is fewer bytes
  re-encoded and no cascading re-delta down the run.

**Status: task only, no design document.** The one measurement that exists is on
a *different* structure — 3-element table-id sets — where the encoding won 62 %
but the verdict was **don't build**, because the absolute saving was 4.78 bytes
per record. That is the *least* favourable shape: the same measurement shows
73–75 % on wide dense sets, which is where index keys live. So the existing
number neither supports nor refutes the real case.

### 8.3 Execution-time ping-pong

**Claim.** Once position 0 has actually drained, the executor knows the one
number the compiler had to *bound* rather than know. If reality contradicts the
assumption by more than a magnitude bucket, re-solve the **suffix**.

**The law that makes it safe**, quoted in the design because it is already law
elsewhere: plan bytes are immutable under their hash, so **a persisted better
plan is a NEW hash**, and re-decision therefore lives on the *strategy* side, not
the plan side. An executor may change which side of a loop is held versus
re-probed, whether a memo is built eagerly, how a recursive term materializes —
never the bytes.

The prefix already emitted stays valid **for inner joins**, because an inner
chain's row set is order-independent. Which is exactly why outer joins need care:
a suffix re-solve must treat every barrier as immovable — the same segmentation
§4.7 already defines.

**Blocked on** two things that do not exist: a per-position row counter visible to
the planner mid-statement, and a re-entry point for a partial re-solve.

### 8.4 A self-tuning cost catalog

**Claim.** MPEE's cost function becomes a **registry of per-access-method
estimators** rather than a fixed formula, fed by persisted statistics maintained
by incremental counters and background sampling — **never on the write hot path**.

**What becomes possible.** The concrete payoff is *cross-modal comparison*: when
a vector index lands, it registers its own recall-versus-cost profile, and the
solver weighs a similarity lookup against a full scan against a full-text match
**on the same footing, with no new optimizer code.** Full-text search already has
a deterministic cost shape (rarest term first). Nothing in the systems this cites
does that.

**The measurement already changed the design.** A census found 94,689 statements
→ 81,036 plans → 119 footprints → 22 table sets. Keying a cost catalog on the
*shape* would pool plenty — but the plans it pools have wildly unlike costs: the
across-plan spread inside one bucket is **20–52× the irreducible floor**, and
worst-to-best inside a bucket is a median **217×**. **Verdict: key on the plan
hash.** The footprint stays a legitimate coarse *index* (which plans touch table
T; invalidate everything that reads T) and is not a cost-sharing key.

**Everything plugs into one seam.** The solver reads cost through exactly one
function, consumed only as a magnitude bucket. When measured history arrives that
widens — **and the solver code does not change.**

### 8.5 A database served from a static file over HTTP

**Claim.** A browser reads a **live, possibly-mutating** `.mpedb` off a plain
static host via range requests, with no format change and no cost to the local
path — because the page store is already the seam.

**What becomes possible.** Publish a file to S3 or GitHub Pages and a browser
runs real SQL against it, fetching only the pages a query needs. No API server,
no database server, no export step.

**The measurement that decides it.** Per-query descent is *already tiny*: a point
lookup is 3 pages, a 100-row range is 5, an index point is 6. The expensive part
is the **one-time bootstrap** — 322 scattered pages. So viability is a
bootstrap-*layout* question, not a per-query one. And the mechanism is free: those
322 pages span about 1000 (~4 MB), so the browser fetches **that whole span in
one request** and serves all 322 from cache. One 4 MB fetch beats 322 round
trips by orders of magnitude. An optional repack shrinks the useful span to
~1.3 MB; it is never required and never touches the read path.

**Live, not frozen.** The meta pages are a double-buffered pair, so a client
re-reads the tiny meta to see the newest committed version and MVCC gives it a
consistent snapshot. *Mutable over HTTP* = re-read the meta, keep the cache,
re-descend only what moved.

**And it composes with §8.4.** The cost model is parameterized by per-fetch cost:
approximately zero locally, a high latency per non-contiguous request over HTTP.
Inject that and **the same solver flips the access-path choice** — a 100-leaf
*contiguous* range scan (one request) beats a 6-*scattered*-page index lookup (up
to six), the opposite of the local decision, with no manual tuning. One cost
model, three storage parameterizations.

**Honest limits, stated in the design.** Full scans are not range-friendly (773
pages here, unbounded on a real table) and belong server-side. Writes over HTTP
are a different problem entirely — a protocol, not a range request. And the page
counts are measured while the *latency* conclusion is arithmetic: the round-trip
timing was never run.

### 8.6 Reversible ETL — eleven constraints on a design not yet written

**Claim.** Register function *pairs* that go both ways, compose them into
pipelines, and let the database store what is needed to reverse. `forward(x) →
(y, residual)`, `inverse(y, residual) → x`. **The round trip works because what
was lost is stored**, never because the mapping is magically invertible.

**The worked example that binds it to storage.** Two versions of a file can share
blocks via filesystem reflink — but an edit that rewrites the file costs 2× disk,
and **on ext4 and macOS reflink does not exist at all**. With reversible pairs,
version 2 is stored as base plus diff, with the diff *as the residual*, verified
byte-identically at ingest. Logical delta compression is filesystem-independent:
**it works where reflink does not, and the two compose.** Precedent for the
chains is twenty years old — git packfiles, with a depth limit and periodic full
snapshots.

**Status: prior-art research complete, design not written, nothing built.** Its
deliverable is eleven commitments that make specific mistakes unrepeatable, and
the three sharpest are worth reading even if this is never built:

- **Never promise a minimal residual.** Minimality is *uncomputable*; the hard
  floor is conditional entropy — a stage that discards k bits must store ≥ k
  bits. Offer a checkpoint-versus-recompute knob instead. *"A `DROP COLUMN`
  stage's residual is the column, full stop."*
- **Verification scales with what you dare delete.** Keep the source: sampled
  property tests. Delete it: **100 % decode-and-compare before commit** — the
  precedent did this on 16 billion images and caught a non-deterministic buffer
  overrun that release qualification would have let through.
- **Correlate artifacts by content hash and a lineage table, never by filename.**
  Every mature tool landed there; positional alignment was a show-stopper in the
  original research. A hash mismatch on reverse is an explicit error — *"artifact
  changed outside the pipeline"* — never silently wrong input.

### 8.7 Distributed: three ideas, one of which is bigger than replication

**The honest framing first.** Everything mpedb does today is single-machine,
single PID namespace, shared memory. Across servers *none* of that holds. **So
distributed mpedb is not "sync, but bigger" — it is a new problem.**

- **Sharded serverless replication.** One shard per user or key range, **one
  master per shard**. The cluster is multi-master but every *shard* has a single
  writer, so conflict-resolution machinery is never needed. A bounded-lag
  follower makes promotion fast and rebalancing a user cheap: ship the file, flip
  the directory entry. **Only the shard→master directory needs consensus** — put
  it in etcd, *not* the data plane. Control plane consistent, data plane
  available. *"Availability is the easy part — 'never two masters' is the thing
  to get right."*
- **Determinism as the enabler.** mpedb's plans are content-hashed and
  deterministic — same plan, parameters and snapshot gives byte-identical results
  everywhere. **That is exactly the replicated-state-machine property most
  databases cannot guarantee**, so replicating the ordered intent log and
  agreeing on its order gives linearizability with the determinism doing the
  work. The precondition is flagged as hard: plans reading `now()` or `random()`
  diverge replicas and must be classified and resolved at the primary.
- **The shard scheme as MPEE's *output*.** Partitioning is the same problem: a
  cost graph, min-cut. Inputs are the app's model (Django's foreign-key graph),
  the query log for edge weights, and mpedb's **exact** row counts rather than
  the sampled ones the canonical method must use. Citus and Vitess have
  human-chosen shard keys; this *discovers* the key — and detects when there
  isn't one. It must report the **residual cross-shard rate**, because genuine
  many-to-many relationships are irreducible and the cost should be known before
  committing. Highly speculative: no measurement of any kind.

### 8.8 The rest, in one line each

- **Zero-downtime per-tenant schema upgrade.** Because each customer is a shard,
  migrate one at a time: canary 1 % of tenants, contain a bad migration to one
  tenant, and roll back by switching to the pre-migration replica that is still
  sitting there. Tenants can transiently run *different schema versions*. **None
  of this is possible in one shared instance where a migration hits everyone at
  once.** Honest limit: *"zero-downtime means no outage, not no work."*
- **Durable execution runtime.** Hold a program's state in mpedb and it becomes
  resumable, auto-parallelizable, hot-swappable and time-travel-debuggable. What
  makes it credible rather than a castle: **the footprint that detects write
  conflicts *is* the parallelization dependency graph** — the programmer writes
  sequential code and the runtime extracts the DAG from a structure that already
  exists for another reason. And the determinism behind the replicated state
  machine is what makes replay, scheduling and hot-swap safe. *"The whole runtime
  is one determinism property applied four ways."* Unmeasured assumption: that
  real workflow steps have disjoint footprints often enough to matter.
- **Bidirectional mirror DDL.** The interesting part is the **refusal boundary**:
  full symmetric schema evolution *invites unreconcilable merges*, so
  unambiguous changes propagate and ambiguous ones route to a full regenerate.
  Review found that a table rename is byte-identical to drop-plus-add at the
  introspection level, and that blindly applying the incremental path would
  destroy un-pushed local changes.
- **Generation-safe table-id reuse.** Designed, not built, and **explicitly
  superseded** — the id space was made sparse instead, so the cap became a cost
  knob rather than a representation limit. Kept because the mechanism is still
  the right answer if exhaustion ever becomes real, and because it dissolves a
  correctness obligation into a comparison: tag each slot with a generation, and
  a stale reference fails a check at *use* time instead of requiring a
  synchronous purge at drop time.

---

## 9. Negative results

Things built or seriously considered, measured, and rejected. This section is
load-bearing: a proposals document is only credible next to the list of what got
killed.

### 9.1 Optimistic parallel writers — instrumented and shipped disabled

**Verdict:** *"Optimistic parallel writers do not beat the serial writer-lock
path on this engine, on this host, in ANY measured configuration."*

The structural obstacle was stated *before* writing code: on a copy-on-write tree
there is no root containing both of two concurrent transactions' changes, so the
only escapes are re-applying logically under the lock, or page-level rebase —
which is unsound the moment two writes touch the same leaf.

Then the ceiling was measured: of 4,476 ns per transaction, **1,991 ns is
unavoidably serial** copy-on-write mutation. Best case is a **1.28× ceiling**,
before any overhead. In durable modes the ceiling analysis is moot, because the
commit is dominated by the disk flush and the group-commit ring amortizes one
flush across a batch.

Measured, non-durable: −3 % to −6 % on mixed, **−45 % to −58 %** on the
increment workload. Durable: **−14 % to −82 %**, and the signature is
unmistakable — *serial scales up with worker count while optimistic stays flat,
so the gap widens with concurrency, the opposite of what the hypothesis needs.*

The conflict mechanism works fine (321 retries per 160,000 applies, 0.2 %). It
just does not matter, because serialization happens at the apply lock.

**Kept, behind a config flag, because it is sound and passes every gate** — and
because keeping it preserves the reproducible A/B.

A later probe put a number on the ceiling from the other side: composing streamed
execution with a small final commit is worth **+0.7 %**, because the writer-lock
hold is 99.3 % device flush — 67 µs of work against 10,028 µs of `msync`. *The
composition was never the prize; the flush count was.*

### 9.2 Locality-sorted batch execution — the routing idea, falsified

The headline transfer from MPEE: sort a commit batch by key so adjacent mutations
share copy-on-write paths. Implemented, instrumented, measured: **+1.4 % / −2.0 %
inside a 7.4 % run-to-run spread**, and direct instrumentation showed **4.23
versus 4.26 pages per batch** — identical to within 1 %.

Why it lost, and the sentence is the whole lesson:

> MPEE's routing wins because travel cost is **path-dependent**. A copy-on-write
> dirty set is not — the pages copied depend only on the key *set*.

Retained anyway: it costs nothing measurable and makes batch linearization
deterministic. That falsification is what redirected the same mechanism onto the
join graph, which *is* path-dependent — where it produced the 19.6×.

### 9.3 Four candidate footprint indexes — three "don't build", none built

A census over 101,343 records, run twice with every count reproduced exactly, and
a microbench run three times reporting elementwise minima.

- **Conflict detection via an inverted index — don't build.** Crossovers exist
  but are **unreachable**: commits serialize on the writer lock and validate
  against a 64-slot ring, where the existing linear scan costs 52.7 ns — roughly
  an order of magnitude *less* than the cheapest index arm's floor. The pairwise
  arm was even measured *without* early exit, biasing in the loser's favour,
  which only strengthens the verdict.
- **Plan-variant families — don't build yet**, break-even at 16 variants per
  statement; **measured today: 1**, because the mechanism that would create
  variants is not built.
- **Route memoization — don't build.** Computed 1.78 ns, memoized 12.73 ns.
  **Memoizing the decision is 7× slower than recomputing it**, because the route
  is a lookup on a small sorted vector plus a hash of a few key parts — cheaper
  than one hash-map probe of a 32-byte key. *"There is no cache worth having for
  a computation cheaper than its own cache lookup."*
- **Delta-compressed table sets — don't build at today's shapes.** 62 % on the
  sets, but 4.78 bytes per record; only material past ~10 M stored footprints.

The document also has a section most repos never write: **what was not measured**
— the on-disk form of one arm, real join fans above 3 (*"the fan-5 rows are
synthetic extrapolation, not observed workload"*), and concurrent multi-process
writers.

### 9.4 One file instead of two — a thorough "no"

Can mpedb live *inside* a single `.db` with no sidecar? Six approaches, **four
dead**, two viable-with-constraints, and the recommendation is **don't build it**.

Two derived facts kill most of it. The only sqlite-legal way to own pages inside a
`.db` is to be **the payload of a row** — freelist pages are reusable by any
writer, unreferenced in-bounds pages are integrity-check errors, and bytes past
the header's size are garbage the library may truncate. And **nothing in the
format pins a page's physical location**; the only fastener is a permanently held
lock plus a permanent `VACUUM` ban.

The ledger against the two-file design: it deletes two of three lock modes,
converts the foreign-writer worst case from *"stale data, reconcile"* into
**"live-mapping corruption and unrecoverable loss of committed transactions"**,
costs **25 % of space permanently on Apple Silicon** (16 KiB pages force coarser
carving inside 64 KiB sqlite pages), fragments blob extents into ≤ 60 KiB
islands, and needs ~70,000 mappings against a default limit of 65,530. *"What it
buys is exactly one thing: single-file packaging."*

Two kill arguments generalize: *"A guard that only the guarded can see is not a
guard"* (hooks and authorizers are per-connection and voluntary; the threat is
the process that did not load them), and *"'a reader table in shared memory
outside the file' is the sidecar, renamed — the one-file premise is abandoned in
the approach's own last clause."*

The residue is banked, not discarded: eight experiments **with kill criteria
stated up front** so *"the outcome is a fact, not a mood."*

### 9.5 Seven dead hypotheses on one leak

The freelist leak (§2.1) is documented as 120 lines of prose and one deliberately
ignored test. Three hypotheses **died by being implemented and measured doing
nothing, or harm** — one made the high-water mark go from 27k to 65k pages and
halved throughput, yielding the sub-lesson *"never infer reclamation from the
pool's length."* One died by **reading the design document** rather than
measuring, because the `high_water` fallback *is* the termination argument.

The instrumentation came first and produced the two facts that constrained
everything: every page ever allocated *was* in the freelist, and the growth rate
was **constant** while the tree grew 10× — so there was no feedback loop and tree
depth was not the driver.

### 9.6 The join solver does nothing on ordinary shapes

The memory result in §4.8 is the largest single effect measured in this
document — 31,905× fewer live join cells at 12 tables, linear against
exponential. The same measurement says the effect is **zero** where most SQL
lives.

On the corpus-median join — a filtered fact table joined to two dimension
tables by their primary keys — both arms choose the **same order**, hold the
**same 686 cells**, and finish inside each other's run-to-run noise. At a
4-table chain the RSS ratio is inside the spread too, and is reported as *no
measurable effect* rather than as a small win.

That is not a disappointment, it is the shape of the technique: a join-order
solver earns its keep exactly where the textual order is pathological, and the
textual order is pathological exactly when a human did not write it — generated
SQL, an ORM's compound query, a test corpus built to be adversarial. Where a
person wrote the FROM clause in the order they think about the data, they
usually wrote a good order already.

The corollary for anyone quoting §4.8: **the 19.6× and the 31,905× are
adversarial-shape numbers.** They are real, they are reproduced, and they are
not what a typical statement sees.

### 9.7 Designed, partly built, or never wired

Stated so nothing here reads as shipped:

- **Conflict-based batch grouping** has no production caller. What runs is the
  locality sort — which was falsified (§9.2).
- **Footprint prefaulting** is described in the design as a live consumer; it is
  **not implemented**.
- **Max-pin-age eviction** — the safety valve for a stalled reader — is not
  built. The *detection* half is: revalidation every 256 cursor steps, a distinct
  error, and the generation CAS that would notice the theft. Today a stopped
  reader can stall writers with no recourse.
- **Cost-model ambition versus reality**: the cost catalog, per-consumer
  specialization and the auto-indexing advisor are all design.

### 9.8 Deliberate deviations, kept

Not everything unfixed is a bug. Some positions are the honest answer:

- **Three foreign-key tests fail on purpose.** mpedb parses `REFERENCES` and
  discards it. Passing them would mean claiming enforcement that does not exist.
- **A float-versus-integer comparison is left refused.** Rounding a parameter to
  fit the column is precisely the wrong answer, and an index probe on a rounded
  bound returns the wrong rows.
- **One test cannot pass structurally**: it asserts a result that is text but not
  valid UTF-8, and the value type is a Rust `String`. Recorded as a deviation,
  not chased.
- **In-file row-level security is cooperative and is NOT a security boundary** —
  with eight enumerated leak classes, including a uniqueness-constraint existence
  oracle that *cannot* be closed while a single global unique domain is
  preserved. Separate files are a real wall only if the deployment runs distinct
  OS users, *"which mpedb cannot enforce or verify; the operator must provide
  it."*

---

## Reading further

`design/DESIGN.md` is the reviewed contract for concurrency, locking and the
commit path — read it before touching any of them.
`design/DESIGN-MPEE-SOLVER.md` is §4. `C-API-COMPAT.md` and `COMPAT.md` carry the
measured compatibility surface. `design/FOOTPRINT-INDEX-MEASURED.md` and
`design/DESIGN-PHASE3.md` are §9's primary sources.
