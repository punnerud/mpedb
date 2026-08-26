# mpedb

Embedded multi-process shared-memory database in Rust: sqlite's operational model
(no server, processes attach and may be SIGKILLed at any instant) + PostgreSQL-grade
concurrency (MVCC snapshots, lock-free readers) + rigid schema validation that sqlite
lacks. SQL compiles once to content-hashed plans (`execute(hash, params)` hot path with
zero parsing). **Read design/DESIGN.md before touching concurrency, lock, or commit-path code —
every protocol there survived a 37-finding adversarial review, and the ordering rules
(fences, meta publication, slot generation-CAS) are load-bearing.**

SQL is user-extensible via stored PySpell functions and `:sym:` operator
macros — SQL-EXTENSIONS.md is the contract; `mpedb fn list` / `mpedb op list`
show what a given database defines. The workload model (design/
DESIGN-MODEL-LANG.md, `mpedb model show`) declares what a database is FOR.

Measured comparisons all live in `benchmarks/` (index: `benchmarks/README.md`) —
head-to-head, OLAP/vector/graph, and the LISTEN/NOTIFY cell against PostgreSQL.
Three rules bind every one of them: a control arm for anything claimed to cost
something, like-for-like durability (#122 — never mpedb's weaker mode against a
log-based engine), and the hardware published when the hardware is the answer.

## Commands

- Build/test all: `cargo test --workspace` (mpedb-capi is its OWN workspace —
  it exports `sqlite3_*` and cannot co-link the bundled sqlite the parent's
  feature unification pulls in; test it with
  `cargo test --manifest-path crates/mpedb-capi/Cargo.toml`)
- One crate: `cargo test -p mpedb-core` (also: mpedb-types, mpedb-sql, mpedb, mpedb-cli)
- Lint (keep clean): `cargo clippy --workspace --all-targets -- -D warnings`
  **plus the satellite workspace CI also lints**, which `--workspace` cannot
  reach: `cargo clippy --manifest-path crates/mpedb-capi/Cargo.toml
  --all-targets -- -D warnings`. Leaving it out is how a lint reached main red:
  a separate workspace is invisible to the parent's `--workspace`, so the
  documented command passed locally and the same command failed in CI, which
  runs both. `mpedb-pg` and `mpedb-fs` are separate workspaces too and are in
  NO CI job at all — lint and test those by hand while that is true.
- Slow/instrumented tests are `#[ignore]`d: `cargo test -p mpedb-core -- --ignored`
- **Point every test's scratch at a real volume**: `MPEDB_TEST_DIR=/path cargo test …`.
  `mpedb_testkit::scratch_base` (and `mpedb_core::scratch_dir`, which spells the
  same knob because the testkit depends on the facade that depends on the core)
  read it. Without it the suite writes to the root filesystem, and a full disk
  does not announce itself as one: it has surfaced as a flapping measurement, as
  two corpus files "hanging", and as `ld: signal 7 [Bus error]` asking for an
  LLVM bug report.

## Crate map (dependency order)

- `crates/mpedb-types` — shared, dependency-light: Value/ColumnType, Schema + canonical
  bytes + blake3 hash, TOML Config, memcmp-ordered key encoding (`keycode`), stack-based
  expression IR with SQL 3VL (`expr`), plan Footprint/PlanHash. Everything decodable is
  bounds-checked: corrupt input must yield `Error::Corrupt`, never a panic.
