# DESIGN-PHASE3 — Optimistic parallel writers: analysis, prototype, and measured verdict

Status: post-implementation, post-measurement. Companion to DESIGN.md §5.2 (serialized
commit), §5.3 (intent ring as-built), §5.4 (durability), and §7.3 (footprints).

**Verdict up front: NO-SHIP as default. Optimistic parallel writers do not beat the
serial writer-lock path (with the Phase-2 intent ring) on this COW B+tree engine, on
this 2-core host, in ANY measured configuration.** The mechanism is structural, not an
implementation defect: the expensive part of a write — the copy-on-write B+tree mutation
— is exactly the part that cannot be moved off the commit critical section, and in
durable modes the intent ring's group commit (which optimistic per-writer execution
throws away) is the actual, measured win. A sound prototype is retained behind the
`concurrency = "optimistic"` config flag (default `serial`, experimental, off by
default) so the A/B is reproducible; the default serial path is byte-for-byte unchanged.
This is the MPEE-workbench outcome again (DESIGN-MPEE-OPT.md): a rigorously-measured
negative is the result.

---

## 1. The central obstacle: OCC on a copy-on-write tree

DESIGN.md §5.3 sketched "optimistic parallel writers … build private COW deltas … validate
footprints at commit (first-committer-wins)". Before writing a line of the feature, the
obstacle was written down and checked against *this* engine.

### 1.1 The COW-rebase problem (generic)

