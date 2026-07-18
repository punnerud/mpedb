# DESIGN-BLOBEXTENT — key/value separation for large values

Status: **v1.0** — v0.9 survived a three-lens adversarial review (crash/
ordering, MVCC/reclaim/fixpoint, API/platform/honesty): 24 findings, 6
critical, all folded in below. The review-shaped rules are marked **[R]**.
Owner task: #50. Prereqs met: #40 (warm end-numbers), #42 (scatter-gather),
#43 (streaming insert), #58 (crash/blob harness to extend).

## 1. Why — the measured gap, honestly attributed

Pushing 256 MiB of 4 KiB blobs (durability `none`, **tmpfs, batched 256
rows/commit**, Linux EPYC — BENCHMARKS.md): sqlite writes **998 MiB/s (38% of
raw)**, mpedb **602 MiB/s (23%)**. **[R]** On the M3 the same cell already
goes the other way (mpedb 2274 vs sqlite 1163) — the gap this document closes
is the Linux one, and the honest metric is *% of raw*, never MiB/s.

Where the missing bandwidth goes — with the measurement each claim actually
has behind it **[R]**:

1. **Fault-and-zero per fresh page** — *measured at 16 MiB* (#40: cold path
   faults; warm path ~2250 MiB/s once clones died). Payload lands through the
   shared mmap; every fresh page is a minor fault plus kernel zeroing that
   the memcpy immediately overwrites. sqlite appends via `pwrite`.
