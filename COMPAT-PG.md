# mpedb PostgreSQL Compatibility

What mpedb answers when a PostgreSQL client connects, feature by feature — the
companion to [COMPAT.md](COMPAT.md), which does the same for sqlite.

**Two rules, the same as everywhere else in this project.** Every ✅ is measured
by a test that fails if it stops being true. Every ❌ is a **named refusal** with
a SQLSTATE a client can branch on — never a silent wrong answer.

**Nothing here is a claim to BE PostgreSQL.** `version()` reports `PostgreSQL
16.14 (mpedb <ver>)` because SQLAlchemy, Django and psycopg parse that string at
CONNECT and fail before they can ask anything else if they cannot; it names mpedb
in the same breath. 16.14 is the version the differential work measures against,
so raising it means re-measuring.

## How to run it

The wire protocol lives in `crates/mpedb-pg`, which is **its own cargo
workspace** and therefore not built by `cargo build` or `cargo test --workspace`
in the parent. That separation IS the build toggle: the default build of mpedb
contains no network code at all.

```sh
cargo build --release --manifest-path crates/mpedb-pg/Cargo.toml
mpedb-pg serve --unix /run/mpedb app.mpedb    # or --inherited-fd under systemd
psql -h /run/mpedb -d app
```

Deployment (socket activation, nginx, cron) is in [`deploy/`](deploy/README.md).

## The three named limits

These are not bugs and they are not "not yet". They are what mpedb's design
means when it meets PostgreSQL's, stated before anyone finds them.

### 1. One writer at a time

mpedb serializes writers (one writer lock, group commit). PostgreSQL has
row-level MVCC for writers. A client that opens `BEGIN`, thinks, and then
`COMMIT`s would hold mpedb's writer lock for the whole think-time and block every
other process on the machine.

So mpedb-pg does not hold it. A transaction block is **buffered** and replayed as
ONE mpedb transaction at `COMMIT`.

| | PostgreSQL | mpedb-pg |
|---|---|---|
| writes inside the block are visible to the block | ✅ | ✅ |
| the block is atomic | ✅ | ✅ |
| `ROLLBACK` discards it | ✅ | ✅ |
| a failed statement poisons the block | ✅ | ✅ |
| **a constraint violation is reported at the offending statement** | ✅ | ❌ — at `COMMIT` |
| **`ROLLBACK` undoes DDL** (transactional DDL) | ✅ | ❌ — DDL commits immediately |
| the writer lock is held from BEGIN to COMMIT | n/a | **no** |

The last three rows are the same trade seen from three sides.

DDL is the one thing the block does NOT buffer, and that is not a shortcut: it
is forced. mpedb's DDL takes its OWN write transaction, so replaying it inside
the block's open `WriteSession` waits on a lock this process already holds —
measured, as a permanent hang on `BEGIN; CREATE TEMPORARY TABLE t(a int);
COMMIT`. Buffering it would not have made it atomic either (mpedb commits the
schema change on its own); it would only have hidden the deadlock.

### 2. Text format only

`RowDescription` advertises format 0 (text) for every column, and a **binary bind
parameter is refused by name**. Binary is where a wrong answer hides best: a
misencoded `int8` is eight bytes that decode to a plausible number with no error
anywhere. `numeric` loses nothing by this — PostgreSQL's text form for it IS the
canonical decimal string, which is exactly how mpedb carries the type.

### 3. The catalog is a separate database

`pg_catalog` and `information_schema` are materialised as real tables in a
session-private in-memory database, built lazily on first reference. A statement
naming **both** a catalog relation and a user table cannot run.

Why not CTEs, which would have kept everything in one database — measured:

```text
WITH c(a) AS (SELECT 1 UNION ALL SELECT 3),
     d(b) AS (SELECT 2 UNION ALL SELECT 4)
SELECT c.a, d.b FROM c JOIN d ON c.a = d.b
  → bind error: CTE `d` body is not a simple SELECT
```

A CTE in join position is *spliced* onto its body's base table, and a `UNION ALL`
body has none. Since `psql \d` joins four catalog relations and every ORM's
reflection does the same, the rows have to arrive as ordinary tables.

## Connection

| Feature | Status | Comment |
|---|---|---|
| Protocol v3 startup | ✅ | v2 (code 131072) is refused **by number**, not hung on |
| `SSLRequest` / `GSSENCRequest` | ✅ | answered `N`; TLS terminates in nginx (`deploy/`) |
| Trust auth on a unix socket | ✅ | the default — the peer is authenticated by filesystem permissions, which is the same fence mpedb uses for the file |
| Cleartext password | ✅ | `--require-password`; any password is accepted, so this is a *speed bump*, not authentication |
| SCRAM-SHA-256 | ❌ | not implemented. Over TCP, put nginx or a firewall in front |
| `CancelRequest` | 🚧 | the connection is accepted and closed silently (as PostgreSQL does); the query is not actually cancelled |
| `BackendKeyData` | ✅ | pid + a pid-derived secret |
| `ParameterStatus` at connect | ✅ | `server_version`, `server_encoding`, `client_encoding`, `DateStyle`, `TimeZone`, `standard_conforming_strings`, … |

## Query protocol

| Feature | Status | Comment |
|---|---|---|
| Simple query (`Q`) | ✅ | multi-statement, split on top-level `;` only — quotes, `--` comments and `$$` bodies are respected |
| Extended query | ✅ | `Parse`/`Bind`/`Describe`/`Execute`/`Sync`/`Close`, named and unnamed. This is the path psycopg uses by default |
| Portals with `max_rows` | ✅ | `PortalSuspended` and resume. mpedb materialises a result set, so this bounds what is SENT, not peak memory |
| `Describe` on a statement | 🚧 | `ParameterDescription` reports the OIDs the client declared (`unknown` where it declared none); the row description is sent at `Execute` rather than here |
| `EmptyQueryResponse` | ✅ | |
| `COPY … FROM STDIN` / `TO STDOUT` | ❌ | refused by name. `pg_dump` and `\copy` need it |
| `LISTEN` / `NOTIFY` as SQL | ❌ | mpedb's notification exists but carries **no payload** (DESIGN-NOTIFY §1); a bare `NOTIFY chan` could be mapped, `NOTIFY chan, 'text'` could not |

## SQL surface

Everything in [COMPAT.md](COMPAT.md) applies — this table is the PostgreSQL-only
surface on top of it.

