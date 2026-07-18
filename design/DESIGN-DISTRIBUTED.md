# DESIGN-DISTRIBUTED — multi-server mpedb: replication mesh, consensus, and the serving topology

**Status: design (2026-07-18). Furthest-out (Phase 7+), layered on
[DESIGN-SERVICE.md](DESIGN-SERVICE.md) + [DESIGN-SYNC-TIERING.md](DESIGN-SYNC-TIERING.md) + the
existing mirror ([DESIGN-MIRROR.md](DESIGN-MIRROR.md)). Capture-ahead: the hard parts here are a
different class of problem than everything mpedb has built, and this doc's job is to be honest about
that and to stage the tractable pieces first.**

## 0. The boundary this crosses — say it plainly

Everything mpedb does today is **single-machine, single-PID-namespace, shared-memory** multi-process
(CLAUDE.md: shm mmap, flock, reader table, boot-id recovery). Across servers *none* of that holds:
no shared memory, no cross-host flock, unsynchronized clocks, and **network partitions are real, not
theoretical**. So distributed mpedb is not "sync, but bigger" — it is a new problem, and the first
duty is the **CAP choice**: under a partition you may keep serving (AP, eventually consistent) or
refuse writes without quorum (CP, linearizable) — never both. Pick per use case; the two models below
each own one side, and conflating them is how distributed systems get silent data loss. In practice
the shape that resolves most of this is **sharded serverless replication** (§6): per-user shards with
one master each, which turns "multi-master" into a set of non-overlapping single-writers — the clean
2a model, cluster-wide, with no conflict machinery.

## 1. The serving topology — the easy, high-value 80%

DNS → several **nginx load balancers** → per-server **mpedb** running Model A (serverless, §1 of
DESIGN-SERVICE). PySpell procs render pages, serve static assets and blobs, answer queries — mpedb
*is* the app + data tier, no separate app server. This part needs **no consensus at all**:
- **Reads scale horizontally for free**: every node serves reads from its local replica; the LB
  spreads load; adding a node adds read throughput. Websites, static data, cached/derived content,
  read-mostly APIs — all of this is just "run Model A on N boxes behind nginx."
- The *only* hard part is **writes + keeping replicas consistent** (§2/§3). Route writes to a
  primary (AP: any node; CP: the leader) and let replication carry them.

So ship §1 first: it delivers distributed *serving* (the website/static/data-hosting use case the
brief describes) with zero distributed-consensus risk.

## 2. Model AP — async replication (eventual consistency; fits the ethos; build this first)

