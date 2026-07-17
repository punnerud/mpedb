# DESIGN-SQLITE-BACKED: the .db as home, the .mpedb as its delta-"WAL"

Status: **v0.1 draft — for adversarial review before any code** (the
checkpoint protocol spans two files and is commit-path class). Task #69.

The idea (Morten, 2026-07-17): work directly against a sqlite `.db` as the
durable, canonical home — every sqlite tool can open it — while mpedb provides
the hot path: `.mpedb` is a **delta overlay** playing the role a WAL plays for
a database. All writes and all MVCC/multi-process reads go through the
overlay; untouched data falls through to the base; a **checkpoint** pushes the
deltas into the `.db` and then empties the overlay, exactly like a WAL
checkpoint. Default: mpedb **holds a lock on the base**, so the fast path
never has to ask whether the base moved; unlocking is a deliberate,
detectable, cooperative act.

## 0. Honest scope (read first)

- **This is not sqlite reimplemented.** mpedb never executes sqlite's write
  protocol. Writes reach the `.db` only through the checkpoint's one sqlite
  transaction (via the sqlite library, as `mirror push` already does).
- **sqlite readers see the last checkpoint**, never un-checkpointed overlay
  deltas. That is inherent to the metaphor — a sqlite reader of a *different
  process's* WAL-before-checkpoint sees the same staleness class. Anyone
  needing fresher reads uses mpedb.
- **Simultaneous writers to both files is not a goal.** In LOCKED mode sqlite
  writers are blocked (by sqlite's own lock discipline, so they see a normal
  `SQLITE_BUSY`). In an UNLOCKED window sqlite writers may run; mpedb detects
  the divergence at re-lock and reconciles through the mirror conflict
  machinery (DESIGN-MIRROR §8) before the fast path resumes. Two live writers
  with mutual immediate visibility would require living inside sqlite's
  protocol — that project is called Turso.
- The mirror subsystem (M1–M8, SIGKILL-fuzzed convergence, per-table cursors,
  conflict taxonomy + policies, dialect handling) is the checkpoint and
  reconcile plumbing. This design adds three genuinely new things: a sqlite
  page **reader** (fall-through), the **read-merge** (overlay shadows base),
  and the **two-file checkpoint lifecycle**.

## 1. Files and roles

| file | role | written by |
|---|---|---|
| `app.db` | base: canonical, durable, sqlite-format | the checkpoint's sqlite transaction; foreign sqlite writers only in UNLOCKED windows |
| `app.db.mpedb` | overlay: deltas since the last checkpoint + all mpedb machinery (MVCC, reader table, plan registry) | mpedb, exactly as today |
| `app.db.mpedb-wal` | the overlay's own WAL (durability modes unchanged) | mpedb |

