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

## 3. The limit that surprised the measurement

The intuition was: a block can be as small as you like — a word — so a paragraph
of 20 words supports 20 × cap editors. Splitting is the lever; the cap is the
safety valve.

**Splitting does not multiply capacity.** Measured, 50 editors per block:

| blocks | editors | actions/s | *unguarded control* |
|---:|---:|---:|---:|
| 1 | 50 | 211 | 365 |
| 4 | 200 | 200 | 356 |
| 20 | 1000 | 182 | 341 |

Throughput is flat — and **so is the unguarded control**, which is what settles
it. The ceiling is the single writer lock, not the guard. Splitting removes
*conflicts*; it cannot raise the rate at which commits can happen at all.

So an admission policy needs both halves:

```text
editors on one block  ≤  cap                      (measured: 16–32 at 1 s)
editors in total      ≤  D × global commit rate   (measured: ~340–480/s)
```

On the Linux box that is ~340 concurrent editors across the whole document, with
≤32 on any one block. A thousand needs a machine whose commit rate is a thousand
per second — not more blocks.

What splitting *does* buy is real: it converts conflicts into independent work,
which is the difference between the guarded rate (182) and nothing at all. It
just cannot exceed the unguarded ceiling.

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
