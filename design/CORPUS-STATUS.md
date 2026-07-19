# Corpus status — measured sqllogictest compatibility

**Measured 2026-07-19** on `worktree-agent-afa0f61bb39be0f63` @ `7e08d80`
(x86-64 Linux, release build), against **sqlite 3.45.1** as the reference for
the hand-checks.

Corpus: `/home/morten/sqllogictest/test` (the gregrahn/sqllogictest mirror) —
**622 `.test` files, 1.1 GB, 44.9 M lines, 7,420,713 `statement`/`query`
directives**. Runner: `crates/mpedb-testkit/src/bin/sqlite_corpus.rs`, driven
from a file list in 78 chunks of 8 files, each chunk under
`ulimit -v 3000000` + `timeout 580`. Reproduce with:

```
find /home/morten/sqllogictest/test -name '*.test' | sort > flist
# then, per chunk of 8:
( ulimit -v 3000000; timeout 580 target/release/sqlite_corpus --samples-all <files> )
```

## Headline

| metric | value |
|---|---|
| files measured | **621 / 622** (`select5.test` excluded, see §4) |
| records seen | 7,419,202 |
| skipped (`onlyif mysql` / `onlyif mssql`) | 1,480,924 |
| **attempted** | **5,938,278** |
| **passed** | **5,931,446 — 99.885 %** |
| ├ statements | 208,746 |
| └ queries | 5,722,700 |
| queries verified against sqlite's md5 result hash | 954,717 |
| **genuine wrong answers** | **0** (4 flagged, all runner artifacts — §3) |
| **error mismatches** (sqlite errors, mpedb succeeds) | **0** |
| refused / unsupported | 6,828 |
| post-file `Database::verify()` page-accounting failures | 0 |

Refusals split **5,460 engine gaps (0.092 %)** vs **1,368 runner artifacts
(0.023 %)**. With the artifacts discounted, the ceiling of the current engine
on this corpus is **99.908 %**.

## 1. Ranked blockers

Primary attribution (each failing record counted once, most-specific cause
first). `sum = 6,828`.

