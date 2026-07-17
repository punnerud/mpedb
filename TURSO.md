# mpedb vs Turso

[Turso](https://github.com/tursodatabase/turso) is the Rust rewrite of SQLite
(formerly "limbo") — an embedded, single-file, WAL-based engine with async I/O,
aiming for full SQLite compatibility. It is the most interesting newcomer in
mpedb's neighborhood, and since 2026-07-17 it runs as the fourth engine in
`mpedb-bench`, measured under the same workloads, honesty rules, and durability
classes as SQLite and PostgreSQL. This page is the curated comparison; the full
generated tables live in the per-machine results files
([Linux](crates/mpedb-bench/RESULTS-linux-amd-epyc-milan-2c.md),
[M3](crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md)), and the
three-engine campaign methodology lives in [BENCHMARKS.md](BENCHMARKS.md).

Version measured: **turso 0.7.0** (crates.io, embedded via its Rust API).
Turso is beta software by its own description; the compatibility notes below
quote its `COMPAT.md` as of 2026-07-17 and will drift as it matures.

## How it is benchmarked (the adapter's honesty decisions)

Every deviation from "just call it like SQLite" is a place a benchmark can lie,
so here is each one, with why:

- **WAL-only, no none-class.** Turso does not implement rollback-journal modes
  ("Not Needed" in its COMPAT.md) and always runs WAL. Its none-class cell is
  therefore "tmpfs + engine defaults" and does strictly more work than
  mpedb/SQLite none-class — the same inherent asymmetry PostgreSQL has, reported
  the same way. If anything the cell under-reports Turso.
- **Commit-class is honest durable-on-ack.** Turso's default sync mode is
  `Full` — one fsync per commit — verified in the 0.7.0 source
  (`turso_core/lib.rs`, `SyncMode::Full` at connection init), not assumed from
  docs. On Apple, plain `fsync()` does not reach the platter, and Turso's
  Apple-only `PRAGMA fullfsync` defaults OFF exactly like SQLite's — the adapter
  turns it on for commit-class connections, same rule as every other engine in
  the suite (an engine allowed to skip it posts numbers 20–165× too good).
- **Busy means retry, not wait.** Turso returns `Busy` to a second concurrent
  writer immediately — a deliberate gap in its own words ("Turso currently
  returns `SQLITE_BUSY` for the second write statement … a deliberate
  compatibility gap"), with no blocking `busy_timeout` arbitration. The adapter
  retries with a yielding backoff (up to 60 s), so contended cells measure
  throughput-under-retry; SQLite arbitrates the identical contention inside its
  busy handler. Different mechanism, same job, both costs measured.
- **No WAL autocheckpoint exists in 0.7 — the adapter supplies one.** SQLite
  checkpoints its WAL automatically every 1000 pages by default; Turso 0.7 has
  no autocheckpoint (the pragma does not exist), and measured on the Linux box
  its WAL grew **~1.9 GB inside a single 3-second disk cell**, filling the host
  disk to ENOSPC. The adapter issues `PRAGMA wal_checkpoint(TRUNCATE)` every
  1000 write ops — the closest analog of SQLite's default policy — and its cost
  is included in the measured time exactly as SQLite's autocheckpoint cost is.
  Bound after the fix: 13 MB peak scratch on the same workload.
- **One connection per thread via a per-thread tokio runtime.** Turso's Rust
  API is async; the harness is synchronous. Each worker owns a current-thread
  runtime and drives its own connection with `block_on` — the same
  one-connection-per-thread shape the SQLite adapter uses, with no shared
  executor to smuggle in scheduling effects.

## Measured head-to-head

Both machines, one run each (2026-07-17), all four engines in the same run —
ops/s from the generated tables, which also carry p50/p99 latencies and the
full caveat list. Compare within a durability class only; "r / w" is the
read-while-write cell (concurrent readers + one writer).

**Linux — AMD EPYC-Milan, 2 cores, 7.6 GiB** (tmpfs for none-class, ext4 for
commit-class; [full tables](crates/mpedb-bench/RESULTS-linux-amd-epyc-milan-2c.md)):

| ops/s | mpedb | SQLite | PostgreSQL | Turso |
|---|---|---|---|---|
| point-insert, none | **177,376** | 42,306 | 13,825 | 40,556 |
| point-select, none | **469,679** | 81,985 | 21,096 | 117,790 |
| contended-writes, none | **146,801** | 30,474 | 36,407 | 24,143 |
| read-while-write, none (r / w) | **467,304** / **30,153** | 2,251 / 24,398 | 36,357 / 7,805 | 59,401 / 11,256 |
| point-insert, commit | 391 | 848 | **1,457** | 718 |
| point-select, commit | **460,791** | 253,422 | 21,107 | 117,607 |
| contended-writes, commit | 687 | 840 | **6,370** | 1,022 |
| read-while-write, commit (r / w) | **569,527** / 441 | 568,318 / 417 | 41,465 / **1,745** | 80,017 / 712 |

The commit-class point-insert row uses mpedb `durability=commit` (one msync
per commit, serialized). mpedb's durable-on-ack mode of record is `wal`, and
the §5.4 single-client durable table in the same run has it at **1,794 ops/s —
ahead of SQLite FULL (852) and PostgreSQL sc=on (1,514)**, and 96,252 ops/s
batched at 100 rows/commit. Turso's commit cell (718) sits between SQLite and
PostgreSQL.

**macOS — Apple M3 Pro, 11 cores, 36 GiB** (HFS+ RAM disk for none-class,
APFS for commit-class; every engine forced through `F_FULLFSYNC`;
[full tables](crates/mpedb-bench/RESULTS-macos-apple-m3-pro-11c.md)):

| ops/s | mpedb | SQLite | PostgreSQL | Turso |
|---|---|---|---|---|
| point-insert, none | **224,158** | 110,658 | 36,394 | 38,517 |
| point-select, none | **1,834,718** | 314,766 | 40,853 | 235,933 |
| contended-writes, none | **146,065** | 106,837 | 115,024 | 37,341 |
| read-while-write, none (r / w) | **4,042,266** / **205,004** | 181 / 86,696 | 89,920 / 26,363 | 383,097 / 37,031 |
| point-insert, commit | 142 | 280 | 269 | **300** |
| point-select, commit | **1,798,415** | 751,668 | 40,716 | 236,188 |
| contended-writes, commit | 356 | 261 | **622** | 325 |
| read-while-write, commit (r / w) | **4,136,068** / 159 | 1,361,001 / 320 | 120,224 / 325 | 527,566 / **326** |

On Apple, durable-on-ack single-writer throughput is the ~3 ms `F_FULLFSYNC`
floor wearing four different logos: the §5.4 table has mpedb `wal` at 296,
SQLite FULL at 333, PostgreSQL at 273, and Turso's commit cell lands at 300.
Nobody beats the flush; differences are within ~20% and move run to run.
mpedb's `durability=commit` cell (142) pays two flushes per commit — its
known, documented Apple floor — which is why `wal` is the mode to compare.

## Compatibility parity, both ways

Two different yardsticks, kept apart on purpose. Turso's column is **its own
COMPAT.md self-report** (quoted 2026-07-17) — mpedb has not independently
verified it. mpedb's own page in the same format is [COMPAT.md](COMPAT.md). mpedb's column is **measured**: the sqllogictest select corpus
(127 files, 1,464,520 records) passes 100.0% with zero wrong results against
sqlite3 ground truth, and every listed refusal is a deliberate, documented
error message, not a silent gap (see [GUIDE.md](GUIDE.md) and the
[testkit README](crates/mpedb-testkit/README.md)).

| feature | Turso (self-reported) | mpedb (measured) |
|---|---|---|
| scalar subqueries | yes | yes — >1 row is an error (PostgreSQL's rule; sqlite takes the first row) |
| correlated subqueries / `EXISTS` | `EXISTS`/`IN` listed as supported; correlation not stated | yes, correlated included; refused inside aggregates, `HAVING`, compound arms, `JOIN ON` — each with a message |
| row-value subqueries `(x,y) = (SELECT …)` | no ("only scalar subqueries supported") | no |
| compound `UNION`/`EXCEPT`/`INTERSECT` | yes | yes, chains included |
| `INNER`/`LEFT`/`CROSS` joins | yes | yes, N-way chains |
| `RIGHT`/`FULL` joins | yes | two-table forms; refused inside longer chains (left-deep planner, documented) |
| CTEs (`WITH`) | partial — no `WITH RECURSIVE` | no |
| window functions | partial — `row_number()` and aggregates-over-`OVER` only | no |
| views | yes (`IF NOT EXISTS` not idempotent) | no |
| triggers | yes (no `INSTEAD OF`) | no — the PySpell/ETL layer is the planned mechanism |
| `ALTER TABLE` / live DDL | yes | no — schema is the config file; live DDL designed ([DESIGN-DDL.md](DESIGN-DDL.md)), not built |
| `FROM`-less `SELECT 3+5` | yes | yes — one synthetic row, aggregates and compound arms included |
| typing | SQLite dynamic typing | rigid per-column types — a wrong type is a write-time error, stricter than sqlite `STRICT` |
| plan model | SQL parsed per statement (prepared statements cached) | SQL compiles once to a content-hashed plan; `execute(hash, params)` re-parses nothing |

Read the table honestly in both directions: **on raw SQL surface Turso is far
ahead** — it is a SQLite rewrite and inherits SQLite's ambitions, so views,
triggers, DDL, and window functions exist there and do not exist here. mpedb's
claim is different: a deliberately narrower surface where everything that
compiles is measured to match sqlite3/PostgreSQL semantics record-for-record,
plus the things a SQL surface cannot give you (below). Turso publishes no
corpus pass rates; mpedb's are in the testkit README with the harness to
reproduce them.

## The operational model is where they diverge

| | Turso 0.7 | mpedb |
|---|---|---|
| multi-process access | "We don't support mixed SQLite and Turso in multi-process scenarios" (COMPAT.md); multi-process behavior otherwise undocumented | the core design: many processes attach to one shared-memory file, any of them SIGKILLable at any instant, fuzzed exactly that way (`mpedb crash`, `mirror-collide`) |
| readers vs writer | WAL readers; second writer gets `Busy` | MVCC snapshots, lock-free readers that never block the writer or each other |
| concurrent writers | `SQLITE_BUSY` immediately, no arbitration (deliberate, documented) | writer lock + intent-ring group commit under contention (DESIGN.md §5.3) |
| durability spectrum | WAL, sync `Full`/`Normal`/`Off` per connection | `none` / `commit` / `wal` / `async` per database, each a documented promise |
| power-loss story | WAL; beta, no published torn-write testing | WAL torn-tail simulation (`mpedb powerloss`) run 20/20 green on wal+async, extent payloads included |
| maturity | beta by its own README; moving fast | pre-1.0, but every concurrency/commit protocol survived a 37-finding adversarial review (DESIGN.md) |

## Where the numbers land

Against Turso specifically, the shape is consistent on both machines: **mpedb
leads every none-class cell** — mostly by 4–8×, from 2.7× at the narrowest
(writes-under-readers, Linux) to 10× at the widest (reads-under-writer, M3) —
and **every read cell in both classes** (3.9× on Linux, 7.6× on the M3 for
warm point-selects). Turso's strongest showing is durable
single-writer inserts, where it is competitive with SQLite and PostgreSQL —
everyone is paying the same fsync — and briefly ahead of mpedb's
`durability=commit` cell, though not of mpedb `wal` on Linux (1,794 vs 718).
Its weakest is anything contended: with no blocking arbitration, Busy-retry
storms push its p99 to 51 ms (Linux) and 225 ms (M3) in contended-writes while
the median stays low — the adapter's retry loop is doing the queueing its
engine doesn't.

Read it with the obvious grain of salt: Turso is beta, 0.7.0, and moving fast;
these numbers are a snapshot of 2026-07-17, taken to place a promising
newcomer honestly in the field — not a verdict on where it ends up. The
snapshot's methodology is reproducible: `cargo run --release -p mpedb-bench`
with the adapter decisions documented above.
