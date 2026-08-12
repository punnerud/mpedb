# MPEdb

[![Linux](https://github.com/punnerud/mpedb/actions/workflows/linux.yml/badge.svg?branch=main)](https://github.com/punnerud/mpedb/actions/workflows/linux.yml)
[![macOS](https://github.com/punnerud/mpedb/actions/workflows/macos.yml/badge.svg?branch=main)](https://github.com/punnerud/mpedb/actions/workflows/macos.yml)
[![Windows](https://github.com/punnerud/mpedb/actions/workflows/windows.yml/badge.svg?branch=main)](https://github.com/punnerud/mpedb/actions/workflows/windows.yml)
[![Pages](https://github.com/punnerud/mpedb/actions/workflows/pages.yml/badge.svg?branch=main)](https://github.com/punnerud/mpedb/actions/workflows/pages.yml)
[![PyPI](https://img.shields.io/pypi/v/mpedb.svg?v=2)](https://pypi.org/project/mpedb/)
[![Python](https://img.shields.io/pypi/pyversions/mpedb.svg?v=2)](https://pypi.org/project/mpedb/)

**An embedded, multi-process, shared-memory database in Rust — a measured
drop-in for sqlite3, with PostgreSQL-grade concurrency on top.**

> ### Four suites, four 100 % scores — measured, not claimed
>
> Every number below is **differential**: the same statements run against real
> sqlite, and the ecosystem suites run twice — stock sqlite vs mpedb — so a
> pass means *identical behaviour*, not "no crash". As of 2026-08-01:
>
> | suite | scale | result |
> |---|---|---|
> | sqlite's own **sqllogictest corpus**, record by record against sqlite | 7,420,638 records | **100 % — 0 wrong answers, 0 error mismatches**, on x86-64 AND arm64 |
> | **Django 5.2** — the ENTIRE suite, every label, mpedb as the database | 18,214 tests per arm | **0 shim-only failures** — the 13 failing IDs are identical under stock sqlite |
> | **SQLAlchemy 2.0** — the dialect suite | 1,544 collected | **1277 passed / 0 failed** — exact stock parity, skips included |
> | **CPython's `test_sqlite3`** — the authoritative DB-API test | 466 tests | **466/466** through the C-API shim |
>
> What mpedb does not support is a **named refusal** — a clean error message,
> never a silent wrong answer. The full ledgers: [COMPAT.md](COMPAT.md) (the
> SQL surface, feature by feature), [C-API-COMPAT.md](C-API-COMPAT.md) (the
> libsqlite3 shim), [benchmarks/README.md](benchmarks/README.md) (every
> measured comparison), [INNOVATIONS.md](INNOVATIONS.md) (what is new here —
> and what was measured and rejected), and [design/](design/) (the
> load-bearing contracts).

**The drop-in is ABI-level, not a porting guide.** `crates/mpedb-capi` builds
`libmpedb_sqlite3.so`, a cdylib exporting sqlite3's C-API: `LD_PRELOAD` it (or
link it as libsqlite3) and a libsqlite3 consumer — **CPython's own `sqlite3`
module, unchanged** — runs against mpedb. That binary is what the CPython,
Django and SQLAlchemy scores above go through. In Python, `import mpedb as
sqlite3` (the native PyO3 wheel) is the one-line alternative, and
`mpedb data.db` opens an existing sqlite file directly.

**And compatibility is the floor, not the product.** MPEdb keeps sqlite's
operational model — no server; processes `mmap` a shared file and attach
directly, and any process may be `SIGKILL`ed at any instant without corrupting
the database — and adds what sqlite lacks:

- **PostgreSQL-grade concurrency** — MVCC snapshots over a copy-on-write
  B+tree, and lock-free readers that never block (and are never blocked by)
  the writer. Writes take a single writer lock, as in sqlite — but readers
  keep reading at full speed while they happen, group commit batches durable
  writes, and any attached process may write, with no server and no
  busy-polling `SQLITE_BUSY` storms.
- **Rigid schema & integrity validation** — typed columns, NOT NULL / UNIQUE /
  CHECK, and a file-authoritative schema that hard-errors on config drift.
  (With one deliberate, per-column escape hatch: `type = "any"` accepts any
  scalar in that column — sqlite-style flexibility where you ask for it,
  rigidity everywhere else. An `any` column cannot be a key.)
- **Content-hashed compiled plans** — SQL compiles once; the hot path is
  `execute(hash, params)` with zero parsing, and plans carry precomputed
  read/write footprints.
- **Measured performance** — every comparison has a control arm and lives in
  [`benchmarks/`](benchmarks/README.md), sqlite and PostgreSQL included.

> **[Run mpedb in your browser →](https://punnerud.github.io/mpedb/)** — the
> **real engine** compiled to `wasm32`, not a simulation: write SQL against a
> live in-memory database and see the plan, its content hash, the precomputed
> footprint, and the MPEE join reordering next to your results — including the
> refusals sqlite would have coerced. The page is explicit about the one thing
> it cannot show you: multi-process writers (see
> [DESIGN-WASM-MULTIWRITER.md](design/DESIGN-WASM-MULTIWRITER.md)).

## Install

**Python** — a drop-in `sqlite3` replacement, published to
[PyPI](https://pypi.org/project/mpedb/) automatically whenever the full test
suite is green on `main` (Linux x86-64/aarch64/armv7, Windows x86-64 +
macOS arm64/x86-64 wheels, CPython 3.10+):

```sh
pip install mpedb
```

```python
import mpedb as sqlite3   # existing sqlite3 code runs unchanged
```

`connect("app.db")` reads your existing sqlite file and keeps it in sync;
`connect("app.mpedb")` is the native engine. Details:
[crates/mpedb-py](crates/mpedb-py/README.md).

**Any libsqlite3 consumer** — no build needed: every
[release](https://github.com/punnerud/mpedb/releases) since v0.2.0 ships the
ABI-level shim prebuilt (Linux x86-64 `.so`, macOS arm64 `.dylib`, sha256sums
alongside). Download, unpack, preload — the program's own sqlite3 calls land
in mpedb:

```sh
curl -LO https://github.com/punnerud/mpedb/releases/latest/download/libmpedb_sqlite3-0.2.1-linux-x86_64.tar.gz
tar xzf libmpedb_sqlite3-0.2.1-linux-x86_64.tar.gz
LD_PRELOAD=$PWD/libmpedb_sqlite3.so python3 app.py   # CPython's own sqlite3 module, unchanged
```

`pip install mpedb` is NOT this: the pip package is its own module you import
*instead of* `sqlite3`. The preload route is for code you do not want to touch
— frameworks included. A Django project switches its test run without changing
a line or a setting (the `django.db.backends.sqlite3` backend runs unchanged,
migrations included):

```sh
LD_PRELOAD=$PWD/libmpedb_sqlite3.so python3 manage.py test
```

This is the binary CPython's `test_sqlite3` (466/466) and the Django and
SQLAlchemy suites run through. Building it yourself is one command when you
want HEAD instead of a release:

```sh
cargo build --release --manifest-path crates/mpedb-capi/Cargo.toml
```

Per-function status, macOS interposition (`DYLD_INSERT_LIBRARIES`, with the
macOS-26 caveat), and the refusal list: [C-API-COMPAT.md](C-API-COMPAT.md).

**CLI** — the `mpedb` binary (REPL, dump, stress/crash harnesses, benchmarks,
sqlite mirror/checkpoint). On macOS and Linux, from the tap:

```sh
brew install punnerud/mpedb/mpedb
mpedb data.db     # opens an existing sqlite .db directly
```

Or grab it prebuilt from the [releases](https://github.com/punnerud/mpedb/releases)
— Linux x86-64, macOS arm64 and Windows x86-64 (`mpedb.exe`), no toolchain
required.

Or from source anywhere a Rust toolchain runs:

```sh
cargo install --git https://github.com/punnerud/mpedb mpedb-cli
```

Linux, macOS and Windows all run the whole engine and its multi-process crash
tests. macOS and Windows run nightly, Linux on every change.

> **[mpedb vs sqlite3 vs Cursor's minisqlite →](benchmarks/minisqlite.md)** — what
> each engine can even be asked to do, and speed on operations an application
> actually runs. Both directions plotted: 4000× ahead on indexed `min`/`max`,
> 0.40× on a single-row INSERT.

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

**And "once" now holds for callers that never prepare.** Every cache here is
keyed by the hash OF the finished plan — safe by construction, but you have to
compile to learn the key, so a caller passing SQL text re-derived a plan it had
already derived. That is most of a small statement, and it is where the
sqlite-compatible C-API surface lives: `sqlite3_prepare_v2` stores the text and
each `step` hands it back. A SQL-text memo now sits in front, keyed on
`(text, schema generation, per-table cost magnitude, RLS policy epoch)` — the
compile's actual inputs, so a hit is the plan a recompile would have produced
rather than an approximation of it. Measured on one idle box, 20k single-row
inserts, against three control arms that this change does not touch and which
did not move:

| | before | after | |
|---|---|---|---|
| `query(sql)` | 13.80 µs | **5.41 µs** | 2.55× |
| `session.query(sql)`, in a transaction — every ORM | 11.30 µs | **3.76 µs** | 3.0× |

The equivalence is checked rather than argued: under `MPEDB_VERIFY_PLAN_MEMO=1`
(and in every debug build) each hit is recompiled and the hashes compared, which
turns every test, all 7.4M sqllogictest records and every Django statement into
an invalidation test. It found two invalidation bugs on its first run, neither
of which was on the list that motivated it.

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
answers**, and every ecosystem suite that measures this surface — Django,
SQLAlchemy, CPython's `test_sqlite3` — passes completely through the C-API
shim: the four-suite scoreboard at the top of this page is the measured
answer, and [`C-API-COMPAT.md`](C-API-COMPAT.md) is the per-function ledger
behind it. What is still missing is short — attached-database *writes*
(`ATTACH` + cross-file SELECT work) and loadable extensions (non-goal) — each
a named refusal: a clean error, never a wrong answer. And on one axis mpedb goes *past* sqlite:
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
> 32/64-bit ARM), macOS/Apple Silicon and Windows x86-64 — see
> [Platforms](#platforms). The
> design has been through multiple adversarial review rounds (see the
> `DESIGN*.md` docs), but this is not production-hardened software. Treat it as a
> serious experiment.

### "It's a one-person 0.1.x — if it breaks, I'm alone with it"

That is the right objection, and it is worth separating into the part that is
true and the part that is an assumption.

**The true part.** This is version 0.1.x with one author. There is no
community, no track record in production, and no second pair of eyes that has
not been asked for. Anything below is about how *fixable* a defect is, never
about how many there are.

**The assumption.** "Small project" is usually shorthand for "no specification"
— when something misbehaves, you argue about what it was supposed to do, and
that argument is what you cannot outsource. Here the specification is external
and executable, and none of it was written by this project:

| what pins the behaviour | scale | current | where it runs |
|---|---|---|---|
| sqlite's own **sqllogictest** corpus, run differentially | 7,420,638 records | **0 wrong answers, 0 error mismatches** — on x86-64 AND arm64 ([COMPAT.md](COMPAT.md)) | by hand at a named commit; a curated subset rides `cargo test` |
| the **differential oracle** — the bundled sqlite3 answering the same generated program in-process | every run | any divergence is a record with both engines' answers next to it | in `cargo test --workspace`: Linux, macOS and Windows on every push |
| the **three-way** arm, adding a throwaway PostgreSQL 16 cluster | every run where a cluster can start | catches what sqlite alone cannot: the strictness a deploy will apply | Linux; it SKIPS loudly, by name, where PostgreSQL is absent |
| **CPython's `test_sqlite3`** — the authoritative test of the DB-API surface | 466 tests | **466/466** through the C-API shim — zero failures, zero errors ([C-API-COMPAT.md](C-API-COMPAT.md)) | by hand |
| **Django 5.2**'s own suite, EVERY label, mpedb as the database | 18,214 tests per arm | **0 shim-only failures** — the 13 failing IDs are identical under stock | by hand, both arms serialized |
| **SQLAlchemy 2.0**'s dialect suite (`test/dialect/test_suite.py`) | 1,544 collected | **1277 passed / 0 failed** — exact stock parity, skips included | by hand |

A defect on that surface does not arrive as a hunch. It arrives as a named
failing record with the answer sqlite gives sitting beside it, reproducible by
one command. That is the shape of problem a careful engineer, or an AI agent,
closes in a loop: change, re-run the corpus, keep the number that went up. The
expensive kind of bug is the one where the correct answer has to be *invented*,
and on the SQL and DB-API surface somebody else's test suite already answers it.

Be precise about what runs where, because it is easy to overclaim. The full
corpus and the two ecosystem suites are run **by hand** at named commits, not in
CI. What CI runs on **all three** platforms is the engine and facade suites
including the curated sqllogictest files and the differential tester, all six
`SIGKILL` harnesses, and the CLI end to end. The Python wheel's own suite runs
on Linux and Windows; macOS is the gap, and it is a build-config one
(`-undefined dynamic_lookup` for the PyO3 cdylib), not an engine one. See
[Platforms](#platforms).

**Where that argument stops, and it matters.** No external suite covers the part
of mpedb that is not sqlite: several processes writing one file, MVCC snapshots,
the commit path, crash-safety under `SIGKILL`. There is no corpus to borrow, so
those are held up by this project's own harnesses (`stress`, `crash`,
`powerloss`, `collide`, `queue-collide`, `mirror-collide` — all six on all three
platforms) and by adversarial design review. That is exactly where the genuinely
hard defects have been: an unbounded high-water leak under concurrent churn
(#37), a statement-atomicity hole in the group-commit ring (#119), and a page
shared between two trees that took four writer processes to reproduce (#160).
Each was found by a harness, not by a corpus, and each took real diagnosis. If
you are betting on this engine, that is the surface to be sceptical about — not
whether `strftime('%j', …)` matches.

**So what should you actually do?** Not switch. Add it as a second opinion:

1. **Keep sqlite3 as your default; run the test suite against mpedb too.** The
   rigid schema is the point — it fails on the string in an `IntegerField`, the
   missing `null=False`, the value that will not fit PostgreSQL's `int4`, at the
   moment you write it rather than at deploy. `import mpedb as sqlite3` is the
   whole change — but read [PY-COMPAT.md](PY-COMPAT.md) first for how far that
   actually carries: two real projects' suites were swapped this way and the
   scores are honest rather than flattering (diskcache 67 of 87 at 0.1.3,
   sqlitedict 2 of 89), because what blocks is a small fixed *connection
   bootstrap ritual* — `PRAGMA journal_mode` and friends — that runs before any
   query. The headline finding there is the one that matters for using it as an
   oracle: **zero silent wrong answers across all three suites**. Every
   divergence was a loud exception.
2. **Use `mpedb mirror` to pre-flight the migration you are actually worried
   about** — it validates schema and data against a real PostgreSQL before you
   run it for real.
3. **Only then consider making it the primary local database**, and only if
   multi-process local writes are something you actually want.

The honest summary: use it as a sharpening layer during development, not as a
replacement. What it is *not* is a black box — every claim in this README has a
command under it, and the ones about performance have a control arm.

## Highlights

**Many processes writing one file, and none of them has to cope with that.**
This is the point. Concurrent *readers* are not — sqlite3 in WAL mode has those,
and in a like-for-like durable comparison it out-reads mpedb (658k vs 561k
reads/s; [benchmarks/head-to-head.md](benchmarks/head-to-head.md)). What sqlite3 does not give you is
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
  [benchmarks/head-to-head.md](benchmarks/head-to-head.md#known-issues--improvement-opportunities).
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
| `mpedb-sqlitefmt` | Native reader for the sqlite file format (no sqlite library in the path), differentially verified against sqlite — what `mpedb data.db`, the overlay and `dump` read through. |
| `mpedb` | Facade: `Database`/`Workspace`, prepare/execute/query, write sessions, session context, RLS policy storage + injection, shared plan registry. |
| `mpedb-sdk` | Caching client session. |
| `mpedb-proc` | PySpell-style Python/Rust → budgeted IR stored procedures, streaming cursors. |
| `mpedb-py` | PyO3 module (`abi3-py310`), GIL released around engine calls. |
| `mpedb-capi` | The libsqlite3 ABI shim: a cdylib exporting sqlite3's C-API (`libmpedb_sqlite3.so`), `LD_PRELOAD`ed or linked by any libsqlite3 consumer — CPython's `sqlite3`, language bindings, tools. Its own workspace; status in [C-API-COMPAT.md](C-API-COMPAT.md). |
| `mpedb-mirror` | Bidirectional sqlite3/PostgreSQL ⇄ mpedb mirroring: import, incremental diff-pull under load, write-back, epoch-fenced authority switch. Round-trip differential export/diff is sqlite-only; the CLI drives sqlite only (PostgreSQL is library-level today). |
| `mpedb-cli` | The `mpedb` binary: repl / exec / prepare / call / dump / stress / crash / powerloss / bench / proc / mirror. |
| `mpedb-testkit` | sqllogictest harness + 3-way differential testing vs sqlite3 and PostgreSQL. |
| `mpedb-bench` | Cross-engine benchmarks. |

## Using it

**[SQL-EXTENSIONS.md](SQL-EXTENSIONS.md)** — SQL is user-extensible: stored
PySpell functions (shared across processes, unlike C-API UDFs) and `:sym:`
custom operator macros, both living in the database file and resolved by the
workload model's roles.

**[INGEST-GUIDE.md](INGEST-GUIDE.md)** — pulling an external system IN
without hammering it. You write the code that talks to the API; mpedb decides
what to fetch and when, works out exactly what changed, VERIFIES the cursor
you nominated (and names the rows a lying `updated_at` would have lost), and
carries the parameters for calls whose input is another call's output. The
theory and the named pitfalls are in
[design/DESIGN-INGEST.md](design/DESIGN-INGEST.md); the measured comparison
against "dump everything" and against uniform polling is
[workbench/ingest-lab](workbench/ingest-lab/README.md).

**[PYSPELL-RRETL.md](PYSPELL-RRETL.md)** — reversible transforms once the
data is in: lens pairs verified against a probe corpus, in-place column
transforms that can be reverted or put back onto edits, and table-set maps
that keep two shapes in sync both ways.

**[design/DESIGN-MPEDBFS.md](design/DESIGN-MPEDBFS.md)** — `mpedbfs` mounts
what the database already holds as ordinary paths, read-only, for the
programs that speak nothing else: `/obj/<name>/latest` for a versioned blob,
`/archive/<id>-<name>/…` for a spliced zip's members as a tree.

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
- **Windows — x86-64** — crash-safe: shared `CreateFileMapping` views, a
  `LockFileEx` writer lock with owner-death release, `GetProcessTimes` reader
  identity, `FlushViewOfFile` + `FlushFileBuffers` durability. Several processes
  share one file with MVCC readers against a writer, and **all six crash
  harnesses** (`crash`, `stress`, `powerloss`, `collide`, `queue-collide`,
  `mirror-collide`) run nightly on `windows-latest` — they turned out not to be
  `fork`-bound at all; `mpedb-cli` contains no `fork`. Porting them needed two
  seam functions, and the first thing they found was a corruption that was ours
  on every platform (#160). The sqlite interop lock speaks the same protocol
  there as on POSIX — the lock bytes are sqlite's, not a VFS's — so the overlay's
  cross-engine contract (a foreign sqlite writer gets `SQLITE_BUSY`) holds on
  Windows too. The CLI and the Python wheel run in CI as well.
  **Slower there, and by how much is now measured (#164).** Every number in
  [Performance](#performance) is from Linux and the M3, and Windows was
  described here as "works, throughput unknown" until a paired probe was
  pointed at it. In the contended non-durable insert cell, mpedb reaches
  **17.4 k op/s** on a 4-core Windows runner against **56 k** on a 4-core Linux
  box and **110 k** on the M3 — while sqlite3, measured on the same Windows
  machine as the control arm, loses only 1.2x. So it is not the host. The
  absolute numbers are from a shared runner and are not publishable as
  benchmarks; the 4x RATIO gap, with a control arm on the same machine, is far
  outside what that runner can explain.
  The cause is **not known**. The obvious suspect — the FLD-2 sidecar lock
  costing a `LockFileEx` per transaction where Linux costs a futex — is refuted
  by macOS, which uses the same FLD-2 path and is the fastest of the four.
  Reproduce it yourself with `cargo run --release -p mpedb --example
  runner_noise`. See `design/DESIGN-WINDOWS.md`.

Platform claims are verified on real hardware, and the table says which hardware:

| platform | what has actually run there |
|---|---|
| Linux x86-64 | everything: `cargo test --workspace`, clippy, the `stress`/`crash`/`powerloss`/`collide` harnesses across `none`/`commit`/`wal`, the 3-way differential |
| macOS / Apple Silicon (M3) | `cargo test --workspace`, clippy, and all six crash harnesses in CI (`eowner_recovery=true` — the FLD-2 writer lock is a different construction from Linux's robust mutex, so this is not a formality), plus the benchmark suite by hand |
| **Linux armv7l (32-bit ARM)** | 318 cross-compiled tests, 0 failures — including the whole `mpedb-core` shm/btree/COW suite — plus `examples/multiproc_check.rs`: 4 SIGKILL waves against 3 concurrent writer processes, `verify()` clean after each. A Raspberry Pi 3 B+, kernel 6.1. |
| **Windows x86-64** | the engine's unit tests, four multi-process properties (mapping coherence, writer exclusion, cross-process MVCC snapshots, owner death), two durability arms (`commit`/`wal`, killed writer, third process reopens), the facade's ~1100 integration tests, and all six crash harnesses (`crash`, `stress`, `powerloss` ×2, `collide` ×2, `queue-collide`, `mirror-collide`, `tier crash`) — all on `windows-latest` in CI |
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
`sqlite_corpus` runner in `crates/mpedb-testkit`), all 7,420,638 records of it:
**zero wrong answers and zero error mismatches, on x86-64 and arm64** — of
everything mpedb accepts, 100% matches sqlite, and what does not pass is
deliberate refusals with error messages
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

**Every measured comparison lives in [`benchmarks/`](benchmarks/)** — the
head-to-head, DuckDB (OLAP), Qdrant (vector), Neo4j (graph), PostgreSQL
LISTEN/NOTIFY, and the shared method the whole set follows. Start at
[`benchmarks/README.md`](benchmarks/README.md).

Head-to-head against SQLite and PostgreSQL through one shared Rust measurement
loop (each engine on its own fast path — mpedb's `execute(hash, …)`, prepared
statements for the others). **[`benchmarks/head-to-head.md`](benchmarks/head-to-head.md) is the detailed
comparison** — methodology, every machine, and a link to each machine's full
generated tables. The highlights from all of them are below.
[Turso](https://github.com/tursodatabase/turso), the Rust SQLite rewrite, is
measured as a fourth engine — numbers and a compatibility-parity comparison in
[benchmarks/turso.md](benchmarks/turso.md).

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
improvement as a "regression", with a commit message to match. benchmarks/head-to-head.md has
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
cell most sensitive to core count — see [benchmarks/head-to-head.md](benchmarks/head-to-head.md).

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
`wal`. Details: [benchmarks/head-to-head.md](benchmarks/head-to-head.md#apple-silicon-m3-pro-11-cores--and-the-durability-trap-it-exposed).

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
[benchmarks/head-to-head.md](benchmarks/head-to-head.md#bulk-mbs--and-the-number-that-makes-it-mean-something).

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
> ([method](benchmarks/head-to-head.md#reading-run-to-run-deltas--the-control-group-method)).

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

Released under the [mpedb License 1.0](LICENSE) — source-available, and
**free of charge for every person and every organization**, with one
exception: a corporate group whose consolidated revenue **or** valuation
exceeds **USD 5 billion** (2026 dollars, whichever is higher; ownership
chains count in both directions, non-profits included) owes a **one-time
USD 0.07 per physical device** (2026 dollars) running the software — for
server deployments, per physical device connected, whichever count is
higher. Crossing the threshold once keeps the obligation for the five
following years. Ambiguities resolve in favor of the project. The license
applies to all versions, current and prior. Details and payment contact:
[LICENSE](LICENSE).

---

*MPE stands for Morten Punnerud-Engelstad.*