The overlay is a normal mpedb database whose tables carry **delta rows**:
upserted row images and **tombstones** (a per-table hidden `deleted` marker
column in the overlay schema, never visible through SQL). Its schema is
derived from the base at open (`mirror import`'s schema derivation, types via
`ColumnType::Any` for sqlite affinity — #23.1 exists for exactly this).

## 2. Lock modes — Morten's refinement, and why it is the load-bearing choice

### LOCKED (default): the base cannot move, so nobody checks it

At open, mpedb takes **sqlite's own SHARED lock** on the base (the advisory
byte-range locks at sqlite's reserved offsets — `SHARED_FIRST`/`SHARED_SIZE`).
Consequences, all inherited from sqlite's discipline rather than invented:

- Foreign sqlite **readers proceed normally** (SHARED is compatible with
  SHARED). They see the last checkpoint.
- Foreign sqlite **writers block**: their EXCLUSIVE/PENDING acquisition
  conflicts with our SHARED lock, so they get `SQLITE_BUSY` through their own
  busy handler — the failure mode every sqlite program already handles.
- **The fast path needs zero validation**: while the lock is held the base is
  immutable, so fall-through reads carry no version check, no header read, no
  stat. The only cost relative to a pure `.mpedb` is sqlite page decoding on
  cold rows — hot rows live in the overlay and are read at full mpedb speed.

The lock is held by ONE designated process on behalf of the attachment (see
§7 — the mpedb writer-lock owner), not by every reader.

### UNLOCKED (cooperative window): let sqlite tools write, then reconcile

`mpedb release` (API/CLI) drops the SHARED lock after a checkpoint, leaving
the base fully owned by whoever wants it. While unlocked, mpedb may continue
serving **overlay-only** operations, but every fall-through read is refused
(`BaseUnlocked` error) — serving possibly-moving base bytes would be a wrong
answer waiting to happen, and callers chose this window on purpose.

Re-lock validates a **BaseStamp** captured at release:

- the sqlite header **change counter** (4 bytes at offset 24 — bumped by
  every rollback-journal write transaction),
- file size and the `-wal` tail state (salts + frame count) if present,
- schema cookie (offset 40) for DDL drift.

Unchanged stamp → resume LOCKED instantly, overlay still valid. Changed →
**reconcile before resuming**: the mirror pull path re-reads diverged rows
(tracked mode with triggers when installed; full re-diff otherwise — the
`regenerate`/`reconcile` machinery), conflicts land in the DESIGN-MIRROR §8
taxonomy with the same policies (authority-wins default, `manual` parks both
images). Only after convergence does the fast path reopen.

"Om ingen andre trenger vi ikke sjekke .db" — correct, and the lock is what
turns that intuition into an invariant rather than a hope.

## 3. Read-merge (v2)

Per-PK rule: **overlay shadows base**. `get(pk)`: overlay hit (row or
tombstone) answers; miss falls through to the base reader. Scans: a merge
iterator over (overlay in PK order — mpedb's native order) × (base table
b-tree in rowid/PK order via the sqlite reader), overlay wins per key,
tombstones suppress. Secondary-index probes: v2 serves them as overlay-index
probe + **base full scan with residual** (honest and correct); reading
sqlite's index b-trees for base probes is v3. `EXPLAIN` must say which side
each access takes.

RLS/footprints: policies bind over the merged row — same shape as today, the
merge happens below the policy layer. Footprints treat base tables as the
same table id as their overlay twin (one logical table).

## 4. The sqlite page reader (v1)

Read-only, from the mapping of a **locked, quiescent** base: file header,
page b-trees, varint records, overflow chains, freelist skipping. The format
is documented and frozen (sqlite.org/fileformat2). Refusals by name: WAL-mode
bases with a non-empty `-wal` (v1 requires a checkpointed base — `mpedb open`
runs `PRAGMA wal_checkpoint(TRUNCATE)` through the sqlite library first, or
refuses if it cannot), encrypted files, non-UTF8 text per mirror's existing
rules. Every decoder bounds-checked: corrupt input yields `Error::Corrupt`,
never a panic — the house rule.

## 5. Checkpoint: push, then truncate — with a crash story at every arrow

```
freeze overlay epoch E                      (mpedb txn boundary)
  → mirror push of E's deltas into .db     (ONE sqlite transaction)
  → sqlite COMMIT (base now canonical for E)
  → fsync(.db) per its journal mode
  → overlay txn: mark E checkpointed + truncate delta tables
  → mpedb commit (overlay's own durability modes apply)
```

Crash matrix:

| crash point | state on recovery | action |
|---|---|---|
| before sqlite COMMIT | sqlite rolls its journal back; overlay intact | re-push E — idempotent (row images upsert, tombstones delete-if-present) |
| after COMMIT, before overlay truncate | base has E; overlay still has E | re-push E is a no-op by idempotence; then truncate — the marker says E was pushed, skip straight to truncate |
| mid-truncate | mpedb's own atomicity (COW + meta flip) | overlay commit either happened or not; retry |

Idempotence is the entire argument, and it is the same argument mirror push
already survives under `mirror-collide` (a daemon SIGKILLed at every
instant must converge). New writes during the push land in epoch E+1 —
checkpointing never blocks the overlay's writers longer than the freeze.

Truncate ("tømme .mpedb") reclaims overlay space through the normal freelist;
the overlay file itself stays at its configured `size_mb` (mpedb files do not
grow or shrink — the point is the DELTAS stay small, so a small `size_mb`
suffices and checkpoint pressure is the valve; overlay-full = `DbFull` names
`mpedb checkpoint` in the message).

## 6. What "working directly against .db" is true, and what is not

| claim | verdict |
|---|---|
| every sqlite tool can read `app.db` at any time | ✅ (sees last checkpoint) |
| sqlite tools can write `app.db` | ✅ in UNLOCKED windows; `SQLITE_BUSY` while LOCKED — their normal experience of "another writer" |
| mpedb reads+writes at full mpedb speed | ✅ for overlay-resident rows; cold fall-through pays sqlite decoding once per read (v3 option: promote hot fall-through rows into the overlay as a cache — measure first) |
| no import step on open | ✅ schema derivation only; data stays in the base |
| fresh mpedb writes visible to sqlite readers immediately | ❌ visible at next checkpoint — the WAL metaphor's honest edge |
| two writers, both files, simultaneously | ❌ by design; UNLOCKED + reconcile is the supported shape |

## 7. Multi-process, and who holds what

mpedb's own multi-process story is unchanged (the overlay IS an mpedb file).
The base SHARED lock and the checkpoint duty belong to the **writer-lock
owner's incarnation**, recovered exactly like the writer lock itself when a
holder is SIGKILLed (the FLD-2/robust-mutex machinery; the base lock is
re-acquired by the next owner — fcntl locks die with the process, which is
the correct failure direction: a dead mpedb never blocks sqlite writers
forever). Readers never touch the base lock; they read the base through the
owner-validated stamp.

## 8. Staging (build order)

- **v0 — UX over today's machinery, no new engine code**: `mpedb open app.db`
  = mirror import to a sidecar + tracked sync; `mpedb checkpoint` = push;
  full copy rather than overlay. Proves the CLI shape and the checkpoint
  habit. Cheap, useful immediately.
- **v1 — the sqlite reader + read-only attach**: query a locked `.db` with
  zero import, exposed through #51's cross-file machinery. COMPAT.md's
  "open in place" row goes ❌ → 🚧 (read-only).
- **v2 — the overlay**: delta writes, read-merge, BaseStamp, LOCKED/UNLOCKED,
  checkpoint+truncate. COMPAT row → ✅ (with the §6 table's honest edges).
- v3 (measured, maybe never): sqlite index probes for base, hot-row
  promotion cache, WAL-mode bases without pre-checkpoint.

## 9. Open questions for the review

- **Q1**: sqlite WAL-mode bases in v2 — require journal_mode=DELETE at open
  (simple, restrictive) or learn `-wal`/`-shm` reading + their lock protocol
  (large)? v1/v2 ship with the requirement; measure demand.
- **Q2**: non-PK-predicate reads over a large cold base without index probes
  are honest full scans — is v2 acceptable shipping that, with EXPLAIN
  labeling, or does v3's index reader gate the overlay release?
- **Q3**: checkpoint trigger policy — explicit only (v2 default), or also
  size/time watermarks? An unattended attachment that never checkpoints
  makes sqlite readers arbitrarily stale and the overlay arbitrarily full.
- **Q4**: advisory-lock bypass (a raw `cp` or an NFS client that drops
  fcntl locks does not respect SHARED) — BaseStamp re-validation on every
  re-lock catches it after the fact; is a periodic in-LOCKED stamp audit
  (cheap: one header read) worth the syscall?
