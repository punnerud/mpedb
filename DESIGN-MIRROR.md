# DESIGN-MIRROR: bidirectional sqlite3/PostgreSQL ⇄ mpedb mirroring

Status: **v1.1** — review-hardened. v1.0 draft survived an 8-lens adversarial
review (73 agents); **58 confirmed defects + 6 partials folded in**. The
correctness rules below are load-bearing; the "Review-driven invariants" boxes
mark protocol details a naive implementation gets wrong.
Scope: `mpedb mirror` — import a live sqlite3 file or PostgreSQL database into
`.mpedb`, use mpedb standalone for a long time (days) while both sides keep
writing, incrementally diff-pull from the source **under concurrent source
write load**, write local changes back (push), and switch which side is
authoritative — in both directions, repeatably.

## Changelog v1.0 → v1.1 (what the review changed)

1. **Capture suppression** is mandatory and was missing: the applier/importer's
   own writes were self-captured → echo storms + `park_overflow` wedge. Added an
   engine-level per-WriteTxn suppress flag (§3.8).
2. **The control plane (freeze, HALTED, reserved pages, quiesce) moved to the
   engine** — v1.0 enforced freeze in the facade, which §3 itself proves is
   bypassable. Now a generic `cdc` write-block bit + reserved page pool (§3.9,
   §3.10).
3. **Push high-water protocol rewritten** — v1.0's `applied_high_water=H` +
   blanket step-1 clear silently lost local writes across bounded batches. Now
   high-water advances only at full-coverage, split out of data txns (§6).
4. **Pull apply restructured to DECIDE-before-mutate** — v1.0's
   delete-then-insert destroyed the local row before parking was decided (§5.4).
5. **Switch is reconcile-then-verify, gated on both arrows**, cursor baseline
   pinned at the verify snapshot, quarantine/park excluded (§7).
6. **Fixed-size dirty key** (blake3 of the keycode) removes the unenforceable
   900 B PK cap (§2, §3.7).
7. Many fidelity + adapter fixes: invalid-UTF-8, PG `infinity`, timestamp
   conventions, collation on unique columns, injective checksum encoding, sha256
   (FIPS), xmin torn-reads, TRUNCATE, xid-window push conflict detection,
   sequence rewind, multi-replica scoping, regenerate flow. Inline below.

---

## 0. Honest scope (read first)

- **State-based mirror**, not an op-log replicator. We converge each side to the
  other's latest row state per PK; intermediate states between rounds are not
  preserved. Readers never observe a torn source transaction **when the source
  supplies commit boundaries** (PG) — see §5.4 for the sqlite caveat and the
  oversize-txn path. Several source txns may collapse into one mpedb commit.
  Cross-database atomicity between the pair does not exist. Guarantee: *each side
  only ever shows a consistent cut of the other* (PG); *sqlite sub-batches within
  one pull may be transiently reader-visible* (§5.4 caveat).
- **Single authoritative side with epoch-fenced, repeatable switch**, not
  multi-master. Both sides accept writes in steady state; "authority" = default
  conflict winner + which switch arrow is legal next.
- The sync applier operates at the **engine typed API level — below RLS** (a
  replication-superuser plane). Policies gate sessions, not replication.
- **v1 mirrors one authoritative source per .mpedb file.** Consumer topology
  (§9): EITHER exactly one `rw`/`push_only` consumer, OR N `pull_only` consumers
  — enforced at `mirror init`. A Workspace of N mirrored files = N independent
  mirror instances (cross-file commits have no atomicity — DESIGN-MULTIDB §1.5).
- Fencing of third-party source writers during a switch is **cooperative** unless
  the opt-in fence trigger is installed (privileged, §6). No-touch mode cannot
  fence — its switch is advisory, and its divergence heals only via reconcile
  (§7). A persistent rogue source writer can livelock a switch — surfaced in
  `mirror status`, the honest limit of cooperative fencing.

## 1. Architecture

```
crates/mpedb-mirror        new crate: adapters + protocol + type mapping
  src/adapter.rs           source-agnostic Adapter trait (pull/push/state)
  src/sqlite.rs            rusqlite 0.31 (bundled 3.45) adapter
  src/pg.rs                postgres 0.19 (sync) adapter
  src/protocol.rs          state machine, epoch fencing, conflict engine
  src/mapping.rs           schema introspection → TableDef, type policy
  src/checksum.rs          injective canonical encoding + merge-diff (§5.5)
  src/state.rs             mir/* + cdc/* sys-record codec (bounds-checked)
crates/mpedb-core          generic CDC primitive: dirty-set capture, per-txn
                           capture suppress, write-block bit, reserved page pool
                           (the ONLY core change; no mirror logic)
crates/mpedb-cli           `mpedb mirror init|import|pull|push|sync|status|
                           switch|verify|reconcile|conflicts|regenerate|detach`
```

Deps: rusqlite 0.31 (bundled) + postgres 0.19 (sync) — both already proven in
`crates/mpedb-bench`. No new dependency tier for v1 (v2 slot adapter adds
`pg_walstream`, §5.6).

## 2. mpedb-side state layout

Two sys-keyspace namespaces (facade sys-record convention `ns ++ 0x00 ++
subkey`; prefix-disjoint from `plan/`, `pol/`, `rlsen/`, `polep/`, `proc`):

**`cdc` — the generic CDC primitive owned by mpedb-core (knows nothing about
mirroring):**

| key (after `cdc\0`) | value |
|---|---|
| `tabs` | capture control record, read by the ENGINE at write-txn begin: captured table_id list ‖ **write-block table_id list** (freeze) ‖ generation u64 |
| `d/<table_id BE4><blake3(pk keycode) 32B>` | **dirty entry** (fixed 43 B key): op u8 (1=upsert, 2=delete) ‖ last_txn u64 BE ‖ wall_us i64 BE ‖ **pk keycode bytes** (carried in value — removes the PK-length cap, §3.7) |

**`mir` — mirror state (owned by mpedb-mirror):**

| key (after `mir\0`) | value |
|---|---|
| `cfg` | mirror_id, source_kind u8, mode u8 (tracked/notouch), canonicalization_id u32, scope (included tables), flags |
| `epoch` | epoch u64 BE ‖ authority u8 ‖ state u8 ‖ frozen u8 |
| `cur` | adapter-opaque pull cursor — sqlite: **per-table** (table_id→seq vector, §5.1); pg: full previous snapshot `xmin:xmax:xip_list` (§5.2) |
| `map/<table_id BE4>` | source name, per-column type policy + no-writeback mask, **unenforceable_unique descriptors**, mode u8 (rw/pull_only/push_only), mpedb schema blake3, source schema fingerprint, pk-lease [lo,hi] |
| `park/<seq BE8>` | conflict record: kind u8, table_id, pk, source image, local image, cursor-at-detection, epoch, wall_us (images >500 KiB spill to sidecar) |
| `skip/<table_id BE4><blake3(pk)>` | manual-policy apply-skip marker (§8) |
| `imp/<table_id BE4>` | import resume watermark (last imported PK keycode) |
| `lease` | daemon lease: pid u32, boot_id 16 B, expires_wall_us i64 |

