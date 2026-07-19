# DESIGN-TABLE-CAP — lifting the table-count ceiling (#95)

**Status: design + BUILT (2026-07-19).** Supersedes the "widen the bitmap once more"
reflex of [#93](DESIGN.md) and the (still-unbuilt, still-correct-in-principle)
[DESIGN-TABLE-ID-GEN.md](DESIGN-TABLE-ID-GEN.md). The decision: **stop representing the
table set as a bitmap.** A footprint's read/write sets become sparse sorted `Vec<u32>`
(`TableSet`), and so does the CDC capture/freeze config. `MAX_TABLES` survives, but as a
*resource* bound chosen for cost — no longer a *representation* bound dictated by an
integer width.

## 0. The problem, measured

The C-API workbench (`crates/mpedb-capi/workbench/README.md`) puts it flatly: the
hardest ceiling under Django is `MAX_TABLES`. Django's `queries` label (493 tests) and
`backends` cannot run **at all** — `migrate` dies at

```
schema error: too many tables (121 > 120)
```

120 = `MAX_TABLES` (128) − an 8-slot system reserve. And because a dropped table's id is
**never reused** (DESIGN-DROP-TABLE §0), the 120 counts **lifetime creates**, not live
tables — a DDL-churny test suite burns through it faster than its live schema suggests.

`#93` (`cf30fa2`) already spent the cheap move: `u64 → u128`, 56 → 120 usable. The next
bitmap step is `u128 → [u64; N]`, and it is where the reflex stops paying.

## 1. What a footprint actually is, and where it lives

`Footprint` is computed once at plan-compile time and stored **inside the compiled plan**
— `plan/<hash>` in the catalog's sys-keyspace, shared by every attached process. It is
also recomputed from scratch and compared on every `CompiledPlan::decode` (`validate.rs`:
a forged footprint in an otherwise-valid plan is `Error::Corrupt`). So the footprint's
encoded size is paid **per persisted plan**, and its decode cost is paid on every
plan-cache miss in every process.

Its consumers, exhaustively (grepped, 2026-07-19):

| site | what it needs |
|---|---|
| `ring_exec::locality_key` | the single written table id (sort key) |
| `ring_exec::optimistic_eligible` | "exactly one written table" + that id |
| `ring_exec::optimistic_prep` | that id |
| `plan/validate.rs` | structural equality against a recompute |
| `plan/explain.rs` | rendering |
| `Footprint::conflicts_with` | set intersection (Phase-2 grouping; no production caller yet) |
| `lib.rs`/`stream.rs`/`sqlite_overlay.rs`/`mpedb-proc` | only the `read_only` flag |

Nothing iterates bits. Nothing needs O(1) random access by id in a hot loop. **Every
real use is "how many tables, and which ones" on a set whose typical cardinality is 1.**

## 2. Options considered

### (a) Widen the bitmap again — `[u64; N]`

- 256 tables → 32 B per set, 64 B per footprint. 1024 → 128 B / 256 B.
- Cost is paid by **every plan**, including the overwhelmingly common single-table plan
  that touches exactly one bit. A 256-byte footprint on a 1-table `SELECT … WHERE id=$1`
  is most of the plan blob.
- It **moves** the wall. Whatever N is picked, "how many tables can Django's biggest
  label create over its lifetime" is a question we would answer again.
- It multiplies the audit surface each time: `#93` had to fix two unmasked `1u64 << id`
  sites that would have silently dropped high bits, plus a CDC config that was dropping
  tables ≥ 64 behind a debug-only assert. An array-of-words bitmap turns every one of
  those into `(id / 64, id % 64)` arithmetic — strictly more places to get wrong.
- **Rejected.**

### (b) Generation-tagged id REUSE (DESIGN-TABLE-ID-GEN)

Keeps a narrow bitmap by making the cap apply to *live* tables and recycling dead slots
under a `(slot, generation)` tag. It is the intellectually right long-term shape and the
doc stands. But it is the deepest change in the engine (schema + plan + footprint + CDC +
mirror + cross-process reclamation gated on the oldest-pinned bound), it rides the
wire/commit-path adversarial-review bar, and it re-opens exactly the invariant
DESIGN-DROP-TABLE §0 closed on purpose. **Not this change.** No-reuse stays.

### (c) Sparse sorted `Vec<u32>` — CHOSEN

`tables_read` / `tables_written` become a `TableSet`: a `Vec<u32>` held **strictly
ascending** (sorted, no duplicates) as a representation invariant.

- **Size scales with what a plan touches, not with the id space.** Encoded as
  `u16 count ‖ count × u32 LE`: a 1-table read set is 6 bytes, an empty write set is 2.
  Today's footprint spends a flat 32 bytes on the two `u128`s; the typical plan
  (1 table read, 0 written) drops to **8 bytes**. Sparse is *smaller than the status
  quo* for every plan touching ≤ 3 tables, and every plan in the corpus touches ≤ 5.
- **Strictly ascending is canonical**, which is what a content-hashed plan format
  requires: one set has exactly one encoding, so the plan hash is stable, and decode
  *enforces* the ordering (a non-ascending or duplicated list is `Error::Corrupt`, not a
  silently-accepted alias).
- **Every operation the consumers need is trivial on a sorted small vec**: `len()`,
  `first()`, `contains` (binary search), `union_with` (merge), `intersects` (merge).
  `conflicts_with` stays a two-line set expression.
- **It removes the representation ceiling entirely** — a `TableSet` can name any `u32`.

## 3. So is there a cap at all — and what bounds it?

Yes, and it is worth being precise about *why*, because the answer changed character.

`MAX_TABLES` is no longer "the width of an integer". After (c), three things still bound
the table count, none of them a bit width:

1. **Tombstone bloat (the real one).** No-reuse + tombstone-in-place means
   `Schema::tables` carries a dead slot for every id ever minted (≈ 17 B encoded each).
   The whole schema is one catalog record, **re-encoded on every DDL** and re-decoded in
   every process whenever `schema_gen` moves. That is `O(lifetime creates)` work per DDL
   — a soft, gradual cost, not a wall, but a real one.
2. **Decode safety.** `from_canonical_bytes` reads `ntables` from untrusted bytes and
   allocates; it must be bounded before it allocates. (Now additionally capped at
   `min(256)` reserve so a corrupt count cannot drive a large speculative allocation.)
3. **The 8-slot system reserve**, unchanged.

So `MAX_TABLES` becomes a **policy number picked for cost**, and the right question is
"what cost curve are we willing to pay", not "what fits in a word".

**Chosen: `MAX_TABLES = 4096`** (⇒ 4088 user-visible live tables).

- 34× Django's `queries` requirement, with room for a churny lifetime-create budget.
- Worst case all-dead schema record ≈ 70 KB — the catalog btree handles it via overflow
  chains, and it is only reached by a workload that actually minted 4096 tables.
- Worst case realistic *live* schema (4088 tables × ~10 columns) ≈ 1.4 MB of schema
  record rewritten per DDL. That is the honest cost of tombstone-in-place at extreme
  scale, and it is proportional to **actual** table count, never to the cap.
- **Measured**: burning the whole 4096-id space through real DROP+CREATE commits
  (`drop_table::create_refuses_after_the_lifetime_id_ceiling`) takes ~110 s — i.e.
  ~27 ms per DDL pair averaged over a record growing from 0 to 4096 tombstones. That
  is the cost curve, and it is why the test is `#[ignore]`d rather than why the cap
  is 4096: a workload doing 4096 lifetime creates has already accepted DDL at that
  scale. If this ever becomes the binding constraint, the answer is the offline
  `regenerate` id-compaction escape hatch (DESIGN-DROP-TABLE §0), not a smaller cap.
- Corrupt-input allocation bound: 4096 × `size_of::<TableDef>()` ≈ 0.5 MB, and the
  `min(256)` reserve keeps even that off the table until the bytes are really there.

If a future workload needs more, raising 4096 is now a **one-constant** change with no
format bump and no bit-audit — which is the point of doing (c) rather than (a).

## 4. The CDC config had to move too — and it was a latent wrong answer

`cdc::CaptureConfig` held `captured` / `blocked` as `u128` bitmaps, with:

```rust
let bit = 1u128 << (table_id & (MAX_TABLES as u32 - 1));   // set_captured / set_blocked
```

a **mod-`MAX_TABLES` fold** behind a `debug_assert!`. Under the old cap no id could reach
128, so it never fired. The moment `MAX_TABLES` moves past 128 this becomes a live
corruption: enabling capture on table 200 sets bit 72, and the mirror then replicates
**table 72's** rows under table 200's identity — a silent cross-table wrong answer of
exactly the class mpedb exists to prevent, in release builds only.

Unlike the OFP ring's `& 63` (§5), this fold is **not** sound: the ring's fold is a
conservative *conflict* signal where aliasing costs a false positive, while the CDC
bitmap is an *identity* map where aliasing changes which table's data moves.

`CaptureConfig.captured` / `.blocked` therefore become the same `TableSet`, and the
control record's encoding becomes variable-length:

```
generation u64 LE ‖ TableSet(captured) ‖ TableSet(blocked)
```

with a strict decode (exact length consumption, strictly-ascending ids, each
`< MAX_TABLES`, count `≤ MAX_TABLES`) and truncation-at-every-offset tests.
`CaptureConfig` loses `Copy`; `WriteTxn::capture_config` is split into a
`ensure_capture_config` + borrow so the per-row `check_write_blocked` /
`capture_dirty` path still allocates nothing.

## 5. What deliberately does NOT change

**The optimistic-footprint (OFP) intent ring and `WriteTxn.written_tables` stay `u64`
with `& 63`.** This is documented at `engine/commit.rs` and `shm.rs` and it is correct at
any `MAX_TABLES`:

> the fold is a many-to-one map from table ids to bits. A given table always folds to the
> same bit, so two writers of the same table always collide ⇒ **a real conflict is never
> missed**. Distinct tables that alias produce a *false* conflict, costing one extra
> optimistic re-validation. The failure mode is "slower", never "wrong".

Widening it is commit-path work (shared-memory layout, recovery, the 37-finding review's
ordering rules) at a much higher bar, and it buys nothing but conflict precision.

**Table-id no-reuse stays.** DESIGN-DROP-TABLE §0's argument is untouched by this change,
and this change makes its bounded cost 32× less binding.

## 6. Formats bumped

- **`PLAN_FORMAT` 40 → 41.** The footprint's wire layout changed shape (two
  length-prefixed id lists where two fixed `u128`s were). A format-40 reader sees the
  changed FORMAT byte at offset 0 and fails CLOSED with `PlanInvalidated`, i.e. the
  documented re-prepare path — never a misread.
  ⚠️ **A sibling change in this batch also bumps `PLAN_FORMAT` for new scalar functions.
  Whoever merges second renumbers; the two changes are independent.**
