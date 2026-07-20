# mpedb — Design Document (v1.1, post-review)

**mpedb** is an embedded, multi-process, shared-memory database engine written in Rust.
It aims for sqlite's operational model (no server; processes attach, work, and may die at
any moment) combined with PostgreSQL-grade concurrency (MVCC snapshots, readers never
block, batch-scheduled writes in Phase 2) and rigid schema validation/integrity that
sqlite lacks.

v1.1 incorporates the findings of a 4-lens adversarial design review (crash-safety,
memory model, scalability, protocol soundness): 37 findings raised, the confirmed ones
folded in below. Sections marked ⚠ describe hazards the design explicitly accepts and
documents rather than solves in Phase 1.

---

## 1. Goals

- **No server.** Processes open the database via a shared TOML config file and attach to a
  memory-mapped region. Any process may crash (SIGKILL) at any instant without corrupting
  committed data; no crash may block other processes for more than a bounded, short time.
- **Massive parallelism.** Target: 1000+ concurrently attached processes. Readers never
  block writers or each other. Reads are lock-free after slot registration.
- **Schema & integrity.** Rigid column types (unlike sqlite), NOT NULL, PRIMARY KEY,
  UNIQUE, DEFAULT, CHECK constraints validated on every write. FOREIGN KEY in Phase 2.
- **Config-selected persistence.** The engine mmaps a file: on `/dev/shm` → pure
  in-memory; on disk → survives reboot subject to the durability mode (§5.4).
- **SQL once, hash forever.** SQL is parsed and planned once (`prepare`), producing a
  content-addressed plan; subsequent calls use `execute(hash, params)` — no parsing on
  the hot path. Plans carry **precomputed read/write footprints** (§7.3).
- **Python later.** PyO3 bindings, CPython 3.12–3.14+, free-threading friendly.

## 2. Non-goals and platform assumptions (Phase 1)

- No network protocol, replication, or multi-node.
- SQL subset only: **single-table** SELECT/INSERT/UPDATE/DELETE (+ ON CONFLICT,
  RETURNING), BEGIN/COMMIT/ROLLBACK, EXPLAIN. Scalar expressions include IN,
  BETWEEN, CASE and scalar functions (lower/upper/length/trim/abs/round/substr,
  coalesce/ifnull/nullif) — the "no functions" this line used to say stopped
  being true in 2026-07.
- **No joins, subqueries or EXISTS *yet*; no aggregates yet.** A build-order
  limit, not an architectural one — and the distinction matters, because the
  first draft of this line asserted the opposite. `Footprint` ALREADY carries a
  per-table access set (`tables_read` / `tables_written` — sparse `TableSet`s
  since design/DESIGN-TABLE-CAP.md, per-table bitmaps before that — plus the
  `indexes_used` bitmap over the per-table index numbering) and
  `KeyAccess::Point|Range|Full`, so it can already describe a multi-table,
  key-scoped access set; `conflicts_with` is a set intersection with key-level
  refinement for `Point`. What is single-table is the BINDER
  (`Binder { table: &TableDef }`) and `ExprProgram` (single-row scalar).
  Cross-**member** JOIN — across separate workspace files — is out of scope
  (DESIGN-MULTIDB §89, "more files, not a wider footprint word"): separate files
  mean separate catalogs and separate commit protocols. **Within one file the
  design says the opposite** (§21: shared data / joins across the wall = yes,
  cross-table atomic commit = yes). A multi-table binder must claim every table it
  touches in the footprint — which is what the table set is for.
- **Platform: Linux (x86-64, 32/64-bit ARM) and macOS/Apple Silicon.** The macOS
  port is the FLD-2 flock writer lock + `F_FULLFSYNC` durability barrier
  (`crate::os`); it is crash-safe and benchmarked, not a stub. Windows later.
  Single PID namespace; robust mutexes / flock locks do not survive reboot
  (boot-id recovery in `post_attach`).
- **Single PID namespace, single machine, local filesystem.** The lock area records the
  creator's PID-namespace inode (`/proc/self/ns/pid`) and boot id; attach from a
  different namespace or after reboot-with-live-file inconsistencies is refused with a
  clear error. NFS and other network filesystems are unsupported (mmap coherence and OFD
  lock semantics do not hold).
- Online schema migration: attach fails on schema mismatch with a diff (the full canonical
  schema is stored **in the file**, §6); `mpedb-cli dump`/`migrate` provide the offline
  escape hatch.

## 3. Operational model & initialization protocol

```
mpedb.toml  (shared config: path, size, durability, max_readers, schema)
     │
     ├── process A ──┐
     ├── process B ──┼──▶ mmap(/dev/shm/app.mpedb, MAP_SHARED)  ← same physical pages
     └── process N ──┘
```

Initialization is the classic crash trap (a creator SIGKILLed mid-format must not wedge
the database), so the **kernel-cleaned file lock is the sole init mutual exclusion** —
never a bare futex handshake, which has no owner-death semantics:

1. `open(path, O_CREAT | O_RDWR)` (never `O_EXCL` — a dead creator's leftover file must
   be adoptable).
2. Fast path: `fstat` shows the full configured size **and** `mmap` + `init_state`
   (an `AtomicU32` in the lock area) loads `READY` with Acquire → validate geometry +
   schema (§4.1) and attach lock-free. This is the common case and takes microseconds.
3. Slow path: take an **exclusive `flock`** (auto-released by the kernel on any death).
   Re-check under the lock: if the file is short, unformatted, or `init_state != READY`
   (a previous initializer died mid-format), (re)format from scratch:
   - `fallocate(fd, 0, 0, size)` — **real preallocation, not `ftruncate`**: an
     `ftruncate`d file is one big hole and the first store into a hole on a full
     filesystem/tmpfs kills the writer with SIGBUS mid-commit. ENOSPC surfaces here,
     at open time, as a clean error.
   - Write lock area, reader table, meta pages; the **last** store, with Release
     ordering, is `init_state = READY`. Any death before that store leaves state
     `!= READY` and the next `flock` holder re-formats.
4. Release the lock. Processes never mmap beyond `fstat`-verified bounds.

The file is created at full configured size and never grows in Phase 1 (growth via
`fallocate` + remap in Phase 2).

## 4. On-file layout

Page size: **4096 bytes**.

```
page 0        Meta A  ┐  double-buffered commit records
page 1        Meta B  ┘
page 2        LockArea: init state, writer mutex, watermarks, identity
page 3..R     ReaderTable: max_readers × 64 B cacheline slots
page R+1..    Data pages: catalog tree, table/index trees, overflow chains,
              freelist tree
```

### 4.1 Meta pages (commit records)

Two classes of fields, with different concurrency rules:

**Init-frozen fields** (written once under the init file lock, published by the
`init_state` Release/Acquire handshake, never changed): magic `"MPEDB1\0\0"`,
`format_version`, `page_size`, `page_count`, `max_readers`, `durability`, `schema_hash`.
Attach validates **all** of them against the local config and hard-errors on any
mismatch — the file, not the config, is authoritative for layout geometry (a
`max_readers` config drift would otherwise silently relocate the data region and corrupt
committed pages).

**Per-commit fields** (rewritten on every commit, concurrently read by every process —
all `AtomicU64`, since concurrent plain access would be a data race and UB):
`txn_id`, `catalog_root`, `freelist_root`, `high_water`, `checksum`
(xxhash64 over the per-commit fields + init-frozen fields).

**Commit publication order** (the load-bearing part; "checksum written last" in source
order guarantees nothing):

```
plain stores into COW data pages
fence(Release)                        // orders data pages before meta fields
meta body field stores (Relaxed)
checksum store (Release)
```

