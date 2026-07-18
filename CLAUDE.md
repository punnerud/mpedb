# mpedb

Embedded multi-process shared-memory database in Rust: sqlite's operational model
(no server, processes attach and may be SIGKILLed at any instant) + PostgreSQL-grade
concurrency (MVCC snapshots, lock-free readers) + rigid schema validation that sqlite
lacks. SQL compiles once to content-hashed plans (`execute(hash, params)` hot path with
zero parsing). **Read design/DESIGN.md before touching concurrency, lock, or commit-path code —
every protocol there survived a 37-finding adversarial review, and the ordering rules
(fences, meta publication, slot generation-CAS) are load-bearing.**

## Commands

- Build/test all: `cargo test --workspace`
- One crate: `cargo test -p mpedb-core` (also: mpedb-types, mpedb-sql, mpedb, mpedb-cli)
- Lint (keep clean): `cargo clippy --workspace --all-targets -- -D warnings`
- Slow/instrumented tests are `#[ignore]`d: `cargo test -p mpedb-core -- --ignored`

## Crate map (dependency order)

- `crates/mpedb-types` — shared, dependency-light: Value/ColumnType, Schema + canonical
  bytes + blake3 hash, TOML Config, memcmp-ordered key encoding (`keycode`), stack-based
  expression IR with SQL 3VL (`expr`), plan Footprint/PlanHash. Everything decodable is
  bounds-checked: corrupt input must yield `Error::Corrupt`, never a panic.
- `crates/mpedb-core` — the engine. `pagestore` (COW page discipline; in-memory TestStore
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
  powerloss/bench + `mirror` (import/export/pull/push/sync/switch/conflicts/resolve)
  and `mirror-collide` (SIGKILL fuzz: source writers + a mirror daemon killed at every
  instant → final drain must converge mpedb exactly to the source). stress/crash take
  `--durability commit|wal` to exercise the intent ring on real disk; `powerloss` is the
  WAL torn-tail power-loss simulation.
- `crates/mpedb-py` — PyO3 module `mpedb` (abi3-py312, GIL released around engine calls);
  build: `cargo build --release -p mpedb-py`, ship `libmpedb_py.so` as `mpedb.so`.

## Invariants that bite

- Page 0/1 = meta A/B, page 2 = lock area, 3.. = reader table; data pages after. Page id
  0 doubles as the "empty tree" sentinel.
- Committed pages are immutable — `page_mut` only on pages allocated by the current
  write txn (COW). TestStore and WriteTxn both enforce this; violations are engine bugs.
- Freelist entries are keyed (txn BE, chunk BE) with values ≤ 960 B so they stay inline;
  the commit fixpoint depends on rewrites not changing tree topology.
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
  The config's schema seeds a new file and must hash-match the frozen SEED
  hash on every attach; the LIVE schema is read from the catalog and may have
  grown past the seed via `CREATE TABLE` (#47). `M_SCHEMA_HASH` = seed forever;
  `schema_gen` in the flipping meta is the DDL staleness signal.
- Crash-safe on Linux (x86-64 + 32/64-bit ARM) and macOS/Apple Silicon (the FLD-2 flock
  writer lock, `crate::os`); single PID namespace; robust mutexes / flock locks do not
  survive reboot (boot-id recovery in `post_attach` handles that — don't remove it).

## Testing conventions

Deterministic xorshift RNGs (no rand dep). Model tests compare against std collections.
Every decoder gets truncation-at-every-offset tests. Multi-process behavior is tested
via the CLI's stress/crash subcommands, not in unit tests.
