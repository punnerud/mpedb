# DESIGN-SQLITE-BACKED: the .db as home, the .mpedb as its delta-"WAL"

Status: **v0.2 — the 20-finding adversarial review of v0.1 is folded in**
([R#n] marks a finding's fix; the review grounded its CONFIRMED findings in
sqlite.org/lockingv3, fileformat2, walformat, howtocorrupt, pragma).
**v0 + v1 + v2 have SHIPPED (2026-07-17, #69) — see §10 for what is code and
where it diverges from this text; only v3 remains.** The prose below is still
written in the design's future tense; the load-bearing mechanisms are the
reviewed ones, and building anything contradicting them re-opens a named
finding. Task #69.

The idea (Morten, 2026-07-17): work directly against a sqlite `.db` as the
durable, canonical home — every sqlite tool can open it — while mpedb provides
the hot path: `.mpedb` is a **delta overlay** playing the role a WAL plays for
a database. All writes and all MVCC/multi-process reads go through the
overlay; untouched data falls through to the base; a **checkpoint** pushes the
deltas into the `.db` and then empties the overlay. Default: mpedb holds
sqlite's own SHARED lock on the base, so the fast path never has to ask
whether the base moved; unlocking is deliberate, detectable, and reconciled.

## 0. Honest scope (read first)

- **This is not sqlite reimplemented.** mpedb never executes sqlite's write
  protocol. Writes reach the `.db` only through the checkpoint's one sqlite
  transaction (via the sqlite library, as `mirror push` already does).
- **Rollback-journal bases only.** A WAL-mode base defeats the entire lock
  model — [R#6]: WAL writers hold only SHARED on the main file (their write
  locks live in `-shm`), so a held SHARED blocks *nothing*, and a WAL
  checkpointer rewrites the main file *under* SHARED. Detection is by fact,
  not assumption: header bytes 18/19 (read/write version; 2 = WAL) are
  checked at open, at every re-lock, and inside every OPTIMISTIC bracket;
  WAL → refuse with the fix in the message (`PRAGMA journal_mode=DELETE`).
  While a SHARED is *held* the mode cannot flip (the transition requires
  EXCLUSIVE); in unlocked windows it can, hence the per-bracket check.
- **sqlite readers see the last checkpoint**, never un-checkpointed overlay
  deltas. Freshness lives in mpedb.
- **Simultaneous writers to both files is not a goal.** LOCKED blocks foreign
  writers via sqlite's own discipline (`SQLITE_BUSY`); unlocked windows admit
  them and are reconciled through DESIGN-MIRROR §8 before the fast path
  resumes. And two windows exist *uncommanded* [R#20]: the per-checkpoint
  relock dance (§5) and an owner crash (§7) — both named, both stamped.
- The mirror subsystem (M1–M8) is the checkpoint/reconcile plumbing. New
  here: the sqlite page reader, the read-merge, and the two-file lifecycle.
- The overlay inherits mirror's structural limits (≤ 56 user tables per
  file, DESIGN-MIRROR §4.1) [R#20].

## 1. Files, roles, and who may create them

| file | role | written by |
|---|---|---|
| `app.db` | base: canonical, durable, sqlite rollback-journal format | the checkpoint's sqlite transaction; foreign writers in unlocked windows |
| `app.db.mpedb` | overlay: deltas since last checkpoint + all mpedb machinery | mpedb |
| `app.db.mpedb-wal` | the overlay's own WAL | mpedb |

**Creation is O_EXCL 0600 and attachment is verified** [R#16]: an existing
overlay must match the base's stored identity (dev+ino via realpath, schema
hash) and the base's owner uid — otherwise a directory-writing attacker
pre-plants an overlay whose fabricated deltas the victim's checkpoint would
push into the base (directory-write → file-write escalation). Scratch files
(granularity probe) are O_EXCL with random names.

Overlay schema derives from the base at open. Types: **sniffed concrete
types per DESIGN-MIRROR §4.5's fidelity rule, not blanket `Any`** [R#17] —
sqlite affinity *coerces on insert*, so an `Any`-typed overlay value pushed
into an INTEGER-affinity column comes back different, and the reconcile sees
a divergence it caused itself (the CONF#35 anti-entropy storm mirror already
closed). Columns that must stay loose use mirror's canonicalizer contract.

**RESOLVED, not by sniffing a concrete type.** [R#17] named the right
failure and the wrong cure. A sqlite file is NOT rigid: an `int` column may
genuinely hold `'abc'`, because sqlite stores whatever survives its
affinity conversion — so a concrete `Int64` overlay column would make mpedb
refuse to READ rows the base happily holds, trading one wrong behaviour for
another. What sqlite actually guarantees is the CONVERSION, so that is what
the overlay carries: the column stays `ColumnType::Any` and gains the base's
declared `Affinity` (`ColumnDef.affinity`, canonical-bytes v7), and mpedb
applies sqlite's store-time affinity itself. A value written through the
overlay is therefore ALREADY in the class sqlite would have put it in, the
push is a no-op for affinity, and the self-inflicted divergence [R#17]
predicted cannot arise. Verified differentially for all five affinities.

The base's NOT NULL and literal DEFAULTs ride along; what cannot be carried
faithfully — CHECK, a non-literal DEFAULT, a GENERATED column — takes the
table out of the attach BY NAME rather than being silently dropped, since a
constraint enforced nowhere let in a row the base itself rejects and failed
the checkpoint later, on an unrelated statement.

## 2. Lock modes

### LOCKED (default): every attaching process holds SHARED

**Each mpedb process takes its own sqlite SHARED lock at attach** [R#7] —
fcntl SHARED is shareable, so this costs one fcntl per attach. The v0.1
single-designated-owner model died in review: fcntl locks die with their
process, so a SIGKILLed owner left every other mpedb reader raw-reading an
unprotected base until the *next writer* happened to run recovery — an
unbounded undetected-torn-read window. Per-process SHARED makes base
protection survive any single death; releasing (for a cooperative window)
becomes a coordinated act through the shm the processes already share.

Consequences, inherited from sqlite's own discipline:

- Foreign sqlite **readers proceed normally** — with one reviewed exception
  [R#8]: if a foreign writer reaches RESERVED, flushes its journal, blocks
  on our SHARED, and crashes, the journal is now *hot*, and every foreign
  reader's mandatory rollback needs EXCLUSIVE → they get `SQLITE_BUSY` until
  we act. mpedb therefore watches for journal appearance while LOCKED
  (cheap stat) and mediates: drop SHARED → one `SELECT 1` through the
  sqlite library (performs the rollback; content-wise a no-op since the
  writer never reached EXCLUSIVE) → re-take SHARED → re-validate the stamp.
- Foreign sqlite **writers block** with their normal `SQLITE_BUSY`.
- **The fast path needs zero validation** while SHARED is genuinely held —
  plus one cheap periodic stamp audit [R#20, Q4-answered]: advisory locks
  bind sqlite tools, not `cp` or an NFS client, and the audit (one header
  pread) is the only detection before the next re-lock.

**Hot journal at open** [R#2]: before the first raw read — at open and at
every re-lock — mpedb opens the base through the sqlite *library* and runs
`SELECT 1`, forcing sqlite's own hot-journal recovery. Raw reads never run
against a base whose journal nobody rolled back.

### OPTIMISTIC (no standing lock): a transient SHARED per fall-through, not a counter trick

v0.1 proposed GETLK + change-counter double-checks. **The review produced a
concrete counterexample** [R#1]: a foreign writer can BEGIN, reach EXCLUSIVE
(nothing stops it — we hold no lock), **spill dirty pages into the base
mid-transaction** (lockingv3 documents cache spill), then ROLLBACK — journal
playback restores every page including the counter's pre-image, all locks
release, and both GETLKs plus the counter equality pass around a read of
garbage. The counter also under-counts by its own definition [R#9]:
fileformat2 says it increments "whenever the database file is **unlocked
after having been modified**" — not per commit (a `locking_mode=EXCLUSIVE`
session bumps once for N transactions; ROLLBACK never bumps).

The sound primitive is sqlite's own reader protocol, minus the rollback:

```
per fall-through STATEMENT:
  F_SETLK SHARED (non-blocking)      — busy → writer active: backoff with a
                                       deadline, then BaseBusy to the caller;
                                       NEVER treated as divergence [R#19]
  hot-journal check                  — per lockingv3's definition (exists ∧
                                       >512 B ∧ valid header); PERSIST-mode
                                       journals need the header read, not an
                                       existence stat [R#1c]; hot → release,
                                       recover via the library, retry
  one 100-byte header pread          — change counter, size, schema cookie,
                                       journal-mode bytes 18/19, all in one
                                       read [R#20]; compare against the stamp
  read the base pages the statement needs
  unlock
```

A held SHARED excludes any EXCLUSIVE — spill and commit alike — for the
bracket's lifetime; the stamp comparison catches committed-since-last-time.
This is "micro-LOCKED per statement": the only shape the review could prove
consistent. Stamp movement (≠ busy) → pause fall-through, reconcile via
mirror, resume on the new stamp; overlay writes continue throughout.

**Isolation is weakened and must say so** [R#12]: mpedb's contract is
snapshot-per-transaction; OPTIMISTIC fall-through re-validates per statement,
so a multi-statement ReadTxn can observe two base states (statement-level
read committed on the base side). This is declared per attachment — an
application that needs the full snapshot contract uses LOCKED. And no
statement output is emitted before its bracket completes — results buffer
until the closing validation passes, never streamed out of an unvalidated
read.

### UNLOCKED-OFFLINE (cooperative window)

All processes release SHARED (coordinated via shm); overlay-only operation;
every fall-through refused (`BaseUnlocked`). For bulk foreign rewrites where
even µs brackets and reconcile churn are unwanted.

## 3. The stamp — one definition, settled at release

**One BaseStamp everywhere** [R#10]: `(mtime, size, change counter, schema
cookie, header bytes 18/19, and if -wal exists: its salt pair (8-byte pread
at offset 16) + size)`. The salts are the monotone WAL witness the counter
cannot be (a WAL reset reuses the file from offset 0 with *new salts and
unchanged size*); bytes 18/19 catch a journal-mode flip in a window [R#6].
The counter's real semantics — "unlocked after modified", with the
EXCLUSIVE-session and WAL under-count exceptions and the 2³² wrap — are
quoted, not paraphrased optimistically [R#9]. The in-header db-size field
(offset 28) is trusted only when its validity rule holds (counter at 24 ==
version-valid-for at 92), else the file size is authoritative [R#20].

**Settling (Morten's granularity trick), specified in the file-clock
domain** [R#11]: while still holding SHARED, touch the scratch file in a
loop until *its* mtime is strictly greater than the base's stamp candidate —
this settles against the filesystem's own timestamp clock, immune to the
wall-clock-vs-file-clock skew and to a probe that lands twice in one tick.
Then read the stamp, then release. Any later mutation lands strictly newer;
one `stat()` answers "touched?" after minutes or days. NFS is refused by
statfs magic by default (both sqlite sources warn; attribute caching defeats
every part of this), opt-in to override.

## 4. The sqlite page reader (v1)

Read-only, against a locked (or bracketed) base: header, table b-trees,
varint records, overflow chains, freelist skipping. Refusals by name:
WAL-mode bases (bytes 18/19), encrypted files, non-UTF8 per mirror's rules.
Every decoder bounds-checked: corrupt input yields `Error::Corrupt`, never a
panic. **Both table layouts are in scope and distinct** [R#15]: rowid tables
(b-tree keyed on rowid; a declared non-INTEGER PK lives in a separate index
b-tree) and WITHOUT ROWID tables (index-b-tree layout as the table).

## 5. Checkpoint — the marker lives in the base, inside the push transaction

```
freeze overlay epoch E                       (mpedb txn boundary)
  → drop this process's base SHARED         [R#5 — see the fcntl trap below]
  → sqlite library: BEGIN IMMEDIATE
      re-validate stamp/counter UNDER the write lock  [R#5c]
      push E's deltas
      update _mpedb_mirror_state: checkpointed_epoch = E   [R#4]
        (epoch-fenced UPDATE … WHERE epoch = $e — mirror §6's own guard)
    COMMIT                                   (synchronous=FULL owns durability
                                              — no after-the-fact fsync [R#13])
  → re-take SHARED, settle + re-stamp
  → overlay: mark E checkpointed + truncate deltas in BOUNDED batches,
    each its own commit                      [R#14]
```

**Why the marker moved into the base** [R#4]: v0.1 put it in the overlay's
truncate commit — so in exactly the crash window the matrix cited it for
(after sqlite COMMIT, before overlay commit), it never existed. In the base,
"was E pushed?" is readable from the base itself, atomically with the push —
mirror §5.4's "DECIDE before mutate" applied to checkpoints.

**Why re-push is NOT blindly idempotent** [R#3]: our SHARED dies with a
crash, so the base is unlocked all through the crash window and a foreign
writer may commit — re-pushing E would then overwrite their v2 with our v1
and resurrect tombstoned rows. Idempotence holds against *ourselves*, not
third parties. Recovery therefore reads `checkpointed_epoch` and validates
the stamp **before any re-push**; a moved stamp routes to full reconcile
(mirror §5.5 merge-diff + §8 policy), never to a blind replay.

**The POSIX fcntl trap** [R#5]: classic POSIX locks are per (process,
inode); sqlite's `close()` cancels *all* the process's locks on the file
(howtocorrupt §2.2), and its own unlock at COMMIT releases the byte range
regardless of which fd locked it. So the checkpoint's library use silently
destroys a naive raw SHARED — every checkpoint would open an unnoticed
unlocked window. The design therefore names the dance explicitly (drop →
push with in-transaction re-validation under BEGIN IMMEDIATE's write lock →
re-take → re-stamp), and prefers **OFD locks (`F_OFD_SETLK`)** for mpedb's
own SHARED where available, so library close/unlock cannot cancel it
(macOS OFD support: verify on the M3 before relying on it there). Either
way the per-checkpoint window is *stamped and validated*, not assumed away.

**Truncate under DbFull pressure** [R#14]: deleting delta rows is COW — it
*allocates* before commit frees, so truncating a full overlay in one
transaction deadlocks against the very condition checkpoint is the valve
for. Bounded batches (each commit frees), the marker via the reserved pool,
and a pre-flight COW-footprint estimate (mirror §5.4's) before checkpoint
starts. `mirror regenerate` remains the named last resort.

## 6. Read-merge (v2)

Per-PK: overlay (row or tombstone) shadows base. Scans [R#15]: the merge
iterator requires base order == overlay PK order, which holds for
INTEGER-PK rowid tables and WITHOUT ROWID tables; declared-non-INTEGER-PK
rowid tables (base order = rowid ≠ PK) are served in v2 as base scan +
per-row overlay probe, honestly labeled in EXPLAIN — or wait for v3's index
reader. Secondary-index probes: overlay-index probe + base scan with
residual (v2), sqlite index b-trees (v3).

RLS binds over the merged row. **Schema drift rules** [R#18]: a table
created in the base during a window attaches as pull-only with
policy-default-deny until an operator enables it; plan validation carries a
schema epoch so base DDL invalidates registered plans atomically with the
reconcile; type drift with unpushed deltas parks them under a dedicated
kind (mirror's park store) rather than guessing a conversion.

## 7. What "working directly against .db" is true, and what is not

| claim | verdict |
|---|---|
| every sqlite tool can read `app.db` at any time | ✅ — last-checkpoint state; footnote [R#8]: a crashed foreign writer's hot journal blocks foreign readers until mpedb's mediation runs |
| sqlite tools can write `app.db` | ✅ in unlocked windows (commanded, per-checkpoint [R#5], or post-crash [R#3]) — all stamped and reconciled; `SQLITE_BUSY` otherwise |
| mpedb reads+writes at full mpedb speed | ✅ overlay-resident; cold fall-through pays sqlite decoding (v3 option: hot-row promotion, measure first) |
| no import step on open | ✅ schema derivation only — except v0, whose tracked sync installs mirror triggers in the base (visible to sqlite tools; v0's honest edge [R#20]) |
| fresh mpedb writes visible to sqlite readers immediately | ❌ at next checkpoint |
| two writers, both files, simultaneously | ❌ by design |
| OPTIMISTIC preserves mpedb snapshot isolation on base reads | ❌ statement-level read committed, declared per attachment [R#12] |

## 8. Staging

- **v0 — UX over today's machinery**: `mpedb open app.db` = mirror import +
  tracked sync; `mpedb checkpoint` = push. Full copy, triggers in the base
  (named edge), no new engine code.
- **v1 — sqlite reader + read-only attach** via #51; COMPAT "open in place"
  ❌ → 🚧.
- **v2 — the overlay**: deltas, read-merge, stamp machinery, LOCKED /
  OPTIMISTIC / UNLOCKED-OFFLINE, checkpoint per §5. COMPAT → ✅ with §7's
  table.
- v3 (measured, maybe never): sqlite index probes, hot-row promotion,
  WAL-mode bases.

## 9. Open questions (post-review residue)

- **Q1**: OFD-lock portability — Linux yes; macOS `F_OFD_SETLK` must be
  verified on the M3, else macOS runs the drop/re-take dance only.
- **Q2**: is v2 acceptable shipping non-INTEGER-PK rowid tables as
  probe-per-row scans (honest but O(n) probes), or does that gate v3?
- **Q3**: checkpoint trigger policy — explicit only, or size/time
  watermarks against unbounded staleness + overlay pressure.
- **Q4**: the OPTIMISTIC bracket's real cost after the review's fixes
  (SETLK pair + journal check + 100-byte pread) — measure; if the µs claim
  doesn't survive, say the honest number in §2.

## 10. v0–v2 shipped (2026-07-17, #69) — and the doc-vs-code drift

**What shipped**: `crates/mpedb/src/sqlite_overlay.rs` (~1,290 lines) —
`SqliteOverlay::open`/`open_with_mode`/`open_with`, all three §2 lock modes
(`LockMode::{Locked, Optimistic, Offline}`), the §3 stamp, the §5 checkpoint
including `recover_after_crashed_checkpoint` against the base-side marker
table, the §6 read-merge over upserts + tombstones, and `reconcile` with
per-PK `ConflictPolicy::{Ours, Theirs}`. Tests:
`crates/mpedb/tests/sqlite_overlay.rs` (21) plus
`crates/mpedb/tests/sqlite_checkpoint.rs`. v1's native page reader is
`crates/mpedb-sqlitefmt` (no sqlite library in the read path); v0's full
sidecar mirror is still there behind `--mirror`.

**The drift, deliberate, follow the code:**

- **The overlay is the DEFAULT, not an option.** `mpedb <file.db>` opens the
  v2 delta overlay with zero import (`crates/mpedb-cli/src/openpath.rs`);
  `--mirror`/`--sidecar` opts *back* into v0's full copy, `--direct` is the
  read-only native reader. The module doc in `openpath.rs` still describes v0
  as the flow — believe `main.rs`'s usage text and the argument parsing.
- **The overlay file is `<file>.overlay.mpedb`**, not §1's `app.db.mpedb`
  (that name belongs to the v0 sidecar, and both can exist beside one base).
- **Checkpoint sits behind the `sqlite-checkpoint` feature**, since pushing
  deltas back is the one path that links the sqlite library — which is also
  why `mpedb-capi` cannot be a default workspace member (DESIGN.md §8).
- Everything §8 lists as **v3 is still unbuilt**: sqlite index probes,
  hot-row promotion, WAL-mode bases. WAL bases are refused by fact
  (header bytes 18/19) exactly as §0 specifies, not by assumption.