- `crates/mpedb-core` — the engine. `backup` (RAW whole-database copy:
  `Database::backup()` / `restore_backup()`, `mpedb backup|restore` — the
  data pages up to the snapshot's high water, so the file is sized by the
  DATA and not by the arena. Takes NO lock: committed pages are immutable
  and a pinned reader holds the reuse floor, so the snapshot is frozen
  while writers continue. The lock/reader/ring pages are NOT copied — they
  are process state — which is why `max_readers` rides in the header and is
  checked: it decides where the data region starts, so a mismatch would
  place every page at the wrong id, silently), `pagestore` (COW page
  discipline; in-memory TestStore
  for model tests), `btree` (COW B+tree, overflow chains, model-tested against BTreeMap),
  `row` (null bitmap + fixed + varlen codec), `shm` (mmap, init via flock+fallocate, meta
  double-buffer with atomics/fences, robust ERRORCHECK mutex, reader table with packed
  {pid,seq} generation words + /proc start-time identity), `engine/` (split into
  mod/read/write/freelist/commit: ReadTxn/WriteTxn, catalog, chunked freelist with
  commit-time fixpoint, typed row API, page-accounting verifier).
- `crates/mpedb-sql` — tokenizer → AST → binder (rigid types, param unification, const
  folding) → `planner/` (select/join/aggregate/access/footprint: PkPoint/PkRange/
  IndexPoint/FullScan + footprints) → CompiledPlan in `plan/` (encode/decode/validate/
  explain: canonical bytes, blake3 hash, fully re-validating decode).
- `crates/mpedb` — facade: Database::open(config), prepare/execute/query, WriteSession,
  shared plan registry in the catalog's sys-keyspace (`plan/<hash>`), CHECK compilation,
  the plan executor in `exec/` (mod = TxnCtx + exec_stmt, gather, aggregate), and
  `ring_exec` (Phase-2 group-commit leader; active when durability = commit or wal).
