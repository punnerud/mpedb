# DESIGN-SCHEMA-V2 — stable table ids + explicit indexes (canonical-bytes v2)

**Status: reviewed design, v0.2.** #47 *stage 0* and the format foundation #55
rides. v0.1 went through a three-lens adversarial review (bytes/hashing,
latent couplings, concurrency/upgrade); this revision folds every surviving
finding. The largest change: **v0.1's migration-at-attach was architecturally
impossible** — `M_SCHEMA_HASH` is init-frozen in BOTH meta pages, covered by
each slot's checksum, and validated inside `Shm::open` itself; there is no
"stage it into the meta", and mutating it in place can brick the file (torn
32-byte store ⇒ both checksums invalid ⇒ permanently unopenable). Per the
project's standing rule (no backward-compat burden), the resolution is **no
migration at all**: v2 is the only format, old files refuse loudly
(config path: the existing "schema hash mismatch"; `open_from_file`:
`Corrupt("unknown schema version 1")`), and regenerate/re-import is the
remedy. The frozen-hash mechanism is untouched — the hash merely has a new
preimage for newly created files.

## 0. The blocking gap, precisely

Today `table_id` **is the table's sort position**: `Schema::new` sorts tables
by name, `table_id(name)` binary-searches, `table(id)` indexes the vec, and a
schema test states the property by name — *"a table id is its sort position,
so adding a table renumbers"*. Everything downstream keys on that number:
catalog tree roots `[0x01, table_id, index_no]`, every `CompiledPlan`
footprint, `CheckPrograms`, the executor's `TxnCtx` calls, and the
footprint/CDC **bitmaps that use the id as a bit position** (`1u64 << id`).
`CREATE TABLE mango` on `apple, zebra` would renumber `zebra` 1→2 and point
every plan and tree lookup at the wrong tree. Likewise index numbering is
derived twice from column flags (`engine::secondary_index_columns` /
`mpedb_sql::secondary_indexes`), and a composite index has no place to exist
in that derivation — #55 is format-blocked on the same bytes.

## 1. Decisions

1. `TableDef` gains **`id: u32` — explicit in the bytes, stable for the
   table's life**. Allocation is **lowest-free** (not monotone-forever):
   plans cannot outlive a schema change (they embed the schema hash), and
   DROP deletes its catalog entries in the same commit, so reuse is safe —
   and it keeps every id `< MAX_TABLES` forever, which the footprint/CDC
   bitmap layer requires (`table_bit` refuses id ≥ 64; CDC would silently
   alias via `& 63`). There is **no `next_table_id` field**.
2. **In this format window, ids must be DENSE: exactly 0..n in id order.**
   Decode refuses gaps as `Corrupt`. Dense ⇒ `position == id` is an
   *enforced invariant*, not an assumption — every positional site in the
   engine (per-table caches, `CheckPrograms`, bootstrap, CLI dump, mirror's
   position-as-id derivations) remains provably correct through stage 2
   (CREATE appends at id = n, still dense). Stage 4 (DROP) is the PR that
   relaxes decode to "unique, ascending, < MAX_TABLES" and performs the
   positional audit — the reviewed, enumerated checklist lives in §6. A
   stage-4 file with gaps is *refused* by a stage-0 binary instead of
   silently mis-decoding rows through the wrong table's column types.
3. `TableDef` gains **`indexes: Vec<IndexDef>`**,
   `IndexDef { columns: Vec<u16>, unique: bool }`; `index_no = 1 + position`
   (0 = PK tree). Single source of truth; the duplicated flag-derivation
   pair is deleted.
4. **Index tree membership rule, pinned**: a row contributes an entry iff
   **no indexed column value is NULL** (any-NULL ⇒ skip). At k = 1 this is
   exactly today's skip-if-NULL, so existing trees keep both their key
   *bytes* (k = 1 of `encode_key([v…])` / `encode_key([v…, pk…])`) **and**
   their *membership* — no rebuild. Any-NULL-skip matches SQL uniqueness
   (NULLs never conflict) and is sound for the planner's index access:
   equality and range predicates cannot match NULL under 3VL, and IS NULL
   never uses an index (true today as well).
5. On the wire, per-column `unique`/`indexed` flag bits are **written as
   zero and ignored on decode**; only `nullable` is meaningful. In memory
   the convenience flags are reconstructed from the index list in one place.
   `Schema::new` **normalizes** flag input first: `unique = true` clears
   `indexed` (they produce ONE unique index today; without normalization the
   round-trip `stored_schema() == config.schema` breaks, and v1 hashed the
   two spellings differently for the same physical schema).