The B+tree is copy-on-write (`pagestore::cow`, `btree::insert`/`delete`): a mutation
against snapshot `S` (roots `R_S`) produces new roots `R_A` by copying every page on the
root-to-leaf path; the new internal pages point at a *mix* of freshly-copied pages (this
txn's) and unchanged pages shared with `R_S`. If a concurrent txn `B` commits `S → T`
(roots `R_T`) touching **different** keys, `R_T` likewise descends from `R_S` with `B`'s
path COWed. Now `A` wants to commit on top of `T`:

- `R_A` is missing `B`'s changes (it descends from `R_S`, not `R_T`).
- `R_T` is missing `A`'s changes.
- There is no root that contains both. You cannot splice `R_A` over `T`.

Real OCC on a COW tree therefore has only two escapes:

- **(a) Re-apply.** Discard the private delta; take the current committed tree and
  re-execute `A`'s *logical* mutation against it inside the critical section. The COW
  tree work is redone **serially**; only the read/validation/encode of the statement
  parallelizes. (DESIGN.md's own phrasing: "execution is then serial for the APPLY —
  only read/validation parallelizes".)
- **(b) Page-level rebase/merge.** Build a merged root from `R_T` by overlaying `A`'s
  changed child pointers. Sound **only** if `A`'s and `B`'s changed-node sets are
  disjoint at every level *and* neither caused a split/merge/rebalance of a node the
  other touched. mpedb's B+tree splits, merges, and rebalances (`btree.rs`), so two
  point writes into the same leaf both COW-and-maybe-split it — irreconcilable — and
  detecting "structurally disjoint" is itself work under the lock. Rejected: complex,
  fragile, and no better than (a) for the small txns that dominate.

### 1.2 Engine-specific obstacles (beyond the generic one)

- **Page allocation is lock-bound.** `WriteTxn::alloc` pulls pages from the freelist tree
  (itself a COW B+tree with a commit-time fixpoint, DESIGN.md §4.5) and the shared
  `high_water`. The sketch's "leased per-writer arena" would let a writer grab a page
  range off-lock — but a dying lessee's arena then has to be reclaimed to preserve the
  **page-accounting invariant** (DESIGN.md §4.5), which is *new* crash-safety surface.
  Today's clean property — "an uncommitted writer's allocated pages came from a freelist
  snapshot that was never committed, so they are not leaked and not reachable" (§5.2
  step 1) — holds precisely because allocation happens under the lock and is never
  published. **This prototype deliberately does not build an off-lock arena** (see §3):
  it moves only reads/validation/encoding off-lock and allocates under the lock via the
  normal path, so page accounting and crash-safety are inherited unchanged (verified —
  §5).
- **The freelist fixpoint is inherently serial** (mutates the shared freelist tree under
  the lock). Optimistic writers cannot run it in parallel.
- **Index maintenance defeats key-level footprints.** Exact key-level write sets exist
  only for PK-point DML on tables **without** read-dependent index maintenance
  (DESIGN.md §7.3 honesty rule): `UPDATE … WHERE email = $1`, index-key deletes derived
  from current row values, and multi-row inserts all degrade to table-level (`Full`)
  footprints. So the *only* statements that can avoid whole-table conflicts are
  single-table PK-point INSERT/UPDATE/DELETE on a table with no secondary unique index.
  Everything else must conflict at table granularity → a retry storm → serial fallback.

Both escapes leave the same question for measurement: **does moving read/validate/encode
off the lock and re-applying (or blind-applying) under a short lock beat serial, given
that the COW tree mutation — the expensive part — stays serial, and given the intent
ring already amortizes the durable-mode flush across a batch?**

## 2. The ceiling measurement (decisive, single-thread)

Before trusting any end-to-end number, the serial write path was decomposed to bound what
optimistic execution could *ever* save. Reproduce with:

```
cargo test -p mpedb-core --release -- --ignored decompose_write_phases --nocapture
```

For an autocommit `UPDATE … WHERE id = $1` on a PK-only table (the optimistic-eligible
class), `durability = none`, warm tmpfs, single thread:

| phase                                   | ns/txn | % of txn |
|-----------------------------------------|-------:|---------:|
| begin (writer lock + newest_meta)       |    106 |    2.4 % |
| **execute (COW tree mutation)**         |   2834 | **63.3 %** |
| commit (freelist fixpoint + flip + unlock) | 1417 |   31.7 % |
| total                                   |   4476 |   (223k txn/s) |

The 63 % "execute" is the *candidate* for off-lock work — but it splits as:

| execute sub-phase                       | ns   | note |
|-----------------------------------------|-----:|------|
| read traversal (fetch old row)          |  586 | parallelizable (prep) |
| row encode                              |   85 | parallelizable (prep) |
| **COW write (blind Upsert of payload)** | **1991** | **unavoidably serial** |

The COW *write* is 70 % of execute and is exactly the part that must run against the
**current** committed tree under the lock. So the best case — a specialized *blind apply*
that reuses the prep's encoded payload and skips the re-read — shrinks the critical
section only from:

- serial: begin + execute + commit = **4358 ns**, to
- optimistic-blind: COW-write + commit = **3408 ns**,

a **1.28× critical-section ceiling** — and that is *before* any OCC overhead (reader-slot
pin/unpin with its SeqCst fence, footprint-ring validate + record) and *before* the extra
lock acquisition the routing constraint forces (§3). The parallelizable slice
(read 586 ns + encode 85 ns ≈ 15 % of the txn) is small, and moving it off the lock is
worth at most 1.28×, which realistic overhead consumes. INSERT/DELETE decompose the same
way (INSERT ≈ 2.3 µs exec / 1.4 µs commit; DELETE ≈ 2.9 µs exec / 1.5 µs commit).

**In durable modes the ceiling analysis is moot:** the "commit" term is dominated by
`msync`/`fdatasync` (hundreds of µs to ms), the intent ring amortizes ONE flush across a
whole batch (measured 2.65–2.9×, DESIGN.md §5.3/§5.4), and per-writer optimistic commits
pay their own flush each. Shrinking the µs-scale execute term while losing the group flush
is a large net loss.

## 3. What was prototyped (behind the flag)

`concurrency = "optimistic"` (mpedb-types `config.rs`; default `Serial`). Routed entirely
inside `mpedb_core` + `crates/mpedb/src/ring_exec.rs` via a new `Engine::concurrency()`
getter — **no `lib.rs` edit** (the write path already reaches `ring_exec::lead_and_execute`
from `lib.rs::run_write_plan`).

Because the ceiling and the crash-safety argument (§1.2) both point away from an off-lock
COW delta in a leased arena, the prototype is the **soundest realization of the ceiling**,
not the sketch:

1. **Eligibility** (`optimistic_eligible`): single-table PK-point INSERT/UPDATE/DELETE on
   a table with no secondary index. Everything else (multi-row, range, `Full`, tables
   with a unique index) falls through to the serial direct path — so `unique`-workload
   inserts run serially in optimistic mode (measured parity, §4).
2. **Prep (off the writer lock).** `lead_and_execute` releases the lock it was handed,
   pins a read snapshot `S`, resolves the PK, reads the current row, evaluates the SET /
   builds the INSERT row, validates it (`Engine::validate_row_public`), and encodes the
   payload. This is the parallelizable work. Constraint errors that depend only on the
   input row return immediately; world-dependent outcomes (PK-exists, no-match, SET-eval
   errors) are carried to the critical section for confirmation.
3. **Commit (short critical section).** Re-acquire the writer lock; validate the footprint
   against a **committed-footprint ring** in shm (`shm::opt_conflict`,
   `WriteTxn::optimistic_validate`) — first-committer-wins; on overlap raise
   `Error::WriteConflict`. No conflict → **blind-apply** the pre-built op
   (`WriteTxn::optimistic_insert`/`optimistic_upsert`/`optimistic_delete` — one `btree`
   op, no re-read, no re-validate) and commit via the normal `commit_with` path (normal
   allocation, freelist fixpoint, meta flip, durability).
4. **Retry / fallback.** `WriteConflict` retries against a fresh snapshot up to
   `OPT_MAX_RETRIES = 8`, then falls back to a plain serial execute — guaranteed progress.

### 3.1 The committed-footprint ring (soundness core)

A fixed 64-slot ring in the **free tail of the lock page** (bytes 256.., never touched in
`serial` mode → serial on-disk bytes unchanged; **no geometry / `FORMAT_VERSION` change**).
Each committed txn `N` writes slot `N % 64` with `{txn_id, kind, table_bits, key_hash}`,
**under the writer lock, before the meta flip**, so any successor that reads the flipped
meta is guaranteed to already see the entry (both run under the mutex, a full barrier).
Every commit path in optimistic mode records — data writes as POINT `(table, key_hash)`
or TABLE `(table bitmap)`, catalog/sys-only commits as EMPTY — so a same-mode validator
never sees a spurious gap.

`opt_conflict(snap, current, table, key_hash)` scans the exact txn ids in `(snap, current]`
and returns *conflict* if any wrote our table (TABLE) or our exact key (POINT), **or if
any entry is missing/overwritten** (window older than the ring, a torn dead-writer entry
with `txn_id > current`, or a foreign `serial`-mode writer's gap). It is sound by
construction: it never returns *false* when a real conflicting commit exists in the window.
Key-hash is FNV-1a; a collision only ever causes an extra (false) conflict → retry, never
a missed one.

**Crash-safety** (verified, §5): the entry is written before the flip, so a committed
txn always has a complete entry; a writer that dies mid-write leaves a torn entry whose
`txn_id > current` (excluded from every window) which the successor overwrites when it
re-uses the txn id. The apply itself is a normal `WriteTxn` commit, so its on-disk
effects and abort/EOWNERDEAD behaviour are identical to a serial write.

**Mixing safety** (no file-frozen mode needed): if a `serial` process shares the file, its
commits leave gaps in the ring → the optimistic validator reads those as conflicts →
conservative serial fallback, never a missed conflict. The ring is pure validation state,
never durable/recovery state (after a reboot no pre-reboot snapshot survives, so a stale
ring only ever yields empty conflict windows).

### 3.2 The structural tax of "no lib.rs edit"

`run_write_plan` always acquires the writer lock before reaching `lead_and_execute`, so the
optimistic path must **release that lock and re-acquire** for the apply — two lock
acquisitions per write instead of one. This extra churn is inherent to routing inside
`ring_exec`, and it makes the already-marginal 1.28× ceiling worse. It is part of the
honest finding, not a bug; even without it the ceiling is 1.28×.

## 4. Measured results (this 2-core host; serial vs optimistic)

`mpedb stress`, 4 s/arm, `verify: ok` on every run. `mixed` = random autocommit
INSERT/UPDATE/DELETE by PK (eligible); `incr` = autocommit `UPDATE ctr SET v = v+1 WHERE
id = $1` over 64 keys (eligible; the autocommit **conservation** workload — see §5);
`unique` = has a secondary index → ineligible → serial fallback.

### 4.1 `durability = none` (tmpfs; the intent ring is bypassed — this is the arm that
purely tests optimistic *execution*)

| workload | workers | serial ops/s | optimistic ops/s | Δ |
|----------|--------:|-------------:|-----------------:|----|
| mixed  | 2 | 162,007 | 152,257 | **−6.0 %** |
| mixed  | 4 | 148,428 | 138,692 | **−6.6 %** |
| mixed  | 8 | 128,963 | 124,996 | **−3.1 %** |
| incr   | 2 | 153,706 |  84,473 | **−45 %** |
| incr   | 4 | 181,563 |  80,396 | **−56 %** |
| incr   | 8 | 180,559 |  76,331 | **−58 %** |
| unique | 8 | 717,810 | 685,845 | −4.5 % (ineligible → serial; within noise) |

### 4.2 `durability = commit` (ext4 disk; serial = intent-ring group commit, optimistic =
per-writer flush)

