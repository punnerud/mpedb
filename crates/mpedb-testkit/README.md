# mpedb-testkit

An SQLite-inspired correctness battery for mpedb: a sqllogictest-format
runner, a curated corpus of `.test` files, and a randomized differential
tester against `/usr/bin/sqlite3` and (three-way) PostgreSQL 16.

```
cargo test -p mpedb-testkit                       # everything (< 60 s)
cargo test -p mpedb-testkit -- --ignored          # + the long-haul batteries
```

## What we reuse from SQLite's methodology — and what we cannot

SQLite's reliability comes from several test harnesses. Honestly accounted:

- **sqllogictest** (public domain) is the reusable part: an engine-agnostic
  *file format* (`statement ok` / `statement error` / `query` with expected
  results after `----`) plus the philosophy that correctness assertions
  should live in plain-text corpora, portable across engines. We implement
  the format ([`src/slt.rs`]) and write our own corpus (`tests/slt/`). The
  original sqllogictest *corpus* needed a shim when this crate was written
  (it assumes `CREATE TABLE`, type-coercing storage, and hashed result
  blocks); it now runs end to end through
  [`src/bin/sqlite_corpus.rs`](src/bin/sqlite_corpus.rs) — **99.885 % of
  5,938,278 attempted records, zero wrong answers**, measured 2026-07-19.
  Numbers, the ranked blocker table and the reproduce recipe live in
  [`design/CORPUS-STATUS.md`](../../design/CORPUS-STATUS.md).
- **TH3** is proprietary. Its coverage-driven, anomaly-injecting approach is
  not available to us; nothing here claims to replace it.
- **The TCL test suite** is API-bound to SQLite's C interface and TCL
  bindings; not portable. Its role — exercising the host-language API — is
  played by each mpedb crate's own unit/integration tests, not by this kit.
- **Differential/fuzz testing philosophy** (SQLite runs SQL against multiple
  engines and earlier versions of itself; dbsqlfuzz generates randomized
  input): reused directly as [`src/diff.rs`], which generates seeded random
  programs and compares mpedb against sqlite3 STRICT tables
  statement-by-statement.
- **PostgreSQL's pg_regress** is dialect-bound (psql meta-commands, its
  expected/ directory diffing), so we do not run it — but its
  expected-output methodology (run SQL, diff against checked-in expected
  text) is exactly what the SLT harness does.

## Deliverable 1 — sqllogictest runner (`src/slt.rs`)

`run_slt_file(path) -> Result<SltStats>` executes one `.test` file against a
fresh mpedb database. Supported: `statement ok`, `statement error
[substring]`, `query <typestring> [nosort|rowsort|valuesort] [label]`,
`skipif`/`onlyif` (engine name `mpedb`), `halt`, `hash-threshold`.

**Documented omission:** the hashed expected-result form (`N values hashing
to <md5>`) is NOT supported — only literal expected results. Encountering a
hashed block is a hard error (never a silent pass), and `hash-threshold` is
parsed and ignored. This keeps the crate dependency-free (no md5) and the
corpus human-readable.

**mpedb extensions** (mpedb has no `CREATE TABLE` — schemas come from TOML):

- each `.test` file starts with a `# schema:` … `# end schema` comment block
  holding the TOML `[[table]]` definitions; the runner supplies the
  `[database]` section (a fresh `.mpedb` file under /dev/shm, cleaned up);
- `statement error` takes an optional required error-message substring;
- `EXPLAIN` under `query T` renders one line per plan line, so planner
  behavior is pinned as executable documentation.

Rendering follows sqllogictest conventions: `NULL` for NULL, decimal for
`I`, `%.3f` for `R`, verbatim text for `T` (empty string → `(empty)`);
plus mpedb-typed extras: bool → `true`/`false`, blob → `x'<hex>'`.

## Deliverable 2 — curated corpus (`tests/slt/*.test`)

14 files, 496 directives, all 30+ records each: `basic_crud`,
`order_by_nulls` (NULLS FIRST asc / LAST desc, stable ties),
`limit_offset` (0 / beyond-end / OFFSET-only), `type_rigidity` (the one
int64→float64 coercion; everything else errors), `constraints`
(PK/UNIQUE/NOT NULL/CHECK; NULL passes CHECK; NULLs never collide in UNIQUE),
`three_valued_logic` (`= NULL` matches nothing, Kleene AND/OR, NULL
arithmetic), `like_patterns`, `range_scan_pk` (inclusive/exclusive bounds,
negative keys), `composite_pk` (prefix ranges on multi-column PKs),
`update_semantics` (SET evaluates against the OLD row — `SET a = b, b = a`
swaps), `insert_atomicity` (failed multi-row statements leave nothing),
`expressions` (precedence, fold-time = runtime errors), `bool_blob_text`,
`explain_plans`. Run via `tests/slt_files.rs`.

