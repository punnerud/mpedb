# Many mpedb clients, one authority — sync, measured

**Arm G, `mpedb-bench --sync-load`.** Every client owns its **own `.mpedb`**,
works against it with the ordinary API, and reconciles with a central one. This
page is what that costs and where it stops working.

Run it isolated (`benchmarks/README.md`), on both machines:

```
cargo build --release -p mpedb-bench
./target/release/mpedb-bench --sync-load --disk /path/on/disk
```

## The shape

```
  replica A (.mpedb)  ──push/pull──┐
  replica B (.mpedb)  ──push/pull──┼──▶  authority (.mpedb)
  replica C (.mpedb)  ──push/pull──┘
        ▲
        │ ordinary API — query, execute, begin, submit_batch
   application code
```

The application never sees a sync API. `[sync] role` in the config says whether
this process is `standalone`, a `replica`, or the `authority`; everything else
is the same engine doing the same things. `SyncLink` is the reconciler a service
calls on whatever schedule it likes.

**No network and no process boundary is modelled.** Replicas are threads, each
with its own file and therefore its own writer lock, so the contention measured
is real. A network would add latency to every arm equally. The multi-process
property is proven by `mirror-collide` and the stress/crash harnesses; repeating
it here would measure `fork`.

## G-role — does declaring a role cost anything?