The mirror is **already** the transport and conflict engine: bidirectional CDC, type-provenance,
conflict rules, and a proven convergence guarantee (the `mirror-collide` SIGKILL fuzz: a killed-at-
every-instant mirror daemon's final drain converges *exactly* to the source). Two forms, simplest
first — and the common need (a few-seconds-lagged backup + drain-to-cold) is the simple one.

### 2a. Single-primary async streaming backup — the common case (bounded-lag backup + read replicas)

One node writes; N followers are fed the **continuous intent-log / CDC stream** and apply it with a
small lag. The lag is a config knob — `[replication] max_lag = "5s"` — bounded by the ship interval /
batch window; the intent-ring is already an ordered log, so shipping it is natural. This is exactly
**Litestream (SQLite → continuous backup) / PostgreSQL async streaming replication**, on mpedb's log.
- **No conflict resolution**: a single writer means followers never diverge — they are read-only
  replicas trailing by ≤ `max_lag`. None of the version-vector/CRDT machinery of 2b is needed.
- **RPO = the lag**: on primary failure, promote a follower and lose only the last < `max_lag`
  seconds of un-shipped writes — the deal you accept for availability. For backup + read-scaling that
  is the right trade; no quorum, no leader election, no split-brain (single writer).
- **This is where the write-ack boundary sits.** A request (e.g. a streaming POST, §1a of
  DESIGN-SERVICE) is acked on **local durable commit** — one group-commit fsync, fast — and
  replication to the follower trails asynchronously within `max_lag`, off the critical path. So the
  ack is fast and the only lost-on-failure window equals the RPO. A `sync = backup` knob makes the
  ack wait for the follower to confirm (zero RPO) at the cost of one network round-trip per commit —
  the async default is what a 5–10 s-lag backup wants.
- **The backup node doubles as the cold tier**: the remote follower is also the natural
  [SYNC-TIERING](DESIGN-SYNC-TIERING.md) destination — one remote `.mpedb` is your bounded-lag backup
  AND your drain target for stale data (§5 of DESIGN-SERVICE). Backup and tiering are the same link
  in two directions, which is the topology the brief actually asks for.

This is the tractable, ethos-fitting first build, and it is all a 5–10 s backup delay + drain-to-cold
needs. Failover/promotion policy (who promotes, fencing the old primary to avoid two writers) is the
one careful bit — reuse the existing FLD-2 writer-lock semantics per node, and a manual/`etcd`-gated
promotion (§3) if you want automatic failover without split-brain.

### 2b. Multi-master mesh — the general case (multiple writers, needs merge)

When more than one node must accept writes, generalize 2a with conflict resolution:
- **Topology**: a mesh or ring of mirror links; nodes exchange over the service API (§1 of
  DESIGN-SERVICE) — the "ping-pong" is **anti-entropy gossip**: peers periodically compare per-range
  version state and pull only the deltas.
- **Cheap diffing**: a Merkle/hash tree over key ranges (content-hash + blake3 already exist) makes
  "what differs between us?" an O(log n) tree-walk, not a full-table scan — the same content-hash
  range identity SYNC-TIERING uses.
- **Convergence**: each row carries a version (a Lamport or hybrid-logical clock) + lineage (already
  in type-provenance). Merge rule: last-writer-wins by default; per-column CRDT for commutative
  fields (counters, sets) where LWW would lose increments. Converges after a partition heals — the
  mirror-collide guarantee, N-way.
- **Semantics**: available under partition, may serve slightly stale, converges — the right choice
  for websites, static/derived data, caches, logs, and analytics. This preserves mpedb's no-server
  ethos: nodes are still just files + processes exchanging over an API, none is a required quorum
  member, any can be offline.

## 3. Model CP — replicated state machine (linearizable; advanced; where mpedb's determinism pays off)

The insight that makes this tractable at all: **mpedb plans are content-hashed and deterministic.**
Same plan + same params + same starting snapshot → byte-identical result on every node (deterministic
xorshift RNG, no wall-clock inside plans, and #74's work counter is itself deterministic). That is
precisely the **replicated-state-machine** property — and it is exactly what most databases *cannot*
guarantee (non-deterministic execution order, volatile functions, wall-clock reads).

So: replicate the **ordered intent log** (mpedb's intent-ring / WAL is already an ordered log of
committed transactions), agree on its order across nodes, and **apply it in log order on every node**
→ identical state = linearizable. This is Raft's replicated state machine, with mpedb's determinism
doing the work that would otherwise need deterministic-replay tricks.

**But be honest about the cost:** CP needs quorum + leader election + log agreement (Raft/Paxos) =
**always-on voting nodes** = servers. That is a philosophical shift from the embedded/no-server
contract — the AP model keeps it, CP partly breaks it. Two honest paths:
- Implement a minimal Raft over the intent log (large, and it makes mpedb a clustered server).
- **Pragmatic escape: point at real etcd for the tiny linearizable core.** The set of things that
  *truly* need linearizable consensus is small — leader election, distributed locks, cluster
  membership, the "who is the write primary right now" decision. Use **etcd (or similar) as that
  small CP control plane**, and run mpedb as the **AP data plane** underneath it. You get correct
  leader election without reimplementing Raft, and mpedb stays what it is good at. This is the
  recommended shape unless a use case genuinely needs linearizable *data* writes, not just control.

## 4. The determinism requirement (the load-bearing caveat for §3)

CP-RSM only works if determinism is **airtight**. Any plan that reads wall-clock (`now()`),
randomness (`random()`), or environment diverges replicas. So mpedb must **classify plans as
deterministic vs. volatile**, and volatile inputs must be **resolved at the write primary and shipped
as literal params** in the log entry (exactly how PostgreSQL logical replication handles volatile
functions). The plan Footprint already exists as a static analysis home; a `deterministic` bit
computed at bind time is the hook. Without this, §3 is unsafe — flag it as a hard precondition, not a
detail.

## 5. Staging + recommendation

1. **Serving topology (§1)** — Model A on N boxes behind nginx; read-scaling + static/website/data
   hosting. No consensus. The high-value, low-risk first step.
2. **AP mirror-mesh (§2)** — N-way mirror links + version vectors + anti-entropy gossip + Merkle
   range diffing. Fits the ethos; the main distributed build. Covers the brief's use cases.
3. **CP where genuinely needed (§3)** — prefer *etcd-as-control-plane + mpedb-as-data-plane* over a
   home-grown Raft; only build in-tree consensus if linearizable *data* writes are a hard requirement.
   Gated on §4 (deterministic-plan classification).

All of this is **Phase 7+**, gated far behind the SQL-parity sprint. Each layer is independently
useful; none is a prerequisite for the single-file product. Consensus/replication protocols get
commit-path-class adversarial review (partition, split-brain, log-divergence, clock-skew) before any
line ships — the bar the concurrency core already meets.

## 6. The recommended concrete shape: sharded serverless replication (per-user shards)

The instantiation that makes the whole thing tractable and ethos-fitting — and the one this vision
converges on:

- **Shard = per user** (or per key-range) — naturally a `.mpedb`, or a workspace member (#20/#51
  already model a collection of `.mpedb` members, so the shard primitive half-exists). **One master
  per shard**; different shards' masters live on different servers. So the cluster is "multi-master",
  but every *shard* has a single writer → §2a's clean no-conflict model applies per shard, and 2b's
  version-vector/CRDT merge is **never needed** (non-overlapping writers cannot conflict). This is
  the elegant simplification: multi-master *scale* without the multi-master *problem*.
- **Sharding is the unit of parallelism for serving AND sync.** Servers own different shards' masters;
  load spreads; a shard is small, so bounded lag (§2a `max_lag`) is easy to hold — a follower never
  hangs far behind, so it is always safely promotable ("aldri henge lenge etter").
- **MPEE on sync = precompute on sync.** A shard's root content-hash answers "is this shard dirty?"
  in O(1); ship only dirty shards' deltas, prioritized by staleness/cost — the same cost broker that
  routes SQL batches (#73) routes replication. Sharding turns "what changed?" into O(dirty shards),
  not O(rows); cluster-wide anti-entropy is a shallow tree of shard-root-hashes.
- **Serverless per shard** (§1 Model A): no resident process; a shard is woken on request
  (socket-activation) and its sync woken on change — a "stable replica environment without heavy
  processes." Default role = **read replica** (zero resident); a write burst wakes write-acceptance on
  the shard's master, or promotes a near-current follower — **elastic write reception**, so a server
  contributes capacity only when requests arrive ("ikke trenge service kjørende før det kommer
  requests").
- **The one consistent thing: the shard→master directory.** Which server owns a shard's write master
  must be agreed, or two writers appear. That tiny routing table is *exactly* the small CP core from
  §3 — put it in **etcd / a consistent directory**, NOT the data plane. Data plane = AP/serverless per
  shard; control plane = CP for shard ownership only. Fencing an old master on promotion reuses the
  per-node FLD-2 writer lock.
- **Prior art (not a castle in the air):** Cloudflare D1 / Durable Objects (per-tenant SQLite, one
  writer, wake-on-request), Turso/libSQL per-tenant embedded replicas, Vitess/Citus per-tenant
  sharding — this is the mpedb embedded-file version.
- **Caveats, honestly:** a **cross-shard read** is a scatter-gather over shard members — mpedb's
  cross-file read-join (#51) already does exactly this; a **cross-shard write** needs 2PC or is
  disallowed (per-tenant isolation is usually the right boundary and sidesteps it); **rebalancing** a
  user to another server = ship the shard `.mpedb` + flip the directory entry, cheap precisely because
  the follower was bounded-lag (near-current at cutover).

Recommended build order, refined: §5's staging instantiated as **per-user shards from the start** —
the shard directory (small CP) + §2a async backup per shard + Model A serverless nodes. 2b
multi-master and §3 in-tree consensus stay reserve options for the rare cases per-tenant sharding
cannot express.