| Feature | Status | Comment |
|---|---|---|
| `expr::type` cast | ✅ | binds tighter than unary minus, chains left-to-right. The typmod is parsed and DISCARDED — mpedb has `Text`, not `varchar(8)`, and keeping the number would imply an enforcement that does not exist |
| `$$…$$` / `$tag$…$tag$` | ✅ | the tag rule (may not start with a digit) is exactly what keeps `$1` a parameter |
| `$n` parameters | ✅ | in both dialects |
| `~` / `!~` regex match | ✅ | lowered onto the same node as `x REGEXP y`, so there is one implementation |
| `~*` / `!~*` | ❌ | case-insensitive matching needs the pattern rewritten to `(?i)…`, which only works for a LITERAL — doing it only for literals would make `a ~* b` and `a ~* 'x'` behave differently |
| `ILIKE` | ❌ | not implemented (sqlite-dialect `LIKE` is already case-insensitive; the PG dialect's is not) |
| `OPERATOR(pg_catalog.=)` | ✅ | unwrapped to the operator inside. psql writes every comparison in `\d <table>` this way |
| `COLLATE pg_catalog.default` / `COLLATE "C"` | ✅ | the IDENTITY, not an approximation: mpedb compares text bytewise and the catalog reports `C` everywhere it is asked |
| `E'…'` escape strings | ❌ | |
| `SERIAL` / `BIGSERIAL` | 🚧 | resolves to the integer type. Auto-assignment comes from mpedb's rowid-alias rule (#94), so `id serial PRIMARY KEY` fills itself in and a non-PK `serial` does not |
| Table functions in `FROM` (`generate_series`, `unnest`, …) | ❌ | mpedb has no table-function planner. Refused BY NAME with the workaround where one exists — 1 235 corpus statements, previously an opaque ``unexpected trailing input `LParen` ``. `generate_series` is 578 of the corpus's 1 823 FROM-position calls and the only one broadly implementable: the rest need a stored procedural language, an array type, or JSON |
| Arrays (`int4[]`, `ARRAY[…]`, `unnest`) | ❌ | there is no storable array type. `int4[]` does NOT resolve to its element type — that would make a whole column read back as one number |
| `pg_typeof()` | ❌ | mpedb's `typeof()` speaks sqlite's five storage classes, a different vocabulary |
| `nextval` / `currval` / `setval` | ❌ | no sequences; the refusal points at the rowid-alias rule and `RETURNING id` |
| `octet_length()` | ❌ | counts BYTES; mpedb's `length()` counts characters. Aliasing them would return a plausible wrong number for any non-ASCII text |
| `char_length` / `character_length` | ✅ | → `length()` |
| `strpos` / `position(x in y)` | ✅ | the same function written both ways round; `position` swaps its arguments |
| `version()`, `current_schema()`, `current_database()`, `current_catalog` | ✅ | folded to constants |
| `pg_get_userbyid()`, `pg_table_is_visible()` | ✅ | one role, one namespace — `\d` calls both |
| `pg_get_expr()`, `pg_get_indexdef()` | 🚧 | the identity: mpedb stores DDL TEXT rather than a parse tree, so the argument already IS what the call would have returned |
| `ATTACH` and TEMP objects under the PG dialect | ✅ | the cross-database resolver (`dbref.rs`) lexes under the session's dialect, so `::` and `!~` survive it. This was a named limitation until the resolver was threaded — and the bypass that stood in for it silently disabled `CREATE TEMP TABLE`, which opens 76 of the 222 corpus files |

## Types

The core set maps onto mpedb's seven column types. `numeric` is carried as
canonical TEXT — lossless, and identical to PostgreSQL's own wire form.

| PostgreSQL | mpedb | Fidelity |
|---|---|---|
| `bool` | Bool | exact |
| `int8` | Int64 | exact |
| `int2`, `int4`, `oid` | Int64 | widened — a local write can exceed what the PG column takes |
| `float8` | Float64 | exact |
| `float4` | Float64 | widened |
| `text` | Text | exact |
| `varchar(n)`, `bpchar(n)`, `name`, `char` | Text | widened — **the length is not enforced** |
| `bytea` | Blob | exact |
| `timestamp`, `timestamptz` | Timestamp (µs, UTC) | exact |
| `date` | Timestamp | widened (midnight UTC) |
| `time` | Int64 (µs since midnight) | widened |
| `numeric`, `json`, `jsonb` | Text | via canonical text |
| `uuid` | Blob (16 bytes) | via bytes |
| `interval`, `timetz` | — | ❌ named refusal |
| anything else | — | ❌ unknown type |

The OIDs are PostgreSQL's own, from `pg_type.dat`. They are ABI: every client
has them compiled in, and a wrong one makes psycopg silently produce the wrong
Python type with no error to notice.

## Catalog

| Relation | Status |
|---|---|
| `pg_class`, `pg_namespace`, `pg_attribute`, `pg_type` | ✅ tables AND their indexes |
| `pg_index`, `pg_constraint`, `pg_attrdef` | ✅ PK, UNIQUE, FOREIGN KEY, CHECK |
| `pg_database`, `pg_roles`, `pg_am` | ✅ one database, one role, heap+btree |
| `pg_tables`, `pg_indexes` | ✅ |
| `pg_views`, `pg_matviews`, `pg_description` | ✅ (empty) |
| `information_schema.tables`, `.columns`, `.schemata` | ✅ |
| `information_schema.table_constraints`, `.key_column_usage`, `.referential_constraints` | ✅ |

`pg_catalog` resolves unqualified (it is on PostgreSQL's implicit `search_path`);
`information_schema` does **not**, which is what keeps a user table called
`tables` from being shadowed.

Fidelity notes worth knowing:

- `reltuples` is `-1` ("never analysed"), not a fabricated row count — a made-up
  number here would feed the CLIENT's planner.
- OIDs come from the table's STABLE id, so one survives a `DROP` of an unrelated
  table. A client that cached `attrelid` between two queries would otherwise read
  another table's columns.
- `attnum` is 1-based; `indkey` is space-separated (int2vector), not
  comma-separated, because every client parses exactly that.
- `is_nullable` is the strings `YES`/`NO`, not a boolean.

## psql

| Command | Status |
|---|---|
| `\d` (list relations) | ✅ |
| `\dt` | ✅ |
| `\d <table>` | ❌ — needs `format_type()`, `array_to_string()` and a `CASE` shape mpedb does not parse |
| arbitrary SQL | ✅ |

## What is measured

### The differential against PostgreSQL 16.14

`crates/mpedb-pg/src/bin/pg_regress_diff/` sends each of PostgreSQL's own
`src/test/regress/sql/*.sql` through BOTH a throwaway PG 16.14 cluster and
mpedb, and diffs the two transcripts **against each other**. The `.out` files
are not used at all — they carry PostgreSQL's error wording, OID numbering and
`EXPLAIN` output, none of which mpedb can or should reproduce.

**Full run, all 222 files, 40 576 statements** (PostgreSQL 16.14, measured on a
real volume — see the notes below on why both of those qualifiers matter):

| outcome | statements | share |
|---|---:|---:|
| **match** — both answered, identically | 9 277 | 22.9 % |
| **order-only** — same rows, different order, no `ORDER BY` asked | 20 | 0.0 % |
| **both refused** — both errored | 7 502 | 18.5 % |
| **refused** — PostgreSQL answered, mpedb refused by name | 20 727 | 51.1 % |
| **DIVERGED** — both answered, differently | 3 050 | 7.5 % |

Agreement (match + order-only + both-refused) is **41.4 %**. Divergence — the
only number that means something is WRONG — is **7.5 %**.

**One of those matches was not a match, and a class of them never could be.**
psql spells the difference between a result of ONE EMPTY ROW (`"\n"`) and a
result of NO ROWS (`""`) exactly. The normaliser split transcripts into lines
and rejoined them with `"\n"`, which maps both to `""` — so the harness scored
that pair as agreement. It is closed by keeping the transcript as ROWS and
never rejoining. The hole was narrow and it ran in exactly one direction: it
could only ever turn a wrong answer into a match, never the reverse. How often
the corpus hit it is not claimed here — the two runs either side of the fix
also moved by the corpus's own nondeterminism, and separating the two was not
possible after the fact.

Run to run the totals move by a handful of statements (a `match` here, a
`both-refused` there). That is the corpus's own nondeterminism — OIDs, `now()`,
a `\timing` line — not measurement noise to be averaged away, and it is why a
baseline compares PER FILE rather than on the total.

**The run before this one was thrown away, and why is worth more than the
number.** It reported two files as HUNG that had run clean the pass before
(`cluster`, `collate.icu.utf8`) and 656 new divergences. Run in isolation, both
files reproduced the baseline exactly. The cause was 2 513 files left in the
scratch directory by seven earlier passes: `CREATE TEMPORARY TABLE` opens a
16 MB ephemeral member, `DETACH` unlinked it, and a connection that simply
CLOSED never detached — so every connection that used a temp table left 16 MB
behind, forever. The volume filled enough for two files to time out. Fixed in
the engine (`impl Drop for AttachState`, with a regression test) rather than in
the harness, because the leak was mpedb's. Same shape as the `/dev/shm`
flapping recorded below, from the same root — an ephemeral file nobody was
responsible for removing. A measurement tool that litters eventually measures
its own litter.

Both percentages rose against the previous run over the same code, because the
denominator lost 1 634 statements that were never a compatibility question:
**`EXPLAIN` is excluded from both arms.** It was found by hunting the corpus's
single largest refusal bucket — 1 047 statements reported as `unsupported
statement (`, which turned out to be `EXPLAIN (COSTS OFF) SELECT …`,
PostgreSQL's parenthesised option list hitting a parser whose `EXPLAIN` takes no
options. Teaching the parser to swallow the option list was the obvious move and
the wrong one: it would have converted 1 047 refusals into 1 047 divergences,
because mpedb prints its own access paths and MPEE join order and PostgreSQL
prints `Seq Scan on document / Filter: …`. Neither can produce the other's plan
text, and neither should. In absolute terms divergence did not move (3 040 →
3 036); of the 1 634 excluded statements, 1 121 were `refused` and 521
`both-refused`, i.e. essentially all of them were already scored as failures or
as hollow agreement.

**That agreement figure went DOWN from a previously reported 46.9 %, and the
drop is the measurement getting more honest.** The statement splitter used to
read `COPY … FROM stdin` payloads as SQL: eleven files, 1 553 data lines, each
`;` inside a data row splitting into another statement that was never SQL. Both
engines rejected all of it, so ~6 000 pieces of garbage counted as
`both-refused`, i.e. as AGREEMENT. Removing them cost six points of a number
that was never real.

What moved in the right direction is what matters: real matches rose by 2 070,
and divergence nearly halved, from 13.7 % to 7.5 %.

`both-refused` counts as agreement on purpose: mpedb's contract IS a named
refusal, and refusing what PostgreSQL refuses is the right answer — several
corpus files (`int4`, `limit`, `bit`) are largely error-case tests, where that
is most of the file.

**order-only** is a separate column rather than folded into `match` because it
is the one number that would let this harness flatter itself. SQL does not
define the order of a result set without `ORDER BY`, so two engines returning
the same multiset are both right; counting that as a divergence measures the
scan order of two storage engines, which is not a compatibility question.
sqlite's sqllogictest solves the same problem with an explicit `rowsort` marker
per query — the PostgreSQL corpus has no such declaration, so the statement text
is asked instead (`ORDER BY` at paren depth zero). A PARTIAL `ORDER BY` whose
keys tie is still counted as a divergence even though both engines are right:
the alternative would hide a real ordering bug behind an "it might have been a
tie" excuse.

### What the measurement is worth, and what it is not

Two things about this number are worth stating plainly.

**It was not stable until the scratch directory moved.** The same command
returned three different results for `window.sql` (0, then 7, then 2 matches)
with no code change in between. `/dev/shm` was 100 % full, and the temp member
backing `CREATE TEMP TABLE` is a 16 MB file there — whether it fit depended on
free space at that instant. `mpedb_testkit::scratch_base` was written for
exactly this and says so in its own docs; the two ephemeral paths in
`multifile.rs` were simply never wired to the same knob. They are now, so:

```sh
MPEDB_TEST_DIR=/mnt/ext4/mpedb-scratch cargo test --workspace
```

**One file does not finish.** `temp.sql` hangs mpedb and is recorded as
`HUNG`, its 163 statements counted as diverged. The harness gives each file a
120-second watchdog rather than letting one file stall a 222-file run — a
measurement tool that can be stopped by the thing it measures cannot finish, and
an unfinished run measures nothing. The hang is open; a hang is the same class
of contract violation as a panic.

### The divergence work list — the column that means WRONG

For every round until this one, `diverged` was one number. `refused` had a
ranked cause list that rewrote the roadmap twice; the column that means an
answer is WRONG had nothing — not even an example. The harness now classifies
each divergence by the shape of the DIFFERENCE (`divergence.rs`), and ranks it
the same way:

| statements | shape |
|---:|---|
| **1 058** | **mpedb ANSWERED what PostgreSQL refused** — a FAMILY over 218 reasons, see below |
| 628 | text values differ (the residual catch-all) |
| 518 | row count: mpedb returned **NO** rows |
| 131 | row count: mpedb returned MORE rows |
| 106 | integers that are not equal |
| 105 | row count: mpedb returned FEWER rows |
| 83 | one side empty, the other not |
| 73 | PostgreSQL returned no rows, mpedb returned some |
| 49 | same number, different rendering |
| 37 | one side is infinity or NaN, the other is not |
| 49 | NULL against a value (26 PG-NULL, 23 mpedb-NULL) |
| 19 | row ORDER differs **under an explicit `ORDER BY`** |
| 19 | trailing spaces — PostgreSQL PADS (`character(n)`), mpedb does not |
| 32 | field-count and float-precision shapes |

**A third of the wrong answers are mpedb accepting what PostgreSQL rejects** —
1 058 of 3 101 — and the moment that line was split by PostgreSQL's own reason,
it said something different from what it looked like. That is the SEVENTH time
in this document, and the first time the mistake was made *by this section*:
the paragraph that used to stand here read the 1 058 as mpedb being too
permissive across the board. It is 218 distinct reasons, and the largest ones
are not that at all:

| statements | PostgreSQL's reason | what it is |
|---:|---|---|
| 144 | `relation _ does not exist` | PostgreSQL's OWN cascade — its earlier `CREATE` failed, mpedb's did not |
| 83 | `type _ does not exist` | the same, for types |
| 58 | `invalid input syntax for type json` | **mpedb accepting bad JSON** |
| 42 | `violates foreign key constraint _` | **FIXED** — the wire protocol was running with enforcement off |
| 27 | `invalid input syntax for type numeric` | **mpedb's sqlite CAST rule** |
| 27 | `no schema has been selected to create in` | environment, not a question about mpedb |
| 22 | `current transaction is aborted` | PostgreSQL's cascade again |
| 22 | `division by zero` | **FIXED** — see below |
| 560 | 206 further reasons | a tail |

So roughly 276 of the top are PostgreSQL declining because of something that
already went wrong ON PostgreSQL'S SIDE, or because of the harness's schema
handling — the exact mirror of the `unknown table` shadow measured on the
refusal column, and it inflates this line the same way. The genuinely
actionable core is narrower and much more specific: JSON input validation,
foreign-key enforcement, numeric CAST, and division by zero. `'nan'::numeric`
folding to `0` — already named in this document as a separate item — is
visible right there in the numeric line.

The reading still holds in kind: this is the direction nobody looks for, and
the direction in which "improving compatibility" makes the engine worse. Its
SIZE was overstated by a third, by exactly the reading this document has
warned against six times before doing it once more.

**Second, 518 statements where mpedb answers with NO ROWS.** A feature that
silently returns nothing is worse than one that refuses: a refusal is visible
in the refusal column and in the caller's error handler, and an empty result
looks like data.

**Third, 19 statements where mpedb returns the right rows in the wrong order
under an explicit `ORDER BY`.** That class could not exist as a separate line
before — without an `ORDER BY` the comparator correctly calls a reordering
agreement, so the ones that DID ask were mixed in with wrong values.

Two of the classifier's own labels were wrong in its first run, and both were
the same mistake: **giving the largest possible disagreement a reassuring
name.** `infinity` vs `0` was reported as "agree to ~1e-12", because
`(inf − 0).abs() <= inf * 1e-12` is true. `9223372036854775808` vs
`9223372036854776000` was reported as "same number, different rendering",
because two different integers share one `f64`. A classifier is allowed to be
coarse; it is not allowed to launder. Integers are now compared as integers
before the float path sees them, and non-finite values never reach the
tolerance test.

**Every ranked line now says where it LIVES**, on both lists — the split of
last resort, and the only one available when a shape has no message left to
group on. It is printed as a name when one file holds at least a third, and as
a spread otherwise, because both are findings:

```
   628  text values differ
        spread over 51 files, none holding a third (top: jsonpath.sql, 137)
    27  mpedb ANSWERED, PostgreSQL: invalid input syntax for type numeric: _
        all in numeric.sql
    43  … violates foreign key constraint _
        35 of 43 in foreign_key.sql (over 5 files)
```

The first version printed the file name only when concentrated and nothing
otherwise — so the two largest shapes printed nothing at all, and "nothing"
reads as "not measured" rather than as "everywhere". A spread of 51 files is
as much of an answer as a concentration in one, and it is the answer that says
"this is a hundred small jobs, not one".

The rule the module is built on, stated because it is one edit away from being
broken: **classifying never changes a verdict.** A classifier that recognises
`character(n)` padding is two lines from a comparator that forgives it, and
forgiving it would turn 19 wrong answers into 19 silent ones.

### The first item off that list: division by zero

sqlite yields NULL on a zero divisor; PostgreSQL raises 22012. The difference
is not cosmetic — a NULL flows on through a `WHERE` as "not true", so the row
silently vanishes and the caller sees a short result rather than an error.

Fixed as two OPCODES, `Instr::DivStrict` / `Instr::ModStrict`, chosen by the
binder when the session dialect is PostgreSQL (PLAN\_FORMAT 72). Not a flag
read at eval time, and the reason is the one the LIKE family already gives:
the dialect is a COMPILE-time property, so it has to reach the plan HASH. The
plan registry is shared in the catalog and serves plans by hash to every
attached process; a runtime flag would let a sqlite-dialect session execute a
plan compiled with PostgreSQL's semantics — the same bytes meaning two things.

sqlite loses nothing: `1/0` still folds to NULL, and there is a test that
asserts both dialects in the same function rather than trusting that they were
checked separately.

**Measured: divergence 3 101 → 3 072.** 18 of the 29 became agreement
(both engines refuse); the rest became named refusals, mostly in `float4`/
`float8`, where mpedb's operand handling produces a zero divisor PostgreSQL
never sees.

**And one statement went the other way, which is the part worth reading.**
`select_having.sql` lost a match:

```sql
-- and just to prove that we aren't scanning the table:
SELECT 1 AS one FROM test_having WHERE 1/a = 1 HAVING 1 < 2;
```

PostgreSQL answers `1`. Its own comment says why: a degenerate aggregate with
a constant `HAVING` and no `GROUP BY` returns one row WITHOUT scanning, so
`1/a` is never evaluated and `a = 0` never divides. mpedb scans, and now
raises.

That match was a COINCIDENCE. mpedb was scanning all along; `1/0` gave NULL,
the `a = 0` row was filtered out as not-true, and the `a = 1` row produced the
same answer by a different route. The change did not take a capability away —
it made a planner gap that was already there stop hiding behind a NULL. The
gap is now its own item, which is the correct place for it. Same pattern as
`'NaN'::numeric` and as `plpgsql.sql`: **a feature that works exposes an older
one that does not, and the score moving the wrong way is how you find out.**

### And the second: math domain errors

`sqrt(-1)`, `ln(0)`, `ln(-1)` — sqlite answers NULL, PostgreSQL raises. Five
strict `ScalarFn` codes (`SqrtStrict`, `LnStrict`, `Log10Strict`, `Log2Strict`,
`LogBaseStrict`; PLAN\_FORMAT 73), chosen by the binder on the RESOLVED
function rather than on the name, so a future alias cannot pick up the sqlite
form by spelling.

**`pow` and `exp` are deliberately NOT in that list.** Both engines already
return an infinity there, so a strict form would turn agreement into refusal —
which is the failure mode a compatibility change invites, and the reason this
document's rule is "the cheapest way to move a compatibility metric is to
accept syntax you cannot answer correctly", read in reverse.

**Measured: `numeric.sql` divergence 143 → 129, and its match count did not
move at all.** All 14 became both-refused. Corpus divergence 3 072 → 3 059.

The bucket that produced this item is worth writing down, because the count
alone said "27 numeric input problems" and the split says three jobs:

| statements | PostgreSQL's reason | job |
|---:|---|---|
| 27 | `invalid input syntax for type numeric` | sqlite's permissive TEXT→number affinity at INSERT |
| 14 | logarithm / square root of a non-positive | **done** |
| 10 | `numeric field overflow` | `numeric(p,s)` precision and scale not enforced |
| 9 | `cannot convert NaN/infinity to smallint` | `'NaN'::numeric` folds to `0`, so the int cast has nothing to refuse |
| 6 | `smallint/integer/bigint out of range` | integer width not enforced on a cast |

The `'NaN'::numeric` row is the one this document already names as a root
cause. It is visible here as a SECOND-order effect — nine statements whose
complaint is about `int2`, caused entirely by a cast three steps earlier.

### The third and fourth: the wire protocol enforces referential integrity,
### and `DROP TABLE … CASCADE` parses

These two are one item, because the first is only safe with the second, and
finding that out took a measurement.

**mpedb ran the differential with foreign keys OFF.** `[compat] foreign_keys`
defaults to false — sqlite's default, and the only one that leaves an existing
file's behaviour unchanged. Over the v3 protocol that is a wrong answer with no
way for a client to notice: PostgreSQL has no `PRAGMA foreign_keys`, and a
client's `REFERENCES` was being accepted and then not enforced. `Session::new`
now turns it on for its own CONNECTION — not for the database, so a
sqlite-dialect process attached to the same file keeps its own setting.

Turning it on alone REMOVED 40 divergences and COST 14 matches, all but one of
them in `foreign_key.sql`, and the 13 looked like a gap in mpedb's foreign-key
resolver: `foreign key mismatch - "FKTABLE" referencing "PKTABLE"`. They were
not. They were the shadow of one unsupported keyword:

```
DROP TABLE PKTABLE CASCADE   ->  parse error: unexpected trailing input `CASCADE`
CREATE TABLE PKTABLE (...)   ->  schema error: duplicate table name
CREATE TABLE FKTABLE (ftest1 int REFERENCES PKTABLE MATCH FULL, ...)
```

The DROP failed, so the old `PKTABLE` — whose primary key is a two-column
composite — was still standing; the new single-column one was refused as a
duplicate; and `FKTABLE` then referenced a parent whose key has the wrong
arity. The resolver was right. Everything downstream of the keyword was wrong.

So `DROP TABLE … CASCADE | RESTRICT` now parses. `RESTRICT` is the default and
keeps the orphan-row refusal that enforcement imposes; `CASCADE` drops anyway.
That is NOT PostgreSQL's full meaning, which also drops the dependent
CONSTRAINT — mpedb leaves the child's key definition dangling, which is
sqlite's behaviour and already what happens with enforcement off. Stated rather
than glossed: the child keeps its rows, and its next write says `no such
table`.

**Measured, the two together, against the corpus:**

| | before | after |
|---|---:|---:|
| match | 9 180 | **9 277** |
| both-refused | 7 488 | 7 502 |
| refused | 20 829 | **20 727** |
| DIVERGED | 3 058 | **3 050** |
| agreement | 41.1 % | **41.4 %** |

Twenty-two files moved and **exactly one lost agreement, by one statement** —
`truncate.sql`, where `DROP TABLE truncate_a` is now blocked because the
list-form drop four statements earlier (`DROP TABLE a,b,c,d,e CASCADE`) is
still refused on the comma. The same shadow, one syntax further out.

`updatable_views` is the interesting row: divergence rose 88 → 115 while
agreement rose 19 and refusals fell 46. Both at once, and both for the same
reason — 46 statements that used to fail at parse now RUN, and what runs is
partly right and partly wrong. A rise in the divergence column is not
automatically a regression; it can be the price of getting far enough into a
file to be wrong in a new place, and only the refusal column falling at the
same time tells the two apart.

### A change that was measured, and then thrown away

`DROP TABLE a, b, c` — the comma list — is the obvious next item after
`CASCADE`, for the same reason: refusing the comma fails the WHOLE statement,
so none of the tables go and the file's later `CREATE`s of those names are
duplicates. It was built, tested (resolve every name before dropping any; a
child inside the list does not block its own parent), and measured:

> Fifteen files improved — `alter_table` +10 agreement, `triggers` +4,
> `updatable_views` +2, seven more +1 each. And `foreign_key.sql` **HUNG**.

Not slowly: the watchdog was raised from 120 s to 900 s and the file still did
not finish. So the change was reverted, in full, and the finding kept. What is
known about the hang:

- The autocommit path does not hang. Every one of the file's 1 158 statements
  runs under `mpedb exec` without stalling.
- The **repl** path does not hang either — the whole file, in ONE process, same
  config (`foreign_keys = true`, `dialect = "postgres"`), finishes in under
  90 s.
- Only the WIRE path hangs (`mpedb_pg::Session` over an in-memory database,
  with statements buffered into `txn_log` and replayed by `commit_block`).
- It is only REACHABLE with the list, because more `DROP`s succeeding is what
  gets the file far enough in to reach it.

That is a real bug and it is now its own item rather than a shipped hang.

**The harness change that came out of it is the lasting part.** "This file
hung" is a collapsed line with nothing inside it — the exact shape this
document has learned five times not to trust — and it is the least useful
possible form for the one failure that stops all measurement. The mpedb worker
now publishes a count of completed queries, and a timeout names the statement:

```
foreign_key  HUNG after 120s at statement 1153 of 1158; counted as all-diverged
             stuck on: UPDATE fkpart13_t1 SET a = 2 WHERE a = 1
```

Counted on FRAME boundaries, not by scanning the output for the byte `Z`: a
`ReadyForQuery` tag can appear inside a row value, and a data-dependent
progress counter would name the wrong statement exactly when a hang makes it
matter. There is a test for that, and for a frame split across two writes.

**It named the wrong statement on its first real use anyway.** The startup
handshake ends with its own `ReadyForQuery`, before any query runs, so the
frame count is one ahead of the completed-query count — and the report pointed
at a `BEGIN`, which cannot block, instead of the `COMMIT` before it, which
never returned. The `BEGIN` was a plausible-looking answer: it is a
transaction verb, and "the transaction machinery is stuck" reads as a
diagnosis. It was off by one.

The thing that caught it was not re-reading the code. It was that the named
statement did not make sense — a `BEGIN` in this session sets two fields and
clears a `Vec`. **A tool that names something has to name something falsifiable,
and this one did.** The version that said "this file hung" could not be wrong
about anything.

With it fixed, the reproducer is one command and needs no code change at all —
rewrite the corpus file's list-drops as single drops and run that file:

```
repro403   HUNG after 120s at statement 403 of 403
           stuck on: COMMIT
```

From there the reproducer shrank to **five statements**:

```sql
CREATE TEMP TABLE t ( id int primary key, fk int );
BEGIN;
INSERT INTO t VALUES (0, 20);
UPDATE t SET id = id + 1;      -- fails: cannot update primary key column
COMMIT;                        -- HANGS
```

Reduced all the way, the trigger is simpler than that five-line file makes it
look: **two writes to an ATTACHED member inside one `BEGIN`/`COMMIT` block.**
A main-table version is fine. ONE write inside the block is fine. The foreign
key is irrelevant (the version with no `REFERENCES` at all spins), `DEFERRABLE`
is irrelevant, and so is `TEMP` — an ordinary `ATTACH ':memory:' AS aux` with
two writes to `aux.t` spins identically.

The reduction had to be walked back once, and the walk-back is the useful part.
An earlier pass concluded "a second write that SUCCEEDS is fine", from a run of
variants that all used a `timeout` shorter than the harness's own watchdog (see
below). Re-run correctly, `INSERT` followed by a SUCCEEDING `UPDATE` spins just
the same — so the failing statement was never an ingredient, and the whole
`DEFERRABLE` / primary-key-update story the trail began with was scenery.

**It SPINS — 84 % of a core — rather than blocking.** With the process wedged,
one thread sits in `futex_wait` (the harness waiting on its worker) and the
other is `R` at 84 % CPU. That rules out the first thing anyone would look for:
it is not a lock nobody releases, it is a loop nobody leaves. Two minutes of
`ps -L` said more than an hour of reading the transaction code, and it is the
measurement that should have come first.

**FIXED.** The cause is an unmatched decrement, three frames below where the
trail was pointing. `Shm::try_begin_exclusive_write` is CONDITIONAL — a private
`:memory:` database with no reader pins takes an in-place exclusive and nothing
else does — and `WriteTxn`'s three release sites called `end_exclusive_write`
UNCONDITIONALLY. The nesting depth it decrements is per-THREAD and shared
across engines, while the flag it guards is per-shm. So a transaction on the
MEMBER, which never took an exclusive, still decremented; main's
`exclusive_write` stayed set with the depth at 0; and the next private read on
MAIN read that as a FOREIGN writer and spun waiting for its own thread.

`WriteTxn::end_exclusive_if_taken` gates the release on the same `in_place`
flag the acquisition already recorded. The reproducer's test now passes in
0.07 s, and it keeps its worker-thread-and-deadline shape rather than becoming
a plain assertion: a regression here is a HANG, and a hang in a test suite is a
stuck CI job rather than a red one.

Everything below is what the trail looked like before the cause was found. It
is kept because the route to it is the reusable part.

It points at attached-member writes inside an open transaction. A temp table
lives in an ATTACHED member (`multifile.rs`); `commit_block` opens ONE
`WriteSession` on the main database and replays the log into it, and a
statement touching only the member takes `DbRoute::AttachedOnly` and forwards
to that member's own handle. `multifile.rs`'s own module docs say that
"cross-file statements inside an open `WriteSession`" are **refused by name**.
The `AttachedOnly` arm does not refuse — it forwards — and the second forward
inside one block never returns.

**Two false trails, both from the instrument, both worth writing down:**

1. The shrink ran CONCURRENTLY with a full `cargo test --workspace`. The
   watchdog measures WALL CLOCK. Every result from that window was
   uninterpretable and had to be thrown away — the same rule as any A/B whose
   arms do not share a host, applied to a timeout.
2. The reduction script used `timeout 70` around a harness whose watchdog is
   120 s. A hang was killed before it could be reported and read as a PASS.
   Four "this ingredient is not needed" conclusions were wrong.

Both are caught by the same thing, and it is now in the script: **a null
control in the same batch — a case KNOWN to hang.** An instrument that cannot
be shown to detect the effect is not evidence of its absence.

### The ranked work list

The harness records mpedb's SQLSTATE and message for every statement PostgreSQL
answered and mpedb refused, groups them by SHAPE (anything quoted → `_`, digit
runs → `N`) and ranks them. **1 392 distinct causes over 20 819 refusals** — the
cause list got FINER as the refusals themselves got more specific, which is the
point of naming a refusal. The top, with a real example under each:

| refusals | SQLSTATE | cause | example |
|---:|---|---|---|
| 2 056 | 42P01 | unknown table | **79 % CASCADE** from an earlier failed `CREATE` — counted, see below. 551 is the real content |
| **1 386** | 42601 | **declarative partitioning** | `PARTITION BY` / `PARTITION OF` / `ATTACH PARTITION` — one message, gathered from three |
| 831 | 42601 | containment / jsonb-path operators | `@>`, `<@`, `@?`, `@@` — one message, named |
| 754 | 25P02 | transaction aborted | cascade from an earlier failure inside a block |
| 493 | 42601 | parse: `expected )` closing the argument list | |
| 439 | 42601 | parse: expected an expression | |
| 397 | 42P01 | `DROP TABLE`: no such table | mostly cascade, same shape as row 1 |
| 380 | 42601 | table function `check_estimated_rows(…)` | a plpgsql helper the CORPUS defines — see the family table below |
| 318 | 42601 | parse: `expected (` | |
| 281 | 42601 | empty quoted identifier | |
| 278 | 42601 | array / row constructors | `ARRAY[…]`, `ARRAY(SELECT …)`, `ROW(…)` |
| **2 169** | 42883 | unknown function — **404 DISTINCT names**, biggest 127 | a long tail; the FAMILY total, see below |
| **969** | 42601 | table functions in `FROM` — **107 names**, biggest 380 | the FAMILY total, see below |

**Partitioning is the second-largest item in the corpus, and nothing said so
until the messages were fixed.** `PARTITION BY`, `PARTITION OF` and `ATTACH
PARTITION` each landed on whichever check the parser happened to reach last —
`expected (`, the generic trailing-input complaint, and (for `ATTACH`) "expected
ENABLE, FORCE, or DISABLE ROW LEVEL SECURITY", which is a message about a
different feature entirely. Three of the four spellings pointed the reader
somewhere useless, and the work list read the whole thing as a scatter of
punctuation problems. One named refusal gathers 1 386 statements: `expected (`
fell 1 098 → 316, the RLS message 656 → 418, and the `PARTITION` tail to zero.

The totals did not move, and that is correct — these are refusals either way.
What changed is that the second-biggest job in the corpus is now visible as one.

**A split needs its family total back, and the table-function bucket is why.**

Splitting `unknown function` by name turned one line into a work list. Doing
the same to `table function in FROM` did too — and the result says the opposite
of what the collapsed line said:

| family | refusals | distinct names | biggest single name |
|---|---:|---:|---|
| `unknown function` | 2 169 | 404 | `pg_input_is_valid()` — 127 (6 %) |
| `table function in FROM` | 969 | 107 | `check_estimated_rows()` — **380 (39 %)** |

`check_estimated_rows` is not a function anyone will implement. It is a
plpgsql helper the corpus **defines for its own use** in `stats_ext.sql`, and
39 % of the "table functions" bucket is calls to it. So a table-function
planner does not collect 969 statements; it collects them only in the company
of `LANGUAGE plpgsql … RETURNS TABLE`, which is a different item on this list
(35 row-returning functions in the plpgsql frontend's own reasons). The second
and third names — `pg_input_error_info` (110) and `json_populate_record` (57) —
need a record type and a JSON expander respectively. Three jobs, one line.

But the split also DELETED a number the collapsed line was good at: how big the
family is. Both readings answer different questions — the family says what a
whole subsystem is worth, the split says whether that total is one item or a
tail, and therefore whether the total is reachable at all. The report now
prints both, and that is the general rule: **a bucket split by name needs a
family rollup, or the next reader reconstructs the wrong total by adding up the
top few lines.**

**Seven times now, one number has stood for a population nobody had looked
inside, and seven times the reading was wrong while the number was right.**

| the line | what it looked like | what it was |
|---|---|---|
| `unknown function` 2 158 | the biggest opportunity | 404 names, biggest 127 — a tail |
| `unexpected trailing input` 1 933 | statement-tail keywords (`CASCADE`) | PARTITION; the corpus has 48 CASCADEs |
| tail `TIME` 150 | a type-name gap | `AT TIME ZONE`, swallowed as an alias |
| `unknown table` 1 875 | the #1 item | 86 % shadow of the items below it |
| `table function in FROM` 969 | one feature: table functions | 107 names; 39 % is ONE corpus-local plpgsql helper |
| `mpedb ANSWERED what PG refused` 1 058 | mpedb too permissive, across the board | 218 reasons; ~26 % is PostgreSQL's OWN cascade |

None of those counts was wrong. The INTERPRETATION was, every time, and always
for the same reason: an aggregate with one example under it reads as a
description of the whole, and an example is one arbitrary member.

The seventh happened in the same session as the sixth, in a section written to
warn about it, by the author of that section — which is the strongest available
evidence that knowing the rule does not apply it. What applies it is the tool:
a grouping key that keeps the distinguishing field. The rule that follows is
mechanical rather than wise. **Whenever a bucket reaches the top of a ranked
list, the next move is to find a field that splits it and re-rank — and to
print the family total alongside, so the split does not delete the size.**

**A collapsed line's EXAMPLE is not its description, and this document said so
three times before believing it.** `unknown function` carried
`pg_advisory_xact_lock()` and turned out to be 404 names. `unexpected trailing
input` carried ``Ident("cascade")`` and turned out to be PARTITION — the corpus
has 48 CASCADEs, so it could never have been 1 933. The tail `TIME` turned out
to be `AT TIME ZONE`, swallowed as an alias (`ts AS AT`) so the complaint named
the word after the one that mattered. Each was one arbitrary member standing in
for a population, and each pointed the wrong way.

**The list rewrote the roadmap twice in one afternoon, in opposite directions.**

First it demoted `generate_series`. The plan had it as the second-biggest
blocker on the strength of 803 occurrences across 96 files — a real count — and
it is nowhere in the top 25, because `FROM generate_series(…)` fails at PARSE
time and lands in a different bucket entirely.

Then reading the EXAMPLES promoted it again, under a better name: those
refusals were **table functions in `FROM`** — `rngfunct(1) WITH ORDINALITY`,
`unnest(…)`, `generate_series(…)`. So the feature is worth several times what
the plan estimated, but the thing to build is a table-function row source, not
one function. It now has its own named refusal and its own line (1 235).

Neither correction was available from counting occurrences in the corpus, and
neither was available from the outcome counts alone. Both needed the message.

**The biggest bucket is a long tail, and that took a measurement to learn.**
`unknown function` sat at the top of this list for three rounds as one line of
~2 150 refusals with one arbitrary example under it. That line reads as the
largest opportunity in the corpus. It is not: keeping the NAME in the grouping
key splits it into **404 distinct functions**, and exactly one of them —
`pg_input_is_valid()` at 127 — reaches the top 25 at all. The rest are each
worth fewer than about 120 statements. Per unit of work it is the least
attractive item in the top five, not the most, and the collapsed line said the
opposite. (The top name is not even a function to implement: `pg_input_is_valid`
asks whether PostgreSQL's OWN type-input function accepts a string. The answer
is PostgreSQL's by definition.)

The change that produced that was five lines, and it shipped BROKEN for a full
run: `msg.strip_prefix("unknown function \`")` against a message that reads
`bind error: unknown function \`f()\`; available: …`, where the marker is in
the middle. The prefix matched nothing, the code fell through to the old
grouping, and the output was identical to not having made the change — which
looked like the idea had failed. Its unit test passed throughout, because the
test fed it a stripped message rather than the real one. A test that builds its
own input can confirm a function that never meets production.

`generate_series` is the one that was BUILT rather than reclassified, and what
it measured is worth as much as what it fixed. It is a real row source now —
`AccessPath::Series` over a `SERIES_TABLE` sentinel, rows generated and charged
to the same #74 work meter the engine's scans charge, `KeyPart` bounds so a
correlated series reuses the index nested loop's machinery rather than making
LATERAL a separate concept (PLAN\_FORMAT 71). The table-function bucket fell
1 235 → 946.

**But only about 20 of those 289 became agreement.** The rest moved to OTHER
refusal buckets — `unknown table` +113, `unexpected trailing input` +16 —
because those statements needed more than `generate_series`: LATERAL, arrays,
`::numeric`, or the series in JOIN position. The bucket was the FIRST blocker
in each of them, not the only one. 803 corpus occurrences bought 20 statements,
and the ranked list cannot tell you that in advance — only building it can.

It also cost four wrong answers before it shipped, which the oracle caught and
which are worth naming because both are the same shape. `generate_series('nan'
::numeric, 100, 10)` is an ERROR in PostgreSQL — it has a numeric series and
NaN is not a legal bound for it. mpedb has no NaN-carrying numeric, so
`'nan'::numeric` takes sqlite's CAST rule and folds to the INTEGER 0, at which
point the series is indistinguishable from a written `generate_series(0, 100,
10)` and ANSWERS where PostgreSQL refuses. Neither the folded value nor its
type can see it afterwards — both say `Int(0)` — so the cast is refused while
it is still WRITTEN DOWN, on the AST, before binding. That also refuses
`generate_series(1::numeric, 3::numeric)`, which PostgreSQL accepts: mpedb has
no numeric or timestamp series, and a refusal is the contract where an invented
answer is not. (The root cause is not the series. `CAST('nan' AS numeric)` = 0
is sqlite's rule and mpedb agrees with sqlite there; it is still wrong under
the PostgreSQL dialect, and it is still wrong inside `sum()`, where the cast
refusal cannot reach it. That is a separate item.)

**PL/pgSQL compiles.** PostgreSQL's procedural language is a third FRONTEND to
the PySpell layer (`mpedb_spell::plpgsql`), emitting the same IR the Python and
Rust subsets do — so the security boundary is unchanged (the parser stays on
the host; the runtime only ever sees IR), and so are the budget and the content
hash. The unit is the whole `CREATE FUNCTION … LANGUAGE plpgsql` statement,
because that is what `pg_dump` writes and because the HEADER is where the
parameter names live. `CREATE FUNCTION` is now a DDL statement, so a dump
replays over the wire protocol unchanged and the function is callable from SQL
immediately.

Against PostgreSQL's own corpus it compiles **45 of 417** plpgsql functions
(`cargo test -p mpedb-spell -- --ignored plpgsql_corpus`). That test asserts
nothing about the rate on purpose: a coverage number a test enforces is a
number someone eventually moves by widening what is accepted, and this
frontend's refusals are load-bearing. What it does assert is that no input
panics. The ranked reasons are the work list — 98 `RETURNS trigger`, 45
`RAISE`, 35 row-returning (`SETOF`/`TABLE`), 14 cursors, 11 `OUT` parameters.

`plpgsql.sql` moved 240 → 254 match and 542 → 517 refused. Its divergences rose
32 → 51, and those are NOT the frontend being wrong: the statements simply
reach further into the file now. Same pattern as the `'NaN'::numeric` case — a
feature that works exposes an older gap, and saying so is the point of counting
divergence separately.

> **Correction, once the divergence column was classified rather than
> described.** This paragraph used to attribute those 51 to `character(20)`
> padding — PostgreSQL pads, mpedb stores unpadded. Padding is real and it is
> **19 statements in the whole corpus**, 0.6 % of the divergence column. The
> claim was reasonable, unmeasured, and wrong by two orders of magnitude, and
> it is the same mistake this document catalogues below for the *refusal*
> column: an example read as a description. The list built to catch it in one
> column had not been pointed at the other.

**The biggest line in that table is mostly a SHADOW of the lines below it, and
that is now counted rather than claimed.**

`unknown table` sat at #1 in every measurement this session. The harness now
remembers, per file, which `CREATE TABLE`s mpedb refused, and classifies each
later `unknown table X` as a CONSEQUENCE (the create was refused earlier in the
same file) or an INDEPENDENT gap. Over the WHOLE corpus — all 222 files, no
subset and no extrapolation:

> **2 103 of 2 654 are consequences (79 %); 551 are not.**

So the corpus's largest single item is largely the second-order cost of the
refusals ranked above it. Fixing those removes it; it is not its own job. The
real content is **551**, which puts it below partitioning (1 386) and level
with the containment/jsonb-path operators (831). An earlier round measured this
over the 40 heaviest files and scaled — 86 %, ~260 — and both the share and the
remainder moved once it was measured on everything. The extrapolation was
directionally right and quantitatively wrong, which is the ordinary fate of an
extrapolation from the heaviest members of a skewed population.

The name extractor is a text scan rather than a parse, deliberately: the
statement already failed to parse in at least one engine, so a parser is the
wrong tool, and reading a name wrong here can only misfile ONE refusal between
two buckets — it cannot change whether a statement passed. That tolerance is
honest for this job and would not be for the judging. It is tested against every
spelling the corpus uses AND against `CREATE INDEX`/`VIEW`/`FUNCTION`, because a
false positive there would quietly inflate the consequence bucket and flatter
the split.

(The caveat that used to sit here — "40 files of 222, chosen as the heaviest" —
is retired: the split runs over every file now.)

The same list is what produced the `EXPLAIN` exclusion above — and that one is
worth stating as a general rule, because it is the failure mode a coverage
number invites: **the cheapest way to move a compatibility metric is to accept
syntax you cannot answer correctly.** Every refusal turned into a divergence
reads as progress in the parse-error column while making the engine wronger.
The list is only useful if the response to a big bucket may be "this is not a
compatibility question", and that has now happened twice — for `COPY` payloads
and for `EXPLAIN`.

What the list says overall: **most of the refusals are GRAMMAR**, not missing
features — statement-level coverage (`CREATE <thing>`, `GRANT`, `ANALYZE`, …)
and the system-function surface dominate.

### What the oracle has found so far

Running it is not bookkeeping — it is how these were found, and none of them
was reachable from the sqlite corpus:

- **Four panics**, all one root cause: `INDEX_EXPR_COL` (`u16::MAX`) marks an
  index key part that is an EXPRESSION, and six places in the planner used it to
  index the column list. `IndexDef::has_expression_part()` now guards every
  place an index is chosen as an access path. The sqlite corpus has no
  expression indexes, so nothing had ever reached those lines.
- **One self-deadlock**: `BEGIN; CREATE TEMPORARY TABLE t(a int); COMMIT` hung
  forever. mpedb's DDL takes its own write transaction, and replaying it inside
  an already-open `WriteSession` waits on a lock the process is holding. The
  wire session now executes DDL immediately and buffers only DML.
- **One measurement instability** (`/dev/shm`, above), which had been quietly
  understating the score.

`crates/mpedb-pg/pg-regress-baseline.tsv` records the per-file counts. Any
movement against it — an improvement included — exits non-zero, the same
discipline `corpus-baseline.tsv` applies to the sqlite corpus.

```sh
# One-time: fetch the corpus (deliberately not vendored) and start a cluster.
curl -LO https://ftp.postgresql.org/pub/source/v16.14/postgresql-16.14.tar.bz2
tar xjf postgresql-16.14.tar.bz2 postgresql-16.14/src/test/regress/sql
export MPEDB_PG_REGRESS=$PWD/postgresql-16.14/src/test/regress

cargo run --release --manifest-path crates/mpedb-pg/Cargo.toml \
  --bin pg_regress_diff -- --all \
  --baseline crates/mpedb-pg/pg-regress-baseline.tsv

# Investigating one file: --show prints BOTH transcripts per statement.
cargo run --manifest-path crates/mpedb-pg/Cargo.toml --bin pg_regress_diff -- \
  --show $MPEDB_PG_REGRESS/sql/int4.sql
```

`--show` is not a debugging leftover. The first three runs of this harness
reported 1 550 divergences across five files; every one was a bug in the
HARNESS, not in mpedb, and the counts alone could not tell the difference. After
the fixes the same five files show 39.

### The protocol

`crates/mpedb-pg/tests/wire.rs` drives a `Session` over a pipe — no socket, no
PostgreSQL needed, milliseconds. Plus the unit tests under `crates/mpedb-pg/src/`
and `crates/mpedb-sql/src/pg/`.

### Not yet measured

The ecosystem suites — psycopg3's own tests, SQLAlchemy's PG dialect suite,
Django's `postgresql` backend — two-armed and diffed by test NAME, the way
`crates/mpedb-capi/workbench/djsuite` does it for sqlite. None of them is
installed on the development box, so that harness has to build its own venv
first.
