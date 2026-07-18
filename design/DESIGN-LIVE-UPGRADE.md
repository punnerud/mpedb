# DESIGN-LIVE-UPGRADE — zero-downtime schema/model upgrades (rolling, per-tenant)

**Status: design (2026-07-18). Phase 7+, on top of live DDL (#47, [DESIGN-DDL.md](DESIGN-DDL.md)) +
[DESIGN-DISTRIBUTED.md](DESIGN-DISTRIBUTED.md) (sharding §6, async replication §2a, failover §7,
MPEE §8). Its own doc because it is a distinct capability that composes those pieces.**

## 0. The problem

An ORM migration (Django, Rails) can lock tables or rewrite them → seconds of downtime per deploy.
With iterative development — deploys many times a day — those seconds become a recurring, noticeable
cost. The goal: **zero-downtime, rolling, per-tenant schema/model upgrades**, where a bad change is
contained and reversible.

## 1. Foundation: the DDL is already online for one file

Live DDL (#47) already makes `CREATE`/`DROP`/`ALTER TABLE` live and multi-process on the shared
`.mpedb` — other processes see the change on their next statement, no downtime for the DDL itself. So
a single metadata change (add a nullable column, add a table) is *already* zero-downtime. The hard
part this doc addresses is **data migration** (backfill, transform) and **coordinating the switch**
across shards without a stall.

## 2. Expand–migrate–contract (the parallel-change pattern)

Never a big-bang. Three phases where old and new shapes are BOTH valid at every step, so there is no
instant at which a running client is wrong — zero downtime by construction:

1. **Expand** — add the new shape alongside the old (new nullable columns / a new table); remove
   nothing. Old code keeps working.
2. **Migrate** — backfill existing rows into the new shape and **dual-write** new writes to both
   shapes (a trigger, #54-style, or the app), then switch reads to the new shape. The backfill is
   **bounded work per pass** (the #74 work-counter discipline), paced off-peak by MPEE (§5).
3. **Contract** — once nothing reads the old shape, drop it (live DDL).

Prior art: gh-ost / pt-online-schema-change (shadow table + triggers), Stripe's four-phase
dual-write, Reshape (Postgres, view-backed), and Django's own add-then-remove migration guidance.
mpedb's edge: the DDL is already online and single-writer-per-shard makes the dual-write one local
atomic txn, not a distributed transaction.

## 3. Shadow parallel run (the "two in parallel" switch)

For a heavy migration, start the NEW version's processing **in parallel** on a bounded-lag replica
while the OLD master keeps serving. When the migrated replica is caught up AND verified, do a **fast
switch** — the exact §7 failover mechanism, only the target is a schema-migrated replica rather than a
crash replacement: fence the old master (per-node FLD-2 lock + the single-grant lease), promote the
migrated one, accept writes. Zero downtime, and **rollback = switch back** (the pre-migration shard is
still there as a bounded-lag replica). The migration processing itself distributes across
servers/shards (§8).

## 4. Per-tenant rolling upgrade (the feature a shared instance cannot offer)

Because each customer is a shard (§6), migrate **one customer's shard at a time**: migrate A, verify,
then B, C … A **canary**: migrate 1% of tenants first, watch their metrics, then the rest. A bad
migration is **contained to one tenant** and rolled back per-tenant (its pre-migration `.mpedb` is
still the bounded-lag replica — switch back). Tenants can even run **different schema versions
transiently** (A/B schema testing per customer). None of this is possible in one shared
PostgreSQL/MySQL instance where a migration hits everyone at once; here it falls out of per-tenant
sharding for free.

## 5. MPEE-planned switch

MPEE picks **when and in what order**: sense per-shard load, migrate cold / off-peak shards first,
batch by cost, and never migrate a hot shard during its peak. The schedule rides the same cost model
that plans the sharding (§8) and routing — deterministic and attributed ("migrating shard X now; Y
deferred, load high"), the same warn-with-reason discipline as #74's risk estimate. It can also give
a **prepare-time estimate of the whole migration's cost/duration** before you start.

## 6. Correctness + honest limits

- **Reversibility:** a destructive transform keeps the old shape until the new is verified (retain,
  don't delete) — the residual/provenance discipline the mirror and ETL already use (type-provenance
  #26; the reversible-ETL lessons in [ETL-BIDI.md](ETL-BIDI.md)). A migration that cannot be reversed
  is expand-only until contract.
- **Verification before switch:** the migrated shape must be proven equivalent — row counts, then a
  sampled or full compare (the mirror's roundtrip verification) — BEFORE the lease handoff. Fail →
  do not switch; stay on old. Never cut over on faith.
- **Dual-write atomicity:** during Migrate a write must hit both shapes in one txn; single-writer-
  per-shard makes that a local atomic commit, no 2PC.
- **Not free, just not downtime:** backfilling a large table still costs I/O and time; MPEE paces it
  to avoid a load spike, but "zero-downtime" means no *outage*, not no *work*. Say so.

## 7. Staging

1. **Expand-migrate-contract on ONE `.mpedb`** — live DDL + bounded-pass backfill + trigger dual-write
   + verify + read-switch. Already zero-downtime for a single file; no distribution needed.
2. **Shadow parallel run + fast switch** — reuse §7 failover to cut over to a migrated replica.
3. **Per-tenant rolling + MPEE-planned ordering** across shards, with per-tenant rollback.

Phase 7+, behind the SQL-parity sprint and the distributed foundation. The switch/fencing path gets
commit-path-class review (two-writers-during-cutover, partial-backfill crash, verify-then-switch
ordering) before any line ships.
