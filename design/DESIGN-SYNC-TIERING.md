# DESIGN-SYNC-TIERING — cold-data tiering + drain-and-read-back between `.mpedb` files

**Status: design (2026-07-18). Forward-looking (Phase 6+), sequenced AFTER the SQL-parity sprint.
Built on the existing mirror (bidirectional CDC + type-provenance), extents/blobs (#50), and the
HTTP-range/FrozenDb direction (#22). Pairs with [DESIGN-SERVICE.md](DESIGN-SERVICE.md): the drainer
is a scheduled service task, read-back is a callout.**

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