**Reader order:** load `checksum` with Acquire first, then body fields (Relaxed),
validate the checksum, and only then dereference any root. The Acquire on the checksum
pairs with its Release store, making every data page written before the fence visible.
Readers pick the meta slot with the highest `txn_id` that validates; a torn/partial meta
fails its checksum and the other slot is used — the shadow-paging guarantee: **committed
state is never modified in place, so a writer dying at any instruction leaves the
previous commit fully intact.**

⚠ **The durable-gate retry loop is load-bearing** (`shm.rs::newest_meta`). In
`durability = commit`/`wal` a reader accepts only metas with `txn_id ≤ durable_txn`
(§4.2, §5.4), and the commit path publishes the meta slot *before* it advances that gate:
`write_meta_slot` → `msync_page` (milliseconds on real disk) → `durable_txn.fetch_max`.
A reader that loads the gate, then lands in that window for two consecutive commits, can
find **both** slots newer than the gate it holds and conclude
`Corrupt("no valid meta page")` — a spurious hard error on a perfectly healthy file.
So `newest_meta` re-loads the gate and retries, and gives up only when a *reloaded* gate
yields the same failure (a gate that did not move admits nothing new, so the failure is
genuine corruption). The loop is cheap to prove: replace it with a single
`newest_meta_gated(durable_txn.load(Acquire))` and `cargo run -p mpedb-bench -- --only
mpedb` reports spurious retries within seconds — measured 2026-07-15, 3 with it disabled
against 0 as shipped on the same flags. `mpedb-bench`'s `SPURIOUS_CORRUPT_RETRIES`
counter is the tripwire for that regression.

### 4.2 Lock area

- `init_state: AtomicU32` — `0` empty, `1` formatting, `2` READY (§3).
- `writer_lock: pthread_mutex_t` with `PTHREAD_PROCESS_SHARED` + `PTHREAD_MUTEX_ROBUST`
  + **`PTHREAD_MUTEX_ERRORCHECK`** (relock by the owner returns EDEADLK instead of
  deadlocking — this is what turns "prepare() inside an open write txn" from a hang into
  an error, §7.2). A writer dying while holding it → next locker gets `EOWNERDEAD` →
  `pthread_mutex_consistent` + the recovery steps in §5.2.
- `durable_txn: AtomicU64` — durability watermark (§5.4). In `durability=commit` mode
  readers accept only metas with `txn_id ≤ durable_txn`, so a commit can never be
  *observed* before it is durable.
- `oldest_pinned_cache: AtomicU64` — a conservative lower bound on pinned reader txns
  (§4.3, §5.2). Monotone, so a stale value is always safe (it only delays reclaim).
- Identity fields (init-frozen): `pid_ns_ino` (inode of `/proc/self/ns/pid`), `boot_id`.
  Attach compares and refuses on mismatch (a foreign PID namespace would make every
  `kill(pid, 0)` liveness probe meaningless — ESRCH for live remote readers would free
  live slots and corrupt their snapshots).
- All cross-process shared structs are `#[repr(C)]`, fixed layout; access via raw
  pointers and atomics only — **no Rust `&`/`&mut` references into shared mutable
  memory** (aliasing UB), and no `volatile` (it neither removes data races nor orders).

### 4.3 Reader table

`max_readers` slots (geometry frozen in the file at init), each one cacheline:

```rust
#[repr(C, align(64))]
struct ReaderSlot {
    word:      AtomicU64,  // packed {pid: u32, seq: u32}; pid 0 = free
    txn_id:    AtomicU64,  // pinned snapshot; u64::MAX = claimed, not yet pinned
    pid_start: AtomicU64,  // claimer's process start time (/proc/<pid>/stat fld 22)
}
```

