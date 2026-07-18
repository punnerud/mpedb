# DESIGN-MIRROR-DDL — bidirectional table add/drop through the mirror

**Status: design draft.** Answers Morten's steer (2026-07-17): "det som gir best
toveis sync" — schema changes, not just data, should flow both ways across the
sqlite/PG ⇄ mpedb mirror, now that mpedb has in-band `CREATE TABLE` (#47 stage
2) and a designed `DROP TABLE` (stage 4). This is the mirror-side integration;
it depends on #47's engine DDL but adds no new engine format.

## 0. What "best" means here — and where it stops

Best bidirectional sync is **complete AND reliable**. Full symmetric,
independent schema evolution on both sides (each freely ALTERs/CREATEs/DROPs and
it all merges) is NOT best — it invites schema-merge conflicts that can leave
the two sides unreconcilable, which is *worse* for sync than an honest refusal.
mpedb's mirror already made the pragmatic call: data is bidirectional with
conflict resolution (park/resolve); schema *drift* routes to `regenerate` (a
clean full rebuild, DESIGN-MIRROR line 695).

So the design propagates the **unambiguous, common** schema deltas incrementally
both ways, and routes the **ambiguous** ones to the existing regenerate path:

| change | direction | handling |
|---|---|---|
| table ADD | source → mpedb | incremental: introspect + `CREATE TABLE` on mpedb, extend scope |
| table ADD | mpedb → source | incremental: render dialect `CREATE TABLE`, extend scope |
| table DROP | source → mpedb | incremental: `DROP TABLE` on mpedb (needs #47 stage 4), shrink scope |
| table DROP | mpedb → source | incremental: `DROP TABLE` on source, shrink scope |
| column ALTER | either | **regenerate** (matches DESIGN-MIRROR "no ALTER"; revisit at #47 stage 5) |
| both create same table | conflict | **park + regenerate** — never silently merge two schemas |

Table add/drop is unambiguous ONLY when it is genuinely a whole-object
create/delete. Two review findings (HIGH + MEDIUM) show the introspect-diff
cannot assume that:

- **Table RENAME looks byte-identical to DROP+ADD** at the introspect level
  (`ALTER TABLE users RENAME TO customers` presents as `{users gone, customers
  new}`). Blindly applying the incremental arms would DROP `users` on mpedb —
  destroying its un-pushed local dirty set + parked conflicts, the exact loss
  `regenerate` migrates the dirty set to prevent — and burn two lifetime ids.
  **Rule**: detect rename and route it to `regenerate`. PG: persist and diff
  the table `oid` (name changed, oid stable ⇒ rename). sqlite (no stable table
  id): treat any *same-pull-window* drop-of-A + add-of-B whose column shapes
  are compatible as ambiguous ⇒ regenerate, never incremental DROP+CREATE.
- **A slow drop-and-recreate can't be bounded by N-snapshot absence.** The
  missing-table→DROP arm (§3) additionally **refuses to DROP a table whose
  mpedb-side dirty set is non-empty** (force regenerate/operator) and gates on
  an explicit source tombstone signal (the `op='T'`/DDL changelog event),
  never on "absent across two snapshots" alone.

A genuine conflict (same name, two independent shapes) is detected and refused,
not merged. Column-level ALTER stays on the regenerate path until mpedb even
*has* ALTER (#47 stage 5).

## 1. The machinery already exists — this wires it incrementally

Nothing here is a new subsystem; it is a delta-application of proven one-shot
code:

- **Render mpedb TableDef → source-dialect `CREATE TABLE`**: `export.rs:114`
  (sqlite) and `pg_export.rs:266` (PG) already emit `CREATE TABLE … PRIMARY KEY`
  with reverse-mapped types + NOT NULL + UNIQUE. Factor the per-table renderer
  out of the whole-DB export loop; call it for one added table.
- **Introspect a source table → mpedb TableDef**: `sqlite::introspect` /
  `pg::introspect` + the type mapping in `import.rs` already build the mpedb
  schema from a source; run it for the delta and feed the result to mpedb's
  `CREATE TABLE`.
- **Drift detection substrate** (review MEDIUM: verify before relying on it):
  DESIGN-MIRROR line 105 lists "mpedb schema blake3, source schema fingerprint"
  as `map/<id>` fields, but the review found the current `TableMap` codec does
  NOT actually persist them yet. **D3/D4 must first extend the `TableMap`
  record to persist the source-schema fingerprint (incl. typmod) + the mpedb
  schema blake3**, bump its codec version, add truncation tests, and make the
  pull diff recompute+compare them — that comparison IS the add/drop-vs-ALTER
  boundary, so the boundary has no substrate until this lands. A NEW mpedb
  table has no `map`; a DROPPED one leaves an orphan `map` (no-reuse ⇒ inert).
- **The mpedb-side DDL signal is free**: `schema_gen` (#47) bumps on every DDL.
  `push` records the last-pushed gen; a bump means "diff the schema and drain
  any pending schema op."
- **The changelog already carries a DDL op**: source triggers write `op='T'`
  (TRUNCATE) → pull forces a re-diff (DESIGN-MIRROR line 382). Table add/drop
  extends the same op space with `op='C'`/`op='D'` (or the `sys/ddl/<seq>`
  pending-op queue on the mpedb side, §2).

## 2. mpedb → source (a local CREATE/DROP reaches the source)

- CREATE/DROP TABLE on mpedb bumps `schema_gen` and, IF a mirror `cfg`
  sys-record exists, writes a **pending schema op** to `sys/ddl/<seq>`:
  `{kind: C|D, table_name, [rendered-columns for C]}`. Written in the SAME COW
  commit as the DDL. **Build note (review): this MUST use the `WriteTxn`'s own
  `sys_put` inside `create_table`/`drop_table`'s txn** — the obvious facade
  helper `Database::sys_record_put` is a SEPARATE commit and would silently
  break atomicity (the op could exist without the schema change or vice versa
  across a crash). The engine DDL methods therefore take the pending-op payload
  and write it in-txn, or the facade passes a closure the txn runs before
  commit.
- `push_batch` drains `sys/ddl/*` in seq order BEFORE the data dirty-set (a
  create must reach the source before its rows; a drop after its last rows —
  seq ordering with the data watermark handles both). For each op:
  - **C**: render the dialect `CREATE TABLE` (§1), execute on source, add the
    id to `scope`, `set_captured(id, true)`, write the `map/<id>` provenance
    (source name = mpedb name, mpedb blake3, source fingerprint).
  - **D**: `DROP TABLE` on source, remove the id from `scope`, delete `map/<id>`
    and any residual dirty/park for it.
  - Source rejects (permission, `pull_only` mode, a name clash) ⇒ **park the
    op** and surface it (same discipline as a rejected row); the local schema
    stays as-is, no silent divergence.
- Clear the drained op only after the source confirms — a re-dirtied/failed op
  survives to the next round (mirrors the data-path's clear-only-if-applied
  rule, `push.rs`).

## 3. source → mpedb (a source CREATE/DROP reaches mpedb)

- `pull` already introspects/diffs the source. Extend the diff to the **table
  set**, not just rows: compare the source's current table set (introspect)
  against `scope`'s `map/<id>` source names.
  - **new source table** (in introspect, no `map`): introspect its shape,
    `CREATE TABLE` on mpedb (#47 stage 2), add to scope + capture + `map`. Then
    its rows flow through the ordinary pull. **Loop fence (review MEDIUM):**
    the `map`-record-as-known fence has an open window — an mpedb→source CREATE
    makes the source table visible BEFORE its `map/<id>` is written, so a
    concurrent or post-crash pull could see it as "new" and re-import it. So
    "new source table" additionally requires the name to NOT already exist in
    mpedb's LIVE schema: a name match means either the just-pushed table (skip,
    it's ours) or a genuine independent conflict (→ SchemaConflict park), never
    a blind re-import. This closes the window without depending on `map` timing.
  - **missing source table** (has `map`, gone from introspect): `DROP TABLE` on
    mpedb (#47 **stage 4** — the hard dependency), remove from scope + `map`.
    Guard against a transient introspect miss (a source mid-migration) with the
    same fingerprint-drift confirmation the re-diff path uses — a table absent
    across two consecutive introspect snapshots, not one.
  - **shape drift on an existing table** (fingerprint changed, same name):
    NOT a table add/drop → the existing `op='T'`/fingerprint-drift → re-diff or
    regenerate path (unchanged; ALTER territory).
- A **conflict** — a source table whose name already exists on mpedb with a
  DIFFERENT shape (both sides created it independently) — parks with a
  `SchemaConflict` kind and routes to `regenerate`/operator. Never merged.

## 4. id-space under churn (no-reuse holds)

Incremental DDL makes tables churn, and #47 stage 4 chose **no-reuse** (a
dropped id is never reclaimed; the ceiling is 64 lifetime creates). For a
long-lived bidirectional deployment that adds/drops many tables, `regenerate`
is the reset: it rebuilds mpedb from the source with a fresh DENSE schema (ids
0..n), so the lifetime-create counter resets. Regenerate is already a
first-class mirror op (the `db_full` recovery, DESIGN-MIRROR §7); a churny
deployment runs it periodically for space reasons anyway. **No change to the
no-reuse decision** — this design makes regenerate's id-densification an
explicit, documented part of the mirror lifecycle rather than a new mechanism.

## 5. What the adversarial review must break (before build)

1. **Ordering**: a CREATE op drained before rows that need it; a DROP after the
   last row; a create-then-drop of the same name in one push round. Does the
   seq queue + data watermark actually serialize these correctly against the
   dirty-set drain?
2. **Atomicity of the pending-op write**: `sys/ddl/<seq>` written in the SAME
   commit as the schema change — SIGKILL cannot leave one without the other.
   And the drain's clear-only-on-confirm cannot lose an op on a push crash.
3. **The conflict boundary**: is "table add/drop" cleanly separable from
   "column ALTER" at both the introspect-diff and the schema_gen-diff level, so
   the ambiguous case always routes to regenerate and never to a silent
   incremental merge?
4. **Loops**: an mpedb→source CREATE must not bounce back as a source→mpedb
   CREATE on the next pull (the `map` record written at push time is the fence:
   the table now HAS a `map`, so the pull diff sees it as known, not new).
   Verify the fence closes for both C and D in both directions.
5. **Mode interactions**: `pull_only` (source authoritative — reject mpedb→
   source DDL), `push_only`, `notouch` — each must gate the DDL arms exactly as
   it gates the data arms.

## 6. Staging (after #47 stage 4 DROP lands)

- **D1** — mpedb→source CREATE only (works on #47 stage 2, already built):
  pending-op write in the DDL commit, `push` drains, per-table dialect render,
  scope/capture/map extension. The immediately useful half.
- **D2** — mpedb→source DROP (needs #47 stage 4): pending-drop op + push drain.
- **D3** — source→mpedb CREATE: pull table-set diff → `CREATE TABLE` on mpedb.
- **D4** — source→mpedb DROP (needs #47 stage 4): two-snapshot-confirmed
  missing-table → `DROP TABLE` on mpedb.
- **D5** — conflict parking (`SchemaConflict`) + mode gating + the loop fence
  tests; differential round-trip (create/drop on each side, assert the other
  converges) + a `mirror-collide` SIGKILL-fuzz extension covering DDL ops.
- ALTER stays regenerate-only until #47 stage 5.