The control, and it runs first, because every other number on this page is only
interesting if the answer is no. Identical local work, nothing synced, **paired
and interleaved** with the order alternating each repetition (#122), reported as
the median of the per-pair ratios.

Three isolated runs per machine, nine paired repetitions each, same unchanged
binary:

| | Linux (2 cores) | M3 (11 cores) |
|---|---|---|
| `standalone` writes/s | ~450 | ~400 |
| `replica` writes/s | ~445 | ~380 |
| paired median, run 1 / 2 / 3 | **−0.4 % / +2.0 % / +0.6 %** | **−7.4 % / −0.9 % / +1.4 %** |

**No difference either machine can detect.** Both straddle zero and neither shows
a consistent direction, so the claim the arm exists to test holds: an application
does not pay for declaring itself a replica.

What the three-run spread also gives is the **resolution**, which a single median
would have hidden: roughly **±2 % on Linux and ±7 % on the M3**. Anything smaller
than that is not measurable here, and quoting one run's median as *the* answer
would have been quoting one draw from that spread — an earlier draft of this page
said "−1.5 % on the M3" on exactly that basis.

It is also the expected answer from the code. `role` is read once at open and
consulted in exactly one place, `submit_batch`'s verdict; there is no mechanism
by which it could change the cost of an `INSERT`. A control that showed a
*consistent* difference would have meant something was wrong with the
measurement, not with the engine.

Getting even this far took three corrections, recorded because the next person to
add a control here will otherwise repeat them:

- A **single unpaired sample said the replica was 26.5 % faster.** Not a result:
  host noise on two cores.
- Pairing alone still left **+5.7 %**, because standalone always ran first and
  paid every pair's cold-file cost. **Alternating which arm goes first** inside
  each pair removes it.
- Running the benchmark **in the same session as the test suite** produced a
  −9.5 % outlier. `benchmarks/README.md` already says to run isolated; that is
  what ignoring it costs.

## G-rows — general sync

N replicas own disjoint key slices, write locally, and reconcile. Local writes
and the sync are timed **separately**: each local write is its own autocommit
transaction, so a combined number reports the disk's commit rate and hides the
sync completely. The first version of this arm did exactly that and read ~400
rows/s — the disk, not the sync.

`converged` is the assertion, not a statistic. A rate printed beside `false`
would be the rate at which data was lost.

50 rows/client/round × 4 rounds:

| clients | rows | Linux local/s | Linux sync s | Linux synced/s | M3 local/s | M3 sync s | M3 synced/s | converged | conflicts |
|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|
| 2 | 400 | 456 | 0.074 | **5383** | 359 | 0.089 | **4487** | true | 0 |
| 8 | 1600 | 481 | 0.511 | **3131** | 465 | 0.437 | **3661** | true | 0 |
| 32 | 6400 | 396 | 3.890 | **1645** | 448 | 2.658 | **2408** | true | 0 |

**Sync is 4–12× cheaper than the local writes that produced the data**, on both
machines, so the reconciler is not the thing to optimise first — the commit rate
is.

**It does not scale flat with clients**, and the reason is structural rather
than a bug: every replica pulls every other replica's changes, so total work is
O(clients × changes). 32 clients moving 6400 rows is 32 pulls over the same
6400. A topology that needs to beat that wants per-client filtering (RLS-style
scoping of what a replica subscribes to), not a faster pull.

## G-cell — many clients editing ONE value

The collaborative-document case with a real file boundary between the editors.
Every client owns a 16-byte slice of one cell; sub-edits carry `(seq, editor)`
and merge at the authority through `submit_batch` (#155).

`len ok` is the correctness check and it is deliberately a length: each edit
replaces 4 bytes with 4, so a clobber instead of a splice would move it.

| clients | edits | landed | Linux edits/s | M3 edits/s | len ok |
|---:|---:|---:|---:|---:|---|
| 2 | 16 | 16 | 1386 | 813 | true |
| 8 | 64 | 64 | 3068 | 2641 | true |
| 32 | 256 | 256 | **8012** | **8821** | true |

Nothing is lost at any width, because the clients hold disjoint byte ranges —
which is the whole point of the byte-range conflict unit. `benchmarks/documents.md`
has the arms where they collide on purpose.

## G-offline — a replica that was away

| distinct rows | upstream edits while away | row images replayed | Linux secs | M3 secs | converged |
|---:|---:|---:|---:|---:|---|
| 50 | 100 | **50** | 0.003 | 0.004 | true |
| 50 | 1 000 | **50** | 0.002 | 0.004 | true |
| 50 | 10 000 | **50** | 0.002 | 0.004 | true |

**Catch-up cost tracks changed ROWS, not edits** — 100× the editing, identical
work. That is the coalesced CDC dirty set doing its job: a row edited two
hundred times is one image, not two hundred. Without it, an hour offline would
be an hour of replay, and the offline story would not exist.

The limit worth knowing: this is *row* coalescing. A cell being edited by many
people concurrently is a different question, and the sub-edit path (`push_edits`)
is the answer — its ordering is total **within a round**, not over all history,
so an edit that arrives after its round has committed lands late rather than in
its original place. It is never lost and never lands on the wrong bytes.


## What two adversarial passes caught

The benchmark found the echo bug. Two agents then used the API — one as a
first-time user building three real deployments, one trying to break
convergence — and between them found **two more genuine bugs and a missing
call**, none of which any existing test covered.

**Cursors were per link, but the table list is per call.** `pull(["doc"])`
advanced the cursor past every pending `note` entry, which the next
`pull(["note"])` then skipped **forever** — and `lag()` reported 0, because it
consulted the same poisoned cursor. Syncing a hot table and a cold table on
different cadences is an ordinary thing to want. Cursors are now per
`(link, table)`. Pinned by `pulling_one_table_does_not_skip_another`.

**A UNIQUE column permutation wedged the link permanently.** Swapping two rows'
unique values is legal at both endpoints and illegal in between; applying
row-at-a-time as delete+insert walked straight through the illegal state, so
every pull failed with `UniqueViolation`, rolled back, and left the cursor
where it was. Not slow — stuck. Both planes now delete every affected key
before inserting any, which dissolves a swap order-free. Pinned by
`a_unique_column_permutation_still_converges`.

**There was no way to attach a replica to a database that already had data.**
`enable` is not retroactive — capture starts when it is turned on — so rows
written earlier replicated never, silently, with `lag()` at 0. Both agents hit
it independently. `SyncLink::seed` is the missing step.

Three more findings were reporting or ergonomics, and all are fixed:
`sync()` returned `(pulled, pushed)` while its own documentation said "push,
then pull" — two same-typed reports, so binding them the documented way
compiled and silently read the wrong direction; it now returns a named
`SyncOutcome`. `lag()` counted the replica's own push back at it, so a
write-heavy replica showed a permanent "syncing 7…". And a push that adopted
upstream values for rows it lost changed local data while reporting nothing that
said so — that is now `SyncReport::local_writes`.

**One finding is documented rather than fixed.** A *relay chain* — a hub that is
a replica of A and also the upstream of B — does not carry changes downstream.
`pull` applies with capture off (it must, or every pulled row would push straight
back), so a row the hub pulled never enters the hub's own change log, which is
exactly what B's pull reads. The upstream direction relays fine. Until that
changes, give every replica a link to the authority it actually needs data from
rather than chaining links.

## What this arm caught

Two things, and both were found by a *column*, not by a test:

1. **`conflicts` read 400 on 400 disjoint rows.** Every row a replica pushed came
   back on the next pull and was mistaken for somebody else's change. The same
   mistake also discarded a replica's newest unpushed edit in favour of the value
   it had pushed a moment earlier — a silent lost update. Fixed by recording a
   per-row **base**: the image the two ends last agreed on. A row the upstream
   still holds at the base is a row nobody else touched, whoever put it there.
   Pinned by `a_replicas_own_echo_is_not_a_conflict`.

2. **The order of `sync()` was wrong.** It pulled first. Pulling first clobbers an
   unpushed local change *before the upstream has ever been offered it*. It now
   pushes first, so every local change is offered, judged against the base, and
   either accepted or explicitly lost.

## Choosing a policy

`Resolve` is an explicit choice because the right answer genuinely differs:

| your situation | pick | why |
|---|---|---|
| many clients, all online, a server of record | `Resolve::UpstreamWins` (default) | the authority is the truth; a stale client should not undo it |
| one client offline for a long time, its work is the point | `Resolve::LocalWins` | a week of field work must not evaporate because a colleague touched one row |
| you want **neither** to disappear | put the column under the **cell plane** (`push_edits`) | the two edits *merge* rather than one winning |

The third row is the one most applications want and the one a row-plane setting
cannot give: moving whole row images means somebody's version is discarded
either way. If losing an edit is unacceptable, the answer is not a cleverer
winner, it is to stop treating the value as a whole.

## From Python

The same three roles and the same reconciler, embedded:

```python
import mpedb

up = mpedb.Database("authority.toml")   # [sync] role = "authority"
r1 = mpedb.Database("replica1.toml")    # [sync] role = "replica"

r1.sync_enable(up, ["note"])            # once: turn change capture on
r1.sync_seed(up, ["note"], link=1)      # once MORE, if the upstream is not empty

r1.query("INSERT INTO note (id, body) VALUES ($1, $2)", [1, "written offline"])

r1.sync_lag(up, ["note"], link=1)       # rows behind — the "syncing…" number
r1.sync(up, ["note"], link=1)           # -> {"pulled", "pushed", "conflicts",
                                        #     "deleted", "local_writes", "cursor"}
r1.sync(up, ["note"], link=1, resolve="local-wins")

assert up.fingerprint(["note"]) == r1.fingerprint(["note"])
```

Many editors inside one value, also from Python:

```python
up.submit_batch("note", "body", [
    {"editor": 1, "seq": 10, "key": 9, "at": 0,  "remove": 4, "insert": "aa"},
    {"editor": 2, "seq": 11, "key": 9, "at": 8,  "remove": 4, "insert": "bb"},
])   # -> ["committed", "committed"]
```

`seq` is the order the editors **acted**, assigned by the caller and never by the
engine, so the same edits produce the same text however they arrive. On a
replica the verdict is `"provisional"` instead of `"committed"`: the edit stands
locally and no authority has confirmed it.

## Things to know before using this

- **`sync_enable` is required once**, on both ends, for every table you sync.
  Without it the change log stays empty and every sync is a silent no-op.
- **`seed` too, if the upstream already has rows.** `enable` is not
  retroactive: it starts capture, it does not look backwards. Rows written
  before it replicate never, and nothing says so.
- **`sync()`, not `pull()`.** `pull` on a replica with unpushed local changes
  destroys them, uncounted and unrecoverably — conflict detection lives in
  `push`, which is why `sync` runs it first.
- **`link` must be unique per link.** Two replicas of one upstream sharing a
  number share a cursor, and each will think the other's progress is its own.
- **Nothing trims the upstream's change log automatically.** Pull only reads,
  which is what lets N replicas share one upstream; `gc_upstream(watermark)` is
  the trim, and the watermark must be at or below the **slowest live replica's**
  cursor. Above it, that replica misses changes permanently.
- **One table belongs to one plane.** A table synced by `push`/`pull` must not
  also be edited through `push_edits` — the row plane moves whole images and
  would discard the merge the cell plane just computed.
- **Star, not chain.** Give every replica a link to the authority it needs data
  from. A hub that is itself a replica does not carry changes downstream (see
  above), and nothing refuses the topology.