| # | root cause | records | example statement | verdict |
|---|---|---|---|---|
| 1 | **Mixed `CASE` / `COALESCE` arm types** (`int64` ∪ `float64`) refused at bind time | **4,913** | `SELECT COALESCE(NULL, 1, AVG(2))` → *cannot mix coalesce() argument types: int64 and float64* | **engine gap** (deliberate refusal) — hand-verified |
| 2 | **Shim index accumulation** → mpedb's 32-indexes-per-table cap | **1,289** | `CREATE UNIQUE INDEX idx_tab3_2 ON tab3 (col4,col1 DESC)` → *table `tab3` has 33 indexes (max 32)* | **runner artifact** — hand-verified |
| 3 | **Subquery inside a compound SELECT** (incl. a view flattened into a `UNION` arm) | **520**<br><sub>260 attributed `subquery`, 260 `compound-select`</sub> | `SELECT pk, col0 FROM view_1_tab0_742 UNION ALL SELECT pk, col0 FROM view_2_tab0_742` → *a subquery inside a compound SELECT is not supported yet* | **engine gap** |
| 4 | **`SELECT *` inside an `IN` subquery** sees the shim's synthetic `rowid_` column | **72** | `SELECT 1 IN (SELECT * FROM t1)` → *an IN subquery must select exactly one column* | **runner artifact** — hand-verified |
| 5 | **Legacy `CREATE TRIGGER` without a timing keyword** (sqlite defaults to BEFORE) | **23** | `CREATE TRIGGER t1r1 UPDATE ON t1 BEGIN SELECT 1; END;` → *expected BEFORE, AFTER, or INSTEAD OF* | **engine gap** |
| 6 | `REPLACE INTO` / `INSERT OR REPLACE` arity under the shim's injected `rowid_` | **4** | `REPLACE INTO t1 VALUES(2, 'replace')` → *INSERT row has 2 values, expected 3* | **runner artifact** — hand-verified |
| 7 | `INSERT … SELECT` is not rewritten, so copied `rowid_` values collide with the shim counter | **3** | `INSERT INTO t4n VALUES(null)` → *PRIMARY KEY violation in t4n* | **runner artifact** |
| 8 | `DROP INDEX <name>` / `REINDEX <name>` | **2** | `DROP INDEX t1i1;` → *SQL parse error at byte 5: expected `POLICY`* | **engine gap** (`REINDEX` was fixed on `main` after the measured commit; `DROP INDEX` still refuses) |
| 9 | Bare `GROUP BY` column with **two or more** `min()`/`max()` | **2** | `SELECT DISTINCT col2 + -col2 col0 FROM tab0 … GROUP BY col2 HAVING NULL IN (-MAX(DISTINCT …` | **engine gap** (deliberate — sqlite's pick is order-dependent) |

The three named blockers in COMPAT.md's old intro do **not** appear:
MySQL-only casts (`AS SIGNED`, `AS DECIMAL`) are all behind `onlyif mysql`, so
they are skipped, not failed; `SELECT x IN <table>` is item 4, a shim artifact.

### Why the taxonomy changed

The pre-existing `categories()` buckets are *syntactic* (does the SQL text
contain `CAST(`, `(SELECT`, no `FROM`, …). The `random/` corpus is
machine-generated expression soup, so nearly every failing statement matches
several of them at once and the largest real blocker was being reported as
`cast=3470` + `select-without-from=1444`. Three error-message-derived
categories were added to this runner — `mixed-arm-types`,
`shim-index-accumulation`, `shim-star-arity` — and inserted ahead of the
syntactic ones. Totals are unchanged: the full corpus was re-swept with the
new taxonomy and produced byte-identical pass/wrong/errmis numbers.

### Detail on the two big ones

**#1 mixed arm types.** `avg()`, `/` on floats and float literals produce
`float64`; `count()`, integer literals produce `int64`. The generator mixes
them freely inside one `COALESCE`/`CASE`, e.g. `COALESCE(-60, 50/84.0)`.
sqlite types the result per row and answers `-60`; mpedb refuses at bind time.
This is a documented deliberate deviation, not a bug — but it is 72 % of all
corpus failures, so it is the single highest-leverage feature.

**#2 index accumulation.** mpedb's `DROP TABLE` tombstones a table id and caps
lifetime creates at 64; the corpus re-creates its tables hundreds of times per
file, so the runner simulates `DROP TABLE` as `DELETE FROM`. Indexes are not
deleted by that, and mpedb has no `DROP INDEX`, so each block's `CREATE INDEX`
piles onto the same live table until the 33rd trips the cap. sqlite drops the
indexes with the table and never holds more than a handful. Verified by hand:
replaying the 61 `CREATE INDEX … ON tab3` statements of
`index/delete/100/slt_good_0.test` against a real mpedb table fails at
**exactly** #61 with `table 'tab3' has 33 indexes (max 32)`, and 200
sequential `CREATE INDEX` on one table otherwise succeed.

## 2. Hand-verified categories

Top 3 by record count were re-run through `mpedb` and `sqlite3` directly, out
of the runner:

1. **mixed-arm-types → real.** `sqlite3 :memory: "SELECT COALESCE(NULL,1,2.5)"`
   → `1`; mpedb → `bind error: cannot mix coalesce() argument types`. Same for
   `SELECT CASE WHEN 1=1 THEN 1 ELSE 2.5 END` and `COALESCE(-60, 50/84.0)`.
   (`NULLIF(1, 2.5)` is *accepted* by both — the refusal is arm-type mixing in
   `COALESCE`/`CASE`, not every polymorphic function.)
2. **shim-index-accumulation → artifact** (mechanism above). The engine's
   32-index cap is real, but nothing in the corpus reaches it under sqlite's
   `DROP TABLE` semantics.
3. **shim-star-arity → artifact.** Against a real one-column table,
   `SELECT 1 IN (SELECT * FROM t1)`, `SELECT 9 NOT IN (SELECT * FROM t1)` and
   `SELECT null IN (SELECT * FROM t1)` return `true` / `true` / `NULL` — byte
   -equal to sqlite's `1` / `1` / *(null)*. The corpus failure is only the
   shim's synthetic `rowid_` making the subquery two columns wide.

Also re-checked by hand: `INSERT OR REPLACE INTO t1 VALUES(2,'x')` and
`REPLACE INTO t1 VALUES(2,'x')` both replace correctly on a table with its
**declared** `INTEGER PRIMARY KEY` — item 6 is entirely the shim.

## 3. The 4 flagged "wrong" results

All four are in `evidence/slt_lang_replace.test` and all four carry the
runner's own cascade note (a preceding expected-ok statement had already
failed). They are downstream of item 6: the shim's `rowid_` turns `REPLACE`
into either an arity error or a plain insert, so the following `SELECT` sees
the un-replaced row. **Genuine wrong answers: 0.**

## 4. `select5.test` — not measurable inside the budget

`select5.test` (1,436 records, the N-way comma-join battery) **aborts on an
allocation failure** under the 3 GB virtual-memory cap. Isolated by
record-aligned prefix probing:

