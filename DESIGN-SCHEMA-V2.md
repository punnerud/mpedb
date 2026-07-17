# DESIGN-SCHEMA-V2 — stable table ids + explicit indexes (canonical-bytes v2)

**Status: design draft for review.** This is #47 *stage 0* and the format
foundation #55 rides. It is a **one-time format flag day** for the schema's
canonical bytes (`CAT_SCHEMA_KEY` + `M_SCHEMA_HASH`): wire-format work, so per
the project's own calibration it gets a full adversarial review before any code
lands. DESIGN-DDL.md §2 depends on this and is blocked without it.

## 0. The blocking gap, precisely

Today `table_id` **is the table's sort position**: `Schema::new` sorts tables
by name, `table_id(name)` binary-searches, `table(id)` indexes the vec, and a
schema test states the property by name — *"a table id is its sort position, so
adding a table renumbers"*. Everything downstream keys on that number:

- catalog tree roots: `[0x01, table_id, index_no]` → page id,
- every `CompiledPlan` footprint and access path,
- `CheckPrograms` (positional per `schema.tables`),
- the executor's `TxnCtx` calls.

`CREATE TABLE mango` on a schema holding `apple, zebra` would renumber `zebra`
1→2 and silently point every plan and tree-root lookup for `zebra` at the wrong
tree. **DDL is impossible while id = position.** Likewise, index numbering is
*derived twice* from column flags (`engine::secondary_index_columns` /
`mpedb_sql::secondary_indexes`, kept in agreement by a CLAUDE.md invariant),
and a composite index has no place to exist in that derivation — #55 is
format-blocked on the same bytes.

## 1. Decisions (the design in eight lines)

1. `TableDef` gains **`id: u32` — stable, assigned once, never renumbered,
   never reused**. `Schema` gains `next_table_id: u32`, serialized, monotone.
2. `TableDef` gains **`indexes: Vec<IndexDef>`** with
   `IndexDef { columns: Vec<u16>, unique: bool }`; `index_no = 1 + position`
   (0 = PK tree). One source of truth; the duplicated flag-derivation pair is
   deleted.
3. **Seeding assigns ids in name-sorted order 0..n** — deterministic under
   TOML reordering, so the config path keeps its order-independent hash.
4. **Migration of a v1 file assigns `id = old sort position`** and derives
   `indexes` from the column flags in declaration order — exactly today's
   numbering. Tree-root keys `[0x01, id, index_no]` are therefore **unchanged**
   and the migration is schema-bytes-only: one COW commit, no data touched.
5. **Index key encoding is already composite-ready**: today's single-column
   keys are `encode_key([v])` (unique: → pk) and `encode_key([v, pk…])`
   (non-unique). Composite is the same construction with k > 1. k = 1 bytes
   are identical to today's — **existing index trees stay valid byte-for-byte**.