## Deliverable 3 — differential tester (`src/diff.rs`, `tests/differential.rs`)

Seeded xorshift generator (no `rand` dep; every failure reproducible from
its seed) produces INSERT/UPDATE/DELETE/SELECT programs over a fixed schema
`t(pk int64 PK, a int64, b float64, c text)`; the same program runs against
mpedb and `/usr/bin/sqlite3` (one batch process, `.mode list`,
`.nullvalue NULL`, `CREATE TABLE … STRICT` — STRICT is the *closest* sqlite
gets to mpedb's rigid types, not a match; see below). Compared per statement: success/failure
status, and full row output of every SELECT. Divergences are delta-minimized
before reporting. The known semantic differences (float formatting, text
collation/LIKE case, rowid aliasing of `INTEGER PRIMARY KEY`, division
semantics, error-message wording, integer overflow) are each documented in
the `diff` module docs with how they are normalized or kept out of the
generator.

**What STRICT actually enforces**, measured against sqlite 3.45 rather than
assumed (this table is the reason the generator must be type-correct by
construction — see `gen_pred` — and an earlier version of this README claimed
STRICT and mpedb agree, which they do not):

| value → column | sqlite STRICT | mpedb |
|---|---|---|
| `'abc'` → INT | reject | reject |
| `'42'` → INT | **coerces to `42`** | reject |
| `42` → TEXT | **coerces to `'42'`** | reject |
| `1.5` → INT | reject | reject |
| `2.0` → INT | **coerces to `2`** | reject |
| `7` → REAL | coerces to `7.0` | coerces to `7.0` |
| `x'01'` → TEXT | reject | reject |

STRICT's rule is "reject what cannot convert **losslessly**"; mpedb's is
"reject anything that is not the declared type", with `int64 → float64` as the
one exception. So STRICT is strictly weaker, and the generator never emits a
cross-type value — otherwise these cells would show up as divergences that are
not bugs. sqlite STRICT also rejects `VARCHAR(4)`, `NUMERIC(6,2)`, `BOOLEAN`
and `DATETIME` at DDL (only `INT`/`INTEGER`/`REAL`/`TEXT`/`BLOB`/`ANY` are
allowed), which is why the fixed schema uses the spellings it does.

**Three-way mode** adds **PostgreSQL 16** as a third engine
(`run_differential_3way`): a throwaway cluster per run (`src/pg.rs`, the
same `initdb --auth=trust --locale=C` + `pg_ctl` + unix-socket-only recipe
as `mpedb-bench`, guard struct always stops and removes it), driven through
one `psql` batch per program (`-A -t -F'|' -P null=NULL`; per-statement
status via psql's `:ERROR` variable and `@S/@K/@E` echo markers, so
expected constraint failures need no stderr parsing). The PG-specific
normalizations — float output text, `ORDER BY` NULLS placement (every
generated sort key is NULL-free: the NOT NULL `pk`, or a key the same
statement guards with a top-level `IS NOT NULL` conjunct; unit-test-pinned),
no STRICT needed
(PG is rigidly typed already), case-sensitive `LIKE` (matches mpedb), and
empty-string-vs-NULL distinctness (deliberately exercised by generated `''`
literals) — are items 7–11 of the `diff` module docs. If the environment
cannot start PostgreSQL the tests SKIP with a loud message (never a silent
pass).

Default batteries: 200 programs × 80 statements two-way (~8 s) and
100 × 60 three-way (~25 s). `#[ignore]`d long-haul: 2000 programs two-way,
1000 three-way. All batteries: **no divergences**.

## Engine bugs found (status)

- **`like_match` wildcard-vs-literal `%`** — `'a%c' LIKE 'a%'` and
  `'%%' LIKE '%'` wrongly return FALSE: the matcher's literal branch runs
  before the `%`-wildcard branch, so a `%` in the *subject* at the position
  where the *pattern* has `%` eats the wildcard as a one-character match.
  sqlite (and the SQL standard) say TRUE. Found by the LIKE corpus file;
  minimized in `tests/engine_bugs.rs`
  (`engine_bug_like_percent_wildcard_consumed_as_literal`, `#[ignore]`d,
  asserts the CORRECT behavior — un-ignore when fixed and update the two
  cross-referenced records in `tests/slt/like_patterns.test` that pin the
  buggy output). Not fixed here: this crate only tests; the fix belongs in
  `mpedb-types::expr::like_match`.

The 200- and 2000-program differential batteries found **no divergences**
(the generator never puts `%` in text values, so it cannot hit the LIKE bug).
