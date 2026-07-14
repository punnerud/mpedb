# mpedb

**An embedded, multi-process, shared-memory database in Rust.**

mpedb combines three things that normally don't come together:

- **sqlite's operational model** — no server; processes `mmap` a shared file and
  attach directly, and any process may be `SIGKILL`ed at any instant without
  corrupting the database.
- **PostgreSQL-grade concurrency** — MVCC snapshots over a copy-on-write B+tree,
  lock-free readers that never block writers, and group-commit for durable writes.
- **Rigid schema & integrity validation** that sqlite lacks — typed columns,
  NOT NULL / UNIQUE / CHECK, and a file-authoritative schema that hard-errors on
  config drift.

SQL is compiled **once** into a content-hashed plan; the hot path is
`execute(hash, params)` with zero parsing. Plans carry precomputed read/write
footprints ("pre-computed locks", Calvin-style), so the engine knows which
tables and keys a statement touches before it runs.

> ⚠️ **Status: personal research project.** Crash-safe on Linux (x86-64 and
> 32/64-bit ARM) and macOS/Apple Silicon — see [Platforms](#platforms). The
> design has been through multiple adversarial review rounds (see the
> `DESIGN*.md` docs), but this is not production-hardened software. Treat it as a
> serious experiment.

## Highlights

- **Copy-on-write B+tree + MVCC** — double-buffered meta pages, `/proc`-start-time
  reader identity, robust `PROCESS_SHARED` mutexes with `EOWNERDEAD` recovery.
- **50,000+ concurrent lock-free readers** (config-sized reader table); writers
  serialize through one writer lock with intent-ring group commit.
- **Durability modes** — `none`, `commit` (msync), `wal` (sequential log +
  fdatasync, durable-on-ack), `async` (deferred coalesced fsync).
- **Multi-database workspaces** — address several independent database files as
  `alias.table`; separate files = separate writer locks = linear write
  parallelism, and the only OS-enforced isolation boundary.
- **Cooperative row-level security** — PostgreSQL-style `USING` / `WITH CHECK`
  policies keyed on a caller-set session context, injected transparently at plan
  time, with cache leak-proofing (a stale cached plan is re-validated against the
  live policy epoch under the executing snapshot). *In-file RLS is cooperative
  defense-in-depth, not a hard boundary against a hostile process that maps the
  raw pages — see [`DESIGN-MULTIDB.md`](DESIGN-MULTIDB.md) §6.*
- **Near-data execution** — a PySpell/MPEE-inspired stored-procedure layer runs
  Python/Rust subsets next to the data (streaming cursors) instead of shipping
  rows to a client.
- **Client-carried "detached" plans** — the SDK ships `(hash, blob, sql)` and the
  database only validates, never storing anything in the shared registry.

## Crate map (dependency order)

| Crate | What it is |
|---|---|
| `mpedb-types` | Shared, dependency-light: values/types, schema + canonical bytes + blake3 hash, config, memcmp key encoding, expression IR (SQL 3VL), plan footprints, RLS policy defs. |
| `mpedb-core` | The engine: page store, COW B+tree, row codec, shared-memory layer (mmap, meta double-buffer, reader table, WAL), read/write transactions, catalog. |
| `mpedb-sql` | Tokenizer → parser → binder (rigid typing, param unification, const folding) → planner (access-path selection + footprints) → content-hashed compiled plans. |
| `mpedb` | Facade: `Database`/`Workspace`, prepare/execute/query, write sessions, session context, RLS policy storage + injection, shared plan registry. |
| `mpedb-sdk` | Caching client session. |
| `mpedb-proc` | PySpell-style Python/Rust → budgeted IR stored procedures, streaming cursors. |
| `mpedb-py` | PyO3 module (`abi3-py312`), GIL released around engine calls. |
| `mpedb-mirror` | Bidirectional sqlite3/PostgreSQL ⇄ mpedb mirroring & migration: import, incremental diff-pull under load, write-back, epoch-fenced authority switch, and round-trip differential export/diff. |
| `mpedb-cli` | The `mpedb` binary: repl / exec / prepare / call / dump / stress / crash / powerloss / bench / proc / mirror. |
| `mpedb-testkit` | sqllogictest harness + 3-way differential testing vs sqlite3 and PostgreSQL. |
| `mpedb-bench` | Cross-engine benchmarks. |

## Build & test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

# one crate
cargo test -p mpedb-core

# slow/instrumented tests are #[ignore]d
cargo test -p mpedb-core -- --ignored

# the Python module
cargo build --release -p mpedb-py   # ship libmpedb_py.so as mpedb.so
```

Multi-process behaviour (concurrency, crash-safety, power-loss) is exercised
through the CLI's `stress` / `crash` / `powerloss` / `collide` subcommands
rather than unit tests.

## Platforms

- **Linux — x86-64 and 32/64-bit ARM** — the reference platform: full
  crash-safety (robust `PROCESS_SHARED` mutex with `EOWNERDEAD` recovery) and
  durability. 32-bit ARM works because it has lock-free `AtomicU64`.
- **macOS — Apple Silicon** — crash-safe via the **FLD-2 writer lock**: a
  sidecar `flock` (which the kernel releases on holder death) plus a private
  `ERRORCHECK` mutex and a shared tri-state word give owner-death recovery
  equivalent to Linux's robust mutex; durability uses `fcntl(F_FULLFSYNC)` and
  16 KiB-aligned `msync`. All platform code is `#[cfg]`-gated behind
  `crate::os`, so the Linux path stays byte-identical.

Platform claims are verified on real hardware: the multi-process `crash` harness
observes owner-death recovery (`eowner_recovery=true`) under SIGKILL waves across
`none`/`commit`/`wal` durability on both Linux and an M3 Mac, and `cargo test
--workspace` + `cargo clippy … -D warnings` run on both. See
[`DESIGN-MACOS-LOCK.md`](DESIGN-MACOS-LOCK.md).

## Differential testing vs sqlite3 / PostgreSQL

Correctness is checked against the established engines, not just against itself:

- `mpedb-testkit` runs a sqllogictest corpus and a **3-way differential tester**
  (mpedb vs sqlite3 vs PostgreSQL) so identical SQL must produce identical
  results.
- The mirror adds a **round-trip differential**: `sqlite3 → mpedb → sqlite3`,
  then a table-by-table, row-by-row diff. It proves a migration preserves the
  data — and reports exactly which values do *not* survive a mapping. Run it on
  any sqlite file:

  ```sh
  mpedb mirror roundtrip --source app.db
  ```

## Performance

Head-to-head against SQLite and PostgreSQL through one shared Rust measurement
loop (each engine on its own fast path — mpedb's `execute(hash, …)`, prepared
statements for the others). Full methodology and every cell are in
[`BENCHMARKS.md`](BENCHMARKS.md) / the machine-generated
[`crates/mpedb-bench/RESULTS.md`](crates/mpedb-bench/RESULTS.md).

Single-client, embedded, none-class point ops on a 2-core Linux VM (2026-07-14):

| op (none-class) | mpedb | SQLite | PostgreSQL |
|---|--:|--:|--:|
| point-select (PK), ops/s | **229,969** | 38,636 | 20,284 |
| point-insert, ops/s | **54,499** | 19,197 | 13,074 |
| point-update (PK), ops/s | **91,416** | 25,363 | 10,983 |

mpedb leads embedded point ops (~3-11×; zero-parse plans + no IPC + a COW B+tree
in-process) and lock-free reads under a live writer (~300k read ops/s at 2 µs
p50). Its one weak spot is **single-client durable INSERT** — one fsync per commit
with group-commit only under contention — so a lone durable writer trails SQLite
and PostgreSQL; the answer is to batch in a `WriteSession` or use `wal`, where
mpedb's batched durable insert (~89k ops/s) is the fastest of the three.

```sh
cargo run --release -p mpedb-bench      # full head-to-head (writes RESULTS.md)
mpedb bench --auto --durability wal     # quick mpedb-only
```

> Numbers on a shared 2-core VM vary ~20-25% between runs by host load alone;
> read the ratios, not the digits, and run one engine at a time for the cleanest
> comparison.

## Mirroring & cross-database migration

mpedb doubles as a **neutral staging hub between databases**. Because the sqlite
and PostgreSQL adapters both map through mpedb's common type model, you can:

- **Migrate** `sqlite3 → mpedb → PostgreSQL` (or the reverse) — mpedb is the
  lingua franca in the middle.
- **Stage & analyse** — pull a PostgreSQL database into a local `.mpedb`, run
  extra queries, add local tables, compute, then push changes back to
  PostgreSQL **without losing the data PostgreSQL owns**.
- **See what you lose** — the round-trip diff reports exactly which values cannot
  round-trip (e.g. a PostgreSQL type that sqlite/mpedb cannot represent
  losslessly), so a lossy migration is explicit, never silent.

The full protocol — incremental diff-pull under concurrent source write load,
write-back, and an epoch-fenced authority switch in both directions — is
specified in [`DESIGN-MIRROR.md`](DESIGN-MIRROR.md) (v1.1, hardened against a
58-finding adversarial review). The end-to-end pipeline works today for both
sqlite and PostgreSQL sources: import, incremental pull, write-back push (with
source-wins write-write conflict detection), epoch-fenced authority switch in
both directions, anti-entropy reconcile, and operator conflict tooling.

```sh
mpedb mirror import    --source app.db --dest app.mpedb   # snapshot + install change capture
mpedb mirror pull      --source app.db --db app.mpedb     # apply source changes into mpedb
mpedb mirror push      --source app.db --db app.mpedb     # write local changes back (echo-safe)
mpedb mirror sync      --source app.db --db app.mpedb     # pull then push per authority
mpedb mirror switch    --source app.db --db app.mpedb --to mpedb|source   # epoch-fenced cutover
mpedb mirror conflicts --db app.mpedb [--clear]           # inspect parked conflicts
mpedb mirror resolve   --source app.db --db app.mpedb --take source|local # operator override
mpedb mirror roundtrip --source app.db                    # import -> export -> diff (fidelity)
```

Crash-safety of the sync daemon is fuzzed with `mpedb mirror-collide`: source-
writer processes churn the source while a mirror daemon is SIGKILLed at every
instant; after the writers stop, a final drain must converge mpedb *exactly* to
the source — no operation lost or duplicated across the kills.

## Design docs

The design documents are the load-bearing contracts — **read them before touching
concurrency, lock, or commit-path code:**

- [`DESIGN.md`](DESIGN.md) — the core engine, concurrency, and crash-safety protocols.
- [`DESIGN-MULTIDB.md`](DESIGN-MULTIDB.md) — parallel databases + cooperative RLS.
- [`DESIGN-MIRROR.md`](DESIGN-MIRROR.md) — bidirectional sqlite/PostgreSQL mirroring & migration.
- [`DESIGN-MACOS-LOCK.md`](DESIGN-MACOS-LOCK.md) — the FLD-2 macOS crash-safe writer lock.
- [`DESIGN-MPEE-OPT.md`](DESIGN-MPEE-OPT.md), [`DESIGN-PHASE3.md`](DESIGN-PHASE3.md) —
  measured-and-documented explorations (including directions that were falsified
  and deliberately *not* shipped).

Inspired in part by [pyspell](https://github.com/punnerud/pyspell) (parse-once-to-IR)
and [mpee](https://github.com/punnerud/mpee) (streaming matrices / route optimization).

## License

Released under the [MIT License](LICENSE).

---

*MPE stands for Morten Punnerud-Engelstad.*