2. **Per-row engine cost** — *the measured limiter at 4 KiB* (BENCHMARKS.md:
   these cells did not move when the row buffer died; #32's territory).
   Extents shrink the per-row work (no chain page, no chain header, a 20-byte
   reference instead) but the 4 KiB verdict belongs to §13's A/B, not to this
   list.
3. **Overflow-chain bookkeeping** — chain pages are *fresh* pages (COW never
   copies payload), but each passes page accounting and the fixpoint's
   working sets.
4. **WAL double-write** (durability `wal`/`async` only): the WAL logs
   physical page images, so payload is written twice today.

The architectural answer is WiscKey's: **separate keys from values**. Values
above a threshold leave the page-structured COW tree; the tree keeps a
20-byte reference. Payload is written once, sequentially, via `pwrite` — and
the COW/crash machinery only ever handles small pages.

## 2. Design commitments

1. **One file.** Extents live in the `.mpedb` file's page space, allocated as
   page-aligned runs from the same (logical) `high_water` the page allocator
   uses — inside the format-time-preallocated file; nothing grows at runtime
   **[R]**. The `cp`-snapshot property survives. No sidecar.
2. **Extents are immutable once published.** No write ever touches a live
   extent — UPDATE writes a new extent and frees the old one under the
   committing txn. Committed-pages-are-immutable, applied to runs.
3. **Payload before reference.** An extent's bytes are fully written — and in
   durable modes, synced by an explicitly tracked range-sync (§4) — before
   any tree page or WAL record referencing it is published. Scope honestly
   per mode **[R]**: under `none` this holds against process death (the
   shared page cache loses nothing on SIGKILL); against power loss `none`
   promises nothing today and extents inherit exactly that (DESIGN.md §5.4).
4. **The whole run is written before the reference publishes.** **[R]** The
   tail of the last page is zero-filled, and `insert_streaming` with a source
   that yields fewer bytes than declared HARD-ABORTS the txn — a reference
   over a partially written run is how a dead writer's residual bytes would
   become readable (§4, crash pair analysis).
5. **Payload bypasses the WAL.** The WAL keeps logging page images for tree
   pages; extent bytes are range-synced in place before the WAL append. The
   double-write dies; the replay argument is rebuilt — not assumed — in §7.
6. **Payload bypasses the mapping on write.** Extent bytes go through
   `pwrite`/`pwritev` (64-bit offset APIs explicitly — `FileExt::write_at`,
   never raw 32-bit `off_t` **[R]**), not mmap stores: no fault-and-zero.
   Reads use the mapping (unified page cache; §5).
7. **Reclaim = the page rule PLUS the durable frontier.** A freed run is
   reusable when its freeing txn ≤ oldest-pinned bound (pages' rule, same ≤)
   **and, in WAL-class modes, the freeing commit's WAL record is durable**
   **[R]** — §6 has the two-line rule and §7 the attack it blocks. Recording
   goes through the freelist with a run entry kind, in the same fixpoint,
   with the termination argument preserved by an attribution rule (§3.3).
8. **The threshold is a config knob with a measured default.** Values whose
   encoded payload > `extent_threshold` take an extent; default decided by
   §13's paired A/B. At/below threshold nothing changes — this design adds a
   representation, removes none.

## 3. On-disk layout

### 3.1 The value reference (leaf cell)

Leaf value kinds today: `vkind=0` inline, `vkind=1` overflow. Added, exactly
one format **[R]**:

```
vkind=2  extent:  start_page u64 ‖ total_len u64 ‖ npages u32     (20 bytes)
```

Whole-cell overhead: 3 (cell header) + klen + 20. Decode rules, house-strict:
`npages == ceil(total_len / PAGE_SIZE)` or `Error::Corrupt`; all bounds
arithmetic (`start_page * PAGE_SIZE`, `start_page + npages`) is checked
u64 math — overflow is `Corrupt`, never a wrap, never a panic **[R]**; the
run must lie inside `[first_data_page, page_count)` — bounded by the
*mapping*, not just `high_water` **[R]**.

`vkind=3` is **reserved** for #52's base+diff cell (§10) and refused by name.

### 3.2 The extent map

A new COW btree, root in the meta snapshot: key `start_page` (u64 BE), value
`npages u32`. Holds LIVE extents only; free space is the freelist's job. It
exists for the verifier: every page must be exactly one of meta/lock/reader/
ring, tree-reachable, freelist-listed (bare id or inside a run), extent-
mapped, or ≥ high_water — and with runs in play the check needs **interval
arithmetic**: partial overlap (run∩run, run∩page-id, run∩reachable) is the
new corruption class the set-insert verifier cannot see **[R]**.

### 3.3 Free runs in the freelist

There is no spare byte in today's `(txn u64 BE ‖ chunk u16 BE)` key **[R]**,
so run entries get an explicit key form: `(txn u64 BE ‖ kind u8 ‖ chunk u16
BE)` — 11 bytes, txn still first (the early-stop scan order survives), kind 0
= page ids, kind 1 = runs with values `(start_page u64 ‖ npages u32)` pairs,
entries capped ≤ 960 B inline as today. **All v3 keys are uniformly 11
bytes** — v2 files hard-error at attach, so no v3 file can contain a 10-byte
key and no dual parsing exists (build simplification over v1.0's compat
note). **Every freelist parser learns the
kind** — the review enumerated the sites: `refill_reusable`,
`verify_page_accounting`, `freelist_shape`, `leak_counters`, and
`freelist_plan`'s diffing **[R]**.

Allocation and the fixpoint:

- **Draw is read-only** (the #37 lesson, unchanged): a run entry is drawn
  into the writer's private pool and LEFT in place; the fixpoint strikes out
  only what was consumed.
- **Attribution rule (the fixpoint's new load-bearing line) [R]:**
  *consumed = ever allocated out of the entry.* A sub-run allocated and freed
  again within the same txn goes into the commit's OWN free set under
  `new_txn` — never back into the drawn entry. Write-back values are
  therefore always a shrinking subset of the drawn value: they can never grow
  past the inline cap, never spill to an overflow chain mid-fixpoint, and the
  §4.5 termination argument (monotone sets, height-bounded self-frees,
  high-water fallback frees nothing) carries over verbatim. Without this rule
  a drawn run split by an alloc-then-free GROWS the entry (one run → two
  fragments) and the loop oscillates into the 64-iteration abort.
- **Draw-time pool coalescing [R]:** adjacent runs drawn from *different*
  entries merge in the writer's private pool (draw is read-only, so this is
  free and format-less). This is the answer to the deterministic worst case
  the review constructed — grow-by-append workloads where every hole is
  smaller than every future request and first-fit never hits: pool merging
  re-creates big runs from adjacent small frees. Cross-commit ON-DISK
  coalescing stays a non-goal; **compaction needs no format hook at all** —
  relocating a value is an ordinary UPDATE (new run + new cell + map edit)
  under this document's own rules, so it remains pure policy (§12).
- **Run validation at draw [R]:** runs drawn from an entry must be strictly
  ascending, non-overlapping, and inside `[first_data_page, page_count)` —
  `Corrupt` otherwise, exactly as `refill_reusable` validates bare ids today.
  A corrupt run is how a `pwrite` would otherwise land on the lock pages.
- **Pool separation [R]:** the page allocator MAY draw single pages out of
  run fragments (so sub-extent fragments are not dead space), but only
  outside `in_freelist_op` — the fixpoint itself never touches the run pool,
  for the same reason refill is blocked during it: the termination argument
  assumes the pool does not change shape mid-loop.
- Fallback: bump the logical `high_water` by `npages`. On Linux the file was
  fully `fallocate`d at format time so there is no hole to fall into; on
  macOS `preallocate` is ftruncate-based ("bench-grade") and a pwrite into a
  sparse region can hit ENOSPC mid-commit — which fails the txn cleanly
  (errno, not SIGBUS — strictly better than the mmap-store path) and must be
  documented where `none`-on-macOS already carries caveats **[R]**. On ext4
  the first sync touching fresh fallocated (unwritten-extent) regions pays
  journal conversion — the WAL prezeroes for exactly this reason (measured
  2.7×); §13 controls for it in the A/B rather than guessing **[R]**.

### 3.4 Meta and WAL framing (file format v3, WAL3) [R]

`MetaSnapshot` gains `extent_map_root u64`. That is NOT one free field — it
lands exactly on today's checksum offset, so the layout is stated here, not
discovered by the implementor: `extent_map_root` at offset 96, `M_CHECKSUM`
moves 96 → 104, `META_LOGICAL_LEN` 96 → 104, xxh3 coverage becomes bytes
0..104. `FORMAT_VERSION` 2 → 3; attaching a v2 file is a hard error naming
the version and the `mirror export` path.

**The WAL record framing must move too** — v0.9's "no WAL format change" was
false: the record trailer IS the MetaSnapshot body, and recovery rebuilds the
meta *exclusively from the trailer* (the mapped meta pages are explicitly
untrusted after power loss). A trailer without `extent_map_root` means every
reboot-recovery installs a meta with a stale/lost extent map — verifier
failure and double-allocation later. `WAL_MAGIC` "WAL2" → "WAL3"; trailer
gains `extent_map_root`; `decode_wal_record` and its
truncation-at-every-offset tests extend.

## 4. Write path

`insert_row` with a value > threshold:

1. **Reserve** a run (first-fit over the coalesced private pool, else logical
   high-water bump). Allocation state is writer-private; crash here publishes
   nothing, leaks nothing.
2. **Write payload** via `pwritev` at `start_page * PAGE_SIZE`; zero-fill the
   last page's tail. Streaming (#43) reserves on declared length and
   HARD-ABORTS on a short source (commitment 4). The txn records the run in
   its **extent-dirty list** — the explicit range tracking that §4-step-3
   syncs; nothing infers it from `self.dirty`, which only ever held COW
   mmap pages **[R]**.
3. **Tree insert** of the `vkind=2` cell (COW, small pages).
4. **Extent map insert** (COW, small pages).
5. **Commit** — `commit_inner`'s existing sequence with these insertions:
   - *step 2 (fixpoint):* freed runs join this commit's free set as kind-1
     entries under the attribution rule (§3.3).
   - *step 3, durability `commit`:* the extent-dirty ranges are synced with
     **`msync(MS_SYNC)` over each run's mapping range** — on Linux this is
     `vfs_fsync_range` and covers pwrite-dirtied pages of the same file (the
     unified page cache has one dirty set). This is a PREMISE of the design,
     promoted from v0.9's "worth testing" — and §13.3 tests it with a
     power-cut *after ack*, because v0.9's claim ("the single pre-flip
     barrier covers them") was false on Linux: `sync_barrier()` is a no-op
     there; the per-range msyncs ARE the durability **[R]**.
   - *step 3, durability `wal`/`async`:* payload ranges are synced with the
     same range-bounded msync **before `wal_append`** — NOT with a whole-file
     `fdatasync`, which on Linux flushes every dirty page in the file and
     turns each large-value commit into a surprise mini-checkpoint (cost
     shaped by distance to the last checkpoint; the A/B measures both to
     keep this honest) **[R]**. The WAL logs only tree/map page images.
6. **Flip** publishes the meta (unchanged fences). The extent becomes
   reachable exactly when the tree referencing it does.

**Crash pairs, multi-process [R]:** writer A pwrites a run and dies
(SIGKILL); writer B — serialized by the writer lock, re-deriving allocation
from *published* state that A never changed — may allocate the same run. B is
safe because of two explicit conditions: draw was read-only (A's draw left
the entry intact) and commitment 4 (B overwrites the whole run before any
reference publishes). A's residual bytes in the tail of a split allocation
stay freelist-owned and unreferenced. The streaming short-source abort is
what keeps condition two airtight — §13.4 SIGKILLs a streaming insert
mid-source to hold it.

## 5. Read path — a new API, scoped honestly [R]

v0.9 promised "pread-free slices" from an API that does not exist: #43 is
write-side only, and today every read materializes `Vec<u8>` through
`Value::Blob`. Worse, a long raw borrow of the mapping would disarm the
max-pin-age eviction valve (DESIGN.md §4.3): today's read path copies out and
*re-validates its pin every 256 steps* so eviction "never surfaces as
silently corrupt rows"; an hours-long borrow held across an HTTP Range
stream is exactly the workload that triggers eviction — and then a writer
reuses the run under a live `&[u8]`, which is UB, not just a wrong answer.

So the read design is:

- **New API surface** (named as such): `ReadTxn::blob_read(table, pk, col,
  range) -> BlobChunks`, an iterator yielding **bounded chunks** (default
  256 KiB): each chunk is copied out of the mapping *after* a
  `still_pinned()` revalidation, exactly the cadence contract scans already
  honor. Eviction between chunks surfaces as the existing stale-pin error —
  never as mixed bytes. One contiguous memcpy per chunk is the honest cost;
  "zero-copy" is NOT promised for live databases.
- SQL (`SELECT blob_col`) and py keep materializing full values (py must copy
  regardless); their win is the chain-walk disappearing, not zero-copy.
- **FrozenDb (#22) is where true zero-copy lives**: frozen files have no
  writers, no eviction, no reclaim — a borrowed extent slice is sound there,
  and an extent is one contiguous file range = **one HTTP Range request**.
  The pack step may reorder extents for locality without format changes.

## 6. Reclaim, MVCC, the verifier

Free of run R by txn T; R reusable when **both**:

1. `T ≤ oldest-pinned` — pages' visibility rule, same ≤ (the #37 off-by-one
   would leak runs identically; the regression test grows a run flavor);
2. **durable-frontier gate [R]** (WAL-class modes): T's WAL record end-offset
   ≤ the durable WAL frontier (`wal_len` after its fdatasync). In `wal` mode
   this is free — ack is durable before the writer lock releases, so any
   later writer already satisfies it. In `async` it is one comparison against
   the flushed watermark. Why it exists: pages are protected from
   reuse-before-durability by being *logged* — replay overwrites a reused
   page with the record's image. Extents opted out of logging (commitment 5),
   so they need the gate the log was silently providing. §7 has the attack.

Pinned readers keep runs unreusable by construction — via the *chunked* read
API of §5, never via long raw borrows.

The verifier learns interval arithmetic (§3.2), the leak ledger gains
`EXTENT_ALLOC_PAGES`/`EXTENT_FREED_PAGES`/`REFLINK_HITS` **[R]**, and the
run-decode-at-draw validation (§3.3) guards the allocator itself.

## 7. Durability modes and recovery — all four [R]

| mode | payload | ack | recovery argument |
|---|---|---|---|
| `none` | pwrite, no sync | flip | **Process death**: page cache is coherent; an unflipped reference never publishes, torn payload is unreachable. **Power loss**: outside `none`'s contract today (arbitrary staleness, DESIGN.md §5.4) — extents inherit exactly that, no more, no less. |
| `commit` | pwrite + per-run `msync(MS_SYNC)` before flip | after meta msync | Extent ranges are explicitly range-synced in step 3 (§4) — on Linux the no-op `sync_barrier` means these msyncs ARE the ordering, so the extent-dirty list is load-bearing, not an optimization. Double-buffered meta unchanged. |
| `wal` | pwrite + per-run range-sync **before** wal_append | after WAL fdatasync | Insert direction: a valid record exists only after its payload's sync returned → replayed references never point past the crash. **Reuse direction — the previously unwritten invariant**: *no byte of a freed run may be overwritten before the freeing commit's record is durable.* In `wal` this holds because `wal_commit` makes the record durable before ack/lock-release, so the overwriter (a later txn) always starts after the free is on disk — the valid prefix can never end before the free. Stated here because it is load-bearing, easy to break silently (see `async`), and now tested (§13.3). |
| `async` | as `wal` | flip (durability deferred) | The deferred fdatasync is exactly what breaks the reuse invariant above — a durable prefix can end BEFORE the free while the kernel already persisted the overwriting payload (payload writeback is unordered against the WAL flusher). Page images survive this (replay restores them); extents cannot. Hence the durable-frontier gate (§6): reuse waits for the freeing record's flush. `async`'s declared loss window ("whole recent records vanish") is preserved — without the gate it would degrade to *silent corruption of surviving records*, which is a different and unacceptable class. |

`powerloss` grows three cases **[R]**: torn payload tail (value absent or
whole, never mixed); **cut-after-ack in `commit` mode** (the §4 premise:
range-msync durability of pwrite ranges); **free→reuse→cut-before-record in
`async`** (the §6 gate holds the line).

## 8. Threshold policy

`extent_threshold` (config, bytes, file-frozen like geometry; per-database,
never per-column). Anchors: at 4 KiB the extent saves the chain page and the
fault, costs a map entry, and the *measured* limiter is per-row cost (#32) —
so the 4 KiB verdict belongs to the A/B, not this table. Multi-page values
are unambiguous wins. `Any` columns follow value size. `extent_threshold =
∞` must behave byte-for-byte like today (also the A/B control arm and the
compat mode).

## 9. reflink import [R]

`insert_file` gains `FICLONERANGE` — with the alignment truth v0.9 skipped:
the *length* must be block-aligned too, and imports are arbitrary-length. So:
clone the block-aligned prefix, `pwrite` the tail + zero-fill the last page;
require `fs_block_size ≤ PAGE_SIZE` (else fall back entirely). Btrfs/XFS:
works. ext4: no reflink → silent pwrite fallback. macOS/APFS: `clonefile` is
whole-file-only → always pwrite. `REFLINK_HITS` in the leak ledger makes the
fallback rate *measurable* — a zero-copy path that silently never engages is
dead code, and we would rather see the number.

## 10. base+diff (#52 hook) — reserved, not inherited [R]

`vkind=3` is reserved for `(base_run, diff_run, lens_id)`. v0.9 claimed the
substrate came "with zero new protocol" — false: §6's reclaim is
single-owner, and a base run SHARED by two references either dangles on the
first DELETE or leaks on both. Sharing needs an ownership design (refcounts
in the extent map, or copy-on-reference) that #52 must supply. This document
guarantees the substrate for *single-owner* runs only.

## 11. Migration and compatibility

FORMAT_VERSION 2→3, WAL2→WAL3 (§3.4). Attach to old: hard error, honest
message. `extent_threshold = ∞` gives today's behavior in a v3 file. The
mirror moves logical rows and needs nothing.

## 12. Non-goals

No compression/dedup/content addressing (PySpell/#52 territory). No
per-column thresholds. No cross-commit on-disk coalescing — and no format
hook for compaction either, because **compaction is an ordinary UPDATE
rewrite** under this design's own rules: pure policy, addable any time
**[R]**. No intent-ring/leader changes (§5.3 untouched). No zero-copy reads
on live databases (§5 — FrozenDb only).

## 13. Test & measurement plan

1. **Unit**: vkind=2 truncation-at-every-offset; len/npages cross-check;
   checked-arithmetic overflow cases; 11-byte freelist key + run-entry codec
   incl. the five parser sites; WAL3 trailer truncation tests.
2. **Model**: TestStore run allocation; differential vs BTreeMap-of-blobs
   under insert/update/delete/read churn; interval-verifier after every
   commit; fixpoint convergence under alloc-then-free-in-txn (the §3.3
   attribution rule's regression).
3. **Crash/power**: SIGKILL fuzz (absent-or-whole); `powerloss` × three new
   timelines (§7): torn tail, cut-after-ack (`commit`), free→reuse→cut
   (`async`); SIGKILL mid-streaming-source (§4 crash pairs).
4. **Concurrency**: pinned reader + UPDATE storm on extent values (run-flavor
   #37 regression); chunked blob_read across an eviction (stale-pin error,
   never mixed bytes).
5. **Paired A/B** (Pi = decision instrument, M3/EPYC = absolute numbers):
   `extent_threshold = ∞` vs default, same binary, md5-verified arms. Cells:
   bulk 4 KiB / 64 KiB / 1 MiB; churn (insert/delete interleave —
   fragmentation curve for Q3); grow-by-append (the §3.3 worst case);
   ext4-vs-tmpfs (unwritten-extent conversion control); `wal`-mode
   range-msync vs fdatasync payload sync. **Acceptance is % of raw ≥
   sqlite's % of raw in the SAME paired run on the Linux tmpfs 4 KiB cell**
   (v0.9's "≥ 998 MiB/s on the M3" was already true of the baseline —
   unfalsifiable targets are not targets) **[R]**.
6. **Leak ledger**: EXTENT_*/REFLINK_HITS reconcile across the differential
   run — #40's method, applied to space.

**Measured (reflink empirics, 2026-07-17 — the §"verify EMPIRICALLY before
freezing" gate, now cleared).** On real btrfs AND xfs (loopback, kernel 6.8),
a purpose-built probe answered all three hard questions YES: (1)
`FICLONERANGE` succeeds into a `fallocate`-UNWRITTEN region of a file that is
already `MAP_SHARED`-mapped; (2) the cloned bytes are visible THROUGH a
pre-existing mapping whose page-cache pages were touched before the clone —
the kernel invalidates/updates coherently; (3) an mmap write over a cloned
range CoWs cleanly (`msync`, then the source re-read unchanged). The
reflink import path can therefore be built as designed: clone whole pages
straight into the extent region of the live mapping, pwrite+zero the tail.
Paired extent A/B on the same mounts (4 KiB cells): btrfs 1.05×, xfs 1.07×
— no COW-on-CoW write-amplification pathology, modest win intact.

**Measured (Pi-paired closeout, 2026-07-17).** The Pi (armv7, SD card, the
steadiest A/B instrument) reports **parity at both sizes**: 4 KiB blobs
0.993, 64 KiB 1.004 (median of 4 ABAB reps, 16 MiB/arm, durability=none) —
the box is CPU/SD-bound, and neither the extent path's saved page headers
nor its coalesced pwrites move anything measurable. The wins are where the
syscall/header shape dominates: Linux x86 tmpfs 1.70–1.84× (4–8 KiB, after
per-commit pwrite coalescing) and Apple APFS 1.09–1.39× (16 KiB–1 MiB). The
Linux 4 KiB / macOS 32 KiB defaults stand — on armv7 the default is measured
HARMLESS, not helpful, and needs no per-arch carve-out.

## 14. Open questions

- **Q1 (32-bit ARM) [R]**: the engine full-file-mmaps, so the practical
  database cap on armv7 is ~2 GiB of address space — and `size as usize`
  in `Shm::map` silently truncates a ≥ 4 GiB size on 32-bit (mapping smaller
  than `page_count` believes → SIGSEGV, not `Error`). Independent of extents
  but promoted by them (big values, big files): add a
  `target_pointer_width` guard on `size_mb` at open, and audit offset math
  for u64-before-usize. Ship with this design.
- **Q2 (allocation scan bound)**: first-fit over the private pool under the
  writer lock — cap the scan (fallback: high-water) or prove the pool stays
  small? Measure in the churn cell first.
- **Q3 (fragmentation verdict)**: draw-time pool coalescing (§3.3) is the
  mitigation shipped; the churn + grow-by-append cells decide whether policy
  compaction (§12) is needed, and when.
- **Q4 (fragmented DbFull)**: the error must say why: "no contiguous run of
  N pages (M free in fragments)" — operator-actionable, cheap, do it.
- **Q5 (threshold default)**: 1 page vs 4 — the A/B's 4 KiB and churn cells
  answer; ship `∞`-compat knob regardless.
