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
risk entirely.

## 1. The id high-water (the one persistence change → review gate)

`Schema` gains a logical `next_table_id: u32`, **persisted as a catalog
sys-record** (`sys/next_table_id`), NOT in canonical bytes. Rationale for the
sys-record over a canonical-bytes v3 field: the schema HASH must stay a pure
function of table *shapes* (it is the config-drift and plan-invalidation key);
a monotonic counter that changes on every CREATE would pollute the hash and
break the seed-vs-file hash equivalence. A sys-record is ordinary append-only
catalog data — no format-version bump, written in the same COW commit as the
schema bytes.

Rules:
- CREATE: `id = next_table_id; next_table_id += 1`. **Fail closed** at the
  ceiling: `next_table_id >= MAX_TABLES (64)` ⇒ `Error::Unsupported("table-id
  space exhausted; rebuild required")`. This turns the footprint/CDC `1u64 <<
  id` bitmap cap (§4) from silent overflow into an explicit, recoverable limit.
- The counter must **survive reopen**: dropping the highest-id table erases its
  id from the live set, so `max(live)+1` would silently reuse it. The
  persisted counter is the source of truth; `Schema::new` (config seed) sets it
  to `tables.len()`, and attach reads it from the sys-record (absent ⇒ derive
  `max(id)+1` for pre-stage-4 files, which is safe because those never dropped).
- `with_added_table` (today `def.id = tables.len()`) reworks to take the
  counter and tombstone-aware placement.

## 2. DROP TABLE mechanics (one COW commit, under the writer lock)

`DROP TABLE <name>` (facade DDL route, like CREATE — never a plan):
1. Resolve the id via `schema.table_id(name)` (linear scan; refuse unknown).
2. **Free the data + index pages**: for `index_no` in `0..=n_indexes`, walk
   `cat_tree_key(id, index_no)`'s tree and free every page, then delete the
   catalog tree-root entries. Freeing goes through the ordinary freelist —
   reusable only at/below the oldest-pinned bound (#37), so a reader still
   pinned on a pre-DROP snapshot keeps reading the old table from ITS
   `catalog_root` and its pages are not handed out until it releases. DROP adds
   nothing to that bound logic (DESIGN-DDL §5.1 [believed yes]).
3. **Tombstone the schema**: mark the table dead (a `dead: bool` on `TableDef`,
   or replace with a reserved sentinel), re-canonicalize, write to
   `CAT_SCHEMA_KEY`. The id is NOT reclaimed; `next_table_id` is unchanged.
4. **Purge the id's own catalog-resident soft state that IS keyed by id and
   would mislead even under no-reuse** — cheap and worth it: CDC
   `set_captured(id,false)` / `set_blocked(id,false)` and delete the id's
   dirty entries (`cdc.rs`), so the bitmaps stop marking a dead id. (Mirror
   park/map records are left as orphan leaks — no-reuse makes them inert; an
   optional GC can sweep them later.)
5. Bump `schema_gen` (staleness signal). Commit. Other processes reload at
   their next statement; every compiled plan referencing the dropped table
   dies on the schema-hash check.

Canonical bytes encode LIVE tables plus enough to reconstruct the holes: each
table carries its explicit `id` already (v2), so a gap in the id sequence *is*
the tombstone record — the decoder sees ids `0,1,2,4`, places them at those
positions, and fills position 3 with a dead sentinel. No new per-table field is
strictly required if a dead table is simply absent and the decoder pads gaps;
a `dead` marker is only needed if a dropped name must stay reserved (it need
not, under no-reuse — the name is free to re-CREATE at a NEW id).

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

- **`DROP` of a MIRRORED table must not silently diverge.** `push`/`apply`
  already skip a scope id whose `schema.table(id)` is absent
  (`resolve.rs:97` `else { continue }`), so a tombstoned mirrored table does
  not corrupt — but it would **silently stop syncing** while the SOURCE table
  lives on, an undetected divergence. **Rule for stage 4: DROP refuses a table
  that is in an active mirror's scope**, with a message routing the operator to
  `mirror detach`/`regenerate` first (the same "can't drop what something still
  references" shape SQL uses for FK-referenced tables). Removing-from-scope +
  propagating the drop to the source is a heavier bidirectional-DDL feature,
  deferred; refuse-if-mirrored is the safe, honest v1. The check reads the
  mirror `cfg` sys-record's scope, if present, in the DROP commit's txn.

1. **Crash atomicity of DROP**: page-free + catalog-tree-delete + schema
   rewrite + CDC-bit purge + gen bump — all in one COW commit? SIGKILL at every
   point leaves either the whole old table or none. Verify the freelist commit
   fixpoint (§4.5) tolerates a large multi-tree free in one commit (a big table
   may write thousands of freelist chunks — bounded but heavy; cross-check the
   1 GiB-delete precedent).
2. **The persisted counter**: does `next_table_id` survive every crash point,
   and can dropping-the-highest never silently lower it? Is the absent-record
   fallback (`max(id)+1`) safe for pre-stage-4 files?
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
