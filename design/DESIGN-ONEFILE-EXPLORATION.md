# DESIGN-ONEFILE-EXPLORATION: mpedb living *inside* the .db — no sidecar

Status: **v0.1 draft — an exploration, not a reviewed design.** Nothing here
is committed; the point is to find out whether the idea survives contact with
sqlite's actual file format and lock protocol, and to say so honestly if it
does not. Grounding: sqlite.org/fileformat2 and sqlite.org/c3ref/blob_open
(both fetched 2026-07-17), DESIGN-SQLITE-BACKED v0.2's 20-finding-reviewed
lock facts, and `crates/mpedb-sqlitefmt` (our native format reader — the
overflow-chain and spill math below is the code's, differentially tested
against the real library).

The question (Morten, 2026-07-17): can mpedb operate against **only** a
regular sqlite `.db` file — one file format, no separate `.mpedb` sidecar?
The reference alternative it must beat, not merely coexist with, is
DESIGN-SQLITE-BACKED v0.2: the `.db` as canonical home plus a `.mpedb` delta
overlay, with LOCKED / OPTIMISTIC / UNLOCKED-OFFLINE modes and a checkpoint.
"Is one-file better than the v2 overlay?" is the question §2 must answer.

## 0. Ground truth — what every verdict below stands on

### 0.1 sqlite format facts (GROUNDED, sqlite.org/fileformat2)

- **G1 — overflow chains are linked lists, nothing more.** §1.7: "Overflow
  pages form a linked list. The first four bytes of each overflow page are a
  big-endian integer which is the page number of the next page in the chain,
  or zero for the final page in the chain." The format guarantees **no
  contiguity whatsoever** between the pages of one blob.
- **G2 — every overflow page burns its first 4 bytes.** Content per page is
  `usable − 4`. So even a blob whose pages *are* physically sequential is
  **not a linear byte range**: a 4-byte hole recurs every page.
- **G3 — allocation order is unspecified.** §1.5 documents the freelist
  structure and that unused pages "are reused when additional pages are
  required", but the *algorithm* (freelist-first vs end-of-file, ordering)
  is implementation behavior, not contract. Fresh-file zeroblobs coming out
  physically sequential is an observation about btree.c, never a guarantee
  (E1 measures it; nothing may depend on it for correctness).
