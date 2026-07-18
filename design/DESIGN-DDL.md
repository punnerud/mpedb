# DESIGN-DDL — live schema evolution (CREATE / DROP / ALTER TABLE)

**Status: design draft, not built.** This is the design-first step for #47, in the
same discipline as DESIGN.md / DESIGN-MULTIDB.md / DESIGN-MIRROR.md: the protocol
is worked out and adversarially reviewed *before* any code touches the catalog,
meta, or attach path. Nothing here is implemented yet.

## Goal, and the invariant it appears to break

Let a developer use mpedb like a normal database — `CREATE TABLE`, `DROP TABLE`,
`ALTER TABLE` — on a **live, multi-process, shared-memory** database, not only at
creation and not only via `mirror import`. `import` keeps working alongside; the
two are not in tension.

This appears to break CLAUDE.md's invariant: *"Schema/geometry are
file-authoritative: attach hard-errors on config drift."* It does not. It makes
the file **more** authoritative, not less — see §4.

## 1. The key that makes this tractable: the schema is already MVCC-versioned

The schema is not a side-channel. It lives in the catalog B+tree under
`CAT_SCHEMA_KEY = [0x00]` → canonical schema bytes (engine.rs catalog layout), and
the catalog's root, `catalog_root`, is a field of the committed meta
(`MetaSnapshot`). Data pages and the schema are therefore versioned **together**
by the same COW meta flip.

Consequence, and it is the whole design: **a reader that pins MVCC snapshot T sees
schema-at-T**, because it reads `catalog_root` from the meta it pinned. A writer
holding the lock sees the latest. There is no separate "schema version" to keep in
sync with the data version — they are the same version, by construction.

So `CREATE TABLE` is, mechanically, *another catalog write under the writer lock*.
The hard part is not the mutation; it is what every OTHER attached process does
when the schema it cached goes stale (§3), and reconciling the config-drift check
(§4).

## 2. What each DDL statement does (under the writer lock)

All three take the writer lock, like any write, and commit via the normal meta
flip. Each is a catalog mutation plus, for DROP/ALTER, data work.

- **CREATE TABLE** — append a `TableDef` to the schema, re-canonicalize, write the
  new bytes to `CAT_SCHEMA_KEY`, allocate the new table's empty tree roots in the
  catalog (`[0x01, table_id, index_no]` → empty). Recompute the canonical schema
  blake3 and stage it into the committed meta's `M_SCHEMA_HASH`. No data touched.