6. **Seeding assigns ids in name-sorted order 0..n** — deterministic under
   TOML reordering, so the config path keeps its order-independent hash.
   In memory `Schema.tables` is sorted by id; at seed time that equals
   name-sorted order, so `SqliteAttach`/overlay's "name-sorted, parallel
   vectors align" covenant holds unchanged. (CREATE in stage 2 appends;
   `table_id(name)` becomes a linear scan, ≤ 64 tables.)
7. **No `Default` for `Schema`/`TableDef`** and no other escape that mints
   zeroed ids: the struct-literal constructions (`sqlite_attach.rs`, tests)
   take deliberate compile breaks and are converted to `Schema::new` — a
   `..Default::default()` shortcut would give every table id 0 and silently
   answer every query from table 0.
8. `PLAN_FORMAT` does not change in stage 0. Plans embed the schema hash;
   plans against v1-hash files never meet a v2 schema. #55's composite work
   DOES touch plan validation (see §7) and bumps `PLAN_FORMAT` there.

## 2. Canonical bytes v2

```
u8   version = 2
u32  ntables
per table (in id order; ids MUST be exactly 0..ntables in this window):
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
    u8   unique (0/1; other values Corrupt)
    u16  ncols  ‖ u16 × ncols   (column ordinals, significant order)
```

Decode is fully re-validating: corrupt input yields `Error::Corrupt`, never a
panic, truncation-tested at every offset, **and the check runs before any
length-driven allocation** (no `with_capacity` on claimed lengths). The list
is the UNION of everything the v1 decoder + `Schema::validate` enforce today
plus the new rules — spelled out because v0.1 forgot most of the former:

- counts: `ntables ≤ MAX_TABLES` (checked before allocation), `1 ≤ ncols ≤
  MAX_COLUMNS`, `npk ≥ 1`, `nindexes ≤ MAX_INDEXES` (new, 32),
- identifiers: table/column names pass `valid_identifier` (non-empty, ASCII
  rules, 128-char cap, `__mpedb` reservation); strings ≤ the 1 MiB cap,
- **duplicate table names via a set/sort check, NOT `windows(2)`** — the
  vec is id-sorted now, so the v1 adjacency trick silently stops detecting
  non-adjacent duplicates (review finding),
- duplicate column names within a table,
- ids: strictly ascending AND dense 0..n (this window),
- pk: ordinals in range, no duplicates, columns not nullable, not `Any`,
- defaults: type-fit, NULL-default vs NOT NULL, `Now` requires timestamp,
- indexes: ordinals in range; no duplicate ordinal within one index; no two
  indexes with identical `(columns, unique)`; a single-column index equal to
  the whole single-column PK refused (duplicates index 0); **no index column
  of type `Any`** — v0.1's list allowed one, which would resurrect the
  documented wrong-rows/wrong-DELETE memcmp-ordering bug that
  `Schema::validate` exists to prevent,
- flags: bits 1–7 ignored on read (not round-tripped),
- trailing bytes refused; **any version byte ≠ 2 refused** — there is no v1
  reader anywhere.

`Schema::hash()` = blake3 over these bytes. The version byte is inside the
preimage, so v1/v2 preimages can never collide.

## 3. Who assigns ids

- **Config seed / `Schema::new`** (every constructor path, including
  programmatic ones like `SqliteAttach`): sort declared tables by name,
  assign 0..n. Deterministic under `[[table]]` reordering.
- **CREATE TABLE** (stage 2): lowest free id — which is `n` while dense —
  appended at the end of the id-sorted vec.
- **DROP TABLE** (stage 4): frees the id; the NEXT create may reuse it.
  Safety: plans die with the schema hash; catalog entries die in the DROP
  commit. The stage-4 audit (§6) covers the persisted-id surfaces (mirror
  park/provenance records) before reuse can go live.