> **Review-driven invariant (CONF#19/26/49): the dirty key is fixed-size.**
> A Text/Blob/composite PK keycode is value-dependent and can reach MAX_KEY
> (976 B); embedding it in the `d/` key would overflow the btree at the *first
> replicated write* of a long key (unenforceable "cap at import time"). So `d/`
> keys are `table_id ‖ blake3(keycode)` (43 B, always legal) and the raw keycode
> lives in the **value** (push needs it anyway to re-read the row). blake3 is
> deterministic → coalescing preserved. Only residual limit: the row's own PK
> keycode must fit MAX_KEY on both the data tree and, when captured, nothing
> extra — no artificial reduction remains.

## 3. Capture plane: engine-level CDC (mpedb-core)

**The hook must be in the engine, not the facade** (verified write-path
inventory): a facade hook misses the ring leader executing OTHER processes'
intents (ring_exec.rs:261-272, 730), WriteSession::run (lib.rs:850), raw
mpedb-core users (crash.rs:147), and the **optimistic blind-apply trio**
`optimistic_insert/upsert/delete` (engine.rs:914/931/945) — concurrency is
per-process config, not file-frozen (validate_frozen shm.rs:1983-2035).

1. **Hook all SIX mutators**: insert_row (957), update_by_pk (1053),
   delete_by_pk (1021), optimistic_insert/upsert/delete (914/931/945). Each has
   `table_id` + encoded PK keycode in scope; the hook is one inline sys-put.
2. **Capture flag is file-resident** (`cdc\0tabs`), read at write-txn begin,
   cached per process, invalidated by the generation counter. A ring leader
   without the flag would silently skip capture, so it can never be per-process.
3. **Belt-and-braces**: mirrored tables excluded from `optimistic_eligible`
   (ring_exec.rs:369) as `has_secondary_index` already is.
4. **Savepoint discipline**: dirty entries are btree-resident at mutation time
   (TxnSavepoint captures catalog_root, engine.rs:1266/1503) → ring-intent
   rollback unwinds them. Never buffer in memory.
5. **Commit protocol untouched** — a dirty entry is an ordinary COW page.
6. `last_txn = self.meta.txn_id + 1` (deterministic under the writer lock).
7. **UPDATE-of-PK** is delete+insert → two dirty entries (tombstone old, upsert
   new). Source PK changes arrive the same way (§5), applied delete-then-insert.

### 3.7 Fixed-size dirty key
See §2 box. `d/<table_id><blake3(keycode)>`; keycode in value.

### 3.8 Capture suppression (replication plane) — CONF#1/8/14/21/43

> **Review-driven invariant: the applier and importer must NOT self-capture.**
> Every applier write goes through the six hooked mutators; without suppression a
> pull of N rows creates N dirty entries → the next push echoes them to the
> source, false conflicts flood `park/` → `HALTED(park_overflow)` in plain
> follow mode; and after a switch every pulled row counts as "unpushed local
> dirty."

Add `WriteTxn::set_capture(false)` — a **transient in-memory** flag on the txn
(dies on rollback/abort; never file-resident), a generic CDC control (core stays
mirror-agnostic). Set only by the mirror's replication-plane WriteSession
(applier + importer + push write-backs). The applier holds the writer lock
directly (never rides the ring — §5.4), so no foreign leader executes an applier
txn and no file-resident visibility is needed. Under suppression the six hooks
skip the `d/` sys-put entirely (no COW churn). Import writes `cdc\0tabs` **only
in its final commit** (atomically with `cur`/`epoch`) so no import batch is
captured.

### 3.9 Engine-level write-block / freeze — CONF#5/10/17/47

> **Review-driven invariant: freeze must be engine-level, not facade.** v1.0
> enforced freeze in the facade — exactly the layer §3 proves cannot see all
> writes. A raw-core writer or a ring intent posted pre-freeze and leader-drained
> post-freeze would land a mirrored-table write between the S7 "zero dirty" check
> and cutover, falsifying "no writes leak" and the verify gate.

Fold a **write-block table_id list** into `cdc\0tabs`. The six mutators refuse a
mutation targeting a write-blocked table with `Error::Frozen` (per-mutator, not
just at txn begin — a leader's batch legitimately mixes blocked and unblocked
tables; each queued intent then fails alone under its statement savepoint and
returns to its waiter). Generation-invalidated cache; honored only from the
write txn's own snapshot under the writer lock (never a pre-lock read snapshot).
Facade check remains a fast-path courtesy. The mirror sets/clears the bit in the
same txn as `mir\0epoch.frozen` (both sys keys, one atomic commit) and bumps the
generation.

### 3.10 Reserved page pool for control writes — CONF#44

> **Review-driven invariant: DbFull handling cannot itself need data pages.** At
> exhaustion no ordinary write txn commits (COW frees are not in-txn reusable),
> so writing `HALTED(db_full)`, `frozen`, park-count, or the cursor would fail —
> the recovery state is unreachable exactly when needed.

Engine carves a small fixed **reserved page quota** (32-64 pages) dispensed by
`alloc()` only in `sys_reserved` mode, used by control writes (epoch/HALTED,
write-block, park-count, cursor). Capture dirty-entries and data go through the
**normal** pool, so data pressure can never starve the ability to record
HALTED/frozen. Park *detail* spills to the sidecar; only the tiny marker uses the
reserved pool. `db_full` becomes a first-class recovery via `mirror regenerate`
(§7, §11-M6). (mpedb-core change; subject to DESIGN.md commit-path review.)

## 4. Import (initial full copy)

### 4.1 Common
- Programmatic schema build (all-pub `TableDef`/`Schema`); identifier mangling
  recorded in `map/`.
- **Scope selection (CONF#57)**: `mirror init --include t1,t2 | --exclude ...`,
  recorded in `cfg`/`map`. Subset mirroring of a large source needs no sharding;
  FK co-location advice applies only within the selected set. Adding a table
  later = `mirror regenerate` (§7).
- ≤ **56 user tables/file**; larger schemas shard across Workspace members
  (per-member cursors/epochs/switch; no cross-member queries or atomic commits).
- **File sizing (CONF#44)**: not `k×import-size` but
  `(steady-state source+local growth rate × expected authority lifetime) + COW
  churn + dirty-set headroom + largest-single-source-txn COW footprint`.
  `mirror status` surfaces a pages-used% watermark with a pre-full warning.
- Bounded per-table batches, each writing `imp/<table>` atomically → SIGKILL-safe
  resume (idempotent upsert).
- **Unique mapping (CONF#57)**: single-column, non-partial, non-expression,
  deterministic/BINARY-collation UNIQUE → mpedb `unique` column. Composite UNIQUE
  == PK → composite PK. Every other non-PK unique (composite/partial/expression/
  non-deterministic-collation) is **unrepresentable** (mpedb secondary uniques
  are single-column memcmp, engine.rs:109-119) → recorded as
  `unenforceable_unique`, table forced `pull_only` unless the operator opts into
  `rw` (which then blocks switch-to-mpedb for that table until acknowledged).
- **Views**: pull_only via merge-diff when they expose a stable key; else reject.
  Trigger/tracked capture cannot apply to views.
- Every import emits a **report** (row counts, type decisions, per-row
  violations + samples, PK outcomes, unenforceable uniques, mangled idents).

### 4.2 sqlite recipe
1. (tracked) Install changelog + triggers FIRST, one txn — no capture gap.
2. Read connection: `busy_timeout`; `BEGIN DEFERRED`; first SELECT pins the
   snapshot (WAL end-mark).
3. Read per-table watermark W inside the snapshot (**per table** — §5.1).
4. Stream `SELECT * FROM t ORDER BY <pk>` → typed decode → batched inserts.
5. Final commit stores per-table `cur` (+ cfg/map/epoch), and **`cdc\0tabs`
   (§3.8)**.

Under load: long WAL read blocks checkpointing (-wal grows ≈ writer volume ×
duration; drains after). Big DB: `VACUUM INTO` a copy. Rollback-journal source:
opt-in `journal_mode=WAL` / VACUUM INTO / accept small-DB stall.

### 4.3 PostgreSQL recipe
> **Review-driven step 0 (CONF#32): install the changelog + every AFTER ROW
> trigger and COMMIT them BEFORE `pg_export_snapshot()`.** Installing after the
> snapshot loses writes by txns in-flight at snapshot time. Wrap CREATE TRIGGER
> with `SET lock_timeout` + bounded retry (SHARE ROW EXCLUSIVE stalls behind hot
> writers). Per-table install-then-commit (each before the snapshot) is fine and
> narrows the lock window.

1. Step 0 above.
2. Coordinator `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`;
   `pg_export_snapshot()`.
3. Pin the first-pull cursor: `pg_current_snapshot()` **captured inside this
   snapshot** = the `cur` baseline (§5.2 consecutive-snapshot windows).
4. Workers `SET TRANSACTION SNAPSHOT '<id>'`; `COPY (SELECT ...) TO STDOUT
   (FORMAT binary)` via `BinaryCopyOutIter` (lossless bytea + µs).
5. Final mpedb commit stores `cur`/map/epoch + `cdc\0tabs`.

Requires a **direct (non-pooled) DSN** (export-snapshot + replication + GUC
residue all need session continuity — §6). Multi-hour RR holds back vacuum
(bloat warning in report).

### 4.4 PK policy

| source shape | policy |
|---|---|
| sqlite `INTEGER PRIMARY KEY` | Int64 PK — VACUUM-stable |
| sqlite declared PK (rowid table) | use it; NULL-in-PK rows → quarantine (import hard-fails w/ list unless quarantine chosen) |
| sqlite WITHOUT ROWID / STRICT | map directly |
| sqlite no declared PK | import-only synthetic `_rowid`; **rejected for mirror** (VACUUM renumbers). Promote a unique index iff all-NOT-NULL, non-partial, non-expression, BINARY |
| non-deterministic/non-BINARY collation on PK **or any promoted/unique column** (CONF#40) | reject for mirror mode (weaker equality than memcmp) |
| **value-transforming type policy on a PK** (CONF#25) | forbidden — a PK policy must be injective AND order-isomorphic to the source's compare order. TEXT→Timestamp is neither (format/offset variants collapse & invert order) → falls back to raw Text; Float64-for-NUMERIC forbidden on PK; INTEGER-epoch and REAL-Julian are OK |
| PG with PK | map directly |
| PG without PK / REPLICA IDENTITY NOTHING | reject; pre-flight also protects the source (publishing update/delete for such a table makes source UPDATE/DELETE fail in v2) |

PK generation for local inserts (per-table, at init): **range lease** (default
int PK; burn a block via nextval×N, record [lo,hi] in `map/`; renew at 80%;
**forfeited at S8**, §7); **UUID/text** (no coordination); negative-ids+remap
(discouraged). PG identity → `OVERRIDING SYSTEM VALUE`.

### 4.5 Type mapping

mpedb: Int64, Float64, Bool, Text, Blob, Timestamp(i64 µs Unix UTC), NULL.

**sqlite** (affinity + declared-type sniff): BOOL→Bool (0/1/NULL only);
DATE/TIME→Timestamp (convention **frozen** per column, magnitude sniff is a
**hard import gate** on contradiction — CONF#42 — not a suggestion; for `rw`
tables prefer Int64-raw units to keep round-trip byte-exact); INTEGER→Int64;
REAL→Float64; TEXT→Text; BLOB/none→Blob; NUMERIC→Float64 + 2^53 precision guard
(override Int64/Text).

> **Invalid UTF-8 (CONF#36):** mpedb Text is a Rust `String`; sqlite TEXT can
> hold invalid UTF-8. Extract every TEXT column as **raw bytes** (`get_ref` →
> `ValueRef::Text(&[u8])`), validate in the bridge. Invalid → violation class
> `invalid_utf8` (strict-reject at import / quarantine at pull / coerce =
> whole-**column** Blob remap in `map/`, never `from_utf8_lossy`, never per-row).
> TEXT PK/unique columns scanned for validity at import.

**PostgreSQL:** int2/4/8→Int64; float4/8→Float64; bool→Bool; text/varchar→Text;
bytea→Blob; **timestamptz→Timestamp exact** (offset 946_684_800_000_000, but see
infinity box); timestamp(naive)→Timestamp UTC; date→Timestamp UTC midnight;
time→Int64 µs; numeric→Text default (see normalization box) / Float64 opt-in
(not PK) / s0,p≤18→Int64; uuid→Blob16 (Text36 opt-in); jsonb→Text
(`jsonb::text`); json→Text (raw — note it is NOT canonical, checksum uses
`::jsonb::text`); enum→Text; domain→base; array/composite/range/interval/money/
inet/citext→Text opt-in or reject.

> **PG infinity / range overflow (CONF#39):** timestamptz `infinity`/`-infinity`
> arrive as i64::MAX/MIN; adding the offset overflows. date reaches 5874897 AD.
> Read raw i64/i32, use **checked arithmetic**; sentinel/overflow → per-row
> violation, or per-column opt-in mapping the two sentinels to reserved mpedb µs
> points that push back as ±infinity (those points then unusable as ordinary ts).

> **Normalization divergence (CONF#35):** `numeric`-as-Text / `char(n)` / `jsonb`
> are lossless only for *source-origin* values. A local write of `'3.1'` into
> `numeric(10,2)` pushes `'3.1'::numeric` → source stores `'3.10'`; echo
> suppression blocks the normalized value from returning → permanent divergence +
> anti-entropy re-flag storm. **Fix:** the push upsert uses
> `... DO UPDATE ... RETURNING <col>::text` (canonical form); the step-4 mpedb txn
> writes the canonical bytes back into the row **with capture suppressed** so both
> sides converge. Restated **fidelity rule (CONF#35/42):** an `rw` column requires
> `store∘push == identity` on ALL mpedb-representable values (not just proven
> round-trip of source-origin values). Lossy/normalizing columns without a
> registered canonicalizer → `pull_only`.

Generated columns mirror values but are excluded from write-back (no-writeback
mask). Float64 diffs compare with NaN-normalization + one canonical zero.
Embedded NUL in mpedb Text is legal locally but rejected by PG text → push park
kind `nul_in_text` (CONF#36).

## 5. Pull (incremental diff under load)

### 5.1 sqlite tracked mode
Per table `t`: `_mpedb_log_t(seq INTEGER PRIMARY KEY AUTOINCREMENT, op, origin,
<pk cols>)` + AFTER INSERT/UPDATE/DELETE triggers (UPDATE emits tombstone+upsert
on PK change via `IS NOT`). AUTOINCREMENT mandatory. Payload PK-only; pull
re-reads current state from one snapshot.

> **Per-table cursor (CONF#6):** sqlite AUTOINCREMENT counters are per-table, so
> `cur` is a **per-table (table_id→seq) vector**, not one scalar. GC watermark,
> `pull_log_head`, and the push-conflict watermark are likewise per table.

Round: snapshot read → per-table entries `(W_last, W_now]` → coalesce per PK →
re-SELECT current rows → apply (§5.4) → GC `DELETE ... WHERE seq <= min(consumer
watermarks)` (single-consumer in v1 unless all-pull_only, §9).

Idle gating: one persistent monitor connection polls `PRAGMA data_version`.

> **Restore guard (CONF#24):** each round read live `sqlite_sequence.seq` per log
> table (`S_live`, monotone under AUTOINCREMENT+GC). `S_live < stored cur` (or the
> row is absent while cur>0) → `HALTED(source_restored)` (an older backup of the
> same file was restored; UUID identity can't catch same-lineage restore). Never
> auto-resolve source-wins.

**Verified holes → anti-entropy is mandatory** (§5.5 runs periodically AND before
every switch): `INSERT OR REPLACE` colliding on a *secondary* unique deletes a
different PK without firing delete triggers (recursive_triggers off default);
out-of-band file replacement; FK cascade trigger firing (pinned by test M8).

Rejected primaries (verified): sqlite3session (single-connection),
data_version-alone, max(rowid)/sqlite_sequence, WAL-frame scanning.

### 5.2 PostgreSQL trigger mode (v1 primary) **[DECISION, revised]**

v1 ships the trigger-changelog (not slots): the user's defining scenario is *days
offline* where slots are lethal (unbounded WAL retention → source disk fill;
`max_slot_wal_keep_size` invalidation; PG18 idle timeout; **Neon reaps inactive
slots after ~40 h**). But the review confirmed the trigger changelog **shares the
unbounded-retention risk** (CONF#48/56) — mitigated, not free (see cap-and-resync
below). Slots are v2 (§5.6).

Schema `mpedb_mirror`: `changelog(seq bigserial PK, tbl text, op "char", pk
jsonb, txid int8 DEFAULT txid_current(), origin text, at timestamptz)` + one
generic plpgsql AFTER ROW trigger per table. **Trigger objects (CONF#28/45):**
- `ENABLE ALWAYS` (fires under `session_replication_role=replica`,
  `pg_restore --disable-triggers`).
- Installed on the **partitioned parent** (auto-propagates to future partitions).
- `SECURITY DEFINER`, owned by a privileged role; `REVOKE ALL ON mpedb_mirror.*`
  and `_mpedb_mirror_state` from app roles (else forged tombstones delete data,
  or forged epoch bumps wedge).
- **`AFTER TRUNCATE FOR EACH STATEMENT`** trigger writing `op='T'` (row triggers
  don't fire on TRUNCATE) → pull treats `'T'` as a forced full-table re-diff.
  Partition ATTACH/DETACH/DROP also emits no row events → detected via the
  partition list in the schema fingerprint → same forced re-diff.
- `tgenabled` + partition list are in the schema fingerprint → `DISABLE TRIGGER`
  and partition maintenance become drift signals.

> **Cursor = consecutive-snapshot windows (CONF#30), not `txid < xmin`.** The
> naive `WHERE txid < pg_snapshot_xmin(S)` window is strictly earlier than the
> snapshot S at which images are re-read → publishes **torn transactions**.
> Store the FULL previous snapshot in `cur`; each round:
> `WHERE pg_visible_in_snapshot(txid, $snap_now) AND NOT
> pg_visible_in_snapshot(txid, $snap_prev) ORDER BY seq`; re-read images at
> `snap_now`; persist `snap_now` as the new `snap_prev` in the same apply txn. GC
> deletes only rows covered by a committed `snap_prev`. (Restore guard: verify
> `snap_now` xmin/changelog max has not regressed below stored, else
> `HALTED(source_restored)`.)

Echo suppression: the trigger **always inserts**, storing `origin :=
NULLIF(current_setting('mpedb.mirror_origin', true), '')` (CONF#29 residue-safe;
a mirror_id or NULL). Each consumer's pull filters `origin IS DISTINCT FROM
<own mirror_id>` — suppresses its own echoes while still seeing every OTHER
consumer's writes (CONF#45/51).

> **Cap-and-resync (CONF#48/56):** a source-side job (or the trigger) enforces a
> row/byte/age cap; on overflow write `log_truncated_at` into
> `_mpedb_mirror_state` and truncate the tail. Next contact: stored `cur` below
> the surviving head → `HALTED(log_overflow)` → §5.5 checksum resync → re-seed
> cur. Reclaim via partitioned changelog (DROP old partitions), not row DELETE.
> `mirror offline --expect >N h` installs/verifies the cap proactively. In S6
> M_AUTH, actively advance the GC watermark / drop-recreate the changelog (pulls
> are refused — it won't self-drain). This is the trigger-mode analogue of the v2
> slot-lost transition; §9 has a row.

`updated_at` diff: supported-but-discouraged (misses deletes/late commits);
needs a tombstone trigger; else degrades to §5.5.

### 5.3 No-touch mode & the merge-diff — see §5.5.

### 5.4 The apply transaction (exactly-once pull) — DECIDE before mutate

> **Review-driven invariant (CONF#2/18/23/52): decide parking BEFORE any
> destructive mutation.** v1.0's delete-phase-then-insert-phase deleted the local
> row, THEN discovered the incoming row violates a CHECK / is unique_blocked, and
> parked — committing a state where the row is *absent on both sides' history*.

One mpedb WriteTxn per batch (**capture suppressed**, §3.8; boundaries align to
source commits — PG; see oversize box):

1. **Cursor guard**: read `cur`; ≠ batch.start → abort.
2. **Epoch guard**: read `epoch`; mismatch or write-blocked → abort.
3. **DECIDE phase (no mutations)**: snapshot the pre-batch `d/` entries for the
   batch's PKs (conflict detection reads THIS, not post-mutation state). For each
   upsert compute: (a) `validate_row_public` (types/NOT NULL/CHECK); (b) conflict
   class via the pre-batch `d/` probe + §8 policy; (c) unique feasibility against
   the **post-batch** index state (current − vacated-by-tombstones −
   vacated-by-deletes-of-existing-upserts + intra-batch inserts), iterated to a
   **fixpoint** (a retained parked row keeps its old unique value, which can
   newly block another upsert). Any upsert that fails validation, is
   unique-infeasible, is policy-parked (manual/local-wins), has an oversize PK
   keycode (`unmirrorable_key`), or is `pk_too_large` → **PARKED set**, excluded
   from both phases, park record written here.
4. **DELETE phase**: delete_by_pk for tombstones + surviving (non-parked)
   upserted PKs that exist.
5. **INSERT phase**: insert_row for survivors (guaranteed to pass;
   UniqueViolation is a hard backstop, not a silent park at this stage).
6. `sys_put(cur = batch.end_cursor)` — same txn.
7. Commit — one meta flip.

Idempotent replay: delete-then-insert by PK; DECIDE-phase feasibility is against
post-delete state so legitimate unique swaps still apply order-free. Poisoned
session → rollback + retry whole batch.

> **Oversize source txn (CONF#12):** a 1M-row source txn cannot be one 5k-bounded
> mpedb txn. **PG** (txn boundaries known): keep never-split, but a **pre-flight
> COW-footprint estimate against LIVE free pages** ((page_count − high_water) +
> reclaimable freelist) → `HALTED(db_full)` with "source txn of N ops needs ~P
> pages; F free; resize to ≥X" BEFORE taking the writer lock (never stall writers
> for a doomed apply). **sqlite** (no txn metadata): §0 downgraded — a pull's
> sub-batches are transiently reader-visible; `cur` gains a mid-snapshot seq
> position so a crash mid-pull resumes without re-tearing.

### 5.5 The merge-diff / checksum engine (no-touch, anti-entropy, verify gate)

**One algorithm** serves no-touch pull, periodic anti-entropy (mandated for BOTH
engines — CONF#28), slot-loss/log-overflow resync, and the switch verify gate
(resolves the v1.0 §5.2-vs-§7 contradiction — CONF#31/41).

> **Injective encoding (CONF#41):** v1.0's `chr(1)`-joined, `\N`-NULL encoding
> was injectable via ordinary text. Per column emit `tag_byte ‖ be4(octet_length
> (canon)) ‖ canon` (canon = type-specific bytea via `convert_to(v,'UTF8')`);
> NULL = distinct tag, zero length. Hash each row to a fixed-width digest, THEN
> aggregate digests (empty separator on fixed-width is unambiguous). Every mapped
> type's canonical bytes are defined in §4.5 and golden-file cross-tested vs live
> PG; versioned by `canonicalization_id` in `cfg`.

> **sha256, not md5 (CONF#34):** md5() fails on FIPS-mode PG. Use
> `sha256(convert_to(...,'UTF8'))` (built-in since PG11, FIPS-clean).

> **COLLATE "C" everywhere + full coverage (CONF#7/31):** chunk boundaries are in
> mpedb's memcmp keycode order; PG evaluates ranges/`string_agg ORDER BY` under
> the column collation (non-monotone vs memcmp → rows fall in zero chunks). Force
> `COLLATE "C"` on every text-PK comparison and ORDER BY (per-column for
> composite PKs). Add open-ended `(-inf, b1]` and `(bn, +inf)` chunks (append
> workloads). `LEFT JOIN LATERAL ... ON true` so empty chunks are distinguishable
> (mass deletes detected). The switch gate reads every row (source cannot emit
> mpedb row-codec bytes) — either the bridge re-encodes source rows in Rust
> (`blake3(encode_row)`), or this injective SQL encoding runs on both sides.

Boundaries pushed from mpedb's PK order (~5000/chunk); equal chunk → skip;
mismatch → per-PK hash list → fetch changed rows by PK.

### 5.6 PostgreSQL slot mode (v2, feature-gated)
pgoutput via `pg_walstream` 0.7 behind a decoder trait; embedded tokio runtime.
TupleData `'u'` (unchanged TOAST) merged with the local row. Exactly-once via
FLUSHED = the LSN in `cur`. Slot-lost → §5.5 resync → recreate. Direct DSN;
RDS `rds_replication`; origins only as superuser.

## 6. Push (write-back)

> **Review-driven invariant (CONF#0/13/15/20): `applied_high_water=H` may advance
> ONLY at a full-coverage point.** v1.0 wrote H at the start of each bounded batch
> and blanket-cleared `d/ ≤ H_ack` — the PK-ordered batch covers only a subset of
> the ≤H set, so the clear wiped never-pushed local writes (permanent loss).

Round structure (at-least-once + idempotent; **capture suppressed** on the mpedb
side):

1. **(No blanket clear.)** Step 1's v1.0 recovery clear is deleted — it was a
   pure optimization and the source of the data-loss bug. Crash recovery is
   handled by idempotent re-push (step 4 clears pushed keys).
2. Pin mpedb ReadTxn; H = `txn_id`. Scan `d/` (prefix-bounded) with last_txn ≤ H;
   read upsert images **from that snapshot**; **buffer the batch, then drop the
   ReadTxn BEFORE any source I/O** (CONF#50 — a hung source must not pin a reader
   and starve reclamation). Bounded batches.
3. Per-batch **source txn (READ COMMITTED)** carrying ONLY the epoch fence
   (`WHERE epoch=$e` on `_mpedb_mirror_state`; 0 rows → fenced → ROLLBACK). Per-op
   apply, each wrapped in a **SAVEPOINT** (CONF#38 — not just unique retries): on
   ANY error `ROLLBACK TO savepoint`, classify a `push_rejected` park kind
   (SQLSTATE), continue the batch, commit survivors. Native upsert
   (`ON CONFLICT (pk) DO UPDATE`; never `INSERT OR REPLACE`); FK-topological op
   order (parent upserts before children; child tombstones before parents);
   import expressible source CHECKs into mpedb so they reject at local-write time.
   sqlite: `BEGIN IMMEDIATE` + `PRAGMA foreign_keys=ON`.
4. mpedb WriteTxn: delete the dirty entry for each **successfully pushed** key iff
   last_txn ≤ H (re-dirtied at >H survives; parked keys' entries survive for
   retry — CONF#13). Batched (CONF#50).
5. After the WHOLE ≤H set is drained, a **FINAL source txn** does the CAS
   `UPDATE ... SET applied_high_water=$H WHERE epoch=$e AND applied_high_water<$H`.
   0 rows: re-read to distinguish **fenced** (epoch mismatch → abort/HALT) from
   **already ≥H** (idempotent replay → success). Only now is any H_ack-based
   reasoning sound; since step 1 is gone, `applied_high_water` is effectively
   advisory/status.

> **Push conflict detection = xid-window + lock-then-check (CONF#11/27):** v1.0's
> `seq > W_last_pulled` under READ COMMITTED without a row lock silently
> overwrote concurrent/committed source writes. **PG:** take the row lock first
> (the upsert/delete does, or `SELECT ... FOR UPDATE`), THEN in a fresh statement
> check `EXISTS(SELECT 1 FROM changelog WHERE pk=$p AND txid >= $from_xid AND
> origin IS DISTINCT FROM $self)` (xid-window consistent with the pull cursor).
> **sqlite:** `BEGIN IMMEDIATE`'s single writer makes check-then-write sound;
> per-table seq watermark.

**Unique swaps**: savepoint-per-op fixpoint; a true cycle tries
`SET CONSTRAINTS ALL DEFERRED` (if DEFERRABLE) else parks `unique_blocked`.

**Echo suppression**: PG — `SET LOCAL mpedb.mirror_origin='<id>'` (residue-safe
consumer-side filter, §5.2); the daemon requires a **direct (non-pooled) DSN**
(CONF#29 — pooling both breaks GUC scoping and leaves the session advisory lock
dangling). sqlite — tag `origin=<mirror_id>` on the pusher's own log rows
(`seq > premax`), consumer filters own id; GC own-echo rows in S6.

> **Sequence rewind (CONF#33):** `setval(seq, max(pk))` can rewind below leased/
> in-flight values → post-switch duplicate keys. Use `setval(seq,
> GREATEST(COALESCE(max(pk),0), COALESCE(pg_sequence_last_value(seq),0)), true)`
> (never move backward); run under the fence trigger / `LOCK TABLE ... EXCLUSIVE`
> (read-then-setval is racy); **forfeit the range lease** in `map/` at S8; sqlite
> `UPDATE sqlite_sequence SET seq = MAX(seq, ...)`. §9 row.

## 7. Authority state machine (epoch-fenced switch)

State: mpedb `epoch` (E_m, auth_m, st_m, frozen); source `_mpedb_mirror_state`
(E_s, auth_s, st_s, applied_high_water, pull_log_head, verify_head, fence_writes,
last_pull_at). |E_m − E_s| ≤ 1; divergence >1 → `HALTED(epoch_divergence)`.

> **Fencing rule (CONF#4): EVERY mirror-issued txn — source AND mpedb, including
> the cutover sub-txns and recovery-executed sub-txns — carries an exact
> `(epoch, authority, state, frozen)` CAS predicate** (sys_get+sys_put under the
> writer lock). v1.0 fenced only source txns and pull-apply; the unfenced mpedb
> cutover sub-txns (S5b/S8a/S8c) let a stalled duplicate daemon regress
> epoch/state. Recovery lands any pair outside the enumerated reachable set in
> `HALTED(epoch_divergence)` (the ±1 tripwire misses single-step regressions).

```
S0 → init → S1 IMPORTING → S2 SRC_AUTH
S2 ↔ S3 PUSH (sub-transactional)
S2 → S4 DRAIN_TO_MPEDB → [VERIFY gate] → S5 CUTOVER_M → S6 M_AUTH
S6 → S7 DRAIN_TO_SRC (frozen) → [RECONCILE+VERIFY gate] → S8 CUTOVER_S → S2
any → HALTED(schema_drift|verify_failed|epoch_divergence|park_overflow|
            db_full|source_restored|log_overflow)
```

- **S1**: source state row created BEFORE the copy; `imp/` watermarks; final
  commit writes cfg/map/cur/epoch=(1,source) **and `cdc\0tabs`**.
- **S2 SRC_AUTH**: pull rounds; local writes dirty-logged; source-wins default.
- **S4 DRAIN_TO_MPEDB**: persist st_m then st_s (recovery re-writes the missing
  one). Cooperative quiesce (fence trigger strongly recommended). Loop pull until
  lag=0 twice. **VERIFY gate on THIS arrow too** (CONF#28 — v1.0 gated only
  S7; §5 promised "before every switch"); excludes the quarantine∪park set;
  requires `switch --accept-divergence` to proceed over a declared exclusion.
- **S5 CUTOVER_M** (each sub-txn CAS-fenced): (a) SOURCE epoch→E+1,
  auth='mpedb', state='cutover'; (b) MPEDB epoch→(E+1, mpedb, M_AUTH); (c) SOURCE
  (pred E+1) state='steady'.
- **S6 M_AUTH**: local writes accumulate dirty; **pulls refused**; keep-warm push
  (local-wins) optional; actively bound the changelog (§5.2 M_AUTH GC). Days/weeks
  here is the designed use.
- **S7 DRAIN_TO_SRC**: (i) MPEDB frozen=1 (engine write-block, §3.9) + st_m;
  (ii) push until a fresh snapshot shows zero dirty **excluding the parked set**;
  (iii) **RECONCILE-then-VERIFY (CONF#9/55):** run §5.5 merge-diff to localize
  divergent PKs (incl. third-party source writes to never-locally-dirty PKs that
  push can't see); resolve by force-pushing mpedb's authoritative image
  (local-wins, park-audited, epoch-fenced), loop reconcile→verify to fixpoint;
  persist `verify_head` = the changelog head **captured inside the verify
  snapshot** (CONF#16/22); a persistent rogue writer livelocks this loop
  (surfaced in status). `HALTED(verify_failed)` escape = `mirror reconcile` /
  `mirror unfreeze --abort-switch`. (iv) housekeeping: sequence fix-up (§6),
  lease forfeit.
- **S8 CUTOVER_S** (CAS-fenced): (a) MPEDB epoch→E+1, auth=source, frozen stays;
  (b) SOURCE (pred E) epoch→E+1, auth='source', state='steady', **pull_log_head
  := verify_head** (the pre-captured baseline, NOT live head — CONF#16/22, so
  race-window writes above it are pulled after switch); (c) MPEDB cur :=
  pull_log_head (re-readable from the source row on crash), frozen=0,
  st=SRC_AUTH. Run one anti-entropy pass immediately after S8.

Recovery = total function over the pair; daemon rebuilds from the two DBs.
Single-instance: source session lock (advisory-only, never a startup gate —
CONF#29 pooler note) + `lease`; correctness rests on cursor/epoch CAS +
idempotent push, not the locks.

`mirror regenerate --size N` (CONF#54) — the ONLY arrow out of `HALTED(db_full)`
and the add-table / schema-drift path: freeze → drain-push if reachable →
**read-only** copy of rows + transplant `mir/*` + `cdc\0tabs` + **`cdc\0d/` dirty
entries** into a new larger file (copy from the full file needs no writes to it)
→ one fenced source txn that **resets `applied_high_water` below the smallest
migrated last_txn** (fresh file restarts txn_id at 0 — else push wedges/clears
migrated entries) and re-seeds cur → atomic rename swap → unfreeze. Crash-wave
tested (M6).

## 8. Conflict policy

Detection: pull-side reads the **pre-batch** `d/` snapshot (§5.4 step 3);
push-side per §6. Taxonomy: (source_op)×(local_op); delete-delete auto-resolves.
Kinds: `unique_blocked`, `structural_unique` (unrepresentable composite unique,
§4.1), `push_rejected`, `check_failed`, `invalid_utf8`, `nul_in_text`,
`unmirrorable_key`, `pk_too_large`, `resolved_source`, `resolved_local`.

Policies (per table): **authority-wins (default)** — source-wins in S2/S4,
local-wins in S6/S7; audited to `park/`. `local-wins` on pull skips the op
(cursor advances; dirty entry remains). `source-wins` applies + deletes the
pre-existing dirty entry. **newest-wins** OFF by default (clock skew printed in
status; authority tie-break). **manual** parks both images, installs a
`skip/<table><pk>` marker consulted by §5.4 step 3, until `mirror resolve --take
source|local|value` (source re-reads the CURRENT source row). `park/` bounded →
`HALTED(park_overflow)`; quarantine folded into the same store.

## 9. Failure-mode catalogue

| hazard | mitigation |
|---|---|
| self-capture echo storm | per-txn capture suppress (§3.8) |
| push loses local writes across batches | high-water only at full-coverage; no blanket clear (§6) |
| freeze/HALTED bypassed by raw/ring/optimistic writes | engine write-block bit (§3.9) |
| DbFull unreachable-recovery | reserved page pool + regenerate (§3.10, §7) |
| parked pull op tears the local row | DECIDE-before-mutate (§5.4) |
| switch verify unpassable (rogue source write) | reconcile-then-verify, both arrows (§7) |
| cursor re-seed swallows race-window writes | baseline pinned at verify snapshot (§7 S8b) |
| PG TRUNCATE / partition DDL missed | op='T' + partition-list drift → re-diff (§5.2) |
| PG slot / trigger-log unbounded retention | cap-and-resync, M_AUTH GC, partitioned log (§5.2/§5.6) |
| invalid UTF-8 / embedded NUL / PG infinity | violation classes / checked arith (§4.5) |
| normalization phantom-diff storm | read-back-and-converge; fidelity rule (§4.5) |
| collation weaker than memcmp on unique cols | reject / canonicalizer / pull_only (§4.4/4.5) |
| checksum non-injective / md5-FIPS / collation | injective encoding + sha256 + COLLATE "C" (§5.5) |
| PG xmin torn-read | consecutive-snapshot windows (§5.2) |
| push overwrites concurrent source write | xid-window + lock-then-check (§6) |
| out-of-band / older-backup restore | sequence/xmin regression guard (§5.1/5.2) |
| sequence rewind → dup keys post-switch | GREATEST setval + lease forfeit (§6) |
| multi-consumer silent divergence | one rw OR N pull_only, enforced at init (§2/§5.1) |
| PG import trigger-vs-snapshot gap | step 0 install-before-snapshot (§4.3) |
| unenforceable composite/partial uniques | pull_only or structural_unique park (§4.1) |
| PK keycode > MAX_KEY | fixed-size dirty key (§2/§3.7) |
| oversize source txn stall / DbFull | pre-flight footprint → HALTED before stall (§5.4) |
| schema drift (no ALTER) | fingerprint drift → regenerate (§7) |
| credential storage | see §12 |

## 10. Testing plan

1. **Unit**: mir/cdc codec truncation-at-every-offset; six-mutator hook;
   capture-suppress; write-block per mutator; reserved-page-pool exhaustion;
   savepoint unwinds dirty; sys_scan_range bounds; blake3 dirty-key coalescing.
2. **Capture completeness**: ring foreign-leader mirrored write → dirty entry
   (durability=commit, 2 processes); optimistic-mode process; raw-engine write;
   **pull round leaves the dirty set unchanged** (empty for a follower); import
   commits with empty dirty set; raw-engine write while frozen is refused.
3. **Differential** (reuse mpedb-testkit 3-way): random op streams; golden-file
   PG canonicalization + sha256 chunk checksums; every §4.5 type's canonical
   bytes.
4. **Under-load fuzz** (`mpedb mirror-collide`, modeled on collide.rs): N source
   writers + pull rounds; M mpedb writers + push rounds; SIGKILL the daemon at
   random; assert cursor monotone, no lost/dup vs model, all invariants, epoch
   fencing never violated, park stays bounded in follow mode.
5. **Crash waves at every arrow**: SIGKILL between each S5/S8 sub-txn, mid-import,
   mid-drain, mid-regenerate (copy / source-reset / rename); recovery converges.
6. **Echo-loop**: follow-mode pull concurrent with push both directions →
   fixpoint in one round; residue-GUC test; two-mirror (rw + pull_only) visibility.
7. **sqlite specials**: REPLACE-hole → anti-entropy; FK-cascade firing; VACUUM
   reject; NULL-in-PK / invalid-UTF-8 quarantine; affinity policies; older-backup
   restore guard.
8. **PG specials**: xid-window vs in-flight + torn-read; TRUNCATE/partition
   re-diff; sequence setval GREATEST; identity OVERRIDING; numeric/uuid/jsonb/
   infinity fidelity; FIPS sha256; cap-and-resync; (v2) slot-lost, TOAST 'u'.
9. **Switch drill**: full S2→S6→S2 ×100 under load, verify gate each pass, with an
   injected unfenced source write during S6 asserting reconcile convergence.

## 11. Implementation phases

- **M0** — this doc; review folded (done).
- **M1 core CDC primitive** (mpedb-core; DESIGN.md commit-path review applies):
  `cdc\0tabs` (capture + write-block) + generation cache; six-mutator hook;
  `WriteTxn::set_capture(false)`; write-block enforcement in the six mutators;
  reserved page pool; `sys_scan_range`; `ReadTxn::txn_id()`; WriteSession
  sys-record API; fixed-size dirty key. Generic, no mirror logic. Tests §10.1-2.
- **M2 crate + sqlite import**: Adapter trait; mir/cdc codec; sqlite introspection
  / type mapping / PK policy / scope selection; snapshot import w/ resume +
  report; `mirror init|import|status`.
- **M3 sqlite pull**: tracked mode (per-table cursor, DECIDE-phase apply,
  restore guard), merge-diff/checksum engine (§5.5), anti-entropy, quarantine,
  data_version gating.
- **M4 PG import + pull**: introspection/type mapping (infinity, numeric,
  collation); step-0 install; exported-snapshot + binary COPY; trigger changelog
  (consecutive-snapshot cursor, ENABLE ALWAYS, TRUNCATE/partition, SECURITY
  DEFINER, cap-and-resync); shares §5.5.
- **M5 push**: both adapters — buffered snapshot scan, per-op savepoint,
  xid-window/lock-then-check conflict detection, full-coverage high-water, echo
  suppression, sequence fix-up, read-back-and-converge. `mirror push|sync
  [--follow]`.
- **M6 switch + regenerate**: state machine, engine write-block freeze, CAS-fenced
  cutovers, reconcile-then-verify gate (both arrows), recovery-on-attach,
  `mirror switch|verify|reconcile|unfreeze|regenerate`.
- **M7 conflicts** (done): park store + taxonomy (`ParkRecord`/`ConflictKind`);
  DECIDE-structured pull apply (divergence → source-wins, park offenders, never
  wedge); push-side write-write detection both adapters (sqlite BEGIN IMMEDIATE
  check-then-write; PG lock-then-check xid-window, CONF#11/27) → source-wins +
  park `push_rejected`; `mirror conflicts list|clear`; `mirror resolve --take
  source|local` (operator override, both directions). *Deferred (not needed for
  correctness — authority-wins is the default):* per-table policy config
  (`local-wins`/`manual`/`newest-wins` selection), newest-wins clock-skew
  display, `conflicts export`.
- **M8 adversarial battery**: §10.4-9 (mirror-collide, crash waves, echo,
  fidelity matrices, sqlite/PG specials, switch drill ×100).
- **M9 (v2)**: pgoutput slot adapter (`pg_walstream`), slot-lost resync,
  `mirror offline`.

Chain: M1→M2→M3→M5→M6→M7→M8; M4 ∥ M3 after M2; M9 after M8.
Status: **M1–M7 done** (Linux green, clippy clean, live-PG ignored tests pass);
M8 next.

## 12. Security / credential model
- Source DSN/password: stored in a `0600` mirror config file (reuse the repo's
  `FilePerms` machinery) referenced by `mir/cfg` — never baked into `mirror_id`
  (which is `blake3(canonical-source-identity ‖ nonce)`, no secret) and never
  passed as a CLI arg (visible in `ps`). `mirror_id` must not be reversible to
  the DSN.
- Source objects (`mpedb_mirror.*`, `_mpedb_mirror_state`): `SECURITY DEFINER`
  functions owned by a privileged role; `REVOKE ALL` from app roles (else forged
  changelog/epoch). sqlite has no privilege model — any local writer can defeat
  suppression/fencing; anti-entropy is the only backstop there (documented).

## Appendix A: rejected alternatives
sqlite3session (single-connection); WAL-frame scanning (physical pages);
facade-level capture/freeze (misses ring/raw/optimistic); append-only changelog
as v1 mpedb capture (unbounded offline growth); continuous multi-master (needs
causality metadata); supabase/etl (git-only, heavy); replication origins for echo
(superuser-only); `INSERT OR REPLACE` for apply (re-fires cascades); md5 for
checksums (FIPS); `txid < xmin` window (torn reads); scalar sqlite cursor
(per-table AUTOINCREMENT).

## Appendix B: source-side objects
```sql
CREATE TABLE _mpedb_mirror_state (
  mirror_id TEXT PRIMARY KEY, epoch BIGINT NOT NULL,
  authority TEXT NOT NULL CHECK (authority IN ('source','mpedb')),
  state TEXT NOT NULL, applied_high_water BIGINT NOT NULL DEFAULT 0,
  last_push_at TIMESTAMPTZ, last_pull_at TIMESTAMPTZ,
  pull_log_head BIGINT, verify_head BIGINT,
  log_truncated_at BIGINT, fence_writes BOOLEAN NOT NULL DEFAULT FALSE
);  -- sqlite: TIMESTAMPTZ→TEXT, BOOLEAN→INTEGER
-- sqlite per-table: _mpedb_log_<t>(seq INTEGER PRIMARY KEY AUTOINCREMENT,
--   op INTEGER, origin TEXT, <pk cols>) + 3 AFTER row triggers
-- pg: mpedb_mirror.changelog(seq bigserial PK, tbl text, op "char", pk jsonb,
--   txid int8 DEFAULT txid_current(), origin text, at timestamptz)
--   + 1 generic SECURITY DEFINER plpgsql AFTER ROW trigger per table
--     (origin := NULLIF(current_setting('mpedb.mirror_origin', true), ''))
--   + AFTER TRUNCATE FOR EACH STATEMENT trigger (op='T'), ENABLE ALWAYS
--   + optional BEFORE fence trigger (privileged, per-epoch token)
```