6. In-memory, `Schema.tables` is **sorted by id** (= creation order). For a
   seeded or migrated file this equals today's name-sorted order, so
   `position == id` holds until the first DROP (stage 4's problem, see §6).
   `table(id)` binary-searches by the id field; `table_id(name)` does a linear
   scan (≤ 64 tables).
7. On the wire, the per-column `unique`/`indexed` flag bits are **written as
   zero and ignored on decode**; the in-memory `ColumnDef.unique/indexed`
   convenience flags are *reconstructed from the index list* in one place.
   No dual truth in the bytes.
8. `PLAN_FORMAT` does not change. Plans embed the schema hash; every
   pre-migration plan (registry and detached/SDK) refuses decode against the
   new hash and is re-prepared lazily. That is the whole flag-day cost.

## 2. Canonical bytes v2

```
u8   version = 2
u32  next_table_id
u32  ntables
per table (in id order):
  u32  id
  str  name                     (u32 len ‖ bytes)
  u16  ncols
  per column:
    str  name
    u8   type tag
    u8   flags                  (bit0 = nullable; bits 1–7 written 0, ignored)
    u8   default tag (0/1/2)    (1 ‖ value, 2 = Now)
    u8   check tag (0/1)        (1 ‖ str)
  u16  npk    ‖ u16 × npk       (column ordinals)
  u16  nindexes
  per index (in index_no order, i.e. 1..):
    u8   unique
    u16  ncols  ‖ u16 × ncols   (column ordinals, significant order)
```

Decode is fully re-validating (the project's rule: corrupt input yields
`Error::Corrupt`, never a panic, truncation-tested at every offset):

- ids strictly ascending within the file, every `id < next_table_id`,
- table names non-empty, unique (case-sensitive equality, as today),
- column ordinals in range for pk and every index; no duplicate ordinal
  *within* one index; no two indexes with the identical (columns, unique)
  shape; `nindexes ≤ MAX_INDEXES` (new bound, 32),
- a single-column index on a column that IS the whole single-column PK is
  refused (it duplicates index 0), matching today's derivation rule,
- trailing bytes refused; `version = 1` accepted **only** by the migration
  reader (`from_canonical_bytes_v1`), which the attach path calls explicitly —
  `from_canonical_bytes` proper rejects it so no other caller can silently
  accept old bytes.

`Schema::hash()` = blake3 over these bytes, as today.

## 3. Who assigns ids

- **Config seed** (creating a brand-new file): sort the declared tables by
  name, assign 0..n in that order, `next_table_id = n`. Deterministic under
  `[[table]]` reordering → the existing property "config table order does not
  affect the hash" survives, now by *deterministic assignment* instead of
  sort-at-hash.
- **v1 migration**: `id = position in the v1 (name-sorted) bytes`,
  `next_table_id = ntables`. Identical to what the config seed would have
  produced for the same schema — so a config's v2 hash matches a migrated
  file's v2 hash iff they matched under v1. The config-drift check is
  order-preserved across the flag day.
- **CREATE TABLE** (stage 2, later): `id = next_table_id++`, appended at the
  end of the id-sorted vec. No renumbering, by construction.

Index numbers: seed/migration derives `indexes` from column flags in column
declaration order (today's numbering exactly), then appends explicit
`[[table.index]]` config entries in declaration order. `index_no` is the
position in that list + 1, stable for the life of the table. New indexes
(future `CREATE INDEX`) append; DROP INDEX (future) leaves a hole — the list
stores `columns` per entry, so a tombstoned index is representable when that
day comes (not in this stage).

## 4. Config surface for #55

```toml
[[table]]
name = "orders"
primary_key = ["id"]
  [[table.column]]
  name = "tenant"
  type = "int64"
  [[table.column]]
  name = "created"
  type = "timestamp"
  # single-column sugar, unchanged:
  #   unique = true / indexed = true on a column
  [[table.index]]
  columns = ["tenant", "created"]   # composite, declaration order significant
  unique = false
```

Validation: every named column exists; the same refusals as §2's decode.
The sugar flags and `[[table.index]]` compose; exact-duplicate shapes refuse.

## 5. Migration at attach (the flag day)

In `Database::open`, after shm attach and before `verify_config`'s schema-hash
comparison, under the **writer lock**:

1. Read `CAT_SCHEMA_KEY`. Version 2 → nothing to do.
2. Version 1 → build v2 per §3, write the new bytes to `CAT_SCHEMA_KEY`,
   recompute blake3, stage it into the meta — **one ordinary COW commit**
   (schema bytes and meta hash flip together; SIGKILL before the flip leaves a
   valid v1 file that simply re-migrates next attach; after, a valid v2 file).
3. `verify_config` then compares against the (possibly new) hash. A config
   that matched the v1 file matches the v2 file (§3).

Mixed-version fleets: an OLD binary attaching a v2-migrated file reads version
2, and its `from_canonical_bytes` rejects unknown versions → clean
`Corrupt("unknown schema version 2")` refusal, no misreads. A NEW binary
migrates on first attach even while old-binary processes are attached — that
would strand them mid-flight, so migration must be gated the same way any
writer-lock operation is: it happens under the lock, and *already-attached*
old readers keep working on their pinned pre-migration snapshots until they
re-pin (then their next schema load hits version 2 and refuses). This is the
documented, one-way upgrade cost: **upgrade all attached processes together**.
Detached plan blobs (SDK) re-prepare on first use via the existing
hash-mismatch refusal.

## 6. `position == id` — the latent-coupling audit

Until DROP TABLE exists, ids are dense 0..n and `tables` is id-sorted, so
`position == id` **provably holds** (seed and migration both assign dense
ascending ids; CREATE appends). Rather than pretend to audit every
`tables[id as usize]` now, stage 0 makes the invariant *explicit and checked*:

- `Schema::table(id)` binary-searches by the id field (correct with gaps),
- a `debug_assert!(t.id == pos)` in `Schema::validate` while dense — with a
  comment naming stage 4 (DROP) as the PR that deletes the assert and audits
  every positional site (`CheckPrograms`, exec ctx vectors, the shard map,
  gather buffers) against gapped ids.

This is honest: the format supports gaps NOW (decode accepts non-dense
ascending ids so stage-4 files remain readable by stage-0 binaries), while the
in-memory code declares its current dense assumption instead of hiding it.

## 7. What #55 builds on top (same window, separate PR)

- Planner: composite `IndexPoint` (full-width equality; unique → get, else
  prefix scan) and `IndexRange` (equality on a prefix of the index columns +
  one range/order column next — the existing `range_bounds` prefix-ceiling
  machinery already produces exactly these bounds for composite PKs).
- Engine: index maintenance loops over `IndexDef.columns` building
  `encode_key(&[v1..vk])` / `encode_key(&[v1..vk, pk…])` — the k = 1 case is
  bit-identical to today, covered by the existing suites.
- `ON CONFLICT` targets: "a UNIQUE column" becomes "a unique index whose
  column set equals the target set" — single-column stays as today, composite
  targets arrive for free.
- Differential vs sqlite3 (same CREATE INDEX statements) + SLT + the corpus.

## 8. What the adversarial review must break

1. **Hostile v2 bytes**: duplicate/descending ids, `id ≥ next_table_id`,
   ordinal out of range, duplicate index shapes, `nindexes` overflow,
   truncation at every offset. Decode must yield `Corrupt`, never panic,
   never a half-valid schema.
2. **Migration atomicity**: SIGKILL at every point — is there any state where
   `CAT_SCHEMA_KEY` and `M_SCHEMA_HASH` disagree? (Must be impossible: one
   COW commit.) Is re-migration after a pre-flip crash truly idempotent?
3. **Hash-collision-by-construction**: can a v1 schema and a DIFFERENT v2
   schema hash equal? (Version byte differs → no. Confirm the version byte is
   inside the hashed bytes.)
4. **Determinism**: two configs differing only in `[[table]]` order → same v2
   bytes? Same for `[[table.index]]` order? (Table order: yes by name-sort
   assignment. Index order: NO — index order is significant and documented;
   confirm the docs say so.)
5. **The k = 1 encoding claim**: is `encode_key([v])` for today's index paths
   byte-identical to the composite construction at k = 1, for every type incl.
   NULL, so no index tree needs rebuilding? Prove with a test that opens a
   pre-migration file and probes its existing index trees.
6. **Flag reconstruction**: in-memory `unique`/`indexed` rebuilt from the
   index list — is any consumer sensitive to the difference between "declared
   via flag" and "declared via [[table.index]] with one column"? (`ON CONFLICT`
   target validation is the risk site.)
7. **`position == id` escapes**: any site indexing `schema.tables` by a
   PLAN-carried table id rather than through `table(id)`? (Enumerate; the
   debug assert only catches misuse in tests that construct gapped schemas.)
8. **Old binary, new file / new binary, old file**, read-only attach,
   `SqliteAttach`/overlay programmatic schemas (they build `Schema` directly —
   they must get ids assigned by `Schema::new` and NEVER hit the catalog
   migration path).

## 9. Staging

- **S0a — mpedb-types**: `id` + `indexes` + `next_table_id`, v2
  encode/decode + v1 migration reader, validation, truncation tests,
  determinism tests. `Schema::new` assigns ids name-sorted and derives
  indexes from flags (so every existing programmatic constructor keeps
  working unchanged).
- **S0b — engine**: attach migration (§5), `secondary_index_columns` → reads
  `TableDef.indexes`, index maintenance generalized to k columns (k = 1
  regression: byte-identical keys, proven against a pre-migration fixture).
- **S0c — sql**: `secondary_indexes` → reads `TableDef.indexes`; ON CONFLICT
  unique-target via index shapes; footprint unchanged semantics.
- **S0d — config**: `[[table.index]]` parsing + validation + GUIDE/README.
- **#55 — planner**: composite access paths + differential + corpus (own PR).

Each stage lands green on the full workspace before the next; the flag day is
S0b's migration commit, and until S0b lands nothing observable changes for
existing files.