- every record through the `join-16` block (line 5164) passes **100 %**
  (860/860) — comma joins up to 16 tables are fine;
- `join-17-1` … `join-17-3` also pass;
- the **`join-17-4`** query (line ~5418, `FROM t9,t56,t53,…` — 17 tables whose
  only constant anchor `a38=9` is the 16th of 17 conjuncts) blows past 3 GB.

Earlier `join-17` queries with the constant anchor early in the WHERE list are
fine, so this is join *ordering*, not arity: the left-deep planner is not
reordering to put the selective table first, and the runaway is an OOM abort
rather than a clean error. Two separate items: a cost/ordering heuristic, and
a runtime budget (like #74's recursive-CTE work counter) covering join
execution. `select4.test` is a milder instance of the same — it passes 100 %
but takes 158 s.

## 5. Supplementary: `evidence/` in `--as-sqlite` mode

The 12 `evidence/` files are language-feature (not volume) tests. In default
mode most of them stop early on a `skipif sqlite` + `halt`; with
`--as-sqlite` the whole file runs. **280 / 489 = 57.3 %**, 0 error mismatches,
10 flagged wrong (all cascade-noted, downstream of the `1<<63` insert below).
Feature gaps this surfaces that the volume corpus never reaches:

| gap | example |
|---|---|
| `<<` / `>>` bit-shift operators | `INSERT INTO t1 VALUES(1<<63,'true')` → *expected an expression* |
| postfix `expr NOT NULL` (= `IS NOT NULL`) | `SELECT x FROM t1 WHERE x NOT NULL ORDER BY x` |
| `NOT INDEXED` table hint | `SELECT group_concat(x) FROM t1 NOT INDEXED` |
| 2-argument `group_concat(x, sep)` | `SELECT group_concat(x,':') FROM t1` |
| `CREATE TEMP` / `TEMPORARY VIEW` | `CREATE TEMP VIEW view2 AS SELECT x FROM t1 WHERE x>0` |
| `x IN <table>` (table as set) | `SELECT 1 IN t1` |
| several statements in one record | `INSERT INTO t1 VALUES(-1,'true'); DROP INDEX t1i1;` |

`avg()`/`sum()`/`total()` over a text column are refused on purpose (rigid
types) and are not counted as gaps.

## 6. Next 5 features by corpus impact

| # | feature | records unblocked | note |
|---|---|---|---|
| 1 | **Per-row typing of `CASE` / `COALESCE` arms** (accept an `int64` ∪ `float64` arm mix, widen to float — or type per row as sqlite does) | **4,913** | 72 % of every corpus failure. Alone this takes the sweep from 99.885 % to ≈ 99.968 % |
| 2 | **Subquery inside a compound SELECT arm** (and therefore views under `UNION`) | **520** | all of `index/view/*`'s remaining failures |
| 3 | **Legacy `CREATE TRIGGER <n> {INSERT\|UPDATE\|DELETE} ON t`** (no timing keyword ⇒ BEFORE) | **23** | unblocks `slt_lang_createtrigger` / `droptrigger` end-to-end |
| 4 | **`DROP INDEX [IF EXISTS] <name>`** | 2 direct | tiny by record count, but it is a *parse* error today, and it is what forces the runner's shim into item 2 — fixing it would also let the runner drop indexes with the table and retire 1,289 artifact failures |
| 5 | **Join ordering + a runtime budget on join execution** | 1,436 (`select5.test`, currently unmeasurable) | today a 17-way comma join with a late constant anchor aborts the process on allocation failure instead of erroring |

Below the top 5, in corpus order: bit-shift operators, postfix `NOT NULL`,
2-arg `group_concat`, `CREATE TEMP VIEW`, `x IN <table>`, and the two-`min`/`max`
bare-`GROUP BY` case (a deliberate refusal — leave it).

## 7. Runner changes made for this measurement

`crates/mpedb-testkit/src/bin/sqlite_corpus.rs` only:

- `--samples-all` — keep up to 3 example failing statements **per category**
  (previously only the `other` bucket was sampled, so every ranked category
  was un-exampled). The report section is renamed *failing-statement samples*.
- Three error-message categories inserted ahead of the syntactic buckets:
  `mixed-arm-types`, `shim-index-accumulation`, `shim-star-arity`. Only
  attribution changes — the re-sweep reproduced the pass/wrong/errmis numbers
  exactly.

No engine change was made. Two runner fixes are *not* done here because they
need engine support or a larger shim rewrite, and are recorded instead:
expanding `SELECT *` inside subqueries (item 4), and rewriting
`INSERT … SELECT` / `REPLACE INTO` for the synthetic PK (items 6–7).