- **G4 — pages move.** §1.8: ptrmap pages exist precisely so auto_vacuum /
  incremental_vacuum can relocate pages; entry types 3 and 4 track overflow
  pages specifically. Only b-tree *root* pages are guaranteed immobile ("all
  b-tree root pages must come before any non-root b-tree page … ensures
  that a root page will never be moved"). Overflow pages are movable by
  design. Header offset 52 == 0 ⇔ no ptrmap pages ⇔ neither vacuum mode is
  possible — that byte is checkable and lockable-in at adopt time.
- **G5 — the lock-byte page.** §1.4: the page covering byte offsets
  1073741824–1073742335 "is never read or written by the SQLite core". A
  file > 1 GiB has a permanent hole no blob's chain will ever cover.
- **G6 — the change counter is a *sqlite-unlock* artifact.** §1.3.6: it "is
  incremented whenever the database file is unlocked after having been
  modified". Raw mmap stores never bump it — sqlite-side page caches and
  freshness checks are structurally blind to writes that bypass the library.
- **G7 — size ceilings.** Format: up to 2,147,483,647 bytes per cell
  payload (§1.6). The *library* additionally enforces SQLITE_MAX_LENGTH,
  default 1e9 (limits.html; from memory — E6 verifies). Region rows are
  therefore ≲ 1 GB each with a stock CLI doing the creating.
- **G8 — spill formulas** (§1.6, implemented in `sqlitefmt::cell_payload`):
  table-leaf `X = U−35`, `M = ((U−12)·32/255)−23`, `K = M + (P−M) mod
  (U−4)`. For a multi-hundred-MiB blob effectively the whole payload lives
  on overflow pages; the b-tree leaf keeps ~`M` bytes + the chain head.
- **G9 — freelist pages belong to everyone.** Trunk pages are live linked
  structure; leaf pages are never read or written — but both are *reusable
  by any writer at any time*. Parking data there is not ownership.

### 0.2 Incremental-blob facts (GROUNDED, c3ref/blob_open)

- **B1 — handles expire on any row change.** "If the row that a BLOB handle
  points to is modified by an UPDATE, DELETE, or by ON CONFLICT
  side-effects then the BLOB handle is marked as 'expired'. This is true if
  any column of the row is changed, even a column other than the one the
  BLOB handle is open on." Reads/writes then fail SQLITE_ABORT. The flip
  side is the physical fact that matters here: **an SQL UPDATE of the row —
  any column — rewrites the record and reallocates its overflow chain.**
- **B2 — no resize through the handle**: "The size of a blob may not be
  changed by this interface."
- **B3 — zeroblob is the intended preallocation path** for incremental I/O.
- **B4 — the documentation is silent on cross-process validity and physical
  placement.** There is no handle guarantee to lean on across processes or
  time. Whatever pins physical offsets, it is not this API.

### 0.3 mpedb facts (the invariants that must survive, with cites)

- **M1** — PAGE_SIZE 4096; page 0/1 meta A/B, 2 lock area, 3.. reader
  table, then data (DESIGN.md §4). `Shm` assumes **one linear mapping**:
  `at(id * PAGE_SIZE)` everywhere (`crates/mpedb-core/src/shm.rs:718–748`,
  meta msync `shm.rs:1127`).
- **M2** — robust ERRORCHECK mutex, reader table (packed {pid,seq}
  generation words), meta double-buffer, intent ring all live *in* the
  mapping at fixed offsets; the SeqCst fence pairs and the ring's
  incarnation rules (DESIGN.md §4.3/§5.3) are position-independent but
  **stability-dependent**: the bytes must never move or be rewritten by
  anyone but mpedb.
- **M3** — the file is `fallocate`d at format and never grows in Phase 1
  (DESIGN.md §3); growth-by-remap is the sanctioned Phase-2 shape.
- **M4** — `durability = wal` is *already a second file* (`<db>-wal`,
  `shm.rs:239`), pwrite-appended, preallocated in 4 MiB chunks. A true
  one-file design must host the WAL too.
- **M5** — DESIGN-BLOBEXTENT commits to physically contiguous page runs:
  `pwritev` at `start_page * PAGE_SIZE`, FICLONERANGE import, FrozenDb's
  one-HTTP-Range reads. Any geometry that breaks file-linear runs taxes
  that design directly.
- **M6** — Apple Silicon's OS page is 16 KiB (`shm.rs:1241`) — every mmap
  offset must be 16 KiB-aligned there.

### 0.4 The two facts that shape everything

- **F1 — the only sqlite-legal way to own pages inside a .db is to be the
  payload of a row** (an overflow chain). The alternatives all fail: the
  freelist is reusable by any writer (G9); unreferenced in-bounds pages are
  `PRAGMA integrity_check` errors and VACUUM discards them; bytes past the
  in-header size are garbage the library may truncate at the next commit
  (implementation behavior — E4 makes it a fact in minutes).
- **F2 — nothing in the format pins a page's physical location.** The only
  fastener is the lock ladder: a **permanently held SHARED** (per-process,
  exactly DESIGN-SQLITE-BACKED §2's reviewed LOCKED mode) plus
  `auto_vacuum == 0` verified at adopt (G4) plus a permanent VACUUM ban.
  And advisory locks bind sqlite tools only — `cp`, NFS clients, and hex
  editors bypass them, the same audit caveat the two-file design carries
  ([R#20] there). One-file inherits it with higher stakes (§1.1e).

## 1. The six approaches

Each is quoted verbatim (Morten, 2026-07-17, Norwegian), then measured
against the hard questions: (a) contiguity, (b) relocation, (c) tool
compatibility, (d) mpedb's invariants, (e) write interleaving vs the
two-file reference, (f) durability.

### 1.1 A1 — fixed blob region + dual access — **VIABLE-WITH-CONSTRAINTS**

> Fast allokert BLOB-region + dual access: forhåndsallokerte store BLOB-er;
> hele mpedbs sideområde (meta, reader-table, COW-sider, freelist) inne i
> BLOB-regionen; prosesser regner fysiske fil-offsets og mmap-er direkte;
> liten SQLite-kontrolltabell med epoch/root-pekere/høyeste offset; commit =
> COW-oppdatering i mappet region + kort SQLite-txn som bare oppdaterer
> epoch+root.

**(a) Contiguity — dead as literally stated, alive after two amendments.**
"Processes compute physical file offsets" over a linear region assumes the
blob is a linear byte range. It is not, three times over: no contiguity
guarantee (G1/G3), the recurring 4-byte next-pointer (G2) — which also
breaks 8-byte alignment of anything placed at `page_start + 4` (atomics and
the robust mutex fault or are UB on ARM at misaligned addresses) — and the
1 GiB lock-byte hole (G5). The amendments:

1. **A page-translation table, never an offset formula.** At attach, walk
   the region rows' overflow chains (sqlitefmt already walks chains; E1
   adds page-number exposure) and record `mpedb chunk → file offset`.
   Physical sequentiality, when it occurs, is a *performance* fact.
2. **Aligned carving.** sqlite page_size 65536; inside each 64 KiB overflow
   page, mpedb content starts at +4096 → 15 aligned 4 KiB mpedb pages per
   sqlite page, 6.25 % waste, every mpedb page 4 KiB-aligned in the file.
   On Apple Silicon the mmap offset must be 16 KiB-aligned (M6) → content
   at +16384 → 12 pages, **25 % waste**. That is a real platform tax.

With carving, `Shm`'s linear-view assumption (M1) can survive **unchanged**
via a fixed-address mosaic: reserve one PROT_NONE region, then
`mmap(MAP_SHARED|MAP_FIXED)` each chunk's file range at consecutive virtual
addresses. The virtual address space is contiguous even when the file is
not; `at()`, the fences, msync ranges, even cross-page borrows keep
working. The bound is `vm.max_map_count` (default 65530): a fully
fragmented 4 GiB region needs ~70k mappings — over the default. Physically
sequential runs collapse into one mapping each, so E1's fragmentation
census is the feasibility number, and heavy fragmentation degrades or
refuses at attach rather than assuming.

**(b) Relocation — fastened only by discipline, never by sqlite.**
Anything that rewrites the anchor row moves every page: an SQL UPDATE of
*any* column reallocates the chain (B1), VACUUM rewrites the whole file,
auto_vacuum relocates overflow pages by design (G4). Blob handles promise
nothing across processes (B4). The fasteners: `auto_vacuum == 0` checked at
adopt (header offset 52) and unflippable thereafter (the transition needs a
write txn our SHAREDs block); **no SQL UPDATE of region rows, ever** (region
resize = INSERT new row, new region, remap); **no VACUUM, ever** — the file
never shrinks, deliberately; WAL-mode refusal by header bytes 18/19 (under
WAL a held SHARED blocks nothing and a checkpointer rewrites the main file
under it — DESIGN-SQLITE-BACKED [R#6]).

**(c) Tool compatibility — honest answer: this is "one file", not "sqlite
can read your data".** Every tool opens the file; the data inside the blob
is opaque mpedb format. Worse, sqlite-level copies of a *live* database
(`.dump`, `.backup`, VACUUM INTO from a read-only party) capture region
bytes through a cache the change counter never invalidates (G6): stale
and/or torn region content, silently. sqlite-level backup is valid only at
a quiesced checkpoint boundary. Against the stated goal, A1 alone is closer
to cheating than to compliance — §1.5 is what repairs it.

**(d) mpedb invariants — survive, given (a)+(b).** Meta A/B, lock area,
reader table, intent ring are the same bytes at translated offsets; robust
mutex EOWNERDEAD, {pid,seq} generation words, /proc start-time identity,
boot-id recovery, and the SeqCst pairs are agnostic to *where* the
MAP_SHARED bytes live — they require only stability and exclusivity, which
(b)'s discipline provides. The flock-based init lock and the FLD-2 flock
writer lock coexist with sqlite's fcntl/OFD locks (independent lock
namespaces on Linux; verify macOS in E8). SIGKILL recovery is unchanged
because every participating structure is mpedb-owned; "the file is another
engine's b-tree" matters only at the (few, sqlite-library-mediated) anchor
touchpoints.

**(e) Write interleaving — the catastrophic-vs-reconcilable divide.** In
the two-file design a foreign base write makes data *stale*; reconcile
(mirror §8) repairs it. Here a foreign write can *move or rewrite the bytes
under live mappings* — under the robust mutex, under the reader table —
which is memory corruption of running processes, and every committed-but-
uncheckpointed mpedb transaction is unrecoverable (there is no overlay file
sitting safely elsewhere). Consequently **LOCKED-forever is mandatory, not
a default**: OPTIMISTIC and UNLOCKED-OFFLINE cannot exist in one-file form.
Two of the reference design's three modes are structurally impossible.

**(f) Durability — the hot path survives intact; the anchor txn is rare.**
The literal proposal's "short SQLite txn per commit updating epoch+root"
would put a journal write + two fsyncs on every commit — a regression with
no compensating benefit. The fix: mpedb's meta A/B double-buffer lives
*inside the region* and the commit protocol (COW msync → meta flip msync,
unchanged fence order) runs exactly as today; the sqlite control row is
touched only at region create/grow and checkpoint. Region pages appear in
sqlite's journal only during the creation txn; afterwards sqlite never
touches them, so journal playback of any later rollback cannot clobber them
(E5 proves it, including the cache-spill case that killed the v0.1
counter-trick in DESIGN-SQLITE-BACKED [R#1]). Hot-journal recovery via the
library (`SELECT 1`) runs at attach and at every re-lock before any raw
read ([R#2] verbatim).

**Verdict: VIABLE-WITH-CONSTRAINTS.** The constraint list is long and every
item is load-bearing: translation table + mosaic (no linearity
assumption), 64 KiB carving with 6.25 % (Linux) / 25 % (Apple Silicon)
waste, `auto_vacuum == 0`, LOCKED-forever, VACUUM banned, region rows never
UPDATEd, WAL-mode refused, growth by new zeroblob rows + remap, sqlite-level
backup only at quiesce. And DESIGN-BLOBEXTENT's contiguous-run commitments
(M5) degrade to ≤ 60 KiB islands: `pwritev` scatters, FICLONERANGE is
per-chunk at best, FrozenDb's one-Range-request property dies.

### 1.2 A2 — custom VFS with page ownership — **DEAD**

> Custom VFS med side-eierskap: VFS tar over page-I/O; «mpedb-eide»
> sideintervaller serveres fra mpedbs COW-lag; flush tilbake til
> SQLite-pageren ved commit; reader-tabell/intent-ring i shared memory
> utenfor fila.

Three killers, each sufficient:

1. **A VFS binds only the process that loads it.** VFS selection happens at
   `sqlite3_open_v2` time, per connection; nothing in the file can demand
   it. The dangerous client — the vanilla writer that would trample owned
   ranges — is precisely the one the mechanism cannot see (same class of
   failure as A6).
2. **Between flushes the on-disk file is not a valid sqlite image.** If
   "mpedb-owned page intervals are served from the COW layer", then the
   truth of those intervals lives in mpedb, and a vanilla reader opening
   the raw file mid-flight reads garbage-or-stale with no marker. The
   approach breaks the exact property ("any tool can open the file") it
   exists to preserve.
3. **"Reader table / intent ring in shared memory outside the file" is the
   sidecar, renamed.** A `/dev/shm` object or `-shm` file is a second file
   with a different lifetime. The one-file premise is abandoned in the
   approach's own last clause.

What remains is a harder-to-ship two-file design. Nothing to salvage that
A1/A5 don't already contain.

### 1.3 A3 — epoch-fenced dual mapping, reserved high region — **DEAD as stated**

> Epoch-fenced dual mapping: alle prosesser mapper hele .db; reservert høyt
> område til COW-struktur; SQLite-authority-tabell med current_epoch/root;
> lesere henter epoch via SQL, bytter til direkte minnetilgang; writers
> publiserer med én atomisk SQLite-oppdatering.

The killer is F1: **sqlite has no notion of a "reserved high area".** The
candidate homes for it all fail by the format's own rules:

- *Past the in-header database size*: garbage that the library may truncate
  at the next write-transaction commit (the pager restores the file to its
  logical size; implementation behavior — E4 turns it into a screenshot).
  One foreign — or our own — anchor commit, and the COW structure is gone.
- *In-bounds but unreferenced*: `integrity_check` reports it as corruption,
  and VACUUM (which we cannot ban for *other* people's copies of the file)
  discards it.
- *On the freelist*: reusable by any writer at any instant (G9).

The only surviving home is a blob's overflow chain — at which point this
**is** A1: the epoch/root authority table, the SQL-fetch-then-direct-memory
reader handoff, the single-UPDATE publication are precisely A1's control
table and mpedb's meta flip. The idea's contribution survives; the
"reserved high region" does not.

### 1.4 A4 — append-only blob log + materialization — **DEAD as a distinct approach**

> Append-only BLOB-logg + materialisering: BLOB-er som append-only logg av
> COW-/intent-records; append-offset koordinert via SQLite; leader
> materialiserer konsistent tilstand til en «clean» BLOB; nye writes til ny
> epoch under materialisering.

Two readings, both fail:

- **Appends through the sqlite API** (`sqlite3_blob_write` into a
  preallocated zeroblob — in-place, journaled, honest): every append is a
  sqlite write transaction — RESERVED→EXCLUSIVE, journal, fsyncs,
  serialized across all writers. The write rate *is* sqlite's write rate.
  mpedb's reason to exist evaporates; sqlite alone would be simpler.
- **Appends via raw mmap** into the blob region: that is exactly A1's
  discipline — plus a second log format, plus an append-offset consensus,
  plus a materializer that is a whole second checkpoint protocol and
  doubles the space high-water while running ("clean" blob beside the old
  one).

The salvage is small and real: mpedb already *has* an append-only log — the
`-wal` file (M4) — and hosting it as a second region row under A1's rules
is the one-file answer for `durability = wal`. That is a line item inside
A1/A5, not an architecture.

### 1.5 A5 — hybrid metadata + data blob — **VIABLE-WITH-CONSTRAINTS** (the strongest shape)

> Hybrid metadata + data-BLOB: vanlig relasjonsdata i SQLite-tabeller;
> MVCC-dataene i store BLOB-er; SQL-views/virtuelle tabeller eksponerer
> snapshot fra BLOB via epoch; synk ved epoch-bytte.

This is DESIGN-SQLITE-BACKED's overlay **moved inside the base**: real rows
in real sqlite tables (readable by every tool at last-checkpoint freshness —
the property A1 gives up), and the overlay + all mpedb machinery in an
A1-ruled blob region in the same file. All of A1's constraints inherit
unchanged. What is new:

- **The checkpoint's push txn grows real tables in the same file.** Safe
  under the A1 regime: with `auto_vacuum == 0`, allocation (freelist or
  EOF) never *moves* existing pages, so the region stands still while
  tables grow around it. Region rows deleted at a resize feed the freelist
  and later table growth — controlled recycling.
- **The EXCLUSIVE dance is tighter than two-file's, but its window is
  worse.** Checkpointer C runs BEGIN IMMEDIATE (RESERVED coexists with the
  other processes' SHAREDs), pushes, then COMMIT climbs to PENDING — which
  blocks *new* foreign SHAREDs — while our processes drop SHARED on a shm
  signal; C takes EXCLUSIVE, commits, unlocks; our processes re-take
  SHARED. Almost every instant is covered by our SHAREDs or C's
  PENDING/EXCLUSIVE — but the gap between C's unlock and re-SHARED is
  [R#5]'s window with **catastrophic instead of reconcilable**
  consequences (§1.1e). It must be stamped, and the region's canary
  (magic + epoch in a fixed chunk) re-validated before any process trusts
  its mapping again; a moved stamp means refuse-and-regenerate, not
  reconcile. This is the single scariest line in the one-file story.
- **"SQL views/virtual tables exposing the snapshot" is dead on arrival** —
  a virtual table requires an extension loaded per connection (A6's
  killer). The honest offer is exactly two-file §7's: sqlite readers see
  the last checkpoint; freshness lives in mpedb.
- **Dual journaling never overlaps, by construction.** sqlite journals the
  pages it writes (page 1, control table, real tables during checkpoint);
  mpedb msyncs region pages; after region creation no page is ever in both
  sets. The torn-state matrix: crash inside an mpedb commit → meta
  double-buffer recovery, as today; crash inside the checkpoint txn → hot
  journal, rolled back by the library at next attach *before* any raw
  region read ([R#2]), and the base-resident `checkpointed_epoch` marker
  ([R#4]) says whether E pushed; crash in the re-lock gap → the stamp
  check decides. No new torn class was found on paper — E5 is the test
  that gets to disagree.

**Verdict: VIABLE-WITH-CONSTRAINTS** — the only shape that keeps the
"sqlite tools read real data" half of the goal while going one-file. It is,
frankly, the two-file design with the `.mpedb` bytes relocated into blob
extents inside the `.db`, purchasing single-file packaging at the price
list in §2.

### 1.6 A6 — page ownership via extension/hooks — **DEAD**

> Side-eierskap via extension/hooks: loadable extension m/ pre-update
> hooks/authorizer markerer mpedb-eide sider/tabeller; hindrer
> gjenbruk/flytting; mpedb mapper og administrerer akkurat de sidene.

Three killers:

1. **Hooks and authorizers are per-connection, in-process, voluntary.**
   They constrain exactly the processes that opted in — and no others. The
   threat model is the process that did *not* load the extension; against
   it this provides zero protection. (A guard that only the guarded can
   see is not a guard.)
2. **There is no sqlite API to pin a page's physical location.** The pager
   has no concept of it; hooks operate at row/statement altitude, and
   ptrmap-driven relocation (G4) happens below anything a hook observes.
   "Hindrer gjenbruk/flytting" names an operation the extension surface
   cannot express.
3. **VACUUM's internal page traffic fires no preupdate hooks**; an
   authorizer can veto the VACUUM *statement* in its own connection only.

Nothing to salvage. Ownership must be structural (F1) and fastened by locks
(F2); voluntary in-process markers are neither.

## 2. The most promising form, held against the two-file reference

**The shape: A5 with A1's mechanics — the region-hosted overlay.**
Concretely: a control table `_mpedb_region(id, kind, bytes)` whose rows are
zeroblob extents (each ≲ 1 GB, G7), kinds `core` (meta A/B, lock area,
reader table, intent ring, COW page space, freelist) and `wal` (the log,
M4). Attach: open via the sqlite library and `SELECT 1` (hot-journal
recovery), read the control rows, walk their overflow chains with
sqlitefmt into a translation table, build the MAP_FIXED mosaic, then run
mpedb's existing attach (init handshake, boot-id check, reader slot) on the
virtual view, unchanged. Take the per-process SHARED. Checkpoint pushes
overlay deltas into real tables per DESIGN-SQLITE-BACKED §5, marker in the
base ([R#4]), with §1.5's PENDING dance.

Honest ledger against the reference:

| axis | two-file v0.2 (reference) | one-file (region-hosted overlay) |
|---|---|---|
| files on disk | `.db` + `.mpedb` (+ `-wal`) | `.db` only |
| sqlite readers | last checkpoint ✅ | last checkpoint ✅ (blob opaque; live sqlite-level backup invalid — G6) |
| sqlite writers | ✅ in unlocked windows, reconciled | ❌ **never** — no unlocked mode exists |
| lock modes | LOCKED / OPTIMISTIC / UNLOCKED-OFFLINE | LOCKED-forever, only |
| foreign-write worst case | stale data → reconcile (mirror §8) | live-mapping corruption; uncheckpointed commits **lost**; regenerate |
| VACUUM / auto_vacuum on the base | tolerated in windows (stamp → reconcile) | banned forever / `== 0` enforced; file never shrinks |
| durability hot path | today's (msync meta flip) | today's (flip inside region); sqlite txns only at checkpoint/resize |
| commit-path engine diff | sqlite page reader + merge, engine core untouched | translation + mosaic **under `Shm`** — commit-path-adjacent code |
| blob extents (M5) / FrozenDb | intact | runs fragment at ≤ 60 KiB; one-Range reads die |
| space overhead | overlay file (transient, checkpoint-bounded) | 6.25 % Linux / **25 % Apple Silicon**, permanent |
| failure containment | overlay survives any base accident | one accident, one file, everything |

**Is it better than the v2 overlay? No.** It deletes two of three lock
modes, converts the foreign-writer worst case from "reconcile" into
"unrecoverable loss of committed transactions", taxes Apple Silicon 25 %,
breaks the blob-extent contiguity commitments, and moves new complexity
(translation, mosaic, map-count limits) *under* `Shm` — the layer CLAUDE.md
tells us to fear touching. What it buys is exactly one thing: single-file
packaging (no sidecar to forget, orphan, or mismatch — the [R#16]
identity-check class of bugs disappears). It does **not** buy safe live
copies (`cp` of a running db is equally invalid in both designs), and the
two-file design plus `mirror export` already produces a single-artifact
ship when one is wanted.

The honest recommendation: **DESIGN-SQLITE-BACKED stays the design of
record.** The one-file form is worth keeping in the drawer as a
*deployment profile* of it — same overlay logic, different overlay home —
for a hypothetical appliance whose only writers are mpedb processes and
whose ops mandate one artifact. Do not build it without that customer. Run
§3's cheap experiments regardless: they bank physical facts (contiguity
census, rollback non-interference, truncation) that both this document and
the sqlite-backed reader work benefit from.

## 3. Experiment program — ranked by how fast each can kill

Small, measurable, mostly a day each. Kill criteria stated up front so the
outcome is a fact, not a mood. E1/E4/E5 are the executioners; run them
first.

- **E1 — contiguity census** (feasibility of translation + mosaic; kills
  the offset-formula reading of A1 formally). Fresh db, `page_size=65536`,
  `auto_vacuum=0`: INSERT `zeroblob(256 MiB)`; walk the chain with
  sqlitefmt (small addition: expose chain page numbers from
  `cell_payload`) and count maximal sequential runs. Repeat after freelist
  pollution (create/fill/drop a table first) and with two interleaved
  blob inserts. Expected: fresh ≈ 1 run (observed btree.c behavior, G3
  says never rely on it); polluted = many runs. Output number: mappings
  needed per GiB → feasibility against `vm.max_map_count`. **Kill**: if
  even the fresh case fragments badly, the mosaic dies and with it A1/A5.
- **E2 — in-place stability under the blob API.** `sqlite3_blob_write`
  across the region: chain page numbers before == after? (Expect yes:
  pager writes in place, B2.) Then UPDATE an *unrelated column* of the
  anchor row: expect **full chain reallocation** (B1) — confirming the
  never-UPDATE rule is load-bearing, not paranoia.
- **E3 — neighbor churn × vacuum matrix.** Heavy unrelated DML in other
  tables, `auto_vacuum=0`: chain page numbers must not move (**required
  PASS**). Same with `auto_vacuum=FULL`: expect relocation (G4) — the
  offset-52 gate is load-bearing. Also attempt `PRAGMA auto_vacuum`
  flip + VACUUM while N processes hold SHARED: expect SQLITE_BUSY.
- **E4 — tail truncation** (formally executes A3). Append 1 MiB of raw
  bytes past EOF; run one unrelated INSERT through the library; `stat`.
  Expected: tail gone at commit. If it somehow survives, A3 gets one more
  look — but F1's other two prongs still stand.
- **E5 — rollback/spill non-interference** (the make-or-break for A1/A5).
  Process A mmaps the region and continuously writes canaries; process B
  runs a big txn with a tiny `cache_size` (forcing spill to EXCLUSIVE, the
  [R#1] scenario) and ROLLBACKs. Canaries intact? **Required PASS** —
  journal playback must only touch journaled pages, and region pages are
  in no journal. Then the destructive twin: B runs VACUUM while A is live.
  Document the corpse — that transcript *is* the justification for
  LOCKED-forever, worth more than a paragraph of argument.
- **E6 — the 1 GiB hole and the length ceiling.** Build a 2 GiB region
  across multiple rows; verify the chain skips the lock-byte page (G5) and
  the translation handles the seam; verify `zeroblob(>1e9)` is refused at
  default SQLITE_MAX_LENGTH (G7's from-memory claim becomes grounded).
- **E7 — mosaic + alignment micro.** Build the MAP_FIXED mosaic over an
  E1 file; run a robust mutex + CAS loop and a meta-flip msync at
  translated offsets; measure flip latency vs a `.mpedb` baseline; repeat
  on the M3 with 16 KiB-aligned carving (the 25 % waste variant). Kill:
  any measurable flip regression > noise, or macOS mmap refusing the
  offsets.
- **E8 — lock discipline end-to-end.** N processes hold OFD SHARED; stock
  `sqlite3` CLI attempts a write → SQLITE_BUSY (required). flock (init
  lock, FLD-2) + fcntl coexistence on Linux and macOS. Time §1.5's
  PENDING dance; measure the unlock→re-SHARED gap under adversarial load —
  the number that says how wide the catastrophic window really is.

## 4. Total verdict

**One file is achievable — as a strictly less capable, strictly more
fragile sibling of the two-file design.** Two of the six approaches survive
review, and they converge to the same shape: sqlite-legal ownership exists
only as blob payload (F1), pinned only by permanently-held locks and a
vacuum ban (F2), addressed only through a translation layer because the
format guarantees no contiguity (G1–G5). Everything mpedb needs — robust
mutex, reader table, meta flip, intent ring, its own WAL — works inside
that region with today's protocols; that part of the idea is sound. What
cannot be recovered is the *forgiveness* of the two-file design: one-file
has no OPTIMISTIC mode, no offline window, and a foreign writer's worst
case is corruption of live processes and loss of committed transactions
instead of a reconcile. The exploration's yield is F1/F2, the §3 kill
tests, and a drawer-ready deployment profile of DESIGN-SQLITE-BACKED — not
a successor to it. A thorough "no, and here is why" is the deliverable,
and this is one.