| workload | workers | serial ops/s | optimistic ops/s | Δ |
|----------|--------:|-------------:|-----------------:|----|
| mixed  | 2 | 2,787 | 2,794 | ~0 % |
| mixed  | 4 | 3,354 | 2,899 | **−14 %** |
| mixed  | 8 | 5,090 | 2,538 | **−50 %** |
| incr   | 2 | 2,529 | 1,607 | **−36 %** |
| incr   | 4 | 4,532 | 1,445 | **−68 %** |
| incr   | 8 | 8,212 | 1,442 | **−82 %** |

The signature is unmistakable: **serial scales up with worker count** (the group-commit
ring amortizes the flush over a growing batch), while **optimistic is flat** (each writer
flushes alone). The gap widens with concurrency — the opposite of what the hypothesis
needs.

## 5. Why it loses (mechanism), and the soundness gates it passes

- **`none`/mixed (−3…−7 %):** the parallelizable slice is ~15 % of the txn (§2) and the
  extra lock acquisition (§3.2) + reader-slot pin/unpin + footprint validate/record erase
  it. Net: serial minus a few percent.
- **`none`/incr (−45…−58 %):** the read-modify-write conflicts on 64 keys, and the blind
  apply's win is dwarfed by lock churn; the flat ~80k ceiling is the per-writer
  release/re-acquire cost, independent of worker count. Serial `incr` actually *rises*
  with workers (lock hand-off stays warm); optimistic cannot.