- **Every slot transition is a generation-CAS on `word`, and every store into a slot's
  side fields is owner-only** (code-review hardened — the v1.1 "CAS then store pid_start"
  order let a sweeper free a freshly claimed live slot, and a pre-CAS marker store could
  clobber a concurrent claimant's published pin):
  - claim: CAS `{0, s}` → `{pid | CLAIMING, s+1}` (bit 31 of the pid half; real pids
    never use it) — reservation and identity publication are one atomic step. Then, as
    owner, store `txn = u64::MAX` and `pid_start`; finally CAS `{pid|CLAIMING, s+1}` →
    `{pid, s+1}` to go live. If the final CAS fails the slot was reclaimed: walk away.
  - release (owner): CAS exact `{pid, s}` → `{0, s+1}`.
  - sweep-free: CAS the exact word observed dead → `{0, s+1}`. CLAIMING slots are freed
    **only on ESRCH** (their `pid_start` is not yet trustworthy); a claimer dying in the
    µs claim window whose pid is instantly recycled leaks one slot until that pid exits —
    accepted residual, it pins nothing.
  - a stale `txn` value visible during the claim window is ≤ the newest committed txn,
    so the oldest-pinned scan only becomes more conservative.
  - a benign claim-CAS failure retries the same slot (it may still be free) before
    scanning on, so racing claimers cannot manufacture spurious `ReadersFull`.
- **Pin protocol** (reader):
  1. read newest valid meta → `t`
  2. store `txn_id = t` (Release)
  3. **`fence(SeqCst)`**
  4. re-read newest meta; if `txn_id` changed, goto 1.
  The writer, after acquiring the writer lock, issues **`fence(SeqCst)`** before scanning
  the reader table. The paired SC fences forbid the store-buffering outcome (reader's pin
  store and writer's scan both delayed past each other — a real race on x86 and ARM that
  Release/Acquire alone permits); this was a confirmed critical review finding against
  the v1.0 "Release store then re-check" protocol.
- **Liveness = (pid, start_time), not pid.** A sweep declares a slot dead only if
  `kill(pid, 0) == ESRCH` **or** the process's current start time ≠ `pid_start`
  (detects PID reuse exactly — a recycled pid pinning a snapshot forever would otherwise
  block all page reclaim in a file that cannot grow, until writers wedge). `EPERM` means
  **alive**.
- **Sweeps run**: on attach; from writers *outside* the critical path (amortized); and by
  any reader whose claim scan finds no free slot (inline sweep, then rescan, then
  `ReadersFull`) — a read-only deployment must be able to recover its own slots.
- **`oldest_pinned`**: any pin published after a writer's scan is ≥ the newest committed
  txn at publish time, and pins only leave — so a computed minimum is a *permanently
  valid conservative bound*. Writers use `oldest_pinned_cache` and rescan only when they
  actually need pages freed at ≥ the cached bound; the O(max_readers) scan leaves the
  per-commit hot path.
- ⚠ **Live-but-stalled readers** (SIGSTOP, debugger, hours-long scan) legitimately pin
  their snapshot and cannot be swept; in a fixed-size file this eventually stalls
  writers with `DbFull` naming the culprit slot/pid. A configurable max-pin-age eviction
  (writer bumps the slot `seq`; the reader detects the theft via its generation on
  release/next cursor validation and gets `SnapshotEvicted`) is the intended safety valve
  — **half built.** The *detection* half is shipped and load-bearing: `still_pinned()`,
  revalidation every 256 cursor steps and again at finish, and the `SnapshotEvicted`
  error, so a stolen slot can never yield silently-wrong rows. **Nothing evicts.** There
  is no age policy and no writer that bumps a live slot's `seq`; the only thing that
  frees an occupied slot is the liveness sweep, which requires the process to be *dead*.
  So a live-but-stalled reader still ends in `DbFull` today (`engine/read.rs` marks the
  eviction "(future)"). Everything downstream that reaches for the valve —
  design/DESIGN-BLOBEXTENT.md §5's copy-out argument among them — must read it as
  designed, not available. (That argument stands on its own: it refuses to hand
  out raw borrows *so that* eviction can be added later without becoming UB.)

### 4.4 Pages, B+tree, rows

- **Catalog tree:** table_id/index_no → root page id + row count; also stores the full
  canonical schema bytes under a reserved key (recoverable without any config file).
- **One COW B+tree per table** (key = encoded PK, value = row payload) and **per
  secondary index**. A UNIQUE index is keyed by the encoded column value alone
  (value = encoded PK); a non-unique (`indexed = true`) index is keyed by
  `encoded column value ‖ encoded PK` (value = encoded PK) — the PK suffix makes the
  composite key unique by construction, and equality lookup is a prefix scan,
  O(matches + 1). NULL values are **not indexed**: UNIQUE permits multiple NULLs
  (SQL standard), and `col = NULL` is UNKNOWN.
- Slotted 4 KiB nodes; values > 1 KiB spill to overflow page chains; keys ≤ 976 B.
- **Key encoding** memcmp-ordered (`mpedb-types::keycode`): NULL tag < any value,
  big-endian sign-flipped ints, IEEE total-order floats (-0.0 = 0.0, NaNs equal, > +inf),
  0x00-escaped text/blob. **Text collation is binary** (documented; no locale collation).
- **Index numbering convention** (planner and engine derive identically from the
  schema): index 0 = PK tree; secondary indexes numbered 1, 2, … in
  column-declaration order over columns with `unique = true` OR `indexed = true`,
  skipping a column that is by itself the entire PK.
- **Row encoding:** null bitmap → fixed-width section → varlen section; single-column
  decode without materializing the row.

### 4.5 Freelist & page reclamation

- Freelist B+tree keyed by an 11-byte `(txn_id that freed the pages: u64 BE ‖ kind: u8 ‖
  chunk#: u16 BE)`. **`kind` 0 = a page-id list** (u64 LE ids, written strictly ascending
  — `refill_reusable` validates the ordering rather than assuming it, and the commit
  fixpoint binary-searches it); **`kind` 1 = extent runs** for the out-of-tree blob
  allocator (`(start, npages)` runs, design/DESIGN-BLOBEXTENT.md §3.3/§6). Sorting the
  kind *inside* the txn prefix keeps both classes of an epoch adjacent, so the single
  refill cursor walks them in one ascending pass. A page freed at `t` is reusable when
  `t <= oldest_pinned` bound (§4.3). **Not `t <`**: pages freed BY commit `t` are
  referenced only by snapshots
  *older* than `t` — commit `t` is what replaced them — so a pin exactly at `t` cannot
  see them. This document said `<` until 2026-07-15; the code has always said `<=`, and
  the off-by-one leaks the high-water mark without bound. There is a regression test.
- **Refill is READ-ONLY.** A writer draws an entry's pages into its private `reusable`
  pool and **leaves the entry in the tree**, remembering the provenance (`taken`). It
  never deletes on the way in. Drawing is therefore free: an entry the writer allocates
  nothing out of is left untouched at commit.
  Consequences, all load-bearing:
  - **Drawing is decoupled from writing.** When refill removed the entry, every page
    drawn became a page the commit fixpoint had to write back (it records what is free,
    and a drawn page was then listed nowhere else). That coupled the fixpoint's own page
    appetite to the pool it held — feeding it made it hungrier. Two independent attempts
    to fix the resulting leak by handing the writer more pages BOTH made it strictly
    worse; see `mpedb-core/tests/high_water_leak.rs`.
  - **A writer may hold a deep pool** (`FREELIST_POOL_TARGET`) precisely so the fixpoint,
    which cannot refill, does not fall through to `high_water`.
  - An uncommitted writer's draw is invisible: the entry still lists the pages, so a
    SIGKILL mid-txn loses nothing and needs no undo.
  - Writers are serialized by the writer lock, so no second writer can draw the same
    entry concurrently. **This is why refill must stay under the lock.**
- **Commit-time fixpoint** (the freelist update itself frees and allocates pages —
  LMDB's classic circularity). Each pass computes the desired state and applies only the
  diff against the previous pass:
  - each drawn entry, minus the pages consumed out of it (deleted if drawn dry, omitted
    entirely — not rewritten — if nothing was consumed);
  - this commit's own free set under its txn id: pages COWed away, plus any page left in
    `reusable` that no drawn entry lists (allocated from `high_water` or out of an entry
    since drawn dry, then freed again — nothing else records it).
  ⚠ Once a pass has written a drawn entry it must stay in the plan even if a later pass
  frees every page back and it looks untouched again — otherwise the reconcile pass sees
  a key it wrote that the plan no longer claims and deletes it, with its pages listed
  nowhere. The page-accounting verifier catches this as `page N leaked`.
- **Termination**: each pass can only add pages freed by COWing the ≤ height-bounded
  freelist path itself, the sets grow monotonically, and allocation switches to
  `high_water` (which frees nothing) once the reclaimable pool is consumed — so the loop
  is bounded by O(tree height) passes. **The `high_water` fallback IS the termination
  argument**, which is why refill must stay blocked during the fixpoint (`in_freelist_op`)
  even though it is now read-only: a refill inside the loop would let the pool grow on
  the fly, and the "monotone, bounded" argument no longer closes.
- **Page accounting invariant** (tested by the crash suite): pages reachable from the
  committed meta ⊎ pages listed in the freelist ⊎ [high_water, page_count) partition the
  data region after every commit.

## 5. Transactions

### 5.1 Read transaction

Claim slot + pin (§4.3) → read roots from the pinned meta → traverse committed,
immutable pages. Release = generation-CAS of the slot. Never blocks, no writer-lock
contact, unlimited concurrency. On release (and periodically during long scans) the
reader validates its slot generation; a mismatch means eviction → `SnapshotEvicted`.

### 5.2 Write transaction (Phase 1: serialized commit)

1. `pthread_mutex_lock` → on `EOWNERDEAD`: `pthread_mutex_consistent`, then **recovery**:
   msync both meta pages (re-establishes the double-buffer durability invariant that the
   dead writer may have broken — without this, a durably-acknowledged commit can be lost
   when the *next* commit overwrites its slot), recompute and Release-store
   `durable_txn` from the newest valid meta, then proceed. Nothing else to roll back, by
   COW construction: an uncommitted writer's allocated pages came from a freelist
   snapshot that was never committed, so they are not leaked and not reachable.
2. `fence(SeqCst)` (§4.3 pairing); load latest meta.
3. Mutations: COW pages — private until the meta flip; allocation from freelist entries
   older than the `oldest_pinned` bound, else `high_water`. Allocation failure →
   clean abort with `DbFull` diagnostics (§4.3 ⚠).
4. Constraint validation (types, NOT NULL, CHECK, UNIQUE via index probe).
5. Freelist fixpoint (§4.5), then commit publication (§4.1 ordering), then §5.4
   durability steps, then unlock.

⚠ **Throughput expectations by mode** (2-core host, point writes):
`none`/`async` → µs-scale commits, lock-bound (LMDB-like, 10⁵+/s with batching);
`commit` → sync-bound, ≈ 1/msync-latency ≈ hundreds/s, fully serialized. The Phase 2
group commit exists precisely to amortize the msync across a batch.

⚠ **Footprint prefaulting is staged, not built.** The idea — plans make the footprint
known (§7.3), so a writer could touch those pages *before* taking the lock and never
fault inside it — is sound and unimplemented; the only page-residency call in the engine
is the opportunistic `madvise(MADV_HUGEPAGE)` of §9. Staged in
design/DESIGN-MPEE-OPT.md §1.2a (and its §5 verdict), recorded as partly-built in
INNOVATIONS.md §9.6.

### 5.3 Phase 2 (BUILT): the intent ring — deterministic batch scheduling
     ("the request queue is an index")

Because every write arrives as `(plan_hash, params)` with a precomputed footprint (§7.3),
the set of in-flight requests is readable as an index over imminent access. As built
(`mpedb-core::ring` + the facade's `ring_exec`), with two deviations from the original
sketch, both review/stress-hardened:

- **Slot table, not a FIFO:** 256 × 1 KiB slots after the reader table. Slot header =
  one atomic word `{pid ‖ gen ‖ state}`; every transition is a CAS, reservation and
  identity publication are the same CAS (an enqueuer dying at any instruction leaves an
  identifiable, reclaimable slot).
- **Flip-atomic consumption via per-slot txn stamps** (replaces the sketched
  `committed_batch_seq` counter): before the meta flip the leader stamps each drained
  slot `committed_in_txn = N+1` together with its result fields. A successor compares
  stamps against the committed `meta.txn_id`: `≤` → batch landed, post the staged result;
  `>` → flip never happened, clear the stamp and re-execute (nothing was visible). No
  contiguity requirement, so a slow enqueuer mid-publish never stalls intents behind it.
- **Leader = writer-lock holder.** It drains READY intents in **key-locality order** —
  sorted by (written table id, materialized key bytes, slot idx): `Point` footprints
  resolve their PK parts to keycode bytes, `Range` uses its lo bound, `Full`/unresolvable
  keys sort last within their table, and slot idx is the final tiebreak (same-key intents
  keep their relative slot order, so duplicate-PK races resolve as before). This is a
  free choice of linearization within one meta flip — slot order was already arbitrary
  w.r.t. arrival (enqueue scans from a pid-randomized offset) — chosen so adjacent-key
  mutations share COW root-to-leaf paths: fewer pages copied per batch, fewer/shorter
  msync runs (a rationale #111 made moot for `commit` on Linux, where the data
  barrier is now ONE span regardless of run count — the sort is retained for
  deterministic batch linearization, per DESIGN-MPEE-OPT §5). `MPEDB_NO_BATCH_ROUTING=1` (alias `MPEDB_RING_NO_SORT=1`) restores slot
  order for A/B; `MPEDB_RING_STATS=1` prints per-batch page/run/timing lines. Each
  intent executes under a **statement savepoint** (`WriteTxn::savepoint`/`rollback_to` —
  COW makes these nearly free), so one failing intent errors alone while the batch
  commits around it (⚠ **the savepoint alone does not buy that** — see the
  per-intent atomicity argument below); then ONE meta flip and (durable mode)
  ONE msync for the whole batch.

- **Per-intent atomicity: why the cheap savepoint is not sufficient, and what is.**
  `rollback_to` restores root pointers plus page accounting. It undoes exactly the
  pages the statement COW-*allocated*: the root restore drops them and they return
  to `reusable`. It does **not** undo an in-place mutation of a page that was
  already dirty when the savepoint was taken — the page id never changes, so the
  restored root points at the same, already-mutated page. Inside a batch that case
  is ordinary, not exotic: the key-locality sort deliberately makes adjacent-key
  intents share root-to-leaf paths, so intent k+1 routinely writes in place into a
  leaf intent k COWed. A statement failing part-way through (multi-row INSERT whose
  last row violates a constraint; UPDATE/DELETE tripping one mid-loop; a failing
  trigger body) then left its earlier rows in that leaf and **the batch committed
  them while its caller got the error** — statement atomicity silently lost, but
  only under the ring, only when contended, and only for a member that was not the
  batch's first mutator. (Reproduced deterministically in
  `crates/mpedb/tests/ring_stmt_atomicity.rs`; both arms of the leader round — the
  drained foreign intents and the leader's own statement — had it.)
  The rule that restores the promise:

  1. The executor already computes a precise `partial` out-flag per statement
     (`exec/mod.rs`; contract: *may* have applied part of its effects, never
     under-reports). The leader now reads it.
  2. The cheap undo is **exact** iff `!partial`, or the transaction was
     *pristine* when the statement began (`WriteTxn::is_pristine` — nothing dirty
     and the extent allocator untouched) and the statement touched no extent. From
     a pristine transaction every page a statement writes it also allocated, so the
     root restore is a complete undo even for a torn statement; extent state is
     excluded because it lives outside every savepoint (`savepoint_full` refuses
     across it for the same reason).
  3. Otherwise the round is **torn**. The leader calls `WriteTxn::restart()` —
     discard everything this transaction did and return it to its just-begun state
     **while keeping the writer lock** — and replays the round with that member's
     error *pre-decided*: on the replay it is not executed, its error is staged
     verbatim, and the batch commits around it. Releasing the lock instead is not
     an option: another process would lead and execute the very intents about to be
     replayed, double-applying them.

  Why the replayed error is still the right answer: the replay runs the member's
  predecessors against the same committed snapshot in the same drain order, so it
  reproduces exactly the state in which the error was raised. Why it terminates:
  every restart pre-decides one member that was not pre-decided before, and a
  pre-decided member never executes again, so a round runs at most `len(batch)+1`
  times. Why it is free: on the happy path the whole rule is one bool read per
  member — no page-image capture, so the ring keeps the flush amortization that is
  its entire point. (`savepoint_full` per intent was the obvious alternative and was
  rejected on cost: it copies the *whole* dirty set, which grows through the batch,
  so a batch of N intents pays ~N²/2 page images — megabytes of memcpy per batch,
  spent on every intent to insure against a rare failure. Restricting what may ride
  the ring was rejected as unsound: `partial` can be set for a *single-row* write
  too, e.g. `DbFull` mid-split, so no statement class is safely exempt and the
  restriction could not be derived from the property.)

  ⚠ The exclusion list in `run_write_plan` (busy deadline, `RETURNING`, host UDFs)
  is **not** a safety net for this: none of its three reasons is about partial
  effects, and multi-row DML rides the ring today. Do not weaken the rule above on
  the assumption that only single-row-effect plans reach the leader.
- **Incarnation-safe posting** (a stress-caught TOCTOU class): results are posted
  *under the writer lock*, result-store happens BEFORE the READY→DONE transition, owners
  release from READY or DONE, and recovery never touches DONE slots. Invariant: a READY
  slot with a nonzero stamp is pinned to its incarnation — its owner cannot release
  before the result post, and non-EMPTY slots cannot be re-reserved.
- **Wait-or-lead:** enqueuers futex-wait (shared futex) with a 2 ms bound, then
  `pthread_mutex_trylock` — acquiring it (possibly via EOWNERDEAD) promotes them to
  leader, so a SIGKILLed leader can never strand its waiters.
- **Media-adaptive (⚠ measured):** the ring engages only when commits are expensive —
  `durability = commit`, where each commit costs an msync bounded by the storage medium.
  There, group commit measured **2.9× durable write throughput** at 10 contended writers
  on this host's disk (5.4k vs 1.9k committed ops/s), and the batch size self-clocks:
  slower media → longer msync → more intents queue → bigger batches. On `none`/`async`
  (µs-cheap commits) the direct lock path wins and the ring is bypassed entirely
  (`MPEDB_NO_RING=1` forces the direct path for A/B measurement in any mode).
- Params ≤ 824 B ride the ring; larger fall back to the direct path. Interactive
  multi-statement sessions take the writer lock directly (⚠ they stall the ring for
  their duration). The Phase-3 optimistic per-writer path was prototyped and
  **measured to lose** on this COW engine — the expensive COW B+tree mutation cannot
  leave the commit critical section, and per-writer commits forfeit the ring's
  group-commit flush amortization. Kept behind `concurrency = "optimistic"`
  (experimental, default off); full analysis, ceiling measurement, and numbers in
  **DESIGN-PHASE3.md**.

### 5.4 Durability modes

- **`none`** (default; the right choice for `/dev/shm`): no msync ever. Crash-safe
  against process death; nothing survives reboot. ⚠ On a disk file, an unclean *system*
  shutdown can leave arbitrarily stale-but-valid state; treat disk+none as dev-only.
- **`commit`**: msync(data) → publish meta → msync(meta) → **advance `durable_txn`** →
  unlock. Readers gate on `durable_txn` (§4.2), so no process can observe — and act on —
  a commit that a power failure could still erase (confirmed finding: visibility before
  durability lets an external side effect reference a transaction that never happened).
- **`async`** (BUILT — redefined): **WAL with deferred, coalesced `fdatasync`** — the
  honest "sqlite `synchronous=NORMAL` / PostgreSQL `synchronous_commit=off`" class. Every
  commit still appends its record to `<path>-wal` and flips the meta (so the on-disk log
  is *always* a crash-consistent prefix), but the `fdatasync` is issued by a background
  flusher on a bounded interval, NOT per commit. **Crash-consistent always; power loss
  may lose a bounded recent window of acknowledged commits — never a torn/partial
  database.** This is *weaker than `commit`/`wal`* (which are durable-on-ack) and must
  never be described as durable-on-ack. Full protocol and exact contract in §5.4.2. (The
  old Phase-1 `async` — opportunistic msync with "no reboot integrity" — is gone; this
  replaces it with a real, crash-safe deferred-durability mode.)
- **`wal`** (BUILT): same durability guarantee as `commit` — a commit is acknowledged
  only after it is power-loss-durable, and readers gate on `durable_txn` identically —
  at a fraction of the cost: one sequential append + ONE `fdatasync` per commit (per
  BATCH under the intent ring) instead of `commit`'s data-span msync plus a
  meta-page msync. Full protocol below.

#### 5.4.1 WAL mode (`durability = wal`)

**Motivation (measured).** `commit` msyncs the dirty data plus the meta page
per commit: 485–1,122 durable ops/s on this host's disk vs SQLite-WAL 1,492–1,647 and
PostgreSQL 2,182–4,588 (`crates/mpedb-bench/RESULTS.md`). A sequential log turns the
per-commit disk work into one contiguous write + one flush, and the intent ring
amortizes that flush across a whole batch.

**Files and geometry.** The log is a separate append-only file at `<db-path>-wal`,
created at init/first attach of a wal-mode database. It is preallocated (`fallocate`)
in 4 MiB chunks and grown by `fallocate` before every append that would pass the
allocated size — never sparse-appended, so ENOSPC surfaces at allocation time. The
mode is frozen in the meta pages as durability tag `3`; `FORMAT_VERSION` is unchanged,
so an older engine sees an unknown tag and refuses the attach — the correct failure
for a file whose durability protocol it cannot honor. Format truncates any leftover
`-wal` from a previous incarnation of the file (stale checksum-valid records would
otherwise be replayed into the fresh database by the first post-reboot recovery).

**Record format** (all little-endian; one record per commit):

```
magic u32 ("WAL2") ‖ txn_id u64 ‖ n_pages u32 ‖ rec_len u32
‖ n_pages × page_entry
‖ catalog_root u64 ‖ freelist_root u64 ‖ high_water u64      ← MetaSnapshot body
‖ checksum u64 = xxh3(record file offset LE ‖ all preceding record bytes)

page_entry = page_id u64 ‖ enc u8 ‖ payload
  enc = 0 (FULL):  4096-byte page image
  enc = 1 (SPLIT): prefix_len u16 ‖ suffix_start u16
                   ‖ prefix[prefix_len] ‖ suffix[4096 − suffix_start]
```

A record is valid iff its checksum verifies **at the offset it sits at** — the offset
is part of the checksum preimage, so a stale copy of a record embedded in page-image
bytes (or any bytes not appended at exactly this position) can never validate. Recovery
additionally requires consecutive records to carry consecutive `txn_id`s (writers are
serialized, so the log's txn ids increment by exactly 1). Recovery stops at the first
invalid/partial record: the torn tail. `rec_len` (bounded on decode to
`WAL_RECORD_FIXED .. + n_pages·FULL_ENTRY`) lets the recovery scan skip a
variable-length record without decoding it.

**Lean records (SPLIT encoding).** Only the pages a commit actually touched (its COW
dirty set) are ever logged — confirmed against `commit_with`; a single-row insert dirties
~4–8 pages. Beyond that, a page is logged FULL only when SPLIT would not be smaller;
otherwise the record stores a B+tree node's two *used* regions and omits the unread
middle. For a leaf/branch node the used regions are the header+slot-array prefix
`[0, HDR+nkeys·2)` and the packed-cell suffix `[cell_start, 4096)`; the free middle
between them is elided and **zero-filled on replay**. For an overflow page the used
region is `[0, HDR+payload_len)` and the unused tail is elided. `btree::used_span` is the
single source of truth for the boundaries. This is byte-safe by an audited invariant: no
reader in `btree.rs` ever touches those bytes — `cell_bytes` slices only from offsets
`≥ cell_start`, `read_overflow` reads only `[HDR, HDR+len)`, and there is no per-data-page
checksum or whole-page comparison anywhere in the engine — so a replayed page is
*observationally identical* to the live page even though the elided span is zeroed rather
than restored to its (arbitrary, never-read) in-memory contents. Meta pages are never
logged as images (they are rebuilt from the record trailer), so meta checksums are
unaffected. `MPEDB_WAL_FULL_PAGES=1` forces FULL encoding for A/B measurement. Measured
on this host's ext4: lean cut single-client durable-insert latency enough to raise
throughput ≈ 1.15–1.2× vs full-page records — modest, exactly because one `fdatasync`'s
fixed cost (device cache flush) dominates the few-KiB payload difference; the elision
never *enlarges* a record and is on by default in both wal-class modes.

**Lock-area fields** (page 2; after the existing fields — see the offset table in
`shm.rs`): `wal_len: AtomicU64` at byte 232 — bytes of *durable* log, advanced only
AFTER `fdatasync` returns; `wal_ckpt: AtomicU64` at byte 240 — log offset below which
records are already checkpointed into the main file, advanced only AFTER a full-mapping
`MS_SYNC`. Both are written only under the writer lock (plus init/recovery under the
init flock).

**Commit path** (replaces the `commit`-mode msync steps; everything under the writer
lock): write COW pages into the mapping as always → build the record from the sorted
dirty set + the new meta snapshot → `pwrite` at `wal_len` → `fdatasync(wal)` → advance
`wal_len` → flip the meta in the mapping (§4.1 publication order) → advance
`durable_txn` (readers gate on it exactly as in `commit` mode — no process can observe
a commit that power loss could erase). Appends are serialized by the writer lock. A
successor after EOWNERDEAD trusts the in-memory `wal_len` (it only ever moves
post-fsync) and simply appends over any torn/orphan bytes beyond it — such bytes belong
to a commit that was never acknowledged.

**Group commit.** The intent ring engages for `wal` exactly as for `commit`
(`ring_enabled`): a contended batch costs one record and one `fdatasync` total, and the
batch size self-clocks with media latency (§5.3). This is where the log shines — the
fdatasync is both cheaper than `commit`'s two-flush floor and amortized N ways.

**EOWNERDEAD recovery** (§5.2 step 1, wal variant): make the mutex consistent, refresh
`durable_txn` from the newest valid mapping meta, proceed. The `commit`-mode meta-page
msync is **not** needed, argued against the same §5.2 invariant it protects: that msync
exists because in `commit` mode power-loss recovery *reads the mapping metas*, so an
acknowledged commit must keep a durable meta slot at all times — a dead writer's
never-msynced meta would otherwise be overwritten by the next commit, and a torn write
to the only remaining durable slot could regress the file below the durable watermark.
In wal mode recovery *replays the log, never trusts the mapping metas*: an acknowledged
commit's pages AND meta fields are fdatasync-durable in a record at ≥ `wal_ckpt` before
its meta slot is even written in the mapping, and `wal_ckpt` only advances after a
full-mapping msync that makes both meta slots durable. Overwriting a never-msynced meta
slot therefore cannot lose anything reachable only through it — `wal_recover()`
reconstructs it from the log. Advancing `durable_txn` to the newest mapping meta is
sound for the same reason: a meta exists in the mapping only after its record's
fdatasync returned.

**Checkpoint** (amortized; the committing writer, still under the lock, after the
flip): when `wal_len − wal_ckpt` ≥ 16 MiB (`MPEDB_WAL_CKPT_BYTES` overrides for
tests), (1) `msync` the WHOLE mapping with `MS_SYNC` — every commit up to the current
meta is now durable in the main file, so no record below the current `wal_len` is
needed for recovery; (2) set `wal_ckpt = wal_len` and msync the lock page, making the
new checkpoint offset durable BEFORE any log bytes below it are reclaimed; (3) reclaim
`[0, wal_ckpt)` with `FALLOC_FL_PUNCH_HOLE | KEEP_SIZE` (best-effort). **Deliberate
deviation** from "ftruncate + reset both offsets to 0": punching keeps `wal_len`
strictly monotone, so a log offset is never reused in the file's lifetime. That closes
an entire hazard class — no mixed-epoch (`wal_ckpt`, `wal_len`) pair is ever observable
(a writer dying between the two zero-stores of a truncate-reset leaves exactly such a
pair, and a stale on-disk `wal_ckpt` pointing above a truncated-then-rewritten region
loses acknowledged commits), and no stale-but-valid record can sit at an offset a scan
will visit. Space cost is identical; the logical size grows but is sparse below
`wal_ckpt`. Checkpoint msync failure is swallowed: `wal_ckpt` simply does not advance,
recovery replays more, the next commit retries — failing a commit that is already
durable and acknowledged would be a lie.

**Attach/recovery.** On a live system (no reboot) the mapping is coherent shared
memory and always current — attach never replays. Replay runs exactly once per boot
epoch, in the §4.2 boot-id path in `post_attach`, under the init flock, BEFORE the
volatile reinit (mutex, reader table, boot id): after power loss the lock area itself
is only as durable as the mapping, so recovery trusts nothing but the on-disk
`wal_ckpt` — safe by construction, since any value the on-disk field can hold was
stored in program order after a full-mapping `MS_SYNC` completed. Recovery scans from
`wal_ckpt`, replays every checksum-valid record in order onto the mapping (page images
+ meta), stops at the torn tail, installs the newest replayed meta into BOTH slots,
msyncs the mapping, then sets `wal_ckpt = wal_len =` end-of-valid-prefix. Replay is
idempotent (page images), and dying anywhere inside recovery re-runs it on the next
attach (the boot id is updated only afterwards).

**The replay-sufficiency invariant** (why scanning from `wal_ckpt` recovers
everything): *the main file's durable state is always ≤ the log.* A meta is flipped in
the mapping only after its record's fdatasync returned, so any meta the kernel may have
written back has a durable record; and by COW, committed pages are immutable — every
page whose content changed after the checkpoint txn was freshly allocated by a
post-checkpoint commit and therefore appears with its final content in a record ≥
`wal_ckpt`. Replaying that suffix onto whatever mix of page versions the kernel left
behind therefore reconstructs a state ≥ anything the main file could hold, ending
exactly at the newest durable commit; pages older than the checkpoint are guaranteed by
the checkpoint's own full-mapping msync. If the scan ends BELOW the on-disk `wal_len`,
bytes that were once fdatasync-durable are missing — the wal file was truncated or
replaced behind the engine's back — and recovery refuses with `Corrupt` rather than
silently dropping acknowledged commits (⚠ deleting the `-wal` of a wal-mode database is
an integrity violation, exactly as for SQLite).

**Residual hazards (documented):** the directory entry of a freshly created `-wal`
file is not fsync'd (neither is the database file's — a machine crash in the very first
seconds of a database's life can lose the whole file, unchanged from all other modes);
`wal_len`/`wal_ckpt` pair coherence on disk relies on both fields sharing one sector of
the lock page (they sit at bytes 232/240) — recovery is additionally guarded by the
scan-from-`wal_ckpt` rule which never *needs* the on-disk `wal_len` (it is only an
integrity cross-check); and a `wal` database attached by a pre-wal engine fails with
"bad durability tag", which is intended.

**Measured** (this 2-core host, ext4 disk, `mpedb stress --workers 10 --secs 10`,
medians of ≥3 interleaved commit/wal trials; ⚠ the box ran a foreign CPU-pinned
process throughout, so absolute numbers are depressed and spreads are wide —
ratios were stable):

| workload | commit ops/s | wal ops/s | wal/commit |
|---|---|---|---|
| mixed (70 % DML, ring engaged) | 3,264 (3,095–6,278) | 8,657 (8,002–17,311) | **2.65×** (1.9–2.8×) |
| unique (all-conflict probes, ~0 dirty pages/batch) | 7,411 | 8,745 | 1.18× |
| bank (4 session writers, direct path, no ring) | 845 commits/s | 1,116 commits/s | 1.32× |

Per-batch instrumentation (`MPEDB_RING_STATS`, mixed): commit mode averaged 6.8
intents / 4.4 dirty pages / **3.2 msync runs + 1 meta msync** per batch at 3,487 µs
(pre-#111; the data barrier is now one span on Linux, so that row reads 1 + 1)
commit cost; wal averaged 7.1 intents / 4.6 pages / **1 fdatasync** at 1,345 µs —
batches/s rose 280 → 709. The 3× target was approached but not met on this
contended host (2.65× median, 2.8× best); in absolute terms wal-mode durable DML
(~6,000 writes/s inside the mixed cell) clears the motivating bar — SQLite
FULL+WAL measured 1,492–1,647 and PostgreSQL 2,182–4,588 durable ops/s on this
machine (`mpedb-bench/RESULTS.md`) — by ~4×. `unique` is conflict-probe-bound
(both modes commit near-empty batches: one flush each), hence ~1×, reported for
honesty. Implementation note: WAL growth pre-zeroes new chunks — appending into
`fallocate`d *unwritten* extents makes every fdatasync journal extent conversions,
measured 958 µs vs 350 µs per append+fdatasync (2.7×) on this host's ext4.
Checkpoint-threshold sensitivity: 16 MiB vs 64 MiB made no measurable difference
(9,924 vs 9,890 ops/s), so checkpoint drag is negligible at the default.

#### 5.4.2 Deferred-fsync WAL (`durability = async`) — the crash-consistent fast class

`async` reuses the entire §5.4.1 WAL machinery — same file, same record format, same
recovery and checkpoint — and changes exactly one thing: **the `fdatasync` is deferred
and coalesced instead of issued per commit.** It is the honest analog of sqlite
`synchronous=NORMAL` (WAL, fsync at checkpoint) and PostgreSQL `synchronous_commit=off`
(WAL written, not waited on).

**Two watermarks.** A new lock-area field `wal_appended: AtomicU64` (byte 248; zero in
every non-async mode, so `none`/`commit`/`wal` on-disk bytes are unchanged) is the
*append cursor* — the next append position. `wal_len` keeps its §5.4.1 meaning: the
*durable* watermark, advanced only after an `fdatasync` returns. In `wal`, every append
is immediately synced, so the two coincide; in `async`, `wal_appended` runs ahead of
`wal_len` by the un-flushed window.

**Commit path** (writer lock held): write COW pages → build the record → `pwrite` at
`wal_appended` (**no fdatasync**) → advance `wal_appended` → flip the meta →
`durable_txn` is *not* advanced. The commit returns here — acknowledged after append, not
after sync.

**The deferred flusher.** `Engine::open` for `async` spawns one background thread that,
every `MPEDB_WAL_FLUSH_MS` (default 10 ms), issues `fdatasync(wal)` off the writer lock
and `fetch_max`es `wal_len` up to the append cursor it observed *before* the sync. It runs
lock-free and concurrently with writers: it only ever claims `[0, a)` durable, and `a`
was published (Release) after its `pwrite` completed, so the sync flushes those bytes.
On `Engine::drop` (clean shutdown) a final synchronous flush runs, then the thread is
joined before the mapping is unmapped — so a clean exit loses nothing.

**Exact contract.**
- *Visibility:* a commit is visible to readers the instant its meta flips — i.e. at
  append, BEFORE its bytes are power-loss-durable. `async` reads are **ungated** by
  `durable_txn` (identical to `none`); `durable_txn` is not used as a visibility gate in
  this mode, and the "visibility before durability" hazard (an external side effect
  referencing a commit a later power loss erases) is *present and intended* — it is the
  defining property of this weaker class, documented so no caller mistakes it for
  durable-on-ack.
- *Durability:* a commit is power-loss-durable only once `wal_len` has advanced past it
  (the next flush, ≤ `MPEDB_WAL_FLUSH_MS` later, or the clean-shutdown flush). The
  durable frontier is `wal_len`; the loss window is `[wal_len, wal_appended)`.
- *Crash-consistency (always):* the on-disk WAL prefix is always a valid, crash-consistent
  database. Reboot recovery is *identical to `wal`* — scan from `wal_ckpt`, replay
  checksum-valid consecutive-txn records, stop at the torn tail, cross-check that the
  valid prefix reaches the durable `wal_len` (refuse otherwise). Un-flushed commits are a
  torn/absent tail: they vanish **as whole records**, never partially applied. By COW an
  abandoned tail's freshly-allocated pages came from a freelist/high-water snapshot the
  recovered meta never committed, so no page-accounting leak results — the same argument
  that makes an uncommitted writer safe (§5.2 step 1).
- **Weaker than `commit`/`wal`, and never called durable-on-ack.**

**EOWNERDEAD.** A dead async writer needs no meta-page msync (recovery replays the log,
never trusts the mapping metas — same as `wal`); the successor trusts the in-memory
`wal_appended` and appends after it. A process death loses only the current un-flushed
window, which is the mode's declared loss window — not something the double buffer ever
protected.

**Checkpoint.** `async` checkpoints to `wal_appended` (not `wal_len`): the full-mapping
`MS_SYNC` makes every *appended* commit's pages+meta durable directly in the main file —
strictly stronger than a log `fdatasync` — so `wal_len` is raised to the checkpoint too
(keeping `wal_ckpt ≤ wal_len` for the recovery cross-check) and the log below is
reclaimed. Guarded so `wal` behaviour is byte-for-byte unchanged.

**When to use which.** `none` on `/dev/shm` (no durability wanted). `commit`/`wal` when a
commit must survive power loss the moment it is acknowledged (financial ledger, "the
write is safe" API contract) — `wal` is the cheaper of the two, and batching (a multi-row
statement or a `WriteSession` of N) amortizes its one fsync across the batch for far
higher durable throughput. `async` when you want crash-consistency and high single-client
throughput and can tolerate losing the last few milliseconds of commits on power loss
(caches, derived data, high-rate ingestion with a re-drivable source) — the same
trade-off teams already accept with sqlite `NORMAL` and pg `synchronous_commit=off`.
Measured single-client on this host's ext4 (see `mpedb-bench/RESULTS.md`): `wal` ≈ 2.2–2.7k
inserts/s (durable-on-ack, matching/beating sqlite FULL and pg `sc=on`), `async` ≈ 22–32k
inserts/s (deferred class, matching/beating sqlite NORMAL and pg `sc=off`), and batched
`wal` ≈ 100k rows/s.

## 6. Schema, config, integrity

(TOML format as in v1.0: `[database]` path/size_mb/max_readers/durability +
`[[table]]`/`[[table.column]]` with types int64/float64/bool/text/blob/timestamp,
`primary_key`, `nullable`, `unique`, `default` (const or `now()`), `check`.)

- `schema_hash = blake3(canonical schema bytes)`; the **full canonical bytes are stored
  in the catalog** at init. Attach compares hashes and prints a real field-level diff on
  mismatch (from the stored schema — recovery never depends on someone keeping the old
  toml). `mpedb-cli dump` recovers schema + data from the file alone; `mpedb-cli migrate`
  is the offline rewrite (new file, exclusive flock, plans registry purged).
- Geometry/durability/max_readers are file-authoritative (§4.1).
- CHECK expressions compile via `mpedb-sql::compile_check` — the expression grammar has
  **no functions and no parameters**, so CHECKs are pure and deterministic by
  construction; enforcement cannot diverge across processes. `DEFAULT now()` is resolved
  by the engine at write time (a concrete timestamp is stored; re-execution is
  bit-identical).
- SQL three-valued logic throughout (`mpedb-types::expr`, Kleene AND/OR/NOT, IS NULL):
  CHECK passes on NULL/UNKNOWN (SQL standard), WHERE excludes on UNKNOWN, UNIQUE permits
  multiple NULLs (§4.4).

## 7. SQL front-end and the plan-hash protocol

### 7.1 Pipeline

SQL → tokens → AST → bind (names, rigid types, params) → physical plan (access path +
expression IR + footprint) → canonical bytes → `plan_hash = blake3(canonical bytes ‖
schema_hash ‖ engine format version)`. The plan-IR format version is hashed **and**
embedded as a checked header field of every blob: a version mismatch is
`PlanInvalidated` (re-prepare), never a best-effort parse — mixed engine versions may
attach to one file during rolling upgrades.

### 7.2 Execution API and shared plan registry

```rust
let h: PlanHash = db.prepare("SELECT * FROM users WHERE id = $1")?; // parse once
let rows = db.execute(&h, params![42])?;                            // no parsing
```

- **`prepare` is read-first**: probe the local cache, then the registry in a *read*
  transaction; only a genuine miss opens a short write txn to insert
  `{hash → sql, blob, last_used_txn}` into `__mpedb_plans`. (v1.0 made every prepare a
  write: a read-only workload would serialize on the global writer lock, and a prepare
  inside an open write txn would self-deadlock — the ERRORCHECK mutex (§4.2) turns any
  residual re-lock into an error, and the facade never nests write txns.)
- Any process may `execute(hash)` it never prepared: registry hit → deserialize blob (no
  SQL parsing) → cache locally. `UnknownPlan` is a normal, retryable outcome (caller
  re-prepares from SQL); hash-only shipping between components requires the receiver to
  hold the SQL text as fallback.
- **Registry hygiene:** capped (config, default 4096 plans); eviction by oldest
  `last_used_txn` (updated on registry load, not per-execute); schema-stale entries
  purged opportunistically. ⚠ Interpolating literals instead of using params creates a
  plan per query — the classic misuse; document loudly.
- **Trust model, stated plainly:** every attached process has a writable mapping of the
  whole file — shared memory is shared fate, and validation cannot stop malice. What
  validation MUST do is keep *accidental* corruption from escalating into memory
  unsafety in healthy processes: on load, verify `blake3(blob ‖ …) == key`, check the
  format-version header, structurally validate every opcode/index/offset against the
  schema (bounds-checked decode in `mpedb-types`/`mpedb-sql`), and **recompute the
  footprint from the decoded plan** rather than trusting stored flags.

### 7.3 Precomputed footprints ("pre-computed locks")

Per-plan, computed at prepare: the `tables_read`/`tables_written` table SETS (sparse,
sorted `TableSet`s — design/DESIGN-TABLE-CAP.md; they were per-table bitmaps until the
128-table ceiling made that untenable) + the `indexes_used` bitmap over the per-table
index numbering + `KeyAccess` (Point with param/const slots | Range | Full) +
`read_only`.

- `read_only == true` → `execute` routes to a read txn; the writer lock is never touched.
- **Honesty rule (confirmed finding):** exact key-level write sets exist only for
  PK-point DML on tables without read-dependent index maintenance. `UPDATE … WHERE
  email = $1`, index-key deletes derived from current row values, multi-row inserts —
  all degrade to table/tree-level footprints (`Full`), and the Phase 2 scheduler treats
  those as conflicting with everything on that tree. Never overclaim precision.
- **What footprints actually feed today**, separated from what they were designed to:
  - **read-only routing** (Phase 1) — built, above.
  - **deterministic batch ordering** (Phase 2) — built, but as the *key-locality* sort
    of §5.3, not as conflict grouping. `Footprint::conflicts_with` exists and is unit-
    tested, yet has **no production caller**: the leader sorts the drained batch by
    (written table, materialized key bytes, slot idx) and executes all of it in one
    transaction, so there is no group for a conflict relation to form. The locality
    rationale itself was then measured and largely falsified (#111: on Linux `commit`
    the data barrier is one span regardless of run count); the sort is retained for
    deterministic linearization. `crates/mpedb/examples/footprint_index.rs` is the
    scale study of the conflict relation — an example, not a code path.
  - **prefaulting before the writer lock** (§5.2) — designed, not built.

  INNOVATIONS.md §9.6 states both gaps; this section is subordinate to it.

### 7.4 Expression IR

Stack-based compact IR (`mpedb-types::expr`), compiled once, constant-folded, statically
validated (stack discipline, const bounds) at build *and* at every decode; evaluation is
a tight loop, no AST walking, checked arithmetic, full SQL 3VL.

## 8. Crate layout

The five crates this document was written around (all ✅ since Phase 1):

```
mpedb/                    workspace
├── crates/
│   ├── mpedb-types/      values, schema+hash, config, keycode, expr IR, footprints  ✅
│   ├── mpedb-core/       pagestore, COW B+tree, row codec; shm engine: mapping,
│   │                     meta, locks, reader table, ring, freelist, txns, catalog  ✅
│   ├── mpedb-sql/        tokenizer→AST→binder→planner→plan ser/de+hash              ✅
│   ├── mpedb/            facade: Database, prepare/execute, plan registry, exec,
│   │                     ring_exec, sqlite overlay, tier                            ✅
│   └── mpedb-cli/        REPL, bench, stress/crash/powerloss, dump, mirror, tier    ✅
└── DESIGN.md
```

The workspace has since grown to **13** members. The eight added since (only the
last is covered elsewhere here, in §9):

```
│   ├── mpedb-sdk/        client-side plan cache: detached plans held per Session
│   │                     instead of in the shared registry (§7.2)
│   ├── mpedb-proc/       stored procedures — a Python/Rust subset compiled to a
│   │                     sandboxed content-hashed IR, stored in the database
│   ├── mpedb-mirror/     bidirectional CDC sync with sqlite/PostgreSQL (DESIGN-MIRROR)
│   ├── mpedb-capi/       libsqlite3-compatible C shim (DESIGN-CAPI)
│   ├── mpedb-sqlitefmt/  the sqlite file format read natively, no sqlite library
│   │                     in the graph (DESIGN-SQLITE-BACKED §4)
│   ├── mpedb-testkit/    sqllogictest runner, corpus, differential tester
│   ├── mpedb-bench/      the head-to-head harness vs sqlite/PG (BENCHMARKS.md)
│   └── mpedb-py/         PyO3 bindings (§9)
```

⚠ `mpedb-capi` is a workspace member but **not** a default member: it exports the
`sqlite3_*` symbols and so cannot co-link the bundled sqlite that feature unification
pulls in for `mpedb-cli`/`mpedb-bench`. Build and test it standalone with `-p
mpedb-capi`; use `--workspace --exclude mpedb-capi` for whole-workspace commands.
That is why `default-members` lists 12, not 13.

Dependencies of the five core crates: `libc`, `blake3`, `xxhash-rust`, `serde`+`toml`
(config only). No unsafe outside `mpedb-core`'s shm layer.

## 9. Python bindings (Phase 3 sketch)

PyO3 `abi3-py312`; GIL released around engine calls; no process-global engine lock →
free-threaded 3.14 gets parallel reads natively. Zero-copy text/blob via buffer protocol
scoped to explicit snapshot contexts.

⚠ **Mapping-scale note (corrected §11 claim):** page tables for MAP_SHARED mappings are
per-process — a 4 GB mapping touched fully costs ~8 MB of PTEs *per process* (~8 GB
across 1000 processes) plus ~1M warm-up minor faults each. For big regions use tmpfs
huge pages (`huge=within_size`) or `MADV_COLLAPSE`; the engine calls
`madvise(MADV_HUGEPAGE)` opportunistically. Small (≤ 256 MB) regions are a non-issue.

## 10. Testing & verification strategy

1. Unit tests per module; model-based B+tree tests vs `BTreeMap` (done).
2. Multi-process stress (`mpedb-cli stress`): invariants — bank-sum conservation across
   snapshots, UNIQUE under contention, snapshot stability.
3. Crash injection (`mpedb-cli crash`): each child arms a thread that sleeps a
   **random 5–60 ms and then SIGKILLs itself** — armed *before* attach, so the kill
   walks across the attach, prepare, registry-publication and commit windows as the
   waves accumulate, rather than only the enumerated commit-path points this document
   originally specified. Strictly stronger: it covers the attach/init window too, and
   it cannot miss a kill point nobody thought to name. The parent verifies integrity +
   invariants after every wave. Reader kills exercise slot reclamation (incl.
   seq/pid_start paths); writer kills exercise EOWNERDEAD + durable_txn recovery.

   ⚠ **A randomized window can hide the thing it tests.** On a fast machine one 5–60 ms
   window churns enough COW pages to exhaust a small file, and the child then dies of
   `DbFull` *before* its kill lands — the run looks green while testing nothing, because
   an out-of-space abort masks the recovery path. The harness now counts those aborts
   separately and fails loudly with the fix in the message (`crash --size_mb`, raised
   further for `--blob-kb` runs). Worth stating as method, not just as a flag: a crash
   harness must be able to prove that its crashes happened.
4. **Page accounting invariant** after every commit (§4.5).
5. Init-crash matrix: kill at every step of §3 (post-create, mid-fallocate, mid-format,
   pre-READY) → next attacher must recover; zero-size and short files must be adopted.
6. Miri on safe layers; the shm layer is torture-tested (loom does not model
   cross-process).

## 11. Performance targets (Phase 1, this 2-core host)

- Point read via `execute(hash)`, warm: < 5 µs.
- Point write commit, durability=none: < 25 µs (see §5.2 ⚠ for `commit` mode).
- Attach: < 5 ms; 1000 attached processes supported with max_readers sized to expected
  concurrent read txns (config), slot claim O(1) amortized under churn.
- Engine overhead < 2 MB per process **excluding page tables** (§9 note) for ≤ 256 MB
  regions.
