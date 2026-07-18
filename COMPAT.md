# mpedb SQLite Compatibility

Feature-by-feature status of mpedb's SQL surface against SQLite, in the same
format as [Turso's COMPAT.md](https://github.com/tursodatabase/turso/blob/main/COMPAT.md)
so the two can be read side by side (the measured three-way comparison lives in
[TURSO.md](TURSO.md)). Legend: ✅ yes · 🚧 partial · ❌ no · **Not needed** =
deliberately solved another way, not a gap on the roadmap.

Two things make this page different from a typical compatibility list:

1. **Every ✅ is measured, not remembered.** The `sqlite_corpus` runner
   (`crates/mpedb-testkit`) executes sqlite's own sqllogictest corpus
   differentially against sqlite3. Over the **full 7.4M-record corpus, 99.7% of
   attempted statements pass, with zero error mismatches and zero genuine wrong
   answers** (the 8 flagged divergences are cascades from a preceding
   unsupported statement, not answer bugs). Put the other way: of everything
   mpedb *accepts*, essentially 100% matches sqlite. The ~0.3% that does not
   pass is deliberate refusals — chiefly some subquery forms, `SELECT x IN
   <table>`, and MySQL-only casts (`AS SIGNED`/`AS DECIMAL`).
2. **Every ❌ is an error message, never a silent wrong answer.** SQL that
   mpedb does not support is refused at compile time, usually with the manual
   fix in the message. The narrowness is the design; what compiles, matches.

Schema is the other structural difference: mpedb's tables come from the config
file (or `mirror import` from an existing sqlite/PostgreSQL database) or from
in-band `CREATE TABLE` (#47 — live, multi-process, on the shared file). Columns
are rigidly typed, and a wrong type is a write-time error. sqlite `STRICT` still
converts losslessly (`'42'` → `42`); mpedb does not.

## Statements

| Statement | Status | Comment |
|---|---|---|
| SELECT | ✅ | see the clause table below |
| INSERT INTO … VALUES | ✅ | multi-row VALUES; explicit or implicit column list |
| INSERT … ON CONFLICT DO NOTHING | ✅ | |
| INSERT … ON CONFLICT (target) DO UPDATE SET … [WHERE …] | ✅ | target may be the PK or one UNIQUE column; `excluded.<col>` works |
| INSERT OR IGNORE / ABORT / FAIL / ROLLBACK | ✅ | IGNORE = DO NOTHING; ABORT/FAIL/ROLLBACK = error (the default) |
| INSERT OR REPLACE | ✅ | on a PK conflict, replaces the row (desugars to `ON CONFLICT (pk) DO UPDATE SET …=excluded`). Refused on a table with a secondary UNIQUE index, where sqlite's delete-on-any-constraint semantics differ from a PK upsert |
| INSERT INTO … SELECT | ✅ | `INSERT INTO t [(cols)] SELECT …`; the source is read fully first (self-insert safe), its tuple fills the listed columns, omitted columns default. Compound (UNION) source not yet supported |
| UPDATE … SET … [WHERE …] | ✅ | |
| DELETE FROM … [WHERE …] | ✅ | |
| RETURNING (all three verbs) | ✅ | `RETURNING *` or an expression list |
| BEGIN / COMMIT / ROLLBACK | ✅ | maps to a write session; readers use MVCC snapshots and never block |
| SAVEPOINT / RELEASE | ❌ | savepoints exist at the engine level (import rollback); a SQL surface is planned |
| EXPLAIN | ✅ | plan form (access path, index choice, residuals), not VDBE opcodes |
| CREATE TABLE | ✅ | live, multi-process, on the shared file — `PRIMARY KEY` (inline or table-level composite), `NOT NULL`, `UNIQUE` (column or composite); other processes see the new table on their next statement. `DEFAULT`/`CHECK`/foreign keys refuse by name (declare them in the config schema for now) |
| DROP TABLE | ✅ | live, multi-process — `DROP TABLE [IF EXISTS] <name>`; frees the table's pages, tombstones its id in place (never reused — [DESIGN-DROP-TABLE.md](DESIGN-DROP-TABLE.md) §0), other processes see it gone on their next statement. No-reuse caps *lifetime* table creates at 64 (a bounded capacity limit, not a per-query gap; offline `regenerate` re-densifies) |
| ALTER TABLE RENAME | ✅ | `RENAME TO` (table) and `RENAME [COLUMN] a TO b` — pure metadata, no data rewrite; sqlite/PG-equivalent refusals (name collision, unknown target) |
| ALTER TABLE ADD COLUMN | ✅ | nullable column, live + multi-process (existing rows rewritten with NULL). `NOT NULL`/`UNIQUE`/`PRIMARY KEY` on ADD refuse (no default fill / online index build yet), matching sqlite's NOT-NULL-needs-default rule |
| ALTER TABLE DROP COLUMN | ✅ | live + multi-process (existing rows rewritten without the column; surviving index/PK column references renumbered). Refuses dropping a PK / indexed / last column, matching sqlite |
| CREATE INDEX | ✅ | `CREATE [UNIQUE] INDEX [IF NOT EXISTS] n ON t (cols)` — built over existing rows, live + multi-process; ASC/DESC per column accepted (indexes are ascending, used for equality/prefix/range lookups). Or declare via config `unique`/`indexed`/`[[table.index]]`. The index name is not persisted (indexes are positional) |
| CREATE VIEW / DROP VIEW | ✅ | a query naming the view is flattened onto its base table (WHERE merged; `SELECT *` yields the view's columns; view-over-view chains). Simple projection/filter views over one table; aggregate/join/DISTINCT view bodies are refused at reference time (never answered wrongly) — [DESIGN-VIEW.md](DESIGN-VIEW.md) |
| CREATE TRIGGER | ❌ | planned both as in-SQL triggers and as PySpell/ETL-layer code |
| WITH (CTEs) | ❌ | |
| VALUES (standalone) | ❌ | |
| PRAGMA | **Not needed** | everything a PRAGMA would set lives in the config file, per database, versioned |
| VACUUM | **Not needed** | COW pages + commit-time freelist fixpoint reclaim space continuously |
| ATTACH / DETACH | ❌ | cross-file read-joins over workspace members are planned (#51) |
| ANALYZE | ❌ | the planner is rule-based (PK > unique > non-unique index > scan) |

## SELECT clauses

| Feature | Status | Comment |
|---|---|---|
| FROM-less `SELECT 3+5` | ✅ | one synthetic row (sqlite/PG semantics); WHERE filters it, aggregates see it, compound arms and subqueries may each be FROM-less |
| WHERE | ✅ | full SQL three-valued logic, verified against sqlite 3.45 |
| GROUP BY | ✅ | columns, expressions (`GROUP BY a/100`), output ordinals (`GROUP BY 1`) |
| HAVING | ✅ | subqueries inside HAVING are refused |
| ORDER BY | ✅ | by name, ordinal, or selected expression; hidden sort columns added when needed; under DISTINCT the key must be in the SELECT list (PostgreSQL's rule) |
| LIMIT / OFFSET | ✅ | Top-K heap under ORDER BY + LIMIT |
| DISTINCT | ✅ | also `count(DISTINCT x)` |
| SELECT-item aliases | ✅ | `expr AS name` and bare `expr name`; `ORDER BY alias` resolves the output first |
| `t.*` / `*` | ✅ | |
| INNER JOIN (N-way chains) | ✅ | left-deep, up to 16 tables; equality in ON becomes an index nested loop (PK > full-width unique > longest prefix, composite included), the rest stays residual — EXPLAIN shows which |
| LEFT [OUTER] JOIN | ✅ | NULL-extends; `WHERE inner IS NULL` anti-joins work |
| RIGHT [OUTER] JOIN | 🚧 | two-table form (planned as LEFT with sides swapped); refused inside longer chains with the manual fix in the message |
| FULL [OUTER] JOIN | 🚧 | two-table form; same chain restriction |
| CROSS JOIN / comma-joins | ✅ | desugared to `INNER JOIN … ON true`; WHERE conjuncts push into the chain, so a comma-join equality is an index-nested-loop candidate exactly like an ON equality |
| NATURAL JOIN / JOIN … USING | ❌ | refused — "write the ON condition explicitly"; implicit name-matching is a trap under rigid schemas |
| Table aliases, self-joins | ✅ | alias shadows the table name, as in PostgreSQL |
| UNION / UNION ALL / EXCEPT / INTERSECT | ✅ | chains, left-associative at equal precedence (sqlite's rule; PostgreSQL binds INTERSECT tighter — documented deviation); arms must agree on arity and exact types, `CAST` bridges deliberate mismatches |
| Scalar subqueries `(SELECT …)` | ✅ | uncorrelated and correlated; 0 rows → NULL; **>1 row is an error** (PostgreSQL's rule — sqlite silently takes the first row) |
| [NOT] EXISTS (…) | ✅ | uncorrelated and correlated |
| `x IN (SELECT …)` | ✅ | uncorrelated; the subquery becomes a LIST subplan over the same runtime-typed 3VL membership core as IN lists (empty → FALSE, NULL member without a match → NULL — sqlite-verified); correlated IN is refused with the EXISTS rewrite named |
| Subqueries in FROM (derived tables) | 🚧 | a simple projection/filter body `FROM (SELECT … FROM t [WHERE …]) d` is flattened onto its base at bind time (WHERE merged; the derived alias `d` is kept, so `d.col` refs resolve; joins over the derived table work). Aggregate/join/DISTINCT/LIMIT/renamed-projection bodies are refused (never answered wrongly) — Stage A ([DESIGN-DERIVED-TABLES.md](DESIGN-DERIVED-TABLES.md)) |
| Nested subqueries (subquery in a subquery) | ❌ | refused with a message |
| Window functions | ❌ | |

## Expressions and operators

| Feature | Status | Comment |
|---|---|---|
| `+ - * / %`, unary `+`/`-` | ✅ | **division by zero errors** (sqlite yields NULL); **integer overflow errors** (sqlite promotes to REAL) — both deliberate |
| `= != < <= > >=` | ✅ | |
| AND / OR / NOT | ✅ | SQL 3VL throughout |
| `\|\|` concatenation | ✅ | NULL propagates; ints/bools render as text; floats refused until their formatting is pinned |
| LIKE | ✅ | no ESCAPE clause |
| GLOB / REGEXP / MATCH | ❌ | |
| BETWEEN / NOT BETWEEN | ✅ | |
| IN / NOT IN (value list) | ✅ | also `x IN (SELECT …)` and the sqlite shorthand `x IN <table>` (single-column). The empty set `x IN ()` is accepted (sqlite allows it) and is FALSE for every probe, `NOT IN ()` TRUE; `NULL IN (empty)` is FALSE (3VL), matching sqlite |
| IS NULL / IS NOT NULL | ✅ | |
| `x IS y` (general distinct-from) | ❌ | only the NULL forms |
| CASE (searched and simple) | ✅ | simple form desugars to searched; arms mixing int64 and float64 are refused — sqlite types the winning arm per row, rigid typing cannot, and widening was measured to change division results (add a CAST) |
| CAST(x AS type) | ✅ | NULL→NULL; float→int truncates toward zero (sqlite's rule); **text never parses into a number** — refused instead of guessed |
| COLLATE | ❌ | text compares as UTF-8 bytes |
| Parameters | ✅ | `$1, $2, …` (PostgreSQL style) rather than `?`; types unify at compile time |

## Scalar functions

| Function | Status | Comment |
|---|---|---|
| lower, upper, trim | ✅ | text in, text out; argument types checked at compile time |
| ltrim, rtrim | ✅ | whitespace by default, or a given set of characters (2-arg) |
| replace | ✅ | every occurrence; an empty search string is a no-op (sqlite's rule) |
| instr | ✅ | 1-based character position, 0 when absent (1 for an empty needle) |
| length | ✅ | |
| abs, round, ceil / ceiling, floor | ✅ | keep their argument's numeric type (int stays int) |
| sqrt, pow / power | ✅ | always float; a non-real result (sqrt of a negative) is NULL, matching sqlite |
| sign | ✅ | always an integer: -1 / 0 / 1 |
| substr / substring | ✅ | |
| coalesce, ifnull | ✅ | compiled to lazy control flow, not a call — arguments after the first non-NULL are never evaluated; int64/float64 arm mixing refused, same rule as CASE |
| nullif | ✅ | desugared to CASE |
| everything else | ❌ | an unknown function is a compile error that lists what exists |

Date/time functions (`date`, `strftime`, …) and JSON functions do not exist;
there is a first-class `timestamp` column type (µs since epoch, UTC) instead,
with timestamp parameters accepted by the CLI and the Python API.

## Aggregate functions

| Function | Status | Comment |
|---|---|---|
| count(*) / count(x) / count(DISTINCT x) | ✅ | NULL rules verified against sqlite 3.45 |
| sum, avg, min, max | ✅ | including over joins and with GROUP BY / HAVING |
| total | ✅ | always a float, 0.0 over an empty/all-NULL group (never NULL — the deliberate contrast with `sum`) |
| group_concat | ✅ | non-NULL values' text joined with `,` in scan order; NULL over an empty group. Custom-separator `group_concat(x, sep)` refused (v1) |

## Types

| sqlite | mpedb | Comment |
|---|---|---|
| INTEGER | `int64` | |
| REAL | `float64` | `int64 → float64` is the one implicit widening |
| TEXT | `text` | UTF-8 |
| BLOB | `blob` | plus streaming/incremental blob I/O and extent storage for large values |
| — | `bool` | first-class, not an integer |
| — | `timestamp` | µs since epoch, UTC |
| dynamic typing | `any` | opt-in per column (sqlite-affinity semantics, tagged per value); refused in keys and UNIQUE columns |

The default is the opposite of sqlite's: columns are rigid, and a wrong type is
a write-time error. sqlite `STRICT` accepts anything that converts losslessly
(`'42'` into INTEGER); mpedb refuses it. The full measured conversion matrix is
in the [testkit README](crates/mpedb-testkit/README.md).

## API

There is no C API and no wire protocol — mpedb is a Rust crate (`mpedb`), a
Python module (`mpedb-py`, abi3), and a CLI (`mpedb`). The sqlite3-API shapes
map as follows:

| sqlite3 API | mpedb | Comment |
|---|---|---|
| sqlite3_prepare / step | `prepare()` → content-hashed `CompiledPlan`, `execute(hash, params)` / `query(…)` | SQL is compiled **once**; the hot path re-parses nothing, and plans are shared across processes via the catalog registry |
| sqlite3_bind_* | `params![…]` | typed; a mismatch is a compile-time error of the plan, not a runtime surprise |
| transactions | `WriteSession` (`begin`/`commit`) | readers never block: MVCC snapshots |
| sqlite3_blob_open / read / write | incremental blob API + `insert_file` / `blob put`/`get` | streaming both ways; large values can live in contiguous extents (see [DESIGN-BLOBEXTENT.md](DESIGN-BLOBEXTENT.md)) |
| sqlite3_backup | **Not needed** | the database is one file; copy it (plus `-wal` if present) |
| busy_timeout / busy_handler | **Not needed** | writers queue on a robust cross-process lock with an intent ring for group commit; a SIGKILLed owner is recovered, not waited out |
| user-defined functions | ❌ | planned as the PySpell layer (compiled, typed IR — not callbacks) |
| loadable extensions / virtual tables | ❌ | |

## Journaling and durability

| Mode | Status | Comment |
|---|---|---|
| WAL | ✅ | `durability = "wal"` (durable-on-ack, lean records) or `"async"` (bounded deferred window) |
| Rollback journal | **Not needed** | COW pages + atomic meta flip give process-crash safety in every mode, including `durability = "none"` |
| Checkpointing | ✅ | automatic; power-loss torn-tail behavior is simulation-tested (`mpedb powerloss`) |
| fullfsync (Apple) | ✅ | F_FULLFSYNC issued natively where durability demands it — not an opt-in pragma |

## Concurrency and multi-process

This is where mpedb deliberately leaves the sqlite model rather than
reimplementing it: many OS processes attach to one shared-memory file, readers
take MVCC snapshots without locks and never block (or get blocked by) the
writer, writers queue on a robust cross-process mutex, and any process may be
SIGKILLed at any instant — that exact scenario is fuzzed continuously
(`mpedb crash`, `mirror-collide`). sqlite serializes at the file level with
busy-waiting; [Turso currently returns Busy to a second writer and does not
support multi-process mixed use](TURSO.md).

| | sqlite | mpedb | notes |
|---|---|---|---|
| many processes, one database | ✅ file locks + busy_timeout | ✅ shared-memory attach, MVCC | measured (2-core Linux, readers beside one writer): commit-class mpedb 569k reads/s vs sqlite-WAL 568k — a tie; none-class mpedb 467k vs sqlite-journal 2,251 (that mode serializes readers against the writer) |
| readers block the writer | in rollback-journal mode, yes; in WAL, no | never | |
| a process dies mid-write (SIGKILL) | journal/WAL recovery on next open | robust-mutex takeover + intent-ring recovery, fuzzed at every instant | `mpedb crash` is the harness |
| second concurrent writer | waits (busy_timeout) | queues on the writer lock; group commit under contention | Turso 0.7: immediate Busy, no arbitration — its contended p99 is 51–225 ms in [the measured field](TURSO.md) |

## Memory and resource discipline

The database is a fixed-size file (`size_mb`), mapped once and **shared by
every attaching process** — N processes cost one mapping, not N page caches
(sqlite's default is a per-connection cache). Hitting the size is an honest
`DbFull`, never silent growth; space reclaim is continuous (COW freelist
fixpoint — the unbounded high-water leak class has a regression test), and
reader memory is bounded by construction: scans and large-value reads move in
bounded chunks with pin revalidation, so a reader never materializes the
mapping. The WAL is circular and bounded, with lean records (only touched COW
pages, free space elided).

The contrast that motivated writing this down: sqlite's WAL is bounded by
autocheckpoint (1000 pages, default ON); **Turso 0.7 has no autocheckpoint at
all, and its WAL measured 1.9 GB of growth inside one 3-second write cell** —
enough to fill the host disk — until the benchmark adapter supplied manual
`wal_checkpoint(TRUNCATE)` calls ([TURSO.md](TURSO.md) has the details).

## Migration

| path | status | comment |
|---|---|---|
| sqlite → mpedb | ✅ `mpedb mirror import` | schema + data + type provenance; the measured conversion matrix is in the [testkit README](crates/mpedb-testkit/README.md) |
| mpedb → sqlite | ✅ `mpedb mirror export` | round-trips are verified (`mirror roundtrip`) |
| live two-way sync with sqlite | ✅ `mirror sync` / daemon | SIGKILL-fuzzed to convergence (`mirror-collide`: writers + a daemon killed at every instant must still converge exactly) |
| PostgreSQL ⇄ mpedb | ✅ `mirror` with a PG source/target | same machinery, `--source-config` DSN handling |
| open an existing `.db` file | 🚧 | two ways today. **Sidecar (read-write)**: `mpedb data.db` works like `sqlite3 data.db` (repl or one-shot) — imports on first open, pulls incrementally on later ones, `mpedb checkpoint data.db` pushes writes back with mirror's conflict rules. **Native (read-only, zero import)**: `mpedb dump data.db` and `mpedb::SqliteAttach` read the sqlite file format directly — no sqlite library in the path, both b-tree layouts, differentially verified row-for-row against the real library; PK probes are b-tree seeks, writes are refused by name. The in-place delta overlay with lock modes is the designed next stage ([DESIGN-SQLITE-BACKED.md](DESIGN-SQLITE-BACKED.md), 20-finding review folded) |

## Measured speed against sqlite

From the 2026-07-17 head-to-head runs (one run per machine, all engines in the
same run; full tables with latencies and methodology in
[BENCHMARKS.md](BENCHMARKS.md) and the per-machine RESULTS files, the four-way
field including PostgreSQL and Turso in [TURSO.md](TURSO.md)). Compare within
a durability class only; absolute numbers are those hosts', ratios travel
better. "r / w" is concurrent readers + one writer.

**Linux, AMD EPYC-Milan 2-core** (ops/s, mpedb vs SQLite 3.45):

| workload | mpedb | SQLite | ratio |
|---|---|---|---|
| point-insert, none-class | 177,376 | 42,306 | **4.2×** |
| point-select, none-class | 469,679 | 81,985 | **5.7×** |
| contended-writes, none-class | 146,801 | 30,474 | **4.8×** |
| read-while-write, none-class (r / w) | 467,304 / 30,153 | 2,251 / 24,398 | **208× / 1.2×** |
| point-select, commit-class | 460,791 | 253,422 | **1.8×** |
| read-while-write, commit-class (r / w) | 569,527 / 441 | 568,318 / 417 | tie / tie |
| durable-on-ack single writer (§5.4: mpedb `wal` vs FULL+WAL) | 1,794 | 852 | **2.1×** |
| durable-on-ack, batched 100 rows/commit | 96,252 | 56,749 | **1.7×** |
| point-insert, `durability=commit` | 391 | 848 | sqlite 2.2× — one msync per commit, serialized; use `wal` |

**macOS, Apple M3 Pro 11-core** (every engine forced through `F_FULLFSYNC`):

| workload | mpedb | SQLite | ratio |
|---|---|---|---|
| point-insert, none-class | 224,158 | 110,658 | **2.0×** |
| point-select, none-class | 1,834,718 | 314,766 | **5.8×** |
| read-while-write, none-class (r / w) | 4,042,266 / 205,004 | 181 / 86,696 | **22,000× / 2.4×** |
| point-select, commit-class | 1,798,415 | 751,668 | **2.4×** |
| read-while-write, commit-class (r) | 4,136,068 | 1,361,001 | **3.0×** |
| durable-on-ack single writer (§5.4) | 296 | 333 | sqlite 1.1× — everyone sits at the ~3 ms platter-flush floor |
| durable-on-ack, batched 100 rows/commit | 23,393 | 29,691 | sqlite 1.3× |

The pattern, honestly stated: mpedb wins every read cell and every none-class
cell on both machines — the outlier rows are sqlite's none-class rollback
journal serializing readers against a writer (2,251 reads/s beside a writer on
Linux, 181 on the M3), which is a real property of that configuration, not
benchmark theater. sqlite wins durable single-writer inserts on Apple
(everyone pays the same flush; differences there are ~20% and move run to
run) and against mpedb's `durability=commit` mode everywhere — `wal` is
mpedb's durable-on-ack mode of record, and on Linux it beats sqlite FULL by
2.1×. Bulk blob throughput has its own measured story (extents, WiscKey-style
separation) in [BENCHMARKS.md](BENCHMARKS.md) and
[DESIGN-BLOBEXTENT.md](DESIGN-BLOBEXTENT.md).

## Extensions beyond SQLite

- `current_setting('key')` and `expr IN (current_setting('key'))` — session
  context for serverless row-level security ([DESIGN-MULTIDB.md](DESIGN-MULTIDB.md));
  the values bind as reserved parameters, so one content-hashed plan serves
  every session.
- Row-level security policies (`USING` / `WITH CHECK`) enforced in-plan, on
  every side of a join.
- Multi-database workspaces; bidirectional mirroring to/from sqlite and
  PostgreSQL (`mpedb mirror`), with type provenance and conflict handling.
- Detached, client-borne compiled plans (execute-by-hash with zero parsing).

## Deliberate deviations from sqlite semantics

Each of these is a choice, exercised in `tests/guide.rs`, not an accident —
see [GUIDE.md](GUIDE.md) for the full list with examples:

1. Division by zero and integer overflow **raise**; sqlite yields NULL /
   promotes to REAL.
2. A scalar subquery returning more than one row **errors** (PostgreSQL's
   rule); sqlite silently takes the first row.
3. `ORDER BY` must name something the query outputs; `ORDER BY 1 + 1` is
   refused (sqlite sorts by the constant, i.e. not at all).
4. Text never converts implicitly to numbers — not in CAST, not in storage.
5. Compound set-ops use sqlite's flat precedence; PostgreSQL binds INTERSECT
   tighter. Documented, matching sqlite here.
