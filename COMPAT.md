# mpedb SQLite Compatibility

Feature-by-feature status of mpedb's SQL surface against SQLite, in the same
format as [Turso's COMPAT.md](https://github.com/tursodatabase/turso/blob/main/COMPAT.md)
so the two can be read side by side (the measured three-way comparison lives in
[TURSO.md](TURSO.md)). Legend: ✅ yes · 🚧 partial · ❌ no · **Not needed** =
deliberately solved another way, not a gap on the roadmap.

Two things make this page different from a typical compatibility list:

1. **Every ✅ is measured, not remembered.** The `sqlite_corpus` runner
   (`crates/mpedb-testkit`) executes sqlite's own sqllogictest corpus
   differentially against sqlite3: the classic `select1–3` files and the entire
   random *select* tree — 127 files, 1.46 million records — pass at **100%**,
   with **zero wrong answers and zero error mismatches** across every statement
   both engines accept over the full 5.3M-record corpus. The remaining ~31% of
   the corpus is almost entirely FROM-less `SELECT 3+5` (#67, in progress).
2. **Every ❌ is an error message, never a silent wrong answer.** SQL that
   mpedb does not support is refused at compile time, usually with the manual
   fix in the message. The narrowness is the design; what compiles, matches.

Schema is the other structural difference: mpedb has no in-band DDL — tables
come from the config file (or `mirror import` from an existing sqlite/PostgreSQL
database), columns are rigidly typed, and a wrong type is a write-time error.
sqlite `STRICT` still converts losslessly (`'42'` → `42`); mpedb does not.

## Statements

| Statement | Status | Comment |
|---|---|---|
| SELECT | ✅ | see the clause table below |
| INSERT INTO … VALUES | ✅ | multi-row VALUES; explicit or implicit column list |
| INSERT … ON CONFLICT DO NOTHING | ✅ | |
| INSERT … ON CONFLICT (target) DO UPDATE SET … [WHERE …] | ✅ | target may be the PK or one UNIQUE column; `excluded.<col>` works |
| INSERT INTO … SELECT | ❌ | |
| UPDATE … SET … [WHERE …] | ✅ | |
| DELETE FROM … [WHERE …] | ✅ | |
| RETURNING (all three verbs) | ✅ | `RETURNING *` or an expression list |
| BEGIN / COMMIT / ROLLBACK | ✅ | maps to a write session; readers use MVCC snapshots and never block |
| SAVEPOINT / RELEASE | ❌ | savepoints exist at the engine level (import rollback), not in SQL |
| EXPLAIN | ✅ | plan form (access path, index choice, residuals), not VDBE opcodes |
| CREATE TABLE / DROP TABLE | ❌ | schema is the config file or `mirror import`; live DDL is designed ([DESIGN-DDL.md](DESIGN-DDL.md)), not built |
| ALTER TABLE | ❌ | same — a schema change is a config change today |
| CREATE INDEX | **Not needed** | `unique = true` / `indexed = true` on the column in the config; equality + range scans, visible in EXPLAIN |
| CREATE VIEW / CREATE TRIGGER | ❌ | triggers' job is planned as the PySpell/ETL layer, not in-SQL |
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
| INNER JOIN (N-way chains) | ✅ | left-deep, up to 16 tables; equality in ON becomes an index nested loop (PK > unique > non-unique), the rest stays residual — EXPLAIN shows which |
| LEFT [OUTER] JOIN | ✅ | NULL-extends; `WHERE inner IS NULL` anti-joins work |
| RIGHT [OUTER] JOIN | 🚧 | two-table form (planned as LEFT with sides swapped); refused inside longer chains with the manual fix in the message |
| FULL [OUTER] JOIN | 🚧 | two-table form; same chain restriction |
| CROSS JOIN / comma-joins | ✅ | desugared to `INNER JOIN … ON true` |
| NATURAL JOIN / JOIN … USING | ❌ | refused — "write the ON condition explicitly"; implicit name-matching is a trap under rigid schemas |
| Table aliases, self-joins | ✅ | alias shadows the table name, as in PostgreSQL |
| UNION / UNION ALL / EXCEPT / INTERSECT | ✅ | chains, left-associative at equal precedence (sqlite's rule; PostgreSQL binds INTERSECT tighter — documented deviation); arms must agree on arity and exact types, `CAST` bridges deliberate mismatches |
| Scalar subqueries `(SELECT …)` | ✅ | uncorrelated and correlated; 0 rows → NULL; **>1 row is an error** (PostgreSQL's rule — sqlite silently takes the first row) |
| [NOT] EXISTS (…) | ✅ | uncorrelated and correlated |
| `x IN (SELECT …)` | ❌ | `IN` over value lists and session-context lists only |
| Subqueries in FROM (derived tables) | ❌ | |
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
| IN / NOT IN (value list) | ✅ | |
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
| length | ✅ | |
| abs, round | ✅ | keep their argument's numeric type |
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
| group_concat, total | ❌ | |

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
