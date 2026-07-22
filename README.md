# MPEdb

[![Linux](https://github.com/punnerud/mpedb/actions/workflows/linux.yml/badge.svg?branch=main)](https://github.com/punnerud/mpedb/actions/workflows/linux.yml)
[![macOS](https://github.com/punnerud/mpedb/actions/workflows/macos.yml/badge.svg?branch=main)](https://github.com/punnerud/mpedb/actions/workflows/macos.yml)
[![Windows](https://github.com/punnerud/mpedb/actions/workflows/windows.yml/badge.svg?branch=main)](https://github.com/punnerud/mpedb/actions/workflows/windows.yml)

**An embedded, multi-process, shared-memory database in Rust.**

Linux and macOS run the whole engine and its multi-process crash tests; Windows
runs the portable crates — the type system and the SQL front end — because the
engine is Unix-only by construction (mmap, flock, robust `PROCESS_SHARED`
mutexes, `/proc` reader identity). macOS and Windows run nightly, Linux on every
change.

> **[mpedb vs sqlite3 vs Cursor's minisqlite →](minisqlite-vs-mpedb.md)** — what
> each engine can even be asked to do, and speed on operations an application
> actually runs. Both directions plotted: 4000× ahead on indexed `min`/`max`,
> 0.40× on a single-row INSERT.

MPEdb combines three things that normally don't come together:

- **sqlite's operational model** — no server; processes `mmap` a shared file and
  attach directly, and any process may be `SIGKILL`ed at any instant without
  corrupting the database.
- **PostgreSQL-grade concurrency** — MVCC snapshots over a copy-on-write B+tree,
  lock-free readers that never block writers, and group-commit for durable writes.
- **Rigid schema & integrity validation** that sqlite lacks — typed columns,
  NOT NULL / UNIQUE / CHECK, and a file-authoritative schema that hard-errors on
  config drift. (With one deliberate, per-column escape hatch: `type = "any"`
  accepts any scalar in that column — sqlite-style flexibility where you ask
  for it, rigidity everywhere else. An `any` column cannot be a key.)

**It opens an existing sqlite `.db` file directly** — just `mpedb data.db`, no
flags, no import. Writes land in a `<db>.overlay.mpedb` **write-ahead delta**
beside the file — so several processes write **concurrently** without blocking,
and a `SIGKILL` never corrupts the base — while reads fall through to unchanged
rows in the `.db` via a native sqlite-format reader (no sqlite library in the
path). `mpedb checkpoint data.db` **publishes** the delta back into the `.db`,
**collision-validated** against the current base, holding the `.db`'s lock only
briefly so a foreign `sqlite3` writer can interleave. (`--mirror` = full sidecar
import; `--direct` = read-only.) See [design/SQLITE.md](design/SQLITE.md).

SQL is compiled **once** into a content-hashed plan; the hot path is
`execute(hash, params)` with zero parsing. Plans carry precomputed read/write
footprints ("pre-computed locks", Calvin-style), so the engine knows which
tables and keys a statement touches before it runs.

## Why this exists

The common local-development setup is a lie you find out about in production.
You develop a Django app against sqlite3 because it is a file — instant to
create, trivial to snapshot (`cp`), trivial to throw away, and it costs nothing
while idle. Then you deploy to PostgreSQL, and the parts sqlite never enforced
show up at once: a string that quietly lived in an integer column, a value that
overflowed `int4`, a constraint that was decoration locally and a hard error in
prod. The convenience of the local database is bought with the correctness of
the real one, and the bill arrives late.

mpedb is aimed at that gap: **sqlite's operational model with PostgreSQL's
strictness**. A file you can copy, no daemon, no idle cost — but typed columns,
NOT NULL / UNIQUE / CHECK, and a schema that refuses to drift. The failures you
would have met in production happen on your laptop, at the moment you write the
bad row, while you are still looking at it.

The mirror is the bridge: point it at the sqlite3 database your tests already
use, import it, and mpedb tells you what a strict target will reject — before
PostgreSQL does, and without contacting PostgreSQL at all. It runs in both
directions and records what the source declared, so migration is a thing you
validate rather than hope about.

And you can start from the file you already have, sqlite3-style —
**[design/SQLITE.md](design/SQLITE.md) is the dedicated page**: your .db as the durable
home, the .mpedb beside it as its WAL, checkpoints folding writes back.
In short:
**`mpedb data.db`** opens it exactly like `sqlite3 data.db` does (repl or
one-shot statement). By default it is a true **delta-WAL overlay**: only your
changes live in `data.db.overlay.mpedb` (tombstones included), unchanged data
reads straight from the `.db` through the **native sqlite reader**
(`mpedb-sqlitefmt`, no sqlite library in the path, differentially verified
against it), and **`mpedb checkpoint data.db`** folds the deltas back into the
sqlite file for every other tool to see. Three lock modes (`locked` speaks
sqlite's own byte-range locks, `optimistic` takes a µs-bracket per statement so
foreign sqlite writers interleave freely, `offline` for cooperative windows).
`--mirror` opts into a full sidecar import instead. `mpedb dump data.db`
inspects a `.db` directly, and `mpedb::SqliteAttach` runs read-only mpedb SQL
over one with **zero import**. The full design survived a 20-finding
adversarial review in
[`design/DESIGN-SQLITE-BACKED.md`](design/DESIGN-SQLITE-BACKED.md).

A missing path is created by the first **write** — reading from one creates
nothing, so `mpedb new.db` then `SELECT 1` leaves the directory as it was.
**`mpedb data.db notes.csv`** offers to import the CSV or analyse it in memory
without writing anything. In the repl, **Tab on an empty line lists the tables**
and arrow keys pick one.

**How close to drop-in sqlite3?** Close, and closing — but measure before you
plan around it. The SQL surface now covers aggregates, `GROUP BY`/`HAVING`,
`DISTINCT`, every join kind (aliases, self-joins, N-way chains),
`UNION`/`EXCEPT`/`INTERSECT`, scalar/`EXISTS`/nested/correlated subqueries,
`WITH RECURSIVE`, views, triggers, window functions (`OVER`, incl. explicit
frames), full-text search (`MATCH`/FTS5), `SAVEPOINT`, `COLLATE`,
`LIKE … ESCAPE`/`GLOB`/`REGEXP`, `ORDER BY … NULLS FIRST/LAST`, the JSON
function set (`json`, `json_extract`, `->`/`->>`, …), bitwise operators,
`printf`/`quote`/`strftime`, sqlite's type affinity, truthiness and permissive
`CAST`, rowid-alias `INTEGER PRIMARY KEY`, user-defined functions (scalar and
aggregate, registered through the libsqlite3 C-API shim — CPython's own
`sqlite3` module loads it via `LD_PRELOAD`), secondary/composite indexes
(including partial `CREATE INDEX … WHERE`, stored P1), and live multi-process
DDL — verified against sqlite's own 7.4M-record test corpus with **zero wrong
answers**. CPython's `test_sqlite3` under the shim is **98.3% of stock-passing
on M3** (459/467); Django frozen A and `queries` are **831/831** and **493/493**.
Remaining CPython failures are documented deliberate refusals
([`C-API-COMPAT.md`](C-API-COMPAT.md)). Django measured labels stay high-90s with
zero wrong answers. What is still missing is short — attached-database *writes*
(`ATTACH` + cross-file SELECT work), loadable extensions (non-goal), and a few
honesty refusals (AUTOINCREMENT, fts4 layout, serialize-as-sqlite-image) — each
a clean error, never a wrong answer. And on one axis mpedb goes *past* sqlite:
its own `.mpedb` WAL gives PostgreSQL-style **concurrent multi-process writes**
(MVCC snapshots, lock-free readers) where sqlite serializes every writer. See
[SQL support](#sql-support) for the exact surface, measured against the binary.

This cuts both ways, and honestly so: hardening mpedb against real sqlite3
databases is how mpedb gets hardened. Every dialect mismatch found by importing
someone's messy production data is a bug found before a migration, not during
one. (One is documented in the mirror section below: mpedb's own pre-flight
shipped reading sqlite schemas with PostgreSQL's rules — exactly the class of
error this project exists to catch, found by pointing it at the other dialect.)

**Snapshot and roll back with `cp`.** A `.mpedb` is one self-describing file —
the schema lives inside it, so a copy is a complete, independent database:

```sh
cp app.mpedb app.snap                     # snapshot
pytest                                    # let the suite do its worst
cp app.snap app.mpedb                     # roll back, instantly
```

On a copy-on-write filesystem this is not even a copy: `cp -c` on macOS (APFS)
and `cp --reflink` on Btrfs/XFS clone the file by sharing its blocks, so the
snapshot is instant and free until one side is written. Measured on an M3: `cp -c`
of a 256 MiB `.mpedb` took **0.00 s and used 0 bytes of disk**. On ext4 it is a
real (kernel-accelerated) copy — correct, just not free. Either way a `.mpedb`
being one file is what makes the whole workflow a single command.

Two honest caveats. Copy while **no process is attached and writing** — a live
`mmap`ed file can be caught mid-commit, exactly as with sqlite. And in `wal`
durability the `-wal` sidecar is part of the database: copy both, or neither.

**Where this is going.** The long-term ambition is to match PostgreSQL's
guarantees while keeping sqlite's simplicity — and to be good at the work that
actually happens now: data-science and AI pipelines, where a dataset gets read
by many processes at once, versioned, branched, and thrown away. Lock-free
readers, snapshot isolation, and single-file databases are a better fit for that
than either ancestor. It is not there yet; see Status.

> ⚠️ **Status: personal research project.** Crash-safe on Linux (x86-64 and
> 32/64-bit ARM) and macOS/Apple Silicon — see [Platforms](#platforms). The
> design has been through multiple adversarial review rounds (see the
> `DESIGN*.md` docs), but this is not production-hardened software. Treat it as a
> serious experiment.

## Highlights

**Many processes writing one file, and none of them has to cope with that.**
This is the point. Concurrent *readers* are not — sqlite3 in WAL mode has those,
and in a like-for-like durable comparison it out-reads mpedb (658k vs 561k
reads/s; [BENCHMARKS.md](BENCHMARKS.md)). What sqlite3 does not give you is
several processes *writing* without `SQLITE_BUSY`, a retry loop and a
`busy_timeout` — the benchmark's sqlite3 adapter needs a **60-second**
busy_timeout to survive the contended-write cell at all. mpedb's writers queue
in an intent ring and a leader commits them as a group; nothing returns "database
is locked".

- **Concurrent writes, measured with real processes** — N processes `fork`ed
  onto one file, both engines native Rust, none-class, median of 3:

  | writer processes | mpedb | sqlite3 (WAL, 60 s busy_timeout) | |
  |--:|--:|--:|--:|
  | 1 | 302,284/s | 89,702/s | 3.4× |
  | 2 | 186,479/s | 88,551/s | 2.1× |
  | 4 | 250,992/s | 83,300/s | 3.0× |
  | 8 | 270,822/s | 78,877/s | 3.4× |

  Honest counterpart: with *durability on* concurrent writing is mpedb's
  **worst** cell — a tie with sqlite3 and **8× behind PostgreSQL**, because group
  commit only amortizes what one writer lock lets through. See
  [BENCHMARKS.md](BENCHMARKS.md#known-issues--improvement-opportunities).
- **~300 KB of heap per writer process** — peak `RssAnon` across 4 concurrent
  writers: **1.2 MB for mpedb vs 4.4 MB for sqlite3**. (Peak *VmHWM* goes the
  other way, 196 MB vs 16 MB, and that is an accounting artifact worth knowing:
  mpedb mmaps the database, so the pages it touches are resident and charged to
  it, while sqlite3's same data sits in the OS page cache charged to nobody.
  `RssAnon` — what the engine actually allocated — is the comparable column.)
- **Any writer may be `SIGKILL`ed mid-commit** — no corruption, no wedged lock,
  no recovery step you have to run. Robust `PROCESS_SHARED` mutexes with
  `EOWNERDEAD` recovery, `/proc`-start-time reader identity, and a
  double-buffered meta page. Fuzzed on x86-64, Apple Silicon and 32-bit ARM.
- **Writers never block readers** — MVCC snapshots over a copy-on-write B+tree,
  50,000+ concurrent lock-free readers (config-sized reader table). sqlite3-WAL
  gives you this too; the difference is that here it holds while *many processes*
  write.
- **It runs where a server does not fit** — a Raspberry Pi 3 (armv7, 921 MB,
  already decoding ADS-B) does **6-7k writes/s across 1-4 processes on 72 KB of
  heap**. Slow, and that is the point: no daemon, no postmaster, no per-connection
  backend. PostgreSQL *does* run on a Pi — the difference is not that it cannot,
  it is that mpedb costs 72 KB and nothing while idle.
- **Write parallelism scales with FILES, not locks** — multi-database workspaces
  address several independent database files as `alias.table`. Separate files =
  separate writer locks = linear write parallelism, and the only OS-enforced
  isolation boundary. That is the architectural answer to the single-writer cell
  above, and it is deliberate rather than a workaround.
- **Durability modes** — `none`, `commit` (msync), `wal` (sequential log +
  fdatasync, durable-on-ack), `async` (deferred coalesced fsync). A durable
  commit costs exactly **two device flushes** — data, then meta — which is the
  floor the ordering requires; the batch amortizes it across every writer in it.
- **The join planner trades exponential memory for linear** — reordering a join
  chain is normally sold as a speed optimization. The larger effect is what the
  engine has to *hold*: on a 12-table chain the solved order peaks at **420 live
  join cells against 13.4 million** (31,905×), and the solved peak is exactly
  linear in width where the textual order gains a factor of ten per table. That
  number is deterministic, so it lives in a test rather than a benchmark. It is
  also **absent on ordinary shapes** — the corpus-median join is identical in
  both arms. See [`INNOVATIONS.md`](INNOVATIONS.md) §4.
- **Cooperative row-level security** — PostgreSQL-style `USING` / `WITH CHECK`
  policies keyed on a caller-set session context, injected transparently at plan
  time, with cache leak-proofing (a stale cached plan is re-validated against the
  live policy epoch under the executing snapshot). *In-file RLS is cooperative
  defense-in-depth, not a hard boundary against a hostile process that maps the
  raw pages — see [`design/DESIGN-MULTIDB.md`](design/DESIGN-MULTIDB.md) §6.*
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
| `mpedb-mirror` | Bidirectional sqlite3/PostgreSQL ⇄ mpedb mirroring: import, incremental diff-pull under load, write-back, epoch-fenced authority switch. Round-trip differential export/diff is sqlite-only; the CLI drives sqlite only (PostgreSQL is library-level today). |
| `mpedb-cli` | The `mpedb` binary: repl / exec / prepare / call / dump / stress / crash / powerloss / bench / proc / mirror. |
| `mpedb-testkit` | sqllogictest harness + 3-way differential testing vs sqlite3 and PostgreSQL. |
| `mpedb-bench` | Cross-engine benchmarks. |

## Using it

**[GUIDE.md](GUIDE.md)** is the practical guide: quickstart, the schema config,
queries, transactions, upsert, joins, durability, a side-by-side for people
coming from sqlite3, and migrating a real sqlite3 database. Every Rust snippet
in it is compiled and run by `crates/mpedb/tests/guide.rs`, and every shell
transcript is pasted from a real run.

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

- **Linux — x86-64 and 32-bit ARM** — the reference platform: full crash-safety
  (robust `PROCESS_SHARED` mutex with `EOWNERDEAD` recovery) and durability.
  32-bit ARM works because it has lock-free `AtomicU64`, and that is measured
  rather than argued — see the table below.
- **macOS — Apple Silicon** — crash-safe via the **FLD-2 writer lock**: a
  sidecar `flock` (which the kernel releases on holder death) plus a private
  `ERRORCHECK` mutex and a shared tri-state word give owner-death recovery
  equivalent to Linux's robust mutex; durability uses `fcntl(F_FULLFSYNC)` and
  16 KiB-aligned `msync`. All platform code is `#[cfg]`-gated behind
  `crate::os`, so the Linux path stays byte-identical.

Platform claims are verified on real hardware, and the table says which hardware:

| platform | what has actually run there |
|---|---|
| Linux x86-64 | everything: `cargo test --workspace`, clippy, the `stress`/`crash`/`powerloss`/`collide` harnesses across `none`/`commit`/`wal`, the 3-way differential |
| macOS / Apple Silicon (M3) | `cargo test --workspace`, clippy, the `crash` harness under SIGKILL waves across all durability classes (`eowner_recovery=true`), the benchmark suite |
| **Linux armv7l (32-bit ARM)** | 318 cross-compiled tests, 0 failures — including the whole `mpedb-core` shm/btree/COW suite — plus `examples/multiproc_check.rs`: 4 SIGKILL waves against 3 concurrent writer processes, `verify()` clean after each. A Raspberry Pi 3 B+, kernel 6.1. |
| Linux aarch64 (64-bit ARM) | **nothing yet.** Covered by inference from the other three, which is exactly the kind of claim this table exists to stop making. |

The 32-bit ARM row is the one worth explaining. This README used to assert that
"32-bit ARM works because it has lock-free `AtomicU64`" — a sound argument, and
an argument is not a measurement. It is now measured, and it holds: `armv7`
gives Rust native 64-bit atomics via `ldrexd`/`strexd`, so the packed
`{pid, seq}` reader words and the meta double-buffer are genuinely lock-free
across processes. A lock-based fallback would have been silently wrong — the
lock would live in one process's memory and guard nothing in another's.

ARM is also where the fences earn their keep. x86-64 is TSO, so a missing
barrier in the reader-pin protocol (design/DESIGN.md §4.3) usually hides; ARM is
weakly ordered and it would not.

See [`design/DESIGN-MACOS-LOCK.md`](design/DESIGN-MACOS-LOCK.md) for the macOS lock design.

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

## SQL support

Verified against the binary, not remembered. mpedb compiles SQL once to a
content-hashed plan; the surface is deliberately narrow, and the narrowness is
the design rather than a todo list. The highlights are below;
**[`COMPAT.md`](COMPAT.md) is the full feature-by-feature status** —
statements, clauses, operators, functions, types — in the same format as
Turso's COMPAT.md so the two read side by side.

It is also measured against sqlite's own **sqllogictest corpus** (the
`sqlite_corpus` runner in `crates/mpedb-testkit`), all 7.4 million records of it:
**99.9% of attempted statements pass, with zero error mismatches and zero genuine
wrong answers** — of everything mpedb accepts, essentially 100% matches sqlite.
What does not pass is deliberate refusals with error messages
([`design/CORPUS-STATUS.md`](design/CORPUS-STATUS.md) ranks them).

| | mpedb | note |
|---|---|---|
| `SELECT … WHERE / ORDER BY / LIMIT / OFFSET` | ✅ | |
| `INSERT` / `UPDATE` / `DELETE` | ✅ | |
| `ON CONFLICT DO NOTHING / DO UPDATE` + `excluded.` | ✅ | target: the PK, or one UNIQUE column |
| `RETURNING` | ✅ | on all three verbs |
| `IN` / `NOT IN`, `BETWEEN`, `CASE`, `LIKE`, `IS [NOT] NULL`, unary `+`/`-` | ✅ | full SQL 3VL |
| SELECT-item aliases (`expr AS name`, bare `expr name`) | ✅ | names the output; `ORDER BY alias` resolves the output first, as in PostgreSQL |
| Comma-joins (`FROM a, b WHERE …`) | ✅ | the cartesian product, desugared to `INNER JOIN … ON true` |
| `CAST(x AS type)` | ✅ | NULL→NULL; float→int truncates toward zero (sqlite's rule); **text never parses into a number** — refused instead of guessed |
| `\|\|` concatenation | ✅ | NULL propagates; ints/bools render as text; floats refused until their formatting is pinned |
| `lower upper length trim abs round substr coalesce ifnull nullif` | ✅ | `coalesce` is lazy |
| `<table>.<column>` qualifiers | ✅ | checked, not ignored |
| `COUNT` / `SUM` / `AVG` / `MIN` / `MAX`, `GROUP BY` / `HAVING` | ✅ | NULL rules verified against sqlite 3.45; keys may be expressions (`GROUP BY a/100`) or output ordinals (`GROUP BY 1`) |
| `SELECT DISTINCT`, `COUNT(DISTINCT x)` | ✅ | |
| `ORDER BY` by name, by ordinal (`ORDER BY 1`), or by a selected expression | ✅ | the key must be in the output; see below |
| N-way `INNER JOIN` chains (`FROM a JOIN b ON … JOIN c ON …`), incl. aggregates over them | ✅ | index nested loop when the `ON` has an equality; RLS applies to every side |
| Table aliases (`FROM emp e JOIN emp b ON …`) and self-joins | ✅ | alias shadows the table name, as in PostgreSQL |
| `LEFT [OUTER] JOIN` | ✅ | NULL-extends on no match; `WHERE inner IS NULL` anti-joins work |
| `RIGHT [OUTER] JOIN` (two-table) | ✅ | planned as a `LEFT` with the sides swapped — `SELECT *` keeps the original column order |
| `FULL [OUTER] JOIN` (two-table) | ✅ | NULL-extends BOTH sides; inside a multi-join chain both are refused with the manual fix |
| `CROSS JOIN` | ✅ | the cartesian product — desugars exactly like the comma-join |
| `UNION [ALL]` / `EXCEPT` / `INTERSECT` chains | ✅ | left-associative, sqlite's precedence; set ops dedup (NULLs equal); arms must agree on arity and exact types — `CAST` bridges deliberate mismatches |
| Secondary indexes: `unique = true` and non-unique `indexed = true` | ✅ | equality and range (`IndexScan`/`IndexRange`) — `EXPLAIN` shows which |
| Loose typing per column: `type = "any"` | ✅ | refused in keys and `UNIQUE`; the mirror pre-flight refuses pushing it to PG |
| **FROM-less `SELECT 3+5`** | ✅ | one synthetic row; WHERE filters it, aggregates see it (`SELECT count(*)` → 1), compound arms and subqueries may each be FROM-less |
| Scalar subqueries `(SELECT …)`, `[NOT] EXISTS (…)` — uncorrelated AND correlated | ✅ | one output column; 0 rows → NULL; **>1 row errors** (PG's rule — sqlite silently takes the first); correlated references become inner-plan parameters, the `OuterCol` idea applied to a whole plan |
| **Cross-FILE refs** (`ATTACH`) | ✅ | `ATTACH DATABASE 'f.mpedb' AS name`, then `main.t` / `other.u` qualification — joins, subqueries, aggregates and CTEs across files, each file pinned at its own snapshot per execution (sqlite's attached-WAL rule). Connection-local: never persisted, never published. Writes to an attached database are refused by name (open a handle on that file instead) |
| **Live DDL** (multi-process) | ✅ | `CREATE TABLE` (PK / `NOT NULL` / `UNIQUE`), `DROP TABLE [IF EXISTS]`, `ALTER … RENAME` (table or column), `ALTER … ADD COLUMN` (nullable, or `[NOT NULL] DEFAULT <const>`) / `DROP COLUMN`. Table ids are never reused (≤ 4096 lifetime creates; `regenerate` resets) |
| `ADD COLUMN … DEFAULT <const>` | ✅ | a constant default fills existing rows (and `NOT NULL DEFAULT <const>` is allowed) — differential-tested vs sqlite 3.45. `UNIQUE` / `PRIMARY KEY` on ADD, and `NOT NULL` *without* a non-NULL default, are refused — sqlite refuses these too (a non-constant default likewise). Type-mismatched default = clean error (rigid schema) |

**Joins, and what they cost.** Joins are a left-deep chain of up to 16 tables,
with aliases and self-joins. When a join's `ON` contains a plain equality
(`ON child.parent_id = parent.id`), the planner **consumes it into the inner
fetch**: each outer row does one PK get / index probe instead of pairing with a
held full scan — the index nested loop, preferring the PK, then a unique index,
then a non-unique one. Anything else in the `ON` stays as a residual over the
joined row. An `ON` with no equality keeps the read-once-and-hold nested loop
with its honest `O(n*m)` label. `EXPLAIN` says which form you got and where the
equality went.

**WHERE conjuncts push into the chain** the same way: each one runs at the
earliest join step where every column it reads is bound — so a comma-join
(`FROM a, b WHERE a.id = b.id`) is indexable exactly as if the equality had
been written in an `ON`, and single-table conjuncts prune their own step
instead of surviving to the full product. NULL-extension is the boundary: a
conjunct on a `LEFT` join's inner side stays after the join (it filters the
NULL-extended row — inside the `ON` it would decide matching instead), and a
`FULL` join disables pushdown entirely.

`LEFT JOIN` NULL-extends on no match — the extended row is built *because*
nothing matched, so the `ON` is never evaluated over it and cannot raise on it.

One structural caveat stands: the statement's `key_access` widens to `Full`,
because that field names one key space and a Point on the outer stops
describing what the statement reads once a second table joins in — that costs
conflict precision for concurrent writers, never correctness.

RLS applies to **every** side, each policy over its own row and before the `ON`
(or its pushed-down residual) — mpedb's expressions can raise, and a raise is
observable, so an `ON` that divides by a hidden row's column would report that
row's existence without returning it. Under `LEFT JOIN`, a policy-hidden inner
row reads as *absent*: the outer row survives NULL-extended and never carries
the hidden row's values. The plan stamps every table whose policy it baked in,
so tightening any side's policy invalidates a cached join plan. This ordering
contract is mutation-tested on the raise path, in both execution forms.

The scaling story is still *more files* where it can be: separate files are
separate writer locks, and that is the only OS-enforced isolation boundary here.
And if you need the full relational surface, you need PostgreSQL — mpedb's job is
to get you there safely, not to replace it.

**Where `ORDER BY` is narrower than sqlite/PG.** The sort key must be something
the query outputs — a column of the table, an output position, or an expression
from the `SELECT` list. `SELECT c FROM t ORDER BY a + 1` is refused where both
engines allow it. And under `SELECT DISTINCT` the key must be in the `SELECT`
list, as in PostgreSQL: once duplicates collapse, a key outside the output means
*which* duplicate survived is what decides the order, and the query never said.

**Stable table ids under live DDL.** A table's id keys the catalog roots, the
CDC bitmap, and the mirror's per-table state, so it is explicit in the file (not
a sort position): `CREATE`/`DROP` never renumber, and a dropped id is never
reused — capping lifetime creates at 64 (`regenerate` resets it). See
[`design/DESIGN-DROP-TABLE.md`](design/DESIGN-DROP-TABLE.md).

## Performance

Head-to-head against SQLite and PostgreSQL through one shared Rust measurement
loop (each engine on its own fast path — mpedb's `execute(hash, …)`, prepared
statements for the others). **[`BENCHMARKS.md`](BENCHMARKS.md) is the detailed
comparison** — methodology, every machine, and a link to each machine's full
generated tables. The highlights from all of them are below.
[Turso](https://github.com/tursodatabase/turso), the Rust SQLite rewrite, is
measured as a fourth engine — numbers and a compatibility-parity comparison in
[design/TURSO.md](design/TURSO.md).

Two things to know before reading any of it: numbers are only comparable
**within a durability class** (none-class has no fsync guarantee, commit-class is
durable on ack), and the machine must be **idle** — a stray process holding one
of this box's two cores *compressed* the parallelism results (6.8× → 2.4×)
rather than merely adding noise.

And three practical rules the numbers keep teaching: for **durable writing use
`durability = "wal"`**, not commit-mode — a lone commit-mode writer pays a
serialized flush per commit, while wal wins its class outright; on **macOS,
commit-mode pays two platter flushes by design** (data before meta is the
crash-safety ordering — two is the floor, not a bug); and the **cold blob
numbers measure a fresh file** — a long-lived process sees roughly 4× better,
because the first write to each mapped page pays a fault the steady state does
not.

And one finding worth stealing even if you never use mpedb: **for deciding
whether a change helped, a Raspberry Pi 3 running a live ADS-B decoder is a 6×
better instrument than this dev box** — 1.6% run-to-run CV against 9.0%. Steady
load beats fast-but-bursty. Three reps at 9% CV had us reject a real +3.5%
improvement as a "regression", with a commit message to match. BENCHMARKS.md has
the method and the two other ways the same A/B went wrong first.

### Linux — AMD EPYC-Milan, 2 cores (re-run 2026-07-16)

Single-client, embedded, none-class point ops:

| op (none-class) | mpedb | SQLite | PostgreSQL |
|---|--:|--:|--:|
| point-select (PK), ops/s | **485,215** | 80,467 | 22,329 |
| point-insert, ops/s | **173,054** | 42,170 | 14,739 |
| point-update (PK), ops/s | **212,492** | 46,954 | 10,942 |

Re-measured after the #37 leak fix and the #42 row-buffer removal; every cell
landed within this box's noise floor of the 2026-07-14 run, which is the point —
neither change was supposed to move small-row ops, and neither did.

mpedb leads embedded point ops (~4-22×; zero-parse plans + no IPC + a COW B+tree
in-process). Under a live writer its MVCC readers never take the writer's lock:
**486k read ops/s at 2 µs p50 vs SQLite's 3.5k** (none-class — SQLite's journal
serializes readers against the writer, p99 18 ms). Give SQLite its WAL and it
edges mpedb instead (641k vs 561k) — that cell is single-process, which is
exactly where mpedb's multi-*process* readers and shared plans do not show.
Durable writes: `wal` leads single-client (1,883 vs 864 / 1,742) and batched
100/commit (**132k** vs 62k / 18k). Weakest cell: `durability=commit`
single-client (~390 ops/s) — every commit msyncs with no batching partner; use
`wal`. Contended writes (4 threads) mpedb leads 126k vs 28k/34k, but that is the
cell most sensitive to core count — see [BENCHMARKS.md](BENCHMARKS.md).

### Apple Silicon — M3 Pro, 11 cores, macOS 26.6 (2026-07-14)

All three engines.

Eleven cores is where the design story stops being theoretical. `read-while-write`
none-class: **mpedb 3,704,543 reads/s vs SQLite's ~180, p99 ~150 seconds** —
SQLite's none-class journal serializes readers against a writer that now has ten
spare cores to starve them with. A pathological config rather than a fair fight,
but it is the exact failure mpedb's MVCC readers exist to avoid, and more cores
make it worse rather than better. The same cell on the 2-core Linux box reads
486k vs 3.5k: same phenomenon, two orders of magnitude apart — which is why the
2-core numbers *understate* this one.

Bulk write flips the other way from Linux: mpedb **2,561 MiB/s (39% of raw)** vs
SQLite 988 (15%) — 2.6×. On the 2-core Linux box SQLite leads that cell; give
mpedb cores and a fast SSD and it does not.

**Streaming blob insert (2026-07-16).** `WriteSession::insert_streaming` PULLS a
large value a page at a time instead of taking a `Value::Blob(Vec<u8>)`, so it is
never resident. A 256 MiB blob costs **+132 KiB of anonymous RSS** — 2000× less
than the value itself — and reads back byte-identical. Total RSS still grows (the
file's pages are mapped) but those are page cache the kernel reclaims, not memory
the caller has to find; on a box with no swap that is the difference between
running and being OOM-killed.

It pulls rather than handing out a writer on purpose: a `write_all(chunk)` API
would hold the writer lock across caller code, so a blob arriving off a socket
would block every other writer for as long as the network took. This is also why
sqlite's `sqlite3_blob_open` shape does not port — it assumes in-place mutation
of an existing blob, and mpedb is COW, so an "in-place" write would copy the
whole chain and hand back the memory win it existed to get.

"Put this file in the database" is one call on top of it:
`WriteSession::insert_file(table, values, stream_col, path)` opens the file and
streams it in under the same memory ceiling.

**…and then 5× faster again (2026-07-16, #40 closed).** After the buffer below
was removed, the blob was STILL deep-cloned twice more on its way in — once in
parameter resolution, once building the insert row. Both paths now borrow the
caller's values when nothing needs computing (almost every statement), taking a
warm 16 MiB insert from 12.1 ms to ~2.2 ms. The remaining gap to a raw file
write is page faults on cold pages, which is a storage-layout question
([#50](design/DESIGN.md)), not a copy.

**Large blobs got 77% faster (2026-07-16).** `row::encode_row` materialised the
whole row — blob included — into a fresh heap buffer whose only purpose was to be
copied straight back out into overflow pages; at 16 MiB that malloc faults its own
anonymous pages and cost **42% of the insert**. `btree` now takes the row's parts
and never joins them: **660 → 1,170 MiB/s**. Note the bulk cells above did NOT
move, and that is correct — they use 4 KiB values, where the buffer is a trivial
malloc. The copy was only ever expensive when it was big.

**And the durable-write result is that there is no result.** Once every engine is
made to actually reach the platter, single-client durable inserts land at
**mpedb 318 ops/s, SQLite 310, PostgreSQL 429** — three engines, three
independent implementations, agreeing within 40%. That is not engineering, it is
the ~3 ms an Apple SSD takes to flush, and nobody beats it. Any benchmark showing
one of them far ahead here is showing you a bug.

Getting there took catching all three of them skipping the flush, one at a time:

macOS's `fsync()` does not flush the drive's write cache — only
`fcntl(F_FULLFSYNC)` does. mpedb's `durability=commit` barrier is
`msync(MS_SYNC)`, which on macOS hands pages to the drive and returns *before*
they are on platter. So mpedb reported ~10× SQLite on durable commits by not
actually being durable. Once both were honest, `wal` (293 ops/s) landed level
with SQLite+F_FULLFSYNC (286): **~290 ops/s is simply what an Apple SSD platter
flush costs**, and anything above it on that machine is a promise no one is
keeping.

And mpedb's `durability=commit` is still **2× that floor** on Apple (p50 7.0 ms),
for a reason worth naming: `msync_range` issues one `F_FULLFSYNC` **per call**,
and a commit makes one call per contiguous dirty-page run plus one for the meta
flip — so a commit costs *(runs + 1)* whole drive-cache flushes. `F_FULLFSYNC` is
per-**fd**, not per-range, so one barrier before the ack would do. That is a
Linux-shaped optimisation (there `msync(MS_SYNC)` really does sync only the
range) meeting a platform where it multiplies. Logged as known-issue #0; use
`wal`. Details: [BENCHMARKS.md](BENCHMARKS.md#apple-silicon-m3-pro-11-cores--and-the-durability-trap-it-exposed).

**Bulk bytes: extents changed the game — measured, per platform.** Large
values now take the WiscKey path from
[`design/DESIGN-BLOBEXTENT.md`](design/DESIGN-BLOBEXTENT.md): immutable extents written
once via `pwrite`, with the COW tree keeping a 20-byte reference and every
crash-safety property intact (SIGKILL-fuzzed and power-loss-simulated in
both WAL modes). Paired same-binary A/B (`examples/blob_bulk_ab`): on Linux
the extent path is **2.1–2.8× faster from 64 KiB up** (5.4 GB/s on 1 MiB
blobs, tmpfs) and wins from ~8 KiB; on macOS it currently LOSES below ~1 MiB
(sparse preallocation makes each payload pwrite allocate APFS blocks), so
the default differs per platform: **on by default at 16 KiB on Linux, off on
macOS** — `extent_threshold_kb` in the config overrides either way (`0` =
off). The 4 KiB cell and the macOS curve share one queued fix: per-commit
coalesced `pwritev`. See
[BENCHMARKS.md](BENCHMARKS.md#bulk-mbs--and-the-number-that-makes-it-mean-something).

```sh
cargo run --release -p mpedb-bench      # full head-to-head -> RESULTS-<machine>.md
cargo run --release -p mpedb-bench -- --io   # bulk MiB/s vs a raw-Rust baseline
mpedb bench --auto --durability wal     # quick mpedb-only
```

> Measured on an idle shared 2-core VM (two back-to-back runs agree within ~4%).
> Every earlier run was distorted by a stray process pinning one core — which
> left single-client ratios intact but silently compressed the parallel cells.
> SQLite/PostgreSQL act as the control group: if all three engines move together
> it is the host, not mpedb's code
> ([method](BENCHMARKS.md#reading-run-to-run-deltas--the-control-group-method)).

## Mirroring & cross-database migration

mpedb mirrors a live sqlite or PostgreSQL database into a local `.mpedb`, lets
you use it while **both sides keep writing**, pulls incremental diffs under
concurrent source write load, pushes local changes back, and switches which side
is authoritative — in both directions, repeatably. The protocol is specified in
[`design/DESIGN-MIRROR.md`](design/DESIGN-MIRROR.md) (v1.1, hardened against a 58-finding
adversarial review).

**What works today, and where:**

| | sqlite | PostgreSQL |
|---|---|---|
| import, pull, push, switch, reconcile, conflicts | ✅ library **and** `mpedb mirror` CLI | ✅ library **and** CLI (`--source-config`) |
| export into a **fresh** database (`mpedb → X`) | ✅ `mirror export` / `mirror roundtrip` | ✅ `mirror export --to postgres` |

- **Stage & analyse** — pull a PostgreSQL database into a local `.mpedb`, run
  extra queries, add local tables, compute, then push changes back to
  PostgreSQL **without losing the data PostgreSQL owns**.
- **Migrate** — `sqlite3 → mpedb → PostgreSQL` works end to end. A
  PostgreSQL-sourced mirror round-trips its schema *exactly*: `int4` comes back
  as `int4`, `varchar(8)` as `varchar(8)`, `numeric(6,2)` as `numeric(6,2)` —
  the declared types are recorded at import (`mir/map`) and replayed, rather
  than flattened into mpedb's six types.
- **See what you lose** — the round-trip diff reports exactly which values cannot
  survive `sqlite → mpedb → sqlite`, so a lossy mapping is explicit, never silent.
- **Fail before you write, not halfway through** — `mirror preflight` checks
  every value against the recorded source schema without contacting the source,
  and `export --to postgres` refuses to start if anything would be rejected. A
  half-loaded target is worse than no target.

**Two honest limits.**

*A sqlite source exports with widened types.* sqlite's declared types are
[affinities](https://sqlite.org/datatype3.html), not constraints, and its
vocabulary collides with PostgreSQL's while meaning something different: sqlite's
`INTEGER` is 64-bit where PostgreSQL's `integer` is int4, and sqlite's `REAL` is
a double where PostgreSQL's `real` is single precision. Copying those words into
PostgreSQL would reject every value above 2³¹ and silently round every float to
~7 digits, so `sqlite → PG` deliberately emits the widest safe type
(`bigint`/`double precision`/`text`) and the CLI says which tables that affected.
Exact narrow types survive `PG → mpedb → PG`, not `sqlite → mpedb → PG`, because
sqlite never had them to begin with.

*Credentials are a file, never a flag.* There is no `--dsn`: `ps` shows every
process's argv to every user on the host. A PostgreSQL source is named by a 0600
config file whose mode and owner are re-checked on every read
(DESIGN-MIRROR §12).

```sh
# --- sqlite: --source is a path, no secret involved ---
mpedb mirror import --source app.db --dest app.mpedb   # snapshot + install change capture
mpedb mirror pull   --source app.db --db app.mpedb     # apply source changes into mpedb

# --- PostgreSQL: the DSN lives in a 0600 file, named by path ---
install -m600 /dev/null pg.toml            # born 0600, before a secret is in it
cat >> pg.toml <<'EOT'
kind = "postgres"
dsn  = "host=db.internal dbname=app user=app password=s3cr3t"
EOT

mpedb mirror import --source-config pg.toml --dest app.mpedb
mpedb mirror sync   --db app.mpedb         # the config path is recorded: --db is enough
mpedb mirror switch --db app.mpedb --to mpedb
mpedb exec          app.mpedb "UPDATE items SET qty = qty + 1"
mpedb mirror push   --db app.mpedb         # local writes land back in PostgreSQL

# --- migrate into an EMPTY PostgreSQL ---
mpedb mirror preflight --db app.mpedb                                # analyse first
mpedb mirror export    --db app.mpedb --to postgres --source-config target.toml
```


Crash-safety of the sync daemon is fuzzed with `mpedb mirror-collide`: source-
writer processes churn the source while a mirror daemon is SIGKILLed at every
instant; after the writers stop, a final drain must converge mpedb *exactly* to
the source — no operation lost or duplicated across the kills.

## Design docs

[`INNOVATIONS.md`](INNOVATIONS.md) is the guided tour: what mpedb invented, what
it borrowed and from where, what it moved out of another field into a database —
and, at the same length, what was built, measured and rejected. Each technique
states the problem before the mechanism, so no database background is assumed.

The design documents are the load-bearing contracts — **read them before touching
concurrency, lock, or commit-path code:**

- [`design/DESIGN.md`](design/DESIGN.md) — the core engine, concurrency, and crash-safety protocols.
- [`design/DESIGN-MULTIDB.md`](design/DESIGN-MULTIDB.md) — parallel databases + cooperative RLS.
- [`design/DESIGN-MIRROR.md`](design/DESIGN-MIRROR.md) — bidirectional sqlite/PostgreSQL mirroring & migration.
- [`design/DESIGN-DDL.md`](design/DESIGN-DDL.md) — live `CREATE`/`DROP`/`ALTER TABLE` on a running
  multi-process database (design; stable table ids are stage 0).
- [`design/DESIGN-MACOS-LOCK.md`](design/DESIGN-MACOS-LOCK.md) — the FLD-2 macOS crash-safe writer lock.
- [`design/DESIGN-MPEE-OPT.md`](design/DESIGN-MPEE-OPT.md), [`design/DESIGN-PHASE3.md`](design/DESIGN-PHASE3.md) —
  measured-and-documented explorations (including directions that were falsified
  and deliberately *not* shipped).

Inspired in part by [pyspell](https://github.com/punnerud/pyspell) (parse-once-to-IR)
and [mpee](https://github.com/punnerud/mpee) (streaming matrices / route optimization).

## License

Released under the [MIT License](LICENSE).

---

*MPE stands for Morten Punnerud-Engelstad.*