- **`commit`/both (up to −82 %):** optimistic bypasses the intent ring, so it forfeits the
  2.65–2.9× group-commit amortization that is the whole durable-mode win. This term
  dominates everything in §2.
- **Conflicts are rare but real:** at 8 workers / 64 keys, `MPEDB_OPT_STATS=1` measured
  321 `WriteConflict` retries per 160,000 applies (**0.2 %**) — the mechanism fires and is
  exercised, but most serialization happens at the (short) apply lock, not via conflicts.

**Soundness — every gate passes (this is why the flag is kept experimental, not deleted):**

- **Autocommit conservation** (`stress --mode incr --concurrency optimistic`, 8 workers):
  committed `Σv` equals total successful increments **exactly**, across `none`, `commit`,
  and `wal` — a lost update from a missed `WriteConflict` would break it. `verify: ok`
  every run.
- **Crash injection** (`crash --concurrency optimistic`, waves=4 children=8): all waves
  pass in `none`, `commit`, and `wal` — page accounting holds, the per-row checksum
  invariant (`a+b=0`, `check_sum=id`) survives SIGKILL mid-apply, EOWNERDEAD recovery is
  prompt, and WAL replay is intact.
- **Page-accounting invariant** (`Database::verify`) holds after every optimistic commit.
- **Serial path unregressed:** full `cargo test -p mpedb-core -p mpedb -p mpedb-sql
  -p mpedb-types -p mpedb-cli` green; serial `stress mixed` (none+commit), `bank`
  sum-conservation, and `crash` waves all pass with baseline numbers; clippy clean.

## 6. Decision

- **No-ship as default.** `concurrency = "serial"` remains the default and is byte-for-byte
  unchanged on disk and in the commit protocol (the optimistic footprint ring lives in
  lock-page bytes serial mode never touches; the only serial-mode code delta is one
  bitwise-OR per `set_tree_root`, never read in serial mode).
- **Kept experimental behind the flag.** The prototype is *sound* (not "half-built unsound
  machinery" to delete): it passes conservation, crash, WAL, and page-accounting. Keeping
  it — like the falsified-but-retained `MPEDB_NO_BATCH_ROUTING` locality sort
  (DESIGN-MPEE-OPT.md §5) — preserves the reproducible A/B and documents the negative in
  code. `config.rs` documents it as EXPERIMENTAL / measured-slower; the CLI usage flags it.
- **Would it ever win?** Only where the parallelizable slice is large relative to the COW
  write *and* the flush is not the bottleneck: a higher core count (≥8) with `durability =
  none`, wide tables with expensive CHECK expressions or many parallel disjoint-key
  writers, and a batched-optimistic-commit design that reunites with the intent ring so
  the group flush is not forfeited. None of those hold on this 2-core host, and the COW
  write staying serial caps the upside at ~1.28× before overhead regardless.

## 7. Files changed

- `crates/mpedb-types/src/config.rs` — `Concurrency { Serial, Optimistic }` enum, TOML
  parse, `DbOptions::concurrency` (exported from `lib.rs`).
- `crates/mpedb-types/src/error.rs` — `Error::WriteConflict` (retryable).
- `crates/mpedb-core/src/shm.rs` — committed-footprint ring in lock-page free space
  (`opt_record`/`opt_conflict`); no geometry or `FORMAT_VERSION` change.
- `crates/mpedb-core/src/engine.rs` — `Engine::concurrency()`, `has_secondary_index`,
  `validate_row_public`, `col_types`; `WriteTxn` write-footprint tracking
  (`written_tables`), `optimistic_validate` + blind `optimistic_insert/upsert/delete` +
  `set_commit_point`; footprint recording in `commit_with` (optimistic only, before the
  flip). Plus an `#[ignore]` `decompose_write_phases` measurement (the §2 ceiling).
- `crates/mpedb/src/ring_exec.rs` — the optimistic routing: `ring_enabled` bypass,
  `optimistic_eligible`, `optimistic_prep`, `optimistic_execute` (release/prep/validate/
  blind-apply/retry/fallback), `MPEDB_OPT_STATS` counters.
- `crates/mpedb-cli` — `stress`/`crash` accept `--concurrency serial|optimistic`; new
  `incr` autocommit conservation stress mode; config writer records the mode.
