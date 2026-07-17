# DESIGN-DROP-TABLE — #47 stage 4

**Status: reviewed design draft.** DROP TABLE is the stage the dense-id
enforcement of DESIGN-SCHEMA-V2 §1.2 was deferring: removing a table produces
*gapped* ids, and every site assuming `position == id` or holding a *persisted*
id becomes a hazard. An exhaustive four-region audit (engine, sql/planner,
facade/overlay, mirror/cli/py) found **74 id-touching sites, 58 breaking, 16
verified safe**; this design folds it. The core decision (below) collapses that
58-site surface to a small, bounded set. The one persistence change (a
monotonic id high-water) rides the project's wire/commit-path adversarial-review
rule before build.

## 0. The decision that gates everything

Two orthogonal knobs decide the blast radius:

- **Knob A — id reuse:** does a later CREATE reclaim a dropped id, or always
  mint a fresh one?
- **Knob B — vec shape:** does `schema.tables` *compact* (remove the dropped
  slot, so position ≠ id) or *tombstone in place* (keep a dead placeholder, so
  `position == id` survives)?

**Decision: NO-REUSE (monotonic id) + TOMBSTONE-IN-PLACE.**

Why, precisely:

1. **No-reuse converts a permanent distributed corruption obligation into a
   single bounded limit.** Under *reuse*, correctness would require DROP to
   purge — completely, crash-atomically, and forever, including from every
   *future* subsystem that persists a `table_id` — the CDC capture/freeze
   bitmaps + dirty entries (`cdc.rs`), the mirror's park/skip/map/imp/cfg/
   cursor records and `ParkRecord.table_id` + `scope` (`state.rs`,
   `sqlite_track.rs`), and policy `require_policy` (`policy_store.rs`). A
   single missed hook, or a SIGKILL mid-purge before the id is reused, is
   **silent cross-table data corruption** — the exact failure class mpedb
   exists to prevent. Under no-reuse a dropped id is *never* re-bound, so every
   one of those persisted records becomes a **harmless orphan leak** and needs
   ZERO correctness code.

2. **Reuse doesn't even buy the wire savings it appears to.** It still needs
   the mint rework (the current `def.id = tables.len()` *collides* under any
   gap — `{0,1,2,4}` → len 4 → mints the taken id 4), and its purge must be
   crash-atomic with the schema commit — moving the heavy review from
   wire-format to commit-path (both trigger it) *and* adding a permanent
   maintenance tax on all future code.

3. **No-reuse's cost is bounded, detectable, recoverable.** One persisted
   monotonic counter; exhaustion only after **64 lifetime creates** (the
   existing ≤ 56 *live*-count guard means non-churny workloads never approach
   it); the wall is an explicit `Error::Unsupported` at mint, never
   corruption. The escape hatch is an offline "compact table ids" maintenance
   op (rewrite to dense ids + rewrite every persisted `table_id` record + bump
   schema-gen, run exclusive so there is no aliasing window) — a single batch,
   strictly easier to get right than online per-DROP purge. **Deferred past
   stage 4.**

**Tombstone-in-place** (Knob B) is what keeps the ~35 downstream
`bundle.X[table_id as usize]` sites correct-by-inheritance: the decoder places
each table at `tables[id]` and pads holes with a dead sentinel, so `position ==
id` holds under gaps, and the two SchemaBundle constructors emit a dead entry
per hole. The fix concentrates in **two constructors + one resolver + the mint
+ validate + decoder placement** instead of rewriting 35 index sites to
search-by-id — and it removes the compaction-off-by-one-to-a-*wrong-live-table*
risk entirely. (The dead slot doubles as the id high-water — see §1, no
separate counter.)

## 1. The id high-water IS `ntables` — dead slots are materialized

The first draft's separate `sys/next_table_id` counter (and the HIGH review
finding about deriving vs persisting it) **dissolves** under the right
tombstone representation: keep the dropped table's slot in `schema.tables` as a
MATERIALIZED dead entry, encoded in the canonical bytes. Then:

- `schema.tables` never shrinks; ids stay DENSE `0..len-1` (a dead slot fills
  the dropped id's position). **`position == id` remains exactly true** — no
  gapped vec, no search-by-id, and the ~35 downstream `[id as usize]` sites are
  correct-by-inheritance with a dead entry at the dropped id.
- **`next_table_id` = `tables.len()`** (live + dead). It is persisted (as the
  `ntables` count in the canonical bytes), monotonic (dead slots are never
  removed), and **can never regress** — dropping the highest id keeps its dead
  slot, so `len()` is unchanged and the id is never re-minted. There is no
  separate counter, no `sys/next_table_id` record, and no `max(id)+1`
  derivation to get wrong. The HIGH finding's whole premise (an absent /
  derivable counter) is gone.
