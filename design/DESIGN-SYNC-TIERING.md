# DESIGN-SYNC-TIERING — cold-data tiering + drain-and-read-back between `.mpedb` files

**Status: design (2026-07-18); v1 row-drain SHIPPED 2026-07-20 (#78) — see §8 for what shipped
and where the code deliberately diverges from this doc. Forward-looking (Phase 6+), sequenced
AFTER the SQL-parity sprint. Built on the existing mirror (bidirectional CDC + type-provenance),
extents/blobs (#50), and the HTTP-range/FrozenDb direction (#22). Pairs with
[DESIGN-SERVICE.md](DESIGN-SERVICE.md): the drainer is a scheduled service task, read-back is a
callout.**

## 0. The problem

A hot `.mpedb` accumulates cold data — old log rows, aged records, and especially **blobs** (files
stored in the DB). On an expensive server (say Django serving files out of the base) disk fills with
data that is rarely read. We want the hot file to stay small by **gradually draining stale data to a
cheaper, possibly remote, mirrored `.mpedb`**, while keeping it *transparently readable back on
demand* — pay remote-fetch latency only for the rare cold access, keep hot data local and fast.

This is LSM cold tiers / S3-backed pages / Snowflake's warm-cold split, applied to the embedded
`.mpedb` file and reusing mpedb's own mirror as the transport instead of a bespoke object store.

## 1. What tiers, and the eviction policy

- **Tierable units**: whole rows (by age/policy), and — the high-value case — **blob extents** (#50
  already separates large values into contiguous extents with reflink; an extent is the natural
  offload unit). A row can stay hot while its blob column tiers cold, leaving a small reference.
- **Policy** (config `[tiering]`): by age (`ts < now - :ttl`), by LRU access-time, or by a size
  high-water (evict coldest until under N bytes). Per-table opt-in; a table declares
  `tier = { after = "30d", to = "<remote>", mode = "reference|drain" }`.
  - `drain` = move it out and forget locally (history preserved remotely; §5 of DESIGN-SERVICE).
  - `reference` = replace the local payload with a **stub** (a tombstone-sized pointer) that can be
    resolved back (§3).
- The drainer is a background task (a cron/queue job in the service model) — bounded work per pass,
  never a stop-the-world sweep, so a live multi-process DB keeps serving while it drains.

## 2. Transport: reuse the mirror, don't invent an object store

- The cold store is **another `.mpedb`** (local cheap disk, or a remote host reached via the service
  socket / an mpedb endpoint). The mirror already does `.mpedb ⇄ .mpedb`/sqlite/PG with CDC,
  conflict rules, type-provenance and roundtrip verification (DESIGN-MIRROR, tasks #25/#26/#30/#72).
  Tiering is a *directional, policy-driven* mirror: push cold units out, verify they landed
  (blake3/content-hash roundtrip — the provenance machinery), THEN reclaim locally.
- **Ordering is load-bearing** (same discipline as the freelist fixpoint): a unit is only reclaimed
  locally after the remote write is durable and verified. Crash between push and reclaim → the unit
  exists in both places (safe, idempotent re-drain), never in neither. This is the mirror's existing
  "converge exactly to the source" guarantee (mirror-collide SIGKILL fuzz) pointed at tiering.

## 3. The stub + read-back

- A tiered `reference` unit leaves a **stub**: for a blob, an extent pointer that resolves to
  `(cold-store-id, remote-extent-ref, content-hash, length)` instead of local pages; for a row, a
  tombstone carrying the cold-store key. Stubs are tiny, so the hot file shrinks by ~the payload size.
- **Read-back** is transparent at the storage layer: resolving a stubbed extent/row triggers a fetch
  from the cold store (HTTP-range against a remote `.mpedb`, or the service socket), verifies the
  content-hash, and returns it. Optionally **re-warms** (writes it back hot) on access if the policy
  says so (LRU promotion), or streams through without persisting (pure cold read) to keep the hot
  file small. The HTTP-range fetcher and FrozenDb pack format from #22 are exactly the remote-read
  substrate — this design gives #22 a concrete consumer.
- A cold read that fails (store unreachable, hash mismatch) is an **explicit error**
  ("tiered unit unavailable / changed"), never a silent wrong/empty value — the mirror's
  hash-mismatch rule (§8 of ETL-BIDI's artefact-correlation lessons: fail loud, never wrong input).

## 4. The Django-blob use case, concretely

Django stores uploads as blobs in the base; disk on the pricey app server fills. Enable
`tier = { after = "30d", to = "cold.example:cold.mpedb", mode = "reference" }` on the uploads table's
blob column. The drainer streams 30-day-old blobs to the remote `cold.mpedb`, leaves extent stubs;
the app server's disk usage drops to hot data + stubs. A request for an old file resolves the stub,
range-fetches from cold, serves it — slower for that one request, invisible to the ORM (the blob API
is unchanged; #43's incremental blob read just sources from cold). Re-warm on access is a config knob.
The expensive server gets its disk back without changing application code.

## 5. Consistency, MVCC, multi-process

- Tiering runs inside the normal txn/MVCC discipline: a reader with an open snapshot that predates a
  drain still sees the unit (COW pages aren't reclaimed while pinned — the oldest-pinned bound,
  CLAUDE.md). The drainer reclaims only after both the remote durability AND the local pin bound
  allow it — the same two-sided condition the freelist already enforces, extended with "remote
  verified."
- Multi-process: any process reads a stub and reads-back; only the drainer (one, elected via the
  writer lock / a service singleton) pushes and reclaims, so there is no concurrent-drain race.
- The reverse flow (read-back that re-warms) is a normal write, group-committed like any other.

## 6. Prior art

- LSM cold tiers (RocksDB `ttl`/`FIFO`, universal compaction to slow tier), Snowflake hot/cold,
  S3-backed page stores (Neon, ClickHouse `MergeTree` `TTL ... TO DISK/VOLUME`), and Django's own
  `FileField` + remote storage backends. The twist: the cold store is *the same engine's file
  format*, so read-back is a real query, not an opaque blob get, and the mirror gives verification +
  bidirectional flow for free. pristine-tar/DVC-style content-hash correlation (ETL-BIDI's lessons)
  is the identity model — hash + lineage, never path.

## 7. Staging

1. **Blob-extent tiering, `reference` mode, local cold `.mpedb`** (no remote yet) — drain coldest
   extents to a sibling file, stub + read-back, hash-verified, drainer as a bounded background pass.
2. **Remote cold store** over the service socket / HTTP-range (#22 substrate) + re-warm policy.
3. **Row tiering + `drain` mode** (meets DESIGN-SERVICE §5 log retention) + the `[tiering]` policy
   config surface.
4. Multi-process drainer election + the mirror-collide-style SIGKILL fuzz proving
   "hot+cold converge, no unit lost, no double-charge" — commit-path-class review before it ships.

## 8. v1 shipped (2026-07-20, #78) — and the doc-vs-code drift

**What shipped**: `Database::tier_drain`/`tier_create_cold` (`crates/mpedb/src/tier.rs`) and
`mpedb tier drain <hot> <cold.mpedb> --table T --where PRED [param ...]` + the `mpedb tier crash`
SIGKILL harness (`crates/mpedb-cli/src/tier.rs`). One-shot, batched (`--batch`, hot writer lock
held per batch = the §5 drainer election), predicate-driven row drain into a local cold `.mpedb`
seeded with the hot table's exact definition. Per batch: SELECT under the hot writer lock → copy
into cold → **cold COMMITS** → every copied row is **re-read from a fresh cold snapshot and
compared bit-exactly** (floats by bit pattern) → only then delete-from-hot commits (§2's
push-verify-reclaim ordering, verbatim). SIGKILL anywhere ⇒ every row is in hot, cold, or both —
never neither; the harness proves it (40/40 waves, duplicates-at-kill always exactly one batch,
every re-drain converged).

**Where the code diverges from this doc — deliberate, follow the code:**

- **Read-back rides `ATTACH` (#51), not stubs.** This doc predates the shipped cross-file read
  path (`multifile.rs`). v1's documented read-back is
  `ATTACH '<cold>' AS cold; SELECT … UNION ALL SELECT … FROM cold.t` — per-file-consistent
  snapshots, exactly #51's contract. §3's transparent stub/`reference` resolution (and any
  union VIEW over hot+cold) is v2+; v1 refuses nothing silently — the union is an explicit query.
- **Staging reordered**: §7 puts blob-extent `reference` mode first; v1 shipped step 3's ROW
  `drain` core first, because ATTACH read-back made it the smallest correct vertical slice.
  Extent tiering, remote cold stores, re-warm, `[tiering]` config, and the background drainer
  remain unshipped.
- **Transport is the typed row plane, not the mirror.** §2's CDC/provenance transport applies to
  the remote store (v2). For a local sibling file, direct typed writes + the post-commit
  bit-exact read-back give the same "verified landed before reclaim" guarantee without hashing.
- **"Idempotent re-drain" is sharpened by a refusal**: a cold row under the same PK with
  IDENTICAL content is reconciled (delete-from-hot only); with DIFFERENT content the drain
  errors and touches NOTHING — after a crashed drain, hot may have re-used the PK, and without
  content-hash lineage (§6) "overwrite cold" would destroy the archive while "keep cold" would
  destroy the live row. Fail loud is the only markerless answer; no on-disk marker format was
  added in v1.
- **Crash model**: the protocol is SIGKILL-safe at any durability (commits live in the shared
  mmap). POWER-loss safety of the handoff additionally needs the cold handle at
  `durability = commit|wal` — the CLI creates cold files with `commit` by default. A SIGKILL
  during cold-file CREATION can leave a torn (never-READY) file that opens as `Corrupt`;
  nothing has left hot at that point — delete it and re-run.
- **v1 refusals (by name)**: FTS tables, RLS policies on either side (the typed plane runs
  below policies, like the mirror applier), hot DELETE / cold INSERT triggers on the table,
  and any hot/cold table-definition drift. Hot-side CDC capture stays ON for the drain's
  deletes: a mirror of the hot file keeps converging to the hot file, by the mirror's own
  contract.