- **DROP TABLE** — remove the `TableDef`, delete its catalog tree-root entries,
  and free its data + index pages. ⚠ The freed pages go through the ordinary
  freelist reclamation, so they are reusable only at/below the oldest-pinned bound
  (#37): a reader still pinned on a pre-DROP snapshot reads the old table from its
  own `catalog_root`, and its pages must not be handed out until it releases. The
  existing bound handles this unchanged — DROP must NOT special-case it.

- **ALTER TABLE** — the tractable subset first:
  - ADD COLUMN (nullable, or with a default): schema-only if the row codec treats
    a missing trailing column as NULL/default; a rewrite otherwise. Decide by
    measuring the codec, not by assuming.
  - DROP COLUMN, RENAME: schema-only for RENAME; DROP COLUMN needs a decision on
    whether stored rows are rewritten or the column is tombstoned.
  - Type change: refuse in v1. A loose→strict or narrowing change is exactly the
    class `mirror` preflight exists to validate; do not reinvent it in ALTER.

  ALTER is the largest sub-project and should be staged AFTER CREATE/DROP land.

## 3. Staleness: how a process notices the schema changed

Every process holds an in-memory `Schema` (typed columns, for validation and
codec). After a DDL commit, other processes' copies are stale. Detection is cheap
because the signal already exists: the committed meta carries `M_SCHEMA_HASH`.

- **A writer** reads the newest meta when it takes the lock (it already does).
  Add: compare the meta's `M_SCHEMA_HASH` against the cached one; on mismatch,
  reload `Schema` from `CAT_SCHEMA_KEY` before validating any row. One branch on
  the write path, taken only across a DDL boundary.

- **A reader** pins a snapshot = a specific `catalog_root`. It should load its
  `Schema` from *that* snapshot's catalog, so it is self-consistent by
  construction — it sees exactly the schema its data belongs to. A reader that
  pinned before the DDL correctly does not see the new table; one that pins after
  does. No epoch race, because the schema and the snapshot are the same version.

- **The plan cache invalidates for free.** A `CompiledPlan` carries the blake3 of
  the schema it was compiled against (plan.rs), and decode REJECTS a plan whose
  hash ≠ the live schema hash. So a plan compiled against the pre-DDL schema is
  already refused after the DDL — no new invalidation path, and this is tested.

The word "epoch" in the task title is really just `M_SCHEMA_HASH` doing double
duty: it is already the schema's version stamp; DDL is the first thing that makes
it *change* during a database's life.

## 4. Reconciling the config-drift hard-error

Today `verify_config` requires `config.schema_hash == file.M_SCHEMA_HASH` and
hard-errors otherwise (shm.rs). Once DDL can evolve the file, the config no longer
mirrors it — so this check, as written, would reject every post-DDL attach.

Resolution, and it strengthens the invariant rather than weakening it: **the
config's schema section becomes creation-only.** On first `open` of a
non-existent file, the config's schema seeds the catalog. On every attach after
that, the schema is read FROM the file's catalog and the config's schema section
is ignored (or, transitional: validated as a *subset*/compatible, TBD in review).
Geometry (`size_mb`, `max_readers`, `durability`) stays config-checked and
file-authoritative exactly as today — DDL never changes geometry.

This is *more* file-authoritative than the status quo, where the config had to
carry a redundant copy of the schema that could drift. After this, the file is the
single source of truth for the schema, and there is nothing to drift.

Open question for review: a database that has NEVER had DDL run should keep
behaving exactly as today (config schema must match), so existing users see no
change. A flag in the meta ("DDL has occurred") or simply "config schema is
advisory once the file exists" — decide in review.

## 5. What the adversarial review must break

Written as questions, because the answers are not yet load-bearing findings:

1. **Reader pinned across a DROP.** Snapshot T predates a DROP TABLE at T+1. The
   reader reads the old table from its `catalog_root`. Are its pages provably held
   until release by the oldest-pinned bound, with DROP adding nothing? (Believed
   yes — DROP frees like any delete.)
2. **Two writers racing DDL and DML.** DDL is serialized by the writer lock, so a
   concurrent INSERT either precedes it (old schema, valid) or follows it (reloads
   schema first, §3). Is there any window where an INSERT validates against schema
   N and commits into catalog N+1?
3. **Crash mid-DDL.** CREATE TABLE is one COW commit; a SIGKILL before the meta
   flip leaves the old schema, after it leaves the new — the ordinary meta-flip
   atomicity. But the catalog now has BOTH the schema bytes and the new tree
   roots; are they written in one commit so a torn state is impossible? (Must be.)
4. **A stale plan that decodes anyway.** Can a plan compiled against schema N ever
   pass the hash check against schema N+1 — e.g. if a DROP+CREATE round-trips to
   the same canonical bytes? (The hash is over canonical bytes, so identical
   schemas hash identically, which is correct: the plan is valid for that schema.)
5. **`max_readers`/geometry unchanged.** Confirm no DDL path can alter geometry,
   so the file-authoritative geometry check is untouched.
6. **Config-drift for never-DDL'd databases.** Existing users must see no
   behavioural change until they run DDL. Does the reconciliation in §4 hold that?

## 6. Staging (each its own PR, tested + differential vs sqlite3/PG)

1. Attach reads schema from the catalog; config schema becomes creation-only
   (§4). No DDL yet — this is pure refactor, and it must leave every existing test
   green, because it changes nothing observable until DDL exists.
2. `CREATE TABLE` (parser → binder → catalog mutation under lock → meta hash).
3. Writer/reader schema reload on hash change (§3). Multi-process test: process A
   creates a table, process B sees it on its next op.
4. `DROP TABLE`, with the pinned-reader page-reclamation test (§5.1).
5. `ALTER TABLE ADD COLUMN` (the schema-only cases first).
6. The rest of ALTER, gated on measurement of what needs a rewrite.

Until §1 lands and stays green, none of this is real. The refactor that makes the
file authoritative for the schema is the actual foundation; CREATE TABLE is the
easy part on top of it.
