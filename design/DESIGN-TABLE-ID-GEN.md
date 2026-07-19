# DESIGN-TABLE-ID-GEN — generation-safe table-id reclamation

**Status: design, NOT BUILT — and no longer the pressing problem (2026-07-19).**
[DESIGN-TABLE-CAP.md](DESIGN-TABLE-CAP.md) took the other branch: instead of making the cap
apply to LIVE tables via generation-tagged reuse, it made the footprint (and the CDC capture
config) SPARSE, so the id space stopped being an integer width at all. `MAX_TABLES` is 4096
and is now a cost knob, not a representation limit. Everything below stays correct and stays
the right answer if lifetime-create exhaustion ever becomes real; it is simply no longer
urgent, and its premise ("`MAX_TABLES = 64` comes from the bitmap being a `u64`") is
historical. Original status line follows.

**Status: design (2026-07-18). Supersedes #81's "just widen the bitmap": lift the 64 *lifetime*-create
cap by making the cap apply to LIVE tables only, via safe id reclamation. This is the fix
[DESIGN-DROP-TABLE.md](DESIGN-DROP-TABLE.md) §0 deferred — it is the deepest schema/plan/CDC/
concurrency change, and rides the project's wire/commit-path adversarial-review discipline.**

## 0. Why widening is a band-aid, and what the right fix is

`MAX_TABLES = 64` comes from the footprint/CDC bitmap being a `u64` (one bit per dense table id), and
a dropped table tombstones its id in place (no reuse) — so it caps **lifetime** creates. Widening the
bitmap (`u64 → [u64; N]`) raises the ceiling but keeps a lifetime cap. The right fix, per Morten:
**keep the live tables, archive the dead, grow dynamically** — make the cap = LIVE tables (small,
fits a narrow bitmap) and let lifetime creates be **unbounded** by reclaiming dead id-slots.

## 1. Why reuse was forbidden — and the dissolvent

DROP-TABLE §0 forbade reuse because, under reuse, DROP would have to **synchronously purge every
persisted `table_id`** (CDC freeze cursors, `ParkRecord.table_id` + scope, any future subsystem that
persists an id) — or a stale reference would bind to the *new* table at the reused id and read the
wrong data. One missed hook, or a SIGKILL mid-purge, = silent cross-table corruption. No-reuse traded
that permanent obligation for the bounded 64-cap.

**The dissolvent is generation-tagging + lazy invalidation.** Give each id-slot a **generation**
counter. Every persisted or compiled reference carries `(slot, generation)`. Reclaiming a slot
**bumps its generation**. A stale reference, at *use* time, compares its generation against the
slot's current generation:

- match → the reference is still valid;
- mismatch → the table at this slot was dropped (and maybe re-created as something else) → a clean
  "table gone / plan invalidated" error, **never** a read of the wrong table.

So there is **no synchronous purge**: invalidation is *lazy*, an O(1) check at each id-use site. The
corruption obligation dissolves into a generation compare. **mpedb already uses exactly this pattern**
— the reader table's packed `{pid,seq}` generation words, and the page-reuse epoch (pages freed by
commit T are reusable when T ≤ oldest-pinned). Table ids get the same treatment.

## 2. The model: slots with state + generation

- The id space is **slots** `0..N`. Each slot: `{ state: Live(TableDef) | Free, generation: u64 }`.
- The **footprint/CDC bitmap indexes slots**; because dead slots are *reclaimed* (§4), the bitmap only
  ever needs to cover LIVE + not-yet-reclaimable slots — bounded by the live working set, so a `u64`
  (64 LIVE) is plenty for real apps (widen to `[u64; N]` only if you need >64 tables live *at once*,
  now an orthogonal, optional knob).
- **DROP**: free the slot (data pages already returned by the §1 free-fixpoint), bump its generation.
- **CREATE**: take the lowest reclaimable free slot (or grow the slot vector), stamp the current
  generation. `position == id` is replaced by `slot == id` with a generation; dense-live is the norm,
  gaps are transient (a freed-but-pinned slot).

## 3. Threading the generation (the work — §0's 74 id-touching sites)

- **Catalog / canonical bytes**: `TableDef` carries its generation; canonical schema bytes include it,
  so `schema_hash` accounts for it (a re-created table at the same slot has a different hash → the
  DDL-staleness signal `schema_gen` in the flipping meta already fires).
- **Plans / footprint**: a compiled plan references `(slot, generation)`; `validate`/execute checks
  the live slot's generation, and on mismatch returns `PlanInvalidated` — the *existing* re-prepare
  path (this is why plans are content-hashed and re-validating). The footprint is valid only for its
  generation.
- **CDC / triggers**: capture and freeze records carry `(slot, generation)`; on replay/resume a
  generation mismatch means the table was dropped+recreated → skip/handle, never apply to the wrong
  table. This is the subsystem §0 called the hazard — generation makes it safe *without* a synchronous
  purge.
- **Cross-process / MVCC**: a reader's snapshot sees a consistent `slot → (table, generation)` map for
  its snapshot; a slot freed *after* the snapshot is still visible to it (MVCC), and is not reclaimed
  until no snapshot pins it (§4).

## 4. Reclamation timing — the load-bearing invariant

A freed slot is **reusable by a new CREATE only when no live snapshot could still reference the old
table** — the *same* oldest-pinned-bound the page freelist already enforces (freed by commit T ⇒
reusable when T ≤ oldest-pinned; NOT strict `<`, per the CLAUDE.md off-by-one warning). So slot
reclamation rides the existing pin-bound machinery: a dropped slot is *free* immediately (new
references can't be minted against a dead slot), but *reusable* only once the drop commit ≤
oldest-pinned. Until then it is free-but-reserved (still effectively tombstoned for the pinned
readers that predate the drop). This ordering is the crux and gets full adversarial review.

## 5. Archiving vs. reclaiming

"Archive the stale" falls out for free: a freed slot's **bumped generation IS the archive marker** —
the old identity is retired forever (no reference with the old generation can ever match again), while
the slot itself is reusable. No separate archive store is needed for correctness; an optional
retired-id audit log `(slot, max-generation)` can aid debugging but is not load-bearing.

## 6. Cost, risk, staging

This is the deepest change in the engine — schema + plan + footprint + CDC + concurrency + cross-
process — so §0's four-region audit (74 id-touching sites, 58 breaking) is the map, and it rides the
wire/commit-path adversarial-review bar (the one the 37-finding concurrency review set). Staging, each
step independently testable:

1. Add `generation` to slots + `TableDef` + canonical bytes (schema-hash accounts for it).
2. Plans/footprint carry + check `(slot, generation)` → `PlanInvalidated` on mismatch (reuse the
   existing re-prepare path).
3. CDC/trigger + park records carry generation; replay/resume checks it.
4. Slot reclamation gated on the oldest-pinned bound (§4), reusing the freelist's pin machinery.
5. Drop the 64 *lifetime* cap; keep the LIVE cap at the bitmap width (u64 = 64 live, or widen).

The `mirror-collide` / SIGKILL fuzz is extended to a drop-recreate-churn workload proving **no stale
reference (plan, CDC cursor, cross-process reader) ever reads a reclaimed slot's new table** — the
one property that must hold. Widening (#81 v1) becomes optional, only for >64 concurrently-live tables.
