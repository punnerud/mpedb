# Collaborative editing: a bounded answer, and what bounds it

The contract behind `Database::submit_within` / `act_within` and
`collab::Lease` (#150). The guard that makes an edit safe is
[DESIGN-NOTIFY.md §6](DESIGN-NOTIFY.md); the measurements are in
[benchmarks/documents.md](../benchmarks/documents.md).

## 1. The contract

> Everyone who submits an edit learns within *D* whether it landed.

Three answers, and exactly three:

| Verdict | Meaning |
|---|---|
| `Committed` | It landed. |
| `Lost { at_txn }` | Someone else got there first. `at_txn` names the winning commit, so the client re-reads its own snapshot and re-renders — no polling. |
| `DeadlineExpired` | No definite answer in time. Not a conflict: an admission-control signal. |

**First wins, and the losers know.** That is the whole of it. There is no queue,
no lock held across a person's typing, and no ordering to protect.

Two entry points, because two kinds of edit need different answers:

- **`submit_within(deadline, snap, …)`** — a person read the block at `snap`,
  thought, typed, and pressed send. **One attempt.** Retrying would re-apply a
  decision made against a value that has since moved, which is precisely the
  lost update the guard exists to refuse. The client must see what won.
- **`act_within(deadline, …)`** — an edit that is a *function of the current
  value* (a counter, an append). Re-reads and retries until the deadline,
  which is safe only because each attempt decides afresh.

**An attempt that cannot finish inside the remaining budget is not started.**
Otherwise the bound breaks by construction: the last attempt runs past *D* and
the caller is told at *D* + one attempt. The estimate is the previous attempt's
measured cost — the only honest one available, and it errs toward answering
early.

Live update for everyone else is already built and is not repeated here: the
winner's commit publishes "table T, generation G" with a key region (#141 N2,
#143), and other editors' listeners wake on exactly that block and read their
own MVCC snapshot.

## 2. Why there is a cap on editors

Because the deadline is a promise about the **tail**, and the tail is a function
of how many editors share one **contention unit** — one block row.

Measured, no artificial think time, 99 % of actions inside a 1 s deadline:

| | cap per block |
|---|---:|
| Linux, 2 cores | **32 editors** |
| M3, 11 cores | **16 editors** |

**The cap is measured, not derived.** The obvious formula — `deadline ÷ service
time` — said 386 and 333 on those machines. It is wrong by more than an order of
magnitude, and wrong in the dangerous direction, because a refused editor
**re-does its whole action** rather than queueing behind the winner. Optimistic
concurrency does not conserve work the way a lock queue does. Anyone tempted to
compute this number instead of measuring it should read the sweep first.

## 3. The limit that was an artefact, and the one that is real

The intuition was: a block can be as small as you like — a word — so a paragraph
of 20 words supports 20 × cap editors. Splitting is the lever.

**Splitting alone does not multiply capacity.** Measured, 50 editors per block:

| blocks | editors | actions/s | *unguarded control* |
|---:|---:|---:|---:|
| 1 | 50 | 211 | 365 |
| 4 | 200 | 200 | 356 |
| 20 | 1000 | 182 | 341 |

Flat — and **so is the unguarded control**, which is what settles the
attribution. The ceiling is not the guard.

**But it is not the write path either. It is one commit per edit.** Eight
editors, K edits folded into a single commit:

| K | Linux commits/s | Linux edits/s | M3 commits/s | M3 edits/s |
|---:|---:|---:|---:|---:|
| 1 | 197 | 197 | 1178 | 1178 |
| 8 | 171 | **1370** | 1072 | **8577** |
| 64 | 71 | **4525** | 794 | **50828** |
| 256 | 59 | **14988** | 490 | **125333** |

**76× the edit rate on the two-core box, 106× on the M3 — with commits running
*slower* on both.** The engine's limit is commits, and an edit never had to be
one. A thousand concurrent editors is 1000 edits: a fifth of a second of
capacity at K = 64 on the slow box, eight milliseconds on the M3.

So the rule is one line, not two:

```text
editors on one block  ≤  cap        (16–32 at a 1 s deadline)
```

There is no global editor limit worth stating. What there is instead is a
**design obligation**: an edit must not be a commit of its own.

### The sub-edit: `splice()` (#151, shipped)

```sql
UPDATE doc SET body = splice(body, $at, $remove, $insert) WHERE id = $1
```

An ordinary scalar in an ordinary `UPDATE` — no new statement form, no plan
format change. What makes it a *sub-edit* rather than a value is **when** it is
evaluated: an UPDATE expression reads the row as it stands at write time, not as
of the session's snapshot (verified, not assumed). So two editors splicing
disjoint ranges of one cell both land, where two whole-value writes computed
from the version each of them read would lose one.
`two_disjoint_splices_of_one_cell_both_land` carries that control inside itself:
the same two intentions as whole-value writes drop the first edit.

**Strict about offsets, never clamping.** A range past the end, or a cut inside
a multi-byte character of a TEXT value, is refused. A stale offset is a wrong
question; clamping would answer it with silently mangled text, and on TEXT could
produce invalid UTF-8 — worse than an error and discovered much later.

### What that implies, and what is not built yet

The answer an editor waits for is *"did my edit win"* — a question about
**conflict**, not about durability. Those can be answered at different times:
claim the surface and reply at once; fold the write into the next batch. `#150`
ships the first half (the verdict is decided by the guard at commit and returned
immediately) and measures that the second half pays for itself. It does not yet
ship the batching itself, nor the finer granularity that makes batching most
valuable:

### The fifth dimension: the range is in the request (#151, shipped)

`splice(body, $2, $3, …)` carries its byte range in the same parameters
`WHERE id = $1` carries its key, so the guard can decide the finest dimension
**before any work happens** — a collision is one integer comparison, not a page
read. That is the property that made declaring keys work (#148), applied one
level finer, and it is why the surface is now
`(table, key, shard, column, byte range)`.

Recognised only where the answer is exact: a single-column `UPDATE` whose value
is `splice(<that same column>, at, remove, …)` with constant-or-parameter
offsets. Everything else rewrites the whole value, which is what "no range"
says — and a write with no range conflicts with every sub-edit of that cell.

### Rebase in the engine: the asymmetry is gone

A splice applies to the value *as it stands at write time*, so a commit that
landed before mine and began before my range moved the bytes my offset was
computed against. The first cut at this refused those — correct, but asymmetric:
one person typing at the top of a paragraph invalidated everyone editing below.

The engine now carries the offset forward instead. Each ring entry publishes the
**length delta** beside the range, and one walk of the window answers both
questions at once:

```text
for t in snap+1 ..= current, touching this cell, range [lo, hi), delta d:
    if [at, at+len) overlaps [lo, hi)  -> collision
    else if lo <= at                   -> at += d      (they were before me)
    else                               -> unchanged    (they were after me)
```

**The coordinate systems line up because the walk is in order.** `lo` is in the
value's coordinates as of `t`, and `at` has already absorbed every shift from
`t' < t`, so it is in those coordinates too. Rebasing before comparing, or
comparing out of order, would silently mix two different rulers — and the
failure would not be an error, it would be a splice landing on the wrong bytes.
That is why `a_shifted_offset_is_carried_forward` asserts on the resulting
string rather than on `Ok(())`.

**Why this can run at execution.** The session holds the writer lock from
`begin()`, so no commit can land between the walk and the commit — the window
the walk saw is the window the commit sees. That is also why the guard's
commit-time range test is *skipped* once a walk has decided
(`RangeClaim::Settled`), rather than re-deciding against coordinates that have
since moved.

**Only a guarded session is rebased**, and that is the rule rather than a
limitation: the guard's snapshot IS "the version the client decided against",
and without one there is no coordinate system to carry from. An unguarded splice
means exactly what it says against the value as it stands, and a stale offset
there is caught by `splice()` itself.

A genuine collision is refused **at execution rather than at commit** — earlier
feedback for the same answer, and the caller still sees `WriteConflict`, which
`submit_within` turns into `Lost { at_txn }`.

Still not built: acknowledging on claim so the write can ride a later batch.

- **Acknowledge on claim, commit in batches.** The engine already group-commits
  (`ring_exec`); what is missing is letting the *answer* precede the batch
  rather than ride it.

Both are #145's splice write form seen from the concurrency side, and both are
filed rather than assumed.

## 3b. `submit_batch`: the primitive the engine owes a service (#153)

An edit should not be a commit of its own — measured, the commit is ~2 ms
against ~35 µs of execution, and folding K edits into one transaction was worth
79× (benchmarks/documents.md, F-batch). But a guarded session **cannot**
group-commit with other processes: it holds the writer lock for its life and
never reaches the intent ring, which #152 measured (`intents=0` at every wait
window, so a linger in the leader had nobody to wait for; built, measured,
reverted).

So the batching belongs to whoever collects the edits, and the engine's job is
to make that one call:

```rust
db.submit_batch("block", "body", &submissions) -> Vec<EditVerdict>
```

**We do not build the service.** Transport, fan-out and the client library are
the user's; this is the minimum that makes them easy to write.

### The algorithm, and why the order is a guarantee

1. **Sort by `(seq, editor)`** — the submitters' own counters (§3c). Total, so
   no tie can fall back on arrival order.
2. Guard against the **oldest** snapshot in the set — guarding against a newer
   one would forgive a decision made against a version that had already moved.
   This is the *transaction's* snapshot; each member is rebased from its own
   (§the two conservative choices, below).
3. Apply in counter order, shifting each member by the length delta of the
   members already applied **whose removed span ends at or before it** — which
   is the condition "did that edit move my bytes", stated directly.
   `splice()`'s engine-side rebase (#151) handles everything committed *before*
   the batch; this loop handles the batch itself. They compose and must not be
   conflated: one counts committed transactions, the other counts members.
   - The test is on the predecessor's **end**, not its start, and the difference
     is the equal-offset case: two people typing at one cursor both have
     `at == mine` and neither removes anything, so the one that acted first must
     push the other along. Comparing starts would leave them at the same point
     and silently reverse them.
4. **Overlap within the batch is a collision, not a shift**, tested against the
   set of applied spans in pre-batch coordinates. Getting this wrong is not an
   error — member A rewrites `[0,4)`, member B wants `[2,4)`, and shifting B by
   A's delta relocates it onto A's own inserted text. A perfectly valid splice,
   on bytes B never saw. That is exactly the shape of the committed-path rule,
   and it was a live bug for one compile.
   - An explicit **set**, not a watermark. A watermark was correct only while
     members were applied in ascending offset; under counter order the applied
     members are not a prefix of the offset axis, so it would both miss real
     overlaps and invent false ones.
5. A loser loses **alone** — a savepoint per member, as the ring's leader
   already does.

**Applying in arrival order instead would make every edit's offset depend on
network jitter.** That is why the order is documented as a contract, and why the
counter replaced the stable sort that used to stand in for it.

### Nobody starves

The member that always sorts last still lands, every round, as long as it does
not overlap. It is *rebased*, not rejected. Being slow is not what costs an
edit; wanting the same bytes is. This is why no anti-starvation rule is needed
— the question dissolves rather than being answered, and
`the_last_in_sort_order_still_lands_every_round` pins it.

### The recommended service shape — measured, and it works

Collect submissions per block; flush when a **quorum** of the editors with
outstanding work has delivered, with a time limit as the upper bound that should
normally not fire. `collab::Lease` (#150) is already the presence table. The
number that says whether the idea worked is *flush-on-quorum vs flush-on-timeout*
— if the timeout dominates, the time limit is the design and the quorum is not.

`F-quorum` measured it (benchmarks/documents.md). The timeout fires 0–2 times in
~20 flushes when every editor is healthy and 6–16 in ~25 when a quarter of them
are ten times slower, on both machines. So it is a bound and not the mechanism,
which is what the idea claimed. Average batch size reached 32 — discovered from
arrivals, where the intent ring carried exactly 1.

What it does *not* buy is immunity for the fast editors: throughput roughly
halves when a quarter are slow, because a majority of active editors usually
cannot be assembled from the fast ones alone. Quorum picked the right trigger; it
did not remove the coupling.

### The one place two conservative choices multiply (#154)

`F-quorum` exposed this as a throughput bug. It is worse than that, and the
correction is worth being precise about, because "both halves fail safe" is what
made it look harmless:

- step 2 guards against the **oldest** member snapshot, and used the same
  snapshot for the *rebase walk*; and
- `WriteTxn::record_written_range` publishes `(min lo, max hi)` — the union over
  the whole transaction — so a K-member batch declares one span covering all of
  them, which for 32 editors is most of the block.

The union half fails safe: a spurious refusal. **The walk half does not.** A
member whose own snapshot is newer than the batch minimum has already seen the
commits in between, and its `at` is expressed in coordinates that include them.
Walking it from the minimum applies those deltas a *second* time — a valid splice
on bytes its author never saw, reported as `Committed`.
`collab_mixed_snap.rs` reproduces it: two disjoint edits, two snapshots, and the
newer one landed four bytes early.

**The rule, and the reason the two halves differ:** the guard is about what the
transaction may refuse, so it must be the oldest — anything newer would forgive a
decision made against a version that had already moved. The walk is about where
one member's bytes are, so it must be that member's own. `query_from_snapshot`
carries it per statement rather than as session state, because a hidden mode is
what breaks when two calls get reordered. An origin older than the guard is
clamped to the guard's: the walk may not reach past what the guard validated.

The union half remains, and is now the whole of the residual loss: a member whose
snapshot predates the previous batch's commit collides with that batch's declared
span whatever it asks for. Fixing it means recording a small *set* of ranges
instead of their union (the shape `OFP_MAX_KEYS = 8` already uses for keys, with
the same fall back to the union on overflow) — a shared-memory format change, so
it is filed separately.

## 3c. The flush needs no deadline (#155)

`F-quorum` showed the timeout was a bound rather than the mechanism. `F-counter`
removes it, and the two pieces that make that safe are both already here.

**The counter.** Every `Submission` carries `seq`, drawn by the client from a
shared clock. The batch applies in `(seq, editor)` order — total, no ties — so
the same submissions produce the same text however they arrive. The engine never
assigns `seq`; it only sorts by it.

**The heartbeat carries it.** `Lease::beat(block, editor, now_ms, seq)` publishes
where an editor has got to, and `Lease::spoken` reports the live holders'
positions. So an editor with nothing to add needs no separate "abstain" message —
*having nothing to say is a heartbeat with an unchanged counter*. That is what
makes "a majority of the membership has spoken" reachable with no clock in the
flush loop, and the membership a **known set** (lease holders) rather than
something inferred from arrivals.

Time does not disappear; it moves to the lease TTL, which is membership rather
than commit, and which the deployment already owns. Measured, that trade is worth
+38 % when slow editors can still speak and −47 % when they cannot
(`benchmarks/documents.md`, F-counter) — the same condition etcd puts on a
follower, and the reason the TTL is the number to tune for a network-shaped
workload.

### What the counter does not buy

A submission whose round has already committed cannot be ordered into the past;
#151 rebases it forward and it lands late. The counter is a total order **within
a round**, not over all history. An edit delayed for half an hour is never lost
and never lands on the wrong bytes, but its counter does not reorder what is
already committed — that needs commutative operations, which is the CRDT trade
this design declined.

## 4. Seats, heartbeats, and reaping

Admission control is ordinary rows, modelled on the task queue's
claim/lease/reap (`crates/mpedb-cli/src/queue.rs`): no new cross-process
primitive, SIGKILL-safety for free, and "who is editing right now" is a `SELECT`
that a viewer's UI wants anyway.

```sql
CREATE TABLE edit_lease (
  block, editor, pid, pid_start, beat_at, PRIMARY KEY (block, editor))
```

- **`acquire` reaps, then counts, in one transaction.** The party that benefits
  pays, so a database with no editors never sweeps — the same rule the
  notification registry follows (DESIGN-NOTIFY §2). `AtCapacity` returns
  **immediately**: a viewer learns it is a viewer at once, rather than
  discovering it as a deadline expiry a second later.
- **`beat` is guarded on `(pid, pid_start)`**, so a recycled pid cannot inherit
  someone else's seat — the same guard the queue puts on
  `(claimed_by, claimed_at)`.
- **Reaping is expiry OR a dead process**, and **liveness errs toward alive**:
  `pid_is_alive` refuses to declare a process dead on `EPERM`. Getting this
  backwards evicts a live editor, which is visible; leaving a dead one costs a
  seat until its TTL, which is not. Same asymmetry as #136 and #147.
- **The heartbeat is never on the commit path.** It is one small write every
  10–15 seconds. Putting liveness work into the commit path is exactly what #147
  had to undo.

**A seat is not a lock.** Holding one does not mean your commit wins —
first-committer still decides. The seat bounds *how many* editors contend for a
block so the deadline stays meetable. `a_seat_is_not_a_lock` pins this: two seat
holders write the same block and exactly one commits.

## 5. Declaring it

`[[model.section]]` (advisory metadata — DESIGN-MODEL-LANG §2, never in plan
bytes), so every attached process agrees on one cap rather than each picking
its own:

```toml
[[model.section]]
table = "block"
key_column = "id"
feedback_deadline_ms = 1000
heartbeat_ms = 12000
max_editors = 32          # MEASURED per machine — see §2
```

Refused at parse: `max_editors = 0` (admits nobody), `feedback_deadline_ms = 0`
(expires every edit before it starts), and a heartbeat at or under the deadline
(it would evict editors mid-edit). All three are typos that would otherwise look
like the feature being broken.

## 6. Invariants that bite

- `submit_within` must **not** retry. One attempt is the semantics, not a
  limitation: the client composed its edit against a version it was shown.
- `act_within` must not start an attempt it cannot finish inside the budget.
- `deadline_expired` is counted in `GuardStats` beside the guard's verdicts.
  It is the signal a cap is too high, and it is the only place that shows —
  #149 is the standing lesson about what happens when refusals are attributed
  to a cause nobody counted.
- Liveness errs toward **alive**. Always.
- The heartbeat interval must exceed the feedback deadline, enforced at parse.
- The cap is measured per machine. A number copied from another box is a guess
  wearing a measurement's clothes.