- **CDC control record (`cdc\0tabs`)**: fixed 40 bytes → variable. `ENCODED_LEN` is gone.
  A stale 40-byte record decodes as corrupt (its trailing bytes do not parse as two
  ascending id lists) rather than as a wrong capture set. Per the project's standing
  no-backward-compat rule, no migration: re-run `mirror import`.
- **Schema canonical bytes: NOT bumped.** `ntables` and `TableDef.id` were already `u32`;
  only the *validation bound* moved, which strictly relaxes. v6 files stay readable.

## 7. Identifier length: 128 → 255 bytes

`schema::valid_identifier` capped identifiers at 128 bytes, which independently blocks
Django's `backends` label: a generated m2m through-table name comes out at 134 chars.
The limit is pure policy — `write_str` length-prefixes with a `u32`, `read_str` already
bounds at 1 MiB, and no table/column name is ever a btree key component. Raised to
**255**, in this pass, for the cost of one constant.

## 8. Audit — every shift / mask / cast against a table id

The absolute constraint on this change is *zero wrong answers*: a footprint that silently
misses a table is a concurrency/mirror correctness bug. Full sweep of table-id
arithmetic:

| site | before | after / verdict |
|---|---|---|
| `types/footprint.rs` `reads_table`/`writes_table` | `1u128 << table_id` | binary search — **no shift**, any `u32` |
| `types/footprint.rs` `conflicts_with` | bitmap AND | sorted-merge `intersects` |
| `types/footprint.rs` encode/decode | fixed 16 B LE | `u16` count + `u32` ids, ascending-enforced |
| `sql/planner/footprint.rs` ×2 `table_bit` | `1u128 << id`, guarded `id < MAX_TABLES` | `check_table` → `TableSet::insert`, same guard, **no shift** |
| `sql/planner/footprint.rs` 3× accumulators | `0u128` + `\|=` | `TableSet::union_with` |
| `sql/planner/footprint.rs` `all_secondary_bits`, `1u64 << (pos+1)` | per-table **index** numbering, bounded 63 | unchanged — not a table id |
| `sql/plan/explain.rs` | `{:#x}` | list rendering |
| `mpedb/ring_exec.rs` `trailing_zeros()` ×3 | 128 sentinel for empty | `first()` / `u32::MAX` sentinel |
| `mpedb/ring_exec.rs` `count_ones() != 1` | popcount | `len() != 1` |
| `core/cdc.rs` `is_captured`/`is_blocked` | `>> table_id & 1`, guarded | binary search, **no shift** |
| `core/cdc.rs` `set_captured`/`set_blocked` | `1u128 << (id & (MAX_TABLES-1))` — **LATENT BUG, §4** | sorted insert/remove, no fold |
| `core/shm.rs` `opt_conflict` | `1u64 << (table_id & 63)` | **unchanged, sound** (§5) |
| `core/engine/commit.rs` OFP record | `1u64 << (table & 63)` | **unchanged, sound** (§5) |
| `core/engine/write.rs` `note_write` | `written_tables \|= 1u64 << (id & 63)` | **unchanged, sound** (§5) |
| `policy_store.rs`, `cdc::dirty_key`, mirror `state.rs` keys | `table_id.to_be_bytes()` (u32 BE) | unchanged — already full-width |
| `schema.rs` `tables[id as usize]` (~35 sites) | positional | unchanged — `position == id` still validate-enforced |
| `plan/mod.rs` `DUAL_TABLE` = `u32::MAX`, `CTE_TABLE` = `u32::MAX - 1` | sentinels | unchanged — still far above 4096 |

Grep verified: **no** `as u8` / `as u16` narrowing of a table id anywhere in the
workspace, and no other per-table bitmap besides the four above.

## 9. Verification

- Footprint encode/decode roundtrip incl. ids far past both old ceilings (128, 4095),
  plus truncation at every offset and rejection of non-ascending / duplicated /
  out-of-range id lists.
- `CaptureConfig` roundtrip + truncation at every offset + the §4 regression: capture on
  a high id must not appear as capture on `id mod 128`.
- A **differential** test that creates more tables than the old cap and joins across ids
  above the old boundary — the shape that proved `#93` (70 tables, joins on 64–69,
  286/286 vs sqlite, 0 wrong).
- Drop/create churn to exhaustion, reporting cleanly at the new bound.
- `ENGINE_TABLE_CAP` in the corpus runner tracks `MAX_TABLES - 8`.