- CREATE: `def.id = tables.len()`; append a live slot. **Fail closed** at the
  ceiling `tables.len() >= MAX_TABLES (64)` ⇒ `Error::Unsupported("table-id
  space exhausted; rebuild required")` — turns the footprint/CDC `1u64 << id`
  cap into an explicit limit. (`with_added_table` is nearly unchanged: it
  already does `def.id = self.tables.len()`; it only gains the ceiling guard
  and must count against the ≤ 56 LIVE-count guard, not the total incl. dead.)

Cost: a dead slot costs a few bytes in the canonical schema and one entry in
each SchemaBundle Vec — negligible over ≤ 64 lifetime creates. The schema HASH
includes dead slots (they are part of the schema's identity), so a DROP changes
the hash and invalidates stale plans, which is correct. Config-seed vs file
hash equivalence is only needed at CREATION (no dead slots yet), where it
holds; after any DDL the live schema legitimately diverges from the config and
the config-drift check compares only against the frozen SEED hash (stage 1).
`regenerate` re-densifies (rebuilds without dead slots), resetting the ceiling.

## 2. DROP TABLE mechanics (atomic UNLINK commit + bounded reclamation)

The review (MEDIUM) killed the naive "free every page in one commit": a large
table's page-free would blow the freelist's per-commit `u16` chunk index
(wraps past ~7.86M freed pages ≈ a 32 GiB table) and can hit DbFull mid-DROP
(the 1 GiB-delete precedent already writes ~2185 freelist chunks). So DROP
splits into **one atomic UNLINK commit** (which makes the table gone and its
trees unreachable) plus **bounded page reclamation** (unreachable pages are
safe to reclaim lazily — leaking a page until reclaimed is never corruption).

`DROP TABLE <name>` (facade DDL route, like CREATE — never a plan):

**The UNLINK commit (one COW commit, atomic — the table is gone after it):**
1. Resolve the id via `schema.table_id(name)` (linear scan; refuse unknown).
   Refuse if the table is in an active mirror's scope unless the mirror will
   propagate the drop (see §3.5 — writes a pending-op instead of refusing).
2. **Delete the catalog tree-root entries** `cat_tree_key(id, 0..=n_indexes)`.
   This unlinks the table's trees from the reachable set in ONE step — no
   page walk, O(index count) catalog deletes. The now-unreachable data/index
   pages are recorded for reclamation (their roots go to a
   `sys/drop-reclaim/<id>` worklist record, or the DROP simply frees them
   later via a bounded scan; either way they are NOT freed in this commit).
   A reader pinned on the pre-DROP `catalog_root` still reaches the old trees
   through ITS snapshot and its pages are not reclaimed until the oldest-pinned
   bound passes it (#37) — DROP adds nothing to that bound logic.
3. **Tombstone the schema**: place a dead sentinel at the id's slot (see the
   representation below), re-canonicalize, write to `CAT_SCHEMA_KEY`.
4. (No counter to persist — `ntables` in the tombstoned schema bytes IS the
   high-water; see §1.)
5. **Purge the id's catalog-resident soft state**: CDC `set_captured(id,false)`
   / `set_blocked(id,false)` + delete the id's dirty entries (`cdc.rs`), so the
   bitmaps stop marking a dead id. (Mirror park/map records stay orphan-leaked;
   no-reuse makes them inert.)
6. Bump `schema_gen`. Commit. The table is now gone for every process; compiled
   plans referencing it die on the schema-hash check.

**Bounded reclamation (follow-up commits, like the overlay's bounded
truncate):** walk the unlinked trees and free their pages in per-commit batches
(each batch its own commit, so freeing keeps pace with the COW allocation that
freeing itself needs — the exact discipline `truncate_deltas` uses). Idempotent
and crash-safe: the `sys/drop-reclaim/<id>` worklist survives a crash, so a
reopen resumes reclamation; a partially-reclaimed tree just has fewer pages to
free next round. Space returns eventually, never in one unbounded commit.

**The tombstone sentinel** is a MATERIALIZED entry in `schema.tables` — every
slot `0..ntables` is encoded, so `position == id` is trivially preserved (the
review's soundness proof for the `[id]`-aligned SchemaBundle Vecs). Add
**`dead: bool` to `TableDef`**, bumping the canonical bytes to **version 3**
(an old v2 file then refuses cleanly at the version check with "regenerate",
rather than misreading a flag byte at the wrong offset). A dead slot encodes
minimally: `id`, `dead = 1`, empty name, 0 columns, 0 pk, 0 indexes. `validate`
SKIPS the shape rules (1..=MAX_COLUMNS, non-empty pk, etc.) for a dead slot but
still enforces `position == id`, `id < MAX_TABLES`, and that a dead slot IS
empty (0 cols/pk/indexes). Its name is empty, so `table_id(name)` never matches
it and the dropped name is free to re-CREATE (at a new id, a new live slot).
The SchemaBundle constructors give a dead slot empty `checks`/`sec_indexes`/
`sec_unique`/`col_types` entries — never accessed, since no live plan or write
references a dropped id.

## 3. The fix, grouped by the audit (ordered by risk)

**GROUP 0 — the linchpin (mpedb-types / engine constructors):**
- `Schema::with_added_table` (`schema.rs:162`): mint from `next_table_id` + the
  ceiling guard.
- `Schema::validate` (`schema.rs:172`): permit gaps; keep the ≤ 56 live-count
  guard; enforce ids unique and `< MAX_TABLES` (drop the dense `position==id`
  check, replace with placement-by-id below).
- Decoder placement (`schema.rs:457`): place each decoded table at `tables[id]`,
  padding holes with a dead sentinel — restores the in-memory `position == id`
  invariant under gaps.
- `Schema::table` (`schema.rs:386`): keep O(1) `get(id)`, return `None`/dead for
  a tombstoned slot.
- `SchemaBundle::new` (`engine/mod.rs:329`) + facade `CheckPrograms`
  (`lib.rs:297`): emit a dead/empty entry per tombstoned slot so the parallel
  `checks`/`sec_indexes`/`sec_unique`/`col_types` Vecs stay `[id]`-aligned.
- `reload_schema_from_catalog` (`engine/mod.rs:463`): rebuild the caches
  id-keyed, NOT a positional `checks.resize(count)` (a resize truncates the tail
  on an interior drop).
- `bootstrap_catalog` (`engine/mod.rs:490`): use `table.id`, not `enumerate()`.

**GROUP 1 — position-as-id mints (must fix regardless; mechanical `t.id` swap):**
- Facade: `lib.rs:322` (`require_policy` — resolve against the engine's gapped
  schema via `table_id(name)`, not config position), `lib.rs:1226`
  (`insert_streaming` — use `schema().table_id(table)`).
- Mirror: `import.rs:131,433,476`, `pg_import.rs:119,149`, `export.rs:94`,
  `pg_export.rs:242,244`, `reconcile.rs:64`, `regenerate.rs:102`,
  `sqlite_adapter.rs:53`, `pg_adapter.rs:104` — replace `enumerate()`/`position`
  mints with `t.id`, skip dead slots.
- CLI: `dump.rs:34`, `repl.rs:151` — `t.id` (render-only).

**GROUP 2 — persisted-id sites (NO correctness change under no-reuse; note as
inert orphan leaks + optional GC):** CDC dirty (`cdc.rs:126`), mirror
park/skip/map/imp/cfg/cursor (`state.rs:49-317`, `sqlite_track.rs:163`), policy
(`policy_store.rs:229`). Plus the cheap CDC-bit purge in DROP §2.4.

**GROUP 3 — bitmap cap (leave as tripwires):** footprint `1u64 << id`
(`footprint.rs`) and CDC `< 64` (`cdc.rs`) — Group 0's mint ceiling guarantees
no live id ≥ 64, so these never fire for a live table. Do NOT weaken the
`id >= 64` corruption checks (they still catch forged plans/records).

Downstream `[id as usize]` consumers (engine read/write row codec + index
maintenance, planner `table(id)`, exec/ring/stream) become correct-by-
inheritance once Group 0's constructors + `Schema::table` are gap-aware. The
sharpest to re-verify: `has_secondary_index` (a wrong answer enables the
optimistic blind-apply that SKIPS index maintenance — DESIGN §7.3) and
`planner/mod.rs:396` (`table(id).expect` at compile time, ahead of the
schema-hash gate).

## 3.5. Interaction with the bidirectional mirror

The mirror (`mpedb-mirror`, DESIGN-MIRROR) sync-scopes a FROZEN set of table
ids: `MirrorConfig.scope` and the per-table CDC capture bits are set once, at
`import` (`import.rs:452` is the ONLY `set_captured` call site; nothing extends
scope afterward). This shapes exactly what DDL does and does not preserve:

- **Existing mirrored tables stay correct through any DDL.** Stable ids (stage
  0) + no-reuse (§0) guarantee that a mirrored table keeps its id, scope entry,
  capture bit, and provenance (`map/<id>`) record unchanged; no CREATE
  renumbers it and no DROP ever re-binds its id. This is a primary reason
  stable ids and no-reuse were chosen — the mirror of the *unchanged* tables is
  never disturbed by a schema change to *other* tables.

- **A live-`CREATE`d table is NOT auto-mirrored.** Its id is not in scope and
  its capture bit is unset, so its writes are never captured, never pushed to
  the source, and have no provenance mapping. Extending an active mirror to a
  new table is out of scope for stage 4; DESIGN-MIRROR's answer to a schema
  change is `mirror regenerate` (line 695: "schema drift (no ALTER) →
  regenerate"), and that remains the path. A future `mirror add-table` could
  extend scope+capture+provenance incrementally, but it is a mirror feature,
  not a DROP-TABLE concern.

- **`DROP` of a MIRRORED table PROPAGATES to the source** (best-bidirectional-
  sync direction, chosen 2026-07-17). `push`/`apply` already skip a scope id
  whose `schema.table(id)` is absent (`resolve.rs:97` `else { continue }`), so
  a tombstoned mirrored table never corrupts — but silently *not* syncing a
  drop is a divergence. Instead: the DROP commit records a **pending schema op**
  (a `sys/ddl/<seq>` record: `{drop, table_name}`) alongside the tombstone, and
  the next `push` drains it — dropping the table on the source (the export
  layer already renders per-dialect DDL) and removing it from `scope`. Until
  the push confirms, the local drop is durable but the source still has the
  table; that is the same eventual-consistency contract the data path already
  has. A `push_only`/`pull_only` mode or a genuinely un-droppable source
  (permissions) parks the op and surfaces it, exactly like a rejected row.
  This is one arm of the bidirectional-DDL design (DESIGN-MIRROR-DDL.md); DROP
  stage 4 only needs to WRITE the pending-op record in its commit — the drain
  is mirror-side.

1. **Crash atomicity of DROP**: page-free + catalog-tree-delete + schema
   rewrite + CDC-bit purge + gen bump — all in one COW commit? SIGKILL at every
   point leaves either the whole old table or none. Verify the freelist commit
   fixpoint (§4.5) tolerates a large multi-tree free in one commit (a big table
   may write thousands of freelist chunks — bounded but heavy; cross-check the
   1 GiB-delete precedent).
2. **The high-water = `ntables`**: with dead slots materialized, does
   `tables.len()` never regress across a drop-the-highest + reopen, and does a
   pre-stage-4 (v2) file refuse cleanly at the version-3 check rather than
   misread? (No separate counter to survive — §1.)
3. **Reader pinned across DROP**: proves its pages are held by the oldest-pinned
   bound with DROP adding nothing, and its own (old) bundle still decodes the
   dropped table's rows correctly from its pinned catalog_root.
4. **Tombstone-in-place vs the ≤ 56 live-count guard and the id ceiling**: dead
   slots must count against the ceiling (they hold an id) but not the live
   guard — get the two counts right.
5. **Every Group-0 constructor** produces `[id]`-aligned Vecs under a gap: a
   targeted test constructing a gapped schema and asserting `col_types(4)` etc.
   return table 4's data, not position 3's.

## 5. Staging

- **S4a — mpedb-types**: `next_table_id` (+ sys-record read/write hook exposed
  for the engine), gap-tolerant `validate`, decoder placement-by-id + dead
  sentinel, `with_added_table` rework, gapped-schema unit tests.
- **S4b — engine**: DROP mechanics (page free + catalog delete + CDC purge +
  gen bump in one commit), the Group-0 constructor/reload fixes, the
  counter sys-record, crash tests.
- **S4c — sql/facade**: `DROP TABLE` parser + facade route; Group-1 mint fixes
  (facade + the downstream re-verify).
- **S4d — mirror/cli**: Group-1 mint fixes + the orphan-leak note; optional GC
  deferred.
- Differential vs sqlite3 (`CREATE`/`DROP`/re-`CREATE` at a new id) + the
  multi-process staleness test extended to DROP.