- `crates/mpedb-cli` — `mpedb` binary: repl/exec/prepare/call/dump/stress/crash/
  powerloss/bench + `tier` (drain hot→cold + SIGKILL harness, #78)
  + `lens` (reversible ETL pairs over stored functions, DESIGN-RRETL: every
  class declaration is VERIFIED against a probe corpus and refused with a
  named counter-example — `celsius ⇄ fahrenheit` is the canonical bijective
  refusal; a `residual` pair is the triple fwd/1 rex/1 inv/2 with a DECLARED
  residual type, and its collision check keys on (y, r) jointly)
  + `rretl apply|revert|putback|log` (in-place column transform in ONE txn:
  per-row residuals in `rretl_residual (run_id, pk)`, runs — failed ones
  included — in `rretl_lineage`, and 100% of rows verified against the source
  hash BEFORE the commit that destroys the source; revert is hash-gated,
  `putback` inverts KEEPING edits — PutRes per row replaces the hash gate,
  deleted rows stay deleted. Every pass STREAMS in `pk > last` chunks — no
  row cap, O(chunk) heap; lineage records a `residual_hash` over the
  persisted residual set, which fsck re-checks for EVERY standing run
  (buried included) and revert/putback gate on (a tampered residual survives
  both PutRes halves — the hash is what catches it). Lineage is ordinary
  TABLES with RIGID column types built from specs, never SQL text — DDL
  `BLOB` means TYPELESS, and a typeless `pk_enc` would turn every residual
  lookup into a filter over the run. Never sys-keyspace — #124 is why. The
  Python surface + agent guide is PYSPELL-RRETL.md)
  + `rretl put|get|versions` (blob versioning, rretl_store.rs: newest FULL,
  previous rewritten as reverse-delta verified byte-identical AS PERSISTED
  before commit, every 8th a permanent full anchor; a stored full failing its
  recorded hash HARD-errors the put — rewriting would launder corruption; a
  bloating delta keeps the full and the put succeeds; get hash-verifies every
  walk step) + `rretl pack-in|pack-out|archives` (zip SPLICE per DESIGN-RRETL
  §8.4: members as rows, residual keeps every other byte, byte-identical
  verify BEFORE ingest commits, pack-out hash-gated; zip64/encrypted/
  overlapping refused by name. Both are lineage with outcomes
  versioned/packed — never `applied`, so revert/stacking ignore them)
  + `rretl map define|sync|check|show|list|drop` (stage 4, DESIGN-RRETL §13:
  table-SET maps — source tables mirrored into a different shape through
  lens pairs, synced BOTH ways in one txn. Key insight: both sides exist,
  so residual pairs need no stored residual — `rex(x_current)` is computed
  LIVE and B→A is putback with it, PutRes-gated. `rretl_map_state` records
  both sides' hashes after every push: unchanged-since-recorded = skip,
  which IS the echo guard (no epochs/origin tags); both-moved = named
  CONFLICT, whole sync aborts. `map check` (§13.5) is the read-only twin of
  the sync — its `diverged` list is the audit the echo guard structurally
  cannot do (state clean, forward(A) != B — a pair REBOUND under the map
  is the realistic cause), fsck walks every stored map. check_table and
  sync_table are twin matches: edit one, mirror the other. The duel's
  standing rules: only a byte-identical redefine keeps state (changed spec,
  first define, define-after-drop all re-baseline; drop clears state); a
  table is a map source or a map target, NEVER both and never a target
  twice (shared targets merge masters, reverse maps deadlock on conflicts,
  chains break check/sync twin-ness); a target that is GONE while state
  exists is a named refusal, because reading it as a target-side delete
  emptied the SOURCE. All rRETL bookkeeping keys on `pk_ref` (blake3 of
  the pk's canonical bits, fixed 32 B) — raw bits inside a composite key
  made a legal ~970-char TEXT pk unsyncable, with the ceiling depending on
  how long the MAP was named. State hashes chain in (map, tbl, key) so
  they are not portable between rows. Map records are versioned TOML in
  sys-keyspace `rrmap/<name>`; #94's implicit rowid is a REAL column named
  rowid carrying the pk — detect it via the flag, not via empty pk)
  + `rretl map run|runner|status` (#53, DESIGN-RRETL §14: the DAEMON form
  for cron — a SEPARATE verb from `map sync` because it trades atomicity
  across chunks for progress that survives a kill. Commits as it goes and
  EVERY commit advances the whole set (a chunk from each table, never
  table 1 finished before table 2 starts); `rretl_map_cursor` per (map,
  tbl) is where it resumes, a ROUND is passes 1→2→3 over everything, and
  rows changed behind the cursor wait for the next round. Bounds:
  max-secs (between txns), max-rows (clock-free, so tests are exact),
  runner (policy guard, NOT auth — the fence is the OS), lease (buys
  wasted work, not correctness). Conflicts are counted and SKIPPED —
  aborting would let one row block every other forever. Safe here in a
  way it would not be for `apply`: nothing is destroyed, and each row's
  push + state row share one txn. classify_p1/classify_p2 in rretl_map.rs
  are the ONE decision function sync, check and run all use — three
  copies would drift, which is exactly the bug class the duel found)
  + `stream = true` on a map (#53 stream half, DESIGN-RRETL §15: triggers
  on both sides append the touched key to `rretl_map_dirty`, an append-only
  JOURNAL — a set keyed (map,tbl,pk) would hit the 976-byte key cap and a
  trigger body cannot compute the blake3 that `pk_ref` needs. `map run`
  drains it BEFORE the scan chunk, in the SAME txn, so a kill cannot
  separate the sync from the entry that named it. The journal is a FAST
  PATH, never the truth — triggers fire on the SQL path only, so a mirror
  import or a dropped trigger leaves rows that differ with nothing
  recording it, and only the round finds those. MEASURED: latency, not
  total work — at equal budget a far-end change on 8 000 rows lands after
  1 invocation instead of 40; the round still runs and still costs the
  table. Echo is bounded at 1 (the push re-journals, the next drain reads
  clean and writes nothing). Opt-in because the write path pays)
  + `ingest define|show|state|advise|conflicts|resolve` +
  `next|pending|done|release|reap` (#52 stage B, DESIGN-INGEST: getting
  data IN, which is the half rRETL does not own. A source is a call
  GRAPH, not a list of tables — a cheap root call returns keys that DRIVE
  per-key calls, and fan-out is measured, never declared. mpedb never
  makes a call: it plans, receives, diffs, verifies, and carries the
  parameters. Three things carry their weight: the DUMP is the judge —
  it tries the delta's cursor candidate against where that delta stood
  and names the rows a lying `updated_at` would have lost (empty
  watermark = NO verdict, since everything beats nothing); the objective
  is HARMONIC staleness, because binary freshness's optimum polls the
  fastest-changing table ZERO times (Cho & Garcia-Molina TODS'03 Thm
  5.5) and uniform is the control arm it must beat (Thm 5.1/5.2); and a
  DERIVED edge is SCOPED — it presents only the keys that drove it, so a
  dump receipt through it is refused by name (`presents_whole_table()`
  is the one place that decides). `ingest_derive` queues follow-ups in
  the SAME txn as the receipt that found the keys. Bookkeeping is
  ordinary rigid tables — ingest_stats/state/conflicts/task + the
  in-dump ingest_seen — never sys-keyspace, #124 is why. Guide:
  INGEST-GUIDE.md; the measured lab is workbench/ingest-lab)
  + `mirror` (import/export/pull/push/sync/switch/conflicts/resolve)
  and `mirror-collide` (SIGKILL fuzz: source writers + a mirror daemon killed at every
  instant → final drain must converge mpedb exactly to the source)
  + `map-collide [--mode sync|run]` (the stage-4 member: writers churn
  BOTH sides of a live map while the syncer is SIGKILLed every kill-ms —
  `run` aims it at the daemon's chunk commits; conflicts are the
  syncer's expected diet, anything else fails the run; final drain =
  source-wins resolution → echo 0, map check clean, fsck clean, counts
  1:1). stress/crash take
  `--durability commit|wal` to exercise the intent ring on real disk; `powerloss` is the
  WAL torn-tail power-loss simulation.
- `crates/mpedb-fs` — `mpedbfs`, a READ-ONLY FUSE view (#54, DESIGN-MPEDBFS):
  `/obj/<name>/{latest,v<N>}` for versioned blobs and `/archive/<id>-<name>/…`
  for a spliced zip's members as a real tree, INFLATED (a member is stored as the zip had it, so serving those bytes raw was a wrong answer — method 8 is decoded, anything else refused). Adds no data — it is the adapter
  for programs that only speak paths. Read-only is a DECISION, not a stage
  (a partial write is not a version, and a writable mount would hold the
  single writer lock across a user's `cp`). One snapshot per open file, and
  sizes are cached because a version's CONTENT is immutable even though its
  STORAGE is not (`VersionInfo.bytes` is the ENVELOPE's size — using it would
  truncate reads). Lazy-ETL-as-a-file (v2) is CLOSED, not pending: a file needs a SIZE
  before content, so a derived view runs the whole transform per stat —
  measured 12 ms per 20k-row view, ~120 ms per `ls -l` over ten of them,
  every time, against 12 ms ONCE for an export. Reopens only for a format
  whose length follows from schema + row count. Its OWN workspace, like
  mpedb-capi, so a box without /dev/fuse never compiles it: `cargo build --manifest-path
  crates/mpedb-fs/Cargo.toml`; `fuser` with default-features off, so no
  libfuse headers, mounting via `fusermount3`.
- `crates/mpedb-py` — PyO3 module `mpedb` (abi3-py310, GIL released around engine calls);
  build: `cargo build --release -p mpedb-py`, ship `libmpedb_py.so` as `mpedb.so`.

## Invariants that bite

- Page 0/1 = meta A/B, page 2 = lock area, 3.. = reader table; data pages after. Page id
  0 doubles as the "empty tree" sentinel.
- Committed pages are immutable — `page_mut` only on pages allocated by the current
  write txn (COW). TestStore and WriteTxn both enforce this; violations are engine bugs.
- Freelist entries are keyed by 11 bytes `(txn u64 BE ‖ kind u8 ‖ chunk u16 BE)` — kind 0
  = page ids, kind 1 = extent runs (DESIGN-BLOBEXTENT §3.3) — with values ≤ 960 B so they
  stay inline; the commit fixpoint depends on rewrites not changing tree topology.
- Pages freed by commit T are reusable when T ≤ oldest-pinned bound (NOT strict < — the
  off-by-one causes an unbounded high-water leak; there is a regression test).
- **`refill_reusable` is READ-ONLY**: it draws an entry's pages into the writer's pool
  and LEAVES the entry (tracked in `taken`); the commit fixpoint strikes out only what
  was consumed, and never rewrites an entry nothing was allocated out of. Deleting on
  the way in is what made every drawn page a page the fixpoint had to write back —
  coupling its appetite to the pool and leaking high-water forever (design/DESIGN.md §4.5).
  Freelist values are strictly ascending and binary-searched; `reusable` is kept sorted.
- The fixpoint's fallback to `high_water` **is** its termination argument (§4.5) — it
  frees nothing, so the sets stop growing. That is why `in_freelist_op` must keep
  blocking refill even though refill no longer mutates.
- `rollback_to` (the cheap savepoint) is exact ONLY under
  `WriteTxn::undo_is_exact`: nothing allocated since the savepoint, or the txn
  was pristine when it was taken. It restores roots and accounting, so it cannot
  undo an in-place mutation of an already-dirty page — a page COWed under a dirty
  root stays linked while `reusable` re-offers it, and two trees end up sharing
  it (#160). `partial` is a ROW flag and does not answer this; a split is not a
  row. `rollback_to_full` is exact regardless (it restores dirty-page bytes).
- The reader-pin protocol and writer scan pair SeqCst fences; weakening them reintroduces
  a store-buffering race (design/DESIGN.md §4.3).
- Intent-ring posting is incarnation-safe ONLY because: posts happen under the writer
  lock, the result store precedes the READY→DONE transition, owners may release from
  READY, and recovery never acts on DONE slots (design/DESIGN.md §5.3). Reordering any of these
  reintroduces a stress-reproducible phantom-result TOCTOU.
- Index numbering: 0 = PK tree; `TableDef.indexes` is the SINGLE source
  (DESIGN-SCHEMA-V2) — index_no = position + 1, populated by `Schema::new` (flag-derived
  single-column entries in declaration order, then explicit `[[table.index]]` ones;
  composite supported). UNIQUE trees are keyed `values → pk`; non-unique `(values ‖ pk)
  → pk`. Membership: a row with ANY NULL indexed column has no entry. Table ids are
  explicit in canonical-bytes v2 and DENSE 0..n in this format window (position == id
  is validate-enforced; DROP relaxes it after the §6 positional audit). The planner
  exploits single-column indexes only until #55.
- Schema/geometry are file-authoritative: attach hard-errors on config drift.
  The config's schema seeds a new file (a ZERO-table seed is legal — tables
  arrive via live DDL) and must hash-match the frozen SEED
  hash on every attach; the LIVE schema is read from the catalog and may have
  grown past the seed via `CREATE TABLE` (#47). `M_SCHEMA_HASH` = seed forever;
  `schema_gen` in the flipping meta is the DDL staleness signal.
- Crash-safe on Linux (x86-64 + 32/64-bit ARM), macOS/Apple Silicon and Windows
  x86-64 — the latter two on the FLD-2 flock writer lock (`crate::os`; Windows via
  `wincompat` over kernel32 + `LockFileEx`, `GetProcessTimes` pid identity, #159).
  All six crash harnesses run on all three; `mpedb-cli` contains no `fork`.
  Single PID namespace; robust mutexes / flock locks do not survive reboot
  (boot-id recovery in `post_attach` handles that — don't remove it).

## Testing conventions

Deterministic xorshift RNGs (no rand dep). Model tests compare against std collections.
Every decoder gets truncation-at-every-offset tests. Multi-process behavior is tested
via the CLI's stress/crash subcommands, not in unit tests.