Index numbers: seed derives `indexes` from the normalized column flags in
column-declaration order (today's numbering exactly), then appends explicit
`[[table.index]]` entries in declaration order. `index_no` is stable for the
table's life.

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
  # single-column sugar, unchanged: unique = true / indexed = true
  [[table.index]]
  columns = ["tenant", "created"]   # composite; declaration order significant
  unique = false
```

Validation identical to §2's decode rules (shared code path, not a copy).

## 5. Old files (what replaced migration)

Nothing moves in place. A v1 file attached by a new binary:

- config path: `validate_frozen`'s existing "schema hash mismatch" (config
  now hashes v2 bytes, file holds the v1 hash) — the standard drift refusal,
- `open_from_file` / `dump` / mirror export: `stored_schema` decodes the
  catalog bytes → `Corrupt("unknown schema version 1")`.

Remedy: regenerate (re-import from the mirror source, re-run the seed, or
dump with the old binary first). This is the project's standing rule applied:
formats break freely; only near-free migrations earn their keep, and the
review proved this one was nowhere near free.

## 6. The stage-4 positional audit (enumerated now, executed with DROP)

Dense ids make these correct today; DROP's gap-relaxation PR must convert or
re-verify each before shipping:

- engine per-table parallel caches: `checks`, `sec_indexes`, `sec_unique`,
  `col_types` (built positionally, consumed by `table_id as usize` in index
  maintenance and row codec — a positional miss here corrupts trees),
- `bootstrap_catalog`'s `enumerate()`-as-id, facade `CheckPrograms`,
  `require_policy` position-mint, `insert_row_streaming`'s
  `position(..) as u32` (bypasses `table_id()` entirely),
- CLI `dump`/repl positional scans; mirror's position-as-id derivations
  (import/export/reconcile/preflight/regenerate) **and** its PERSISTED table
  ids (park/skip/map keys) — reuse meets stale persisted ids here,
- iteration-order-observable output (dump listing, export DDL order,
  py `table_names`) flips from name order to creation order at the first
  out-of-order CREATE — cosmetic, but golden tests must not assume,
- overlay/`SqliteAttach` parallel vectors (`pk_idx`, `Attached`) — safe by
  fresh `Schema::new` construction; that assumption is load-bearing and
  stated here.

## 7. What #55 builds on top (same window, own PR — the review's site list)

The composite work is NOT just the planner; these single-column assumptions
all move together under one `PLAN_FORMAT` bump:

- `plan/validate.rs`: `sec[no-1]`-is-THE-column and "IndexRange bound must
  have exactly one part" — rewrite for k columns (decode-side trust
  boundary),
- planner footprint: UPDATE set-column → index-bit mapping currently finds
  only single-column identity — a composite index containing a set column
  must set its bit (ring/optimistic write-set honesty),
- the `&Value` index API chain generalizes to `&[Value]`: engine
  `get_by_index`/`scan_by_index`/`scan_by_index_range` + maintenance,
  `TxnCtx` trait + all impls (incl. overlay `MergeCtx`), exec gather
  (join fetch, `range_bounds` — the prefix-ceiling machinery already
  produces composite bounds for PKs and carries over),
- ON CONFLICT: target = "a unique index whose column set equals the target
  set"; probe encoding + executor `target[0]` assumption,
- RLS §6.4 lint (`policy_store.rs`): treats index entries as single column
  ordinals AND its advice text asserts single-column uniques — with
  composite uniques the lint must check the LEADING column against the
  discriminator or it reopens the cross-tenant existence oracle,
- EXPLAIN rendering of index access (single-column labels today),
- differential vs sqlite3 + SLT + corpus.

## 8. Review outcomes (v0.1 → v0.2)

Folded as hard changes: no migration (was: fatal×2 — frozen-hash meta
contract makes in-place hash movement impossible and attach-order makes the
migration unreachable); dense-id enforcement (was: gapped ids silently
mis-decode via positional caches); lowest-free allocation, no counter (was:
id-as-bit breaks at 65th create; counter overflow at u32::MAX); the full v1
validation list + set-based duplicate detection + any-typed index refusal
(was: hostile-bytes constructions passing v0.1's list); flag normalization
in `Schema::new` (was: `unique+indexed` round-trip inequality and a v1/v2
hash-equivalence asymmetry); NULL-membership rule pinned (was: the k = 1
no-rebuild claim covered encoding only); no-Default rule (was: zeroed-id
silent-wrong-results escape). Verified sound by review: k = 1 key-byte
identity (`index_ikey` = `encode_key` concatenation, no length prefixes),
hash preimage versioning, encode determinism, COW atomicity of the catalog
write itself, workspace (no separate canonical bytes), mirror provenance
(stores decl strings, not schema bytes).

## 9. Staging

- **S0a — mpedb-types**: `id` + `indexes` + v2 encode/decode + the §2
  validation list + truncation/determinism/hostile-bytes tests; `Schema::new`
  assigns dense name-sorted ids, normalizes flags, derives indexes.
  Compile-fix the struct-literal constructors. Delete the renumbering test;
  its replacement asserts dense-id assignment.
- **S0b — engine**: `secondary_index_columns` → `TableDef.indexes`;
  maintenance generalized to k columns with the any-NULL-skip rule (k = 1
  regression: byte-identical keys against an existing-format fixture).
- **S0c — sql**: `secondary_indexes` → `TableDef.indexes`; ON CONFLICT
  unique-target via index shapes (single-column semantics unchanged).
- **S0d — config**: `[[table.index]]` + docs (GUIDE/README/CLAUDE invariant
  note replaces the engine/sql agreement pair with "TableDef.indexes is the
  single source").
- **#55 — the §7 site list** under one `PLAN_FORMAT` bump, then
  differential + corpus.

Each stage lands green on the full workspace before the next.
