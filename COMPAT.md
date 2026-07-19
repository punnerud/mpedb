# mpedb SQLite Compatibility

Feature-by-feature status of mpedb's SQL surface against SQLite, in the same
format as [Turso's COMPAT.md](https://github.com/tursodatabase/turso/blob/main/COMPAT.md)
so the two can be read side by side (the measured three-way comparison lives in
[design/TURSO.md](design/TURSO.md)). Legend: ✅ yes · 🚧 partial · ❌ no · **Not needed** =
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
| INSERT INTO … VALUES | ✅ | multi-row VALUES; explicit or implicit column list. A single-column **INTEGER PRIMARY KEY is a rowid alias** (sqlite): a NULL or omitted id auto-assigns `max(rowid)+1` (1 on an empty table) — the plain, non-AUTOINCREMENT rule, so a deleted top id can be reused (max of *current* rows, unlike AUTOINCREMENT); an explicit id inserts at that id, and a duplicate is a uniqueness error. A composite or non-integer PK is **not** a rowid alias — a NULL there stays a strict NOT-NULL error (mpedb is stricter than sqlite's historical NULL-in-PK leniency; a clean refusal, never a wrong answer). Inferred from the table shape, so no schema-format flag. `last_insert_rowid()` is not implemented (refused as an unknown function) — use `RETURNING id` to read the assigned rowid |
| INSERT … ON CONFLICT DO NOTHING | ✅ | |
| INSERT … ON CONFLICT (target) DO UPDATE SET … [WHERE …] | ✅ | target may be the PK or one UNIQUE column; `excluded.<col>` works |
| INSERT OR IGNORE / ABORT / FAIL / ROLLBACK | ✅ | IGNORE = DO NOTHING; ABORT/FAIL/ROLLBACK = error (the default) |
| INSERT OR REPLACE | ✅ | on a PK conflict, replaces the row (desugars to `ON CONFLICT (pk) DO UPDATE SET …=excluded`). Refused on a table with a secondary UNIQUE index, where sqlite's delete-on-any-constraint semantics differ from a PK upsert |
| INSERT INTO … SELECT | ✅ | `INSERT INTO t [(cols)] SELECT …`; the source is read fully first (self-insert safe), its tuple fills the listed columns, omitted columns default. Compound (UNION) source not yet supported |
| UPDATE … SET … [WHERE …] | ✅ | a column assigned more than once keeps the rightmost occurrence and ignores the rest (not evaluated), matching sqlite (R-34751-18293) |
| DELETE FROM … [WHERE …] | ✅ | |
| RETURNING (all three verbs) | ✅ | `RETURNING *` or an expression list |
| BEGIN / COMMIT / ROLLBACK | ✅ | maps to a write session; readers use MVCC snapshots and never block |
| SAVEPOINT / RELEASE / ROLLBACK TO | ✅ | full nested stack inside a `WriteSession` (an explicit transaction): `SAVEPOINT <name>`, `RELEASE [SAVEPOINT] <name>`, `ROLLBACK [TRANSACTION] TO [SAVEPOINT] <name>`. Names are matched case-insensitively and shadow (innermost wins), `ROLLBACK TO` keeps the savepoint (repeatable) while `RELEASE` merges it up, and an unknown name is sqlite's `no such savepoint: <name>`. Built on the engine's COW snapshot (`WriteTxn::savepoint_full`/`rollback_to_full`), differential-tested vs sqlite 3.45. **Deviations, each a clean error not a wrong answer:** there is no autocommit implicit transaction, so a bare savepoint through `Database::query` (outside `begin()`) is refused — sqlite would open one implicitly; `ROLLBACK TO` across a large blob/overflow-**extent** write is refused (that out-of-tree allocator state is not snapshotted; inline values and in-tree overflow chains ARE reverted); and a *partially-applied* (torn multi-row) statement poisons the whole session, which `ROLLBACK TO` cannot clear (use full `ROLLBACK`) — ordinary pre-mutation constraint failures do not poison, so `ROLLBACK TO` recovers from them exactly as sqlite does |
| EXPLAIN | ✅ | plan form (access path, index choice, residuals), not VDBE opcodes |
| CREATE TABLE | ✅ | live, multi-process, on the shared file — `PRIMARY KEY` (inline or table-level composite), `NOT NULL`, `UNIQUE` (column or composite); other processes see the new table on their next statement. A single-column `INTEGER PRIMARY KEY` becomes a rowid alias (see INSERT). `DEFAULT`/`CHECK`/foreign keys refuse by name (declare them in the config schema for now). `AUTOINCREMENT` refuses by name: a plain single-column `INTEGER PRIMARY KEY` already auto-assigns, and mpedb keeps no persisted high-water counter to promise AUTOINCREMENT's never-reuse guarantee, so it will not silently downgrade it to the reuse-allowed behavior |
| DROP TABLE | ✅ | live, multi-process — `DROP TABLE [IF EXISTS] <name>`; frees the table's pages, tombstones its id in place (never reused — [design/DESIGN-DROP-TABLE.md](design/DESIGN-DROP-TABLE.md) §0), other processes see it gone on their next statement. No-reuse caps *lifetime* table creates at 64 (a bounded capacity limit, not a per-query gap; offline `regenerate` re-densifies) |
| ALTER TABLE RENAME | ✅ | `RENAME TO` (table) and `RENAME [COLUMN] a TO b` — pure metadata, no data rewrite; sqlite/PG-equivalent refusals (name collision, unknown target) |
| ALTER TABLE ADD COLUMN | ✅ | live + multi-process. A nullable column rewrites existing rows with NULL; `DEFAULT <const>` (and `NOT NULL DEFAULT <const>`) rewrites them with the constant and persists it for later INSERTs — differential-tested vs sqlite 3.45. The default must be a literal constant (a signed number, string, blob, boolean, or NULL); a non-constant default (`(1+2)`, a function, `CURRENT_*`) is refused, matching sqlite. Still refused, matching sqlite: `NOT NULL` *without* a non-NULL default, and `UNIQUE`/`PRIMARY KEY` on ADD (no online index build). A type-mismatched default is a clean error (rigid schema; sqlite's loose typing would store it) |
| ALTER TABLE DROP COLUMN | ✅ | live + multi-process (existing rows rewritten without the column; surviving index/PK column references renumbered). Refuses dropping a PK / indexed / last column, matching sqlite |
| CREATE INDEX | ✅ | `CREATE [UNIQUE] INDEX [IF NOT EXISTS] n ON t (cols)` — built over existing rows, live + multi-process; ASC/DESC per column accepted (indexes are ascending, used for equality/prefix/range lookups). Or declare via config `unique`/`indexed`/`[[table.index]]`. The index name is not persisted (indexes are positional) |
| CREATE VIEW / DROP VIEW | ✅ | a query naming the view is flattened onto its base table (WHERE merged; `SELECT *` yields the view's columns; view-over-view chains). Simple projection/filter views over one table; aggregate/join/DISTINCT view bodies are refused at reference time (never answered wrongly) — [design/DESIGN-VIEW.md](design/DESIGN-VIEW.md) |
| CREATE TRIGGER / DROP TRIGGER | 🚧 | `{BEFORE\|AFTER} {INSERT\|UPDATE [OF cols]\|DELETE} … FOR EACH ROW [WHEN …] BEGIN <one-or-more INSERT/UPDATE/DELETE> END` fires (body runs on the same txn, depth-capped; `NEW.<col>` for INSERT, `NEW`+`OLD` for UPDATE, `OLD` for DELETE; multi-statement bodies run in order; `UPDATE OF` fires on SET-target membership); stored as a sys-keyspace catalog record (no plan-format change). NEW is read-only (no `RAISE`-veto yet), and INSTEAD OF / FOR EACH STATEMENT / PySpell `EXECUTE PROCEDURE` bodies / subqueries in the body are refused by name — [design/DESIGN-TRIGGERS.md](design/DESIGN-TRIGGERS.md) |
| WITH (CTEs) | 🚧 | a non-recursive `WITH c AS (SELECT …) …` is a statement-scoped named source — flattened onto its base table at bind time via the derived-table keep-alias splice, so unqualified refs, qualified `c.col`, `FROM c AS x` (`x.col`), `SELECT *`, and joining a CTE work — the CTE may sit in the main `FROM` **or** an INNER/LEFT **JOIN operand** (`… JOIN c ON …`, its WHERE folded into the ON), and a CTE body may reference an **earlier** CTE (multi-CTE backward chains resolve). Same simple projection/filter bodies as views. **`WITH RECURSIVE t(cols) AS (<anchor> UNION\|UNION ALL <recursive>) <outer>`** is now supported (format 26, `PlanStmt::RecursiveCte`): a semi-naive breadth-first (FIFO) fixpoint — the anchor seeds a result set + queue, the recursive term (which references `t` exactly once, in a FROM/JOIN operand) sees only the previous step's new rows, `UNION` dedups on the whole tuple while `UNION ALL` keeps every row, output is in insertion order, and an outer `LIMIT`/`OFFSET` bounds the iteration (the idiom that makes an infinite generator finite). The #74 work counter is the deterministic termination backstop — an unbounded `UNION ALL` recursion aborts with `RuntimeBudget { which: "recursive CTE \"…\"" }` at a fixed, repeatable count. Differential-tested vs sqlite 3.45 (counting generator, Fibonacci/number sequences, tree/graph transitive closure, `UNION` dedup vs `UNION ALL` multiplicity, insertion-order output). Refused (never answered wrongly): a recursive term that references `t` more than once / in a subquery / on the null-extended side of an outer join, or uses aggregate/GROUP BY/DISTINCT/window; mutual or multi-CTE recursion; `ORDER BY`/`LIMIT` inside the CTE body (depth-first ordering is stage 2); and parameters/subqueries in the CTE components. Non-recursive CTEs still refuse (never answered wrongly): explicit column-lists `WITH c(x,y)`, aggregate/join/DISTINCT bodies, a CTE on the preserved side of a RIGHT/FULL JOIN or under a `SELECT *` join that would expose hidden columns, and self/forward/cyclic CTE references (stricter than sqlite, which accepts a non-cyclic forward ref) — [design/DESIGN-CTE.md](design/DESIGN-CTE.md), [design/DESIGN-CTE-RECURSIVE.md](design/DESIGN-CTE-RECURSIVE.md) |
| VALUES (standalone) | ✅ | top-level `VALUES (a,b),(c,d),…` — the listed tuples become the result rows in order; columns named `column1..columnN` (sqlite's names). Desugared at parse time into the equivalent compound `SELECT … UNION ALL SELECT …` of FROM-less SELECTs, so no new plan format. All tuples must have equal arity (ragged is refused). VALUES as a subquery/derived-table source (`FROM (VALUES …)`) is not yet supported — a multi-row VALUES is a compound, which a derived-table body cannot hold — and is refused, never mis-answered |
| PRAGMA | **Not needed** | everything a PRAGMA would set lives in the config file, per database, versioned |
| VACUUM | **Not needed** | COW pages + commit-time freelist fixpoint reclaim space continuously |
| ATTACH / DETACH | ❌ | cross-file read-joins over workspace members are planned (#51) |
| ANALYZE | **Not needed** | accepted as a no-op success (`ANALYZE` / `ANALYZE <name>`): the planner is rule-based (PK > unique > non-unique index > scan) and keeps no statistics, so there is nothing to gather — sqlite-equivalent success so tools/migrations emitting it don't break. The optional target is not required to exist (leniency is never a wrong answer) |
| REINDEX | **Not needed** | accepted as a no-op success (`REINDEX` / `REINDEX <name>`): mpedb maintains every index eagerly on each write, so there is never a stale index to rebuild. A table vs index name is indistinguishable here (indexes are positional, names not persisted), so any target is accepted leniently |

## SELECT clauses

| Feature | Status | Comment |
|---|---|---|
| FROM-less `SELECT 3+5` | ✅ | one synthetic row (sqlite/PG semantics); WHERE filters it, aggregates see it, compound arms and subqueries may each be FROM-less |
| WHERE | ✅ | full SQL three-valued logic, verified against sqlite 3.45 |
| GROUP BY | ✅ | columns, expressions (`GROUP BY a/100`), output ordinals (`GROUP BY 1`). **Bare (non-aggregated, non-grouped) columns are config-selectable** (`[compat] bare_group_by`, PLAN_FORMAT 30): `"sqlite"` (**the default**) accepts a bare column, `"postgres"` refuses it (`… must appear in GROUP BY …`) — and a `mirror import` from PostgreSQL is born `"postgres"`, so the strictness travels with the data's origin. In sqlite mode a bare column is accepted ONLY where mpedb reproduces sqlite's value EXACTLY (never a guessed value — mpedb's core guarantee); the pick rule is inferred at execution from the aggregate set, needing no plan byte: **(1) never evaluated** — a `COALESCE(-24, col)` (or dead-`CASE`-branch) column that constant folding removes, so the result is the constant regardless of the row; **(2) min/max-determined** — a query with EXACTLY ONE `min()`/`max()` (other aggregates such as `count`/`sum`/`avg` may sit alongside it), where every bare column takes its value from the extremum's input row (a tie → the first such row; an all-NULL-argument group → that group's last row; an empty table → NULL — the exact sqlite 3.45 rule, differential-tested), holding over a join too; **(3) lowest-rowid (#88)** — a query with NO `min()`/`max()` (a `count`/`sum`/`avg` aggregate, or no aggregate at all): sqlite's "arbitrary" pick is really the group's **lowest-rowid row**, which mpedb reproduces by carrying each group's **minimum-PK row** — matching sqlite EXACTLY over a single INTEGER-PK table (where the PK *is* the rowid), even when the access path is not rowid-ordered (an index or descending-range scan) and even for out-of-rowid-order inserts, and the pick is taken over the WHERE-surviving rows just as sqlite's is. A bare column is **still REFUSED** (`… must appear in GROUP BY, be inside an aggregate, or be determined by a single min()/max()`) only where mpedb cannot reproduce sqlite's exact value: the lowest-rowid case **over a join** (the `[outer ‖ inner]` row has no single rowid) or **over a non-rowid (text/composite) primary key** (mpedb's min-PK is not sqlite's implicit rowid); and **two-or-more `min()`/`max()`** (bare + BOTH min and max, or two mins), where sqlite fills the bare column from the LAST min/max in the query — an order-dependent pick its own docs call "arbitrary". postgres mode refuses every bare column, matching PostgreSQL (verified vs PG 16) |
| HAVING | ✅ | subqueries inside HAVING are refused |
| ORDER BY | ✅ | by name, ordinal, or selected expression; hidden sort columns added when needed; under DISTINCT the key must be in the SELECT list (PostgreSQL's rule) |
| LIMIT / OFFSET | ✅ | Top-K heap under ORDER BY + LIMIT |
| DISTINCT | ✅ | also `count(DISTINCT x)` |
| SELECT-item aliases | ✅ | `expr AS name` and bare `expr name`; `ORDER BY alias` resolves the output first |
| `t.*` / `*` | ✅ | |
| INNER JOIN (N-way chains) | ✅ | left-deep, up to 16 tables; equality in ON becomes an index nested loop (PK > full-width unique > longest prefix, composite included), the rest stays residual — EXPLAIN shows which |
| LEFT [OUTER] JOIN | ✅ | NULL-extends; `WHERE inner IS NULL` anti-joins work |
| RIGHT [OUTER] JOIN | 🚧 | two-table form, AND a RIGHT as the **FIRST** join of a longer chain — `A RIGHT JOIN B ON … [INNER\|LEFT JOIN C …]` — both work: the leading RIGHT is rewritten to the equivalent left-deep `B LEFT JOIN A` (same row set), and `SELECT *` keeps the original a.\*, b.\*, … column order. Differential-tested vs sqlite 3.45 (3- and 4-table chains mixing RIGHT/INNER/LEFT, the outer-join null cases, `WHERE inner IS NULL` anti-joins, aggregate, DISTINCT). Refused (never answered wrong, with the manual LEFT-JOIN fix in the message): a RIGHT that is **not** the first join (it would need the accumulated left side as a join SUBTREE on the preserved side, which a left-deep plan cannot express), a **second** RIGHT in the chain, and a USING/NATURAL join that TRAILS a leading RIGHT (the side-swap would shift which side is the coalesce representative — sqlite rejects that as ambiguous too) |
| FULL [OUTER] JOIN | 🚧 | two-table form only. FULL inside any multi-join chain stays refused (never answered wrong, message says the fix): it needs both sides whole, and — unlike a leading RIGHT — no side-swap makes it left-deep, so a chain would require a plan-carried FULL-in-chain join kind the left-deep executor does not yet have |
| CROSS JOIN / comma-joins | ✅ | desugared to `INNER JOIN … ON true`; WHERE conjuncts push into the chain, so a comma-join equality is an index-nested-loop candidate exactly like an ON equality |
| JOIN … USING (c, …) | 🚧 | [INNER] JOIN and LEFT JOIN only; desugars to `left.c = right.c AND …` at plan time, so the ON-equality is an index-nested-loop candidate like a written ON; under `SELECT *` the join column is COALESCED (appears once, from the left side) — sqlite-verified. RIGHT/FULL USING refused (the coalesced column would have to survive the side-swap/both-sides-whole rewrites); a USING column missing on either side is a clean bind error |
| NATURAL [INNER/LEFT] JOIN | 🚧 | desugars to `JOIN … USING (…)` over the columns common to the two sides, resolved at PLAN time from the schema (a rigid schema makes the common set STATIC — the plan is content-hashed against it, so the match cannot silently drift as it can in a schemaless engine); everything then flows through the USING path (ON-equalities + `SELECT *` coalesce). No common column → a cross join (`ON true`); a column common to two already-joined left tables equates the leftmost — both sqlite-verified. NATURAL RIGHT/FULL/CROSS refused (the coalesced column cannot survive the side-swap/both-sides-whole rewrites, same as USING) |
| Table aliases, self-joins | ✅ | alias shadows the table name, as in PostgreSQL |
| UNION / UNION ALL / EXCEPT / INTERSECT | ✅ | chains, left-associative at equal precedence (sqlite's rule; PostgreSQL binds INTERSECT tighter — documented deviation); arms must agree on arity and exact types, `CAST` bridges deliberate mismatches |
| Scalar subqueries `(SELECT …)` | ✅ | uncorrelated and correlated; 0 rows → NULL; **>1 row is an error** (PostgreSQL's rule — sqlite silently takes the first row). The body may be a whole compound `(SELECT … UNION/UNION ALL/INTERSECT/EXCEPT … LIMIT 1)` (uncorrelated only — format 31) |
| [NOT] EXISTS (…) | ✅ | uncorrelated and correlated; the body may be a compound `EXISTS (SELECT … UNION/… …)` (uncorrelated — format 31) |
| `x IN (SELECT …)` | ✅ | uncorrelated; the subquery becomes a LIST subplan over the same runtime-typed 3VL membership core as IN lists (empty → FALSE, NULL member without a match → NULL — sqlite-verified); correlated IN is refused with the EXISTS rewrite named. The body may be a whole compound `x IN (SELECT … UNION/UNION ALL/INTERSECT/EXCEPT …)` (uncorrelated — format 31), differential-tested vs sqlite 3.45 (correct rows, dedup, 3VL) |
| Subqueries in FROM (derived tables) | 🚧 | a simple projection/filter body `FROM (SELECT … FROM t [WHERE …]) d` is flattened onto its base at bind time (WHERE merged; the derived alias `d` is kept, so `d.col` refs resolve; joins over the derived table work). Aggregate/join/DISTINCT/LIMIT/renamed-projection bodies are refused (never answered wrongly) — Stage A ([design/DESIGN-DERIVED-TABLES.md](design/DESIGN-DERIVED-TABLES.md)). A **compound** body `FROM (SELECT … UNION …) d` is likewise refused by name: a derived table is flattened onto its base, which a compound cannot be, so it needs a materialized FROM-source of its own — a follow-up (the compound-body support at format 31 covers the LIFT positions above, not FROM) |
| Nested subqueries (subquery in a subquery) | ✅ | uncorrelated nested (`x IN (SELECT … WHERE y IN (SELECT …))`, `EXISTS(… EXISTS(…))`, scalar-in-scalar, arbitrary depth), a nested subquery correlated to its IMMEDIATE parent, AND correlation to a MIDDLE or the OUTERMOST scope (the innermost skips the level(s) in between) all work, cross-checked vs sqlite (#73 §3 stages 1–3). Mid-scope correlation is threaded as a TRANSIT arg on the ancestor's direct child (no plan-format change). The only remaining refusal is a correlated `IN (SELECT …)` that would itself have to carry a transit — refused with a message (rewrite as EXISTS), never a wrong answer |
| Window functions | 🚧 | `<fn>(…) OVER ([PARTITION BY …] [ORDER BY … [ASC\|DESC]])` (design/DESIGN-WINDOW.md stages 1–2, PLAN_FORMAT 34). **Ranking**: `row_number()` (distinct 1..n, ties broken by scan/PK order), `rank()` (ties share, next skips: 1,1,3), `dense_rank()` (ties share, no gap: 1,1,2). **Aggregate OVER**: `sum`/`count(*)`/`count(x)`/`avg`/`min`/`max`/`total` with the DEFAULT frame only — with ORDER BY it is cumulative `RANGE UNBOUNDED PRECEDING → CURRENT ROW` (peers share one value: the running total through the end of their peer group, the RANGE-vs-ROWS distinction), without ORDER BY it is the whole partition. **Value/offset (stage 2)**: `lag(expr[, offset[, default]])` / `lead(expr[, offset[, default]])` (frame-independent physical-row offset in window order; out of range ⇒ the `default`, evaluated at the current row, or NULL; the offset is a CONSTANT integer, default 1, and a negative/zero offset is honoured exactly as sqlite computes `p∓offset`), `first_value(expr)` (the partition's first row), `last_value(expr)` (the current row's PEER-GROUP end under the default RANGE frame — tied rows share it — or the partition end with no ORDER BY, matching sqlite), `nth_value(expr, n)` (the fixed n-th row of the frame, NULL until the growing frame reaches it; `n` is a CONSTANT ≥ 1). PARTITION BY groups NULLs together; window ORDER BY is NULLS FIRST for ASC (sqlite's default); the reused `Accum` keeps the aggregate NULL/overflow rules (integer `sum` overflow and `sum`/`avg` of text ERROR, as elsewhere). Multiple distinct windows per SELECT, windows in the SELECT list and the outer ORDER BY (junk-column path), and windows over a join all work. Computed AFTER WHERE, BEFORE the outer ORDER BY/LIMIT — cross-checked vs sqlite 3.45. **Refused (each a clean error, never a wrong answer)**: explicit frames (`ROWS`/`RANGE`/`GROUPS BETWEEN …`), `ntile`/`percent_rank`/`cume_dist`, named `WINDOW w AS …`, `FILTER`, `DISTINCT` inside a window aggregate (sqlite refuses it too), a **non-constant or non-integer** lag/lead offset / nth_value `n` (sqlite's per-row coercion — a non-integer float yields all-NULL, non-numeric text yields 0 — is not reproducible, so it is refused rather than guessed) and a lag/lead `default` whose type differs from the value's, a window together with GROUP BY / an aggregate in the same SELECT, a window together with a correlated subquery, and a window anywhere but the SELECT list / ORDER BY (WHERE, HAVING, GROUP BY, a JOIN condition, an aggregate's argument, or nested inside another window) |

## Expressions and operators

| Feature | Status | Comment |
|---|---|---|
| `+ - * / %`, unary `+`/`-` | ✅ | **division / modulo by zero yields NULL**, matching sqlite (integer and real, literal or row value); **integer overflow errors** (sqlite promotes to REAL) — the overflow deviation is deliberate |
| `= != < <= > >=` | ✅ | |
| AND / OR / NOT | ✅ | SQL 3VL throughout |
| `\|\|` concatenation | ✅ | NULL propagates; ints/bools render as text; floats refused until their formatting is pinned |
| LIKE | ✅ | no ESCAPE clause |
| GLOB / NOT GLOB | ✅ | sqlite semantics: case-SENSITIVE, `*` (any run) and `?` (one char) wildcards, `[...]` character classes (incl. `[^...]` and ranges); pattern must be a literal, as with LIKE |
| REGEXP / NOT REGEXP | ✅ | sqlite's bundled `ext/misc/regexp.c` dialect: case-SENSITIVE, unanchored substring match with `.` (any char, incl. newline), `*` `+` `?`, counts `{p}`/`{p,}`/`{p,q}`, classes `[...]`/`[^...]` with ranges, anchors `^`/`$`, `\|` alternation, `(...)` grouping, the Perl escapes `\d \D \w \W \s \S`, word-boundary `\b`, the C escapes `\a \f \n \r \t \v`, `\uXXXX`/`\xXX` and `\`-before-a-metacharacter; pattern must be a literal, as with LIKE/GLOB. Hand-rolled Thompson NFA (no backtracking, no regex crate). Deviation: a MALFORMED pattern (one sqlite rejects with a runtime error — unmatched `(`/`{`, unterminated `[`, unknown escape, `{m,n}` with n<m or both zero, a quantifier with no operand) matches NOTHING here (`REGEXP` → FALSE, `NOT REGEXP` → TRUE) rather than raising — mpedb never errors on a REGEXP pattern |
| MATCH | 🚧 (stage 1, #76) | Native FTS5 (design/DESIGN-FTS.md stage 1): `CREATE VIRTUAL TABLE ft USING fts5(cols [, tokenize='unicode61'\|'ascii'])` builds a `TableKind::Fts` content table + an inverted index on the COW B+tree (→ MVCC + crash-safety for free), maintained transactionally on every INSERT/UPDATE/DELETE (a NULL column contributes no postings). `<col-or-table> MATCH 'query'` compiles to an `FtsScan`; supported query grammar: bare terms, `AND`/`OR`/`NOT` + implicit-AND juxtaposition, parentheses, prefix `term*`, column filter `col:term` and `{a b}:term`, initial-token `^term`; whole-row (`ft MATCH …`) and column-scoped (`col MATCH …`); results in **rowid order** (no ranking yet). `unicode61` (default; casefold + common-Latin diacritic fold) and `ascii` tokenizers. `MATCH` on a non-FTS column/scalar errors **identically to sqlite** — *unable to use function MATCH in the requested context* (mpedb rejects at bind, on any table; sqlite at step, only once a row is processed). **Deviations (clean error, never a wrong answer):** stage 1 requires an explicit `rowid` on insert (auto-rowid is stage 1b); `MATCH` must be a top-level `AND` conjunct against the single FROM table — `MATCH` inside `OR`, a second `MATCH`, or `MATCH` with a join is refused; `SELECT *` on an fts table returns `rowid` first (sqlite hides it). **Deferred, refused by name:** phrases `"a b"`, `NEAR`, `rank`/`bm25()` (stage 2); `highlight`/`snippet`/`offsets`, `porter`/`trigram` tokenizers, contentless/external-content, `fts5vocab`, the `INSERT INTO ft(ft) VALUES('rebuild'…)` maintenance verbs (stage 3) |
| BETWEEN / NOT BETWEEN | ✅ | |
| IN / NOT IN (value list) | ✅ | also `x IN (SELECT …)` and the sqlite shorthand `x IN <table>` (single-column). The empty set `x IN ()` is accepted (sqlite allows it) and is FALSE for every probe, `NOT IN ()` TRUE; `NULL IN (empty)` is FALSE (3VL), matching sqlite |
| IS NULL / IS NOT NULL | ✅ | |
| `x IS y` (general distinct-from) | ✅ | NULL-safe equality (`IS`) / inequality (`IS NOT`): both-NULL is TRUE, one-NULL is FALSE, else `=`. Two-valued — never NULL, unlike `=` |
| CASE (searched and simple) | ✅ | simple form desugars to searched; arms mixing int64 and float64 are refused — sqlite types the winning arm per row, rigid typing cannot, and widening was measured to change division results (add a CAST) |
| CAST(x AS type) | ✅ | sqlite's **permissive, affinity-based** casting, differential-tested against sqlite 3.45. ANY type name is accepted (`SIGNED`, `DECIMAL`, `VARCHAR(10)`, `DOUBLE PRECISION`, …) and folded by sqlite's substring rule to one of five affinities: name contains `INT`→INTEGER, else `CHAR`/`CLOB`/`TEXT`→TEXT, else `BLOB`→BLOB, else `REAL`/`FLOA`/`DOUB`→REAL, else→NUMERIC. Conversions match sqlite exactly: NULL→NULL; real→int truncates toward zero (saturating, so NaN→0, ±inf→i64 bounds); text/blob→int parse a leading *integer* prefix (`'12ab'`→12, `'1e3'`→1, `'abc'`→0); text/blob→real parse a leading *float* prefix (`'1e3'`→1000.0); int/real/bool→text render as sqlite text (real via `%!.15g`, e.g. `2.9`→`'2.9'`, `1e20`→`'1.0e+20'`); →blob is the value's text bytes (`90`→`x'3930'`); NUMERIC keeps an already-typed int/real (a real stays real even when integral) but makes text/blob an int when the string is a pure `i64` or the value is integral with `|v| < 2^51`, else a real. **Deviations (clean errors, never a wrong answer):** a non-UTF-8 BLOB cast to TEXT is refused (mpedb `Text` is a Rust `String`; sqlite keeps raw bytes); an empty type name (`CAST(x AS "")`) is a parse error, so sqlite's empty→NUMERIC quirk is not expressible; and where a NUMERIC-affinity cast of a text/blob column yields a per-value int-or-real (`Any`), mixing it with a concretely-typed operand in arithmetic/comparison/UNION is refused by rigid typing rather than silently coerced |
| COLLATE | 🚧 | the three sqlite built-in collating sequences — **BINARY** (default; memcmp of the UTF-8 bytes), **NOCASE** (case-insensitive for ASCII `A–Z` ONLY — Unicode is NOT casefolded, exactly like sqlite), **RTRIM** (ignore trailing spaces) — as an explicit postfix `COLLATE` operator. Honored on a **comparison operand** (`= <> < <= > >=`, `IN (list)`, `BETWEEN`) with sqlite's precedence (explicit `COLLATE` on the left operand wins, else the right's, else BINARY), and on an **`ORDER BY <expr> COLLATE <coll>`** term (collated sort). Collation applies to TEXT comparison only; numeric/blob comparisons are unaffected. Differential-tested vs sqlite 3.45 (comparisons, precedence, ORDER BY, IN/BETWEEN, DISTINCT/GROUP BY with a collated sort, and Unicode-not-folded). **Refused (never a wrong sort/compare), all stage 1b:** column-declared collation in `CREATE TABLE`/`ADD COLUMN` (`name TEXT COLLATE NOCASE`) — so there are no collated indexes yet; a `COLLATE` in any position where it could not change a comparison or a sort (a bare projected `COLLATE`, or inside a GROUP BY / DISTINCT key, where honoring it would need a collated regroup) is a clean error, not a silently-ignored no-op; and `IN (SELECT …)` / window-`ORDER BY` collation. Plan format 28 |
| Parameters | ✅ | `$1, $2, …` (PostgreSQL style) rather than `?`; types unify at compile time |

## Scalar functions

| Function | Status | Comment |
|---|---|---|
| lower, upper | ✅ | text in, text out; argument types checked at compile time |
| trim, ltrim, rtrim | ✅ | whitespace by default, or a given set of characters (2-arg); `trim` strips both ends, `ltrim`/`rtrim` one end |
| replace | ✅ | every occurrence; an empty search string is a no-op (sqlite's rule) |
| instr | ✅ | 1-based character position, 0 when absent (1 for an empty needle) |
| length | ✅ | character count (not bytes) |
| char | ✅ | variadic; Unicode code points → text (`char()` is the empty string). A NULL argument yields NULL — sqlite reads it as code point 0, the one documented gap |
| unicode | ✅ | Unicode code point of the first character; NULL for the empty string |
| hex | ✅ | uppercase hex of the argument's bytes (text or blob); a number is refused (sqlite renders it to text first). `hex(NULL)` is NULL, where sqlite gives `''` |
| typeof | ✅ | datatype name; `typeof(NULL)` is `'null'` (the one scalar that does not NULL-propagate). The sqlite core names match (`integer`/`real`/`text`/`blob`); `bool`/`timestamp` report their own honest names |
| abs, round, ceil / ceiling, floor, trunc | ✅ | keep their argument's numeric type (int stays int); `trunc` rounds toward zero |
| sqrt, pow / power | ✅ | always float; a non-real result (sqrt of a negative) is NULL, matching sqlite |
| sign | ✅ | always an integer: -1 / 0 / 1 |
| exp, ln, log10 / log, log2, log(b, x) | ✅ | always float; `log`/`log10` is base-10, `log(b, x)` is base `b`; a non-positive argument is NULL (sqlite), and `log(b, x)` requires base `b > 1` |
| sin, cos, tan, asin, acos, atan, atan2 | ✅ | radians; always float; `asin`/`acos` outside [-1, 1] → NULL; `atan2(y, x)` takes `y` first |
| sinh, cosh, tanh | ✅ | hyperbolic; always float (overflow is `Inf`, matching sqlite) |
| radians, degrees, pi | ✅ | angle conversions and the constant π; `pi()` is the one nullary function |
| mod | ✅ | floating-point remainder `x - y*trunc(x/y)` (sign of the dividend); a zero divisor is NULL — the same NULL the `%` operator yields — matching sqlite |
| substr / substring | ✅ | |
| coalesce, ifnull | ✅ | compiled to lazy control flow, not a call — arguments after the first non-NULL are never evaluated; int64/float64 arm mixing refused, same rule as CASE |
| nullif | ✅ | desugared to CASE |
| iif | ✅ | `iif(c, a, b)` = `CASE WHEN c THEN a ELSE b END` (control flow, does not NULL-propagate); the condition is a rigid boolean, not sqlite truthiness |
| printf / format | ✅ | sqlite's C-printf formatter (`format` is an exact alias), variadic, differential-tested against sqlite 3.45 across ~2,800 value×specifier cases. Conversions: `%d %i %u %x %X %o %c %s %q %Q %w %% %f %e %E %g %G`; flags `- + space 0 # , !`; field width, `.precision`, and `*` (width/precision from an argument). sqlite's dialect, matched exactly (not C stdio): `%c` is the first *character of the argument's text* (`printf('%c',65)`→`6`, not `A`); `%u`/`%x`/`%o` are 64-bit; integer `.precision` zero-pads; text→number coercion parses a leading numeric prefix (`printf('%d','12ab')`→`12`, `'abc'`→`0`); `%q`/`%Q`/`%w` are the SQL escapes (NULL renders `(NULL)`/`NULL`/`(NULL)`); `%.0f` of `3.5` is `3` (sqlite's decimal decoder, ported faithfully). A NULL *data* argument is formatted per specifier, never propagated; a NULL or empty *format* yields NULL. Floats use the portable double-double decoder (bit-identical to sqlite's long-double CLI, and deterministic across mpedb's platforms). **Deviations (each a clean compile error, never a wrong answer):** the format argument must be text — mpedb refuses a non-text format that sqlite would coerce; and an *untyped bare parameter* in a data slot is refused (the consuming specifier is only known at runtime, so a rigid engine cannot type it — add a `CAST`). A conversion character outside the supported set halts output at that point, as sqlite does for an unknown specifier. One float deviation, deterministic by design: the `!` alt form asks the decoder for up to 26 significant digits, and beyond ~17 the portable double-double decoder can differ from a *long-double* sqlite build in the last digit(s) — mpedb's value is the same on every platform (a long-double sqlite build is not), so `%!` with precision past ~17 significant digits is the one place the byte-for-byte match with the CLI ends |
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
| sqlite3_blob_open / read / write | incremental blob API + `insert_file` / `blob put`/`get` | streaming both ways; large values can live in contiguous extents (see [design/DESIGN-BLOBEXTENT.md](design/DESIGN-BLOBEXTENT.md)) |
| sqlite3_backup | **Not needed** | the database is one file; copy it (plus `-wal` if present) |
| busy_timeout / busy_handler | **Not needed** | writers queue on a robust cross-process lock with an intent ring for group commit; a SIGKILLed owner is recovered, not waited out |
| user-defined functions | ❌ | planned as the PySpell layer (compiled, typed IR — not callbacks) |
| loadable extensions / virtual tables | ❌ | no extension ABI (deliberate — mpedb is rigid and in-process). The one virtual table that matters, **FTS5, is NATIVE** as of stage 1 (`CREATE VIRTUAL TABLE … USING fts5(…)` → `TableKind::Fts`; see the `MATCH` row and [design/DESIGN-FTS.md](design/DESIGN-FTS.md), #76), not a plugin. The general `sqlite3_create_module` plugin ABI (and other modules — fts3/fts4/rtree) remains a deliberate non-goal, refused by name at `CREATE VIRTUAL TABLE` |

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
support multi-process mixed use](design/TURSO.md).

| | sqlite | mpedb | notes |
|---|---|---|---|
| many processes, one database | ✅ file locks + busy_timeout | ✅ shared-memory attach, MVCC | measured (2-core Linux, readers beside one writer): commit-class mpedb 569k reads/s vs sqlite-WAL 568k — a tie; none-class mpedb 467k vs sqlite-journal 2,251 (that mode serializes readers against the writer) |
| readers block the writer | in rollback-journal mode, yes; in WAL, no | never | |
| a process dies mid-write (SIGKILL) | journal/WAL recovery on next open | robust-mutex takeover + intent-ring recovery, fuzzed at every instant | `mpedb crash` is the harness |
| second concurrent writer | waits (busy_timeout) | queues on the writer lock; group commit under contention | Turso 0.7: immediate Busy, no arbitration — its contended p99 is 51–225 ms in [the measured field](design/TURSO.md) |

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
`wal_checkpoint(TRUNCATE)` calls ([design/TURSO.md](design/TURSO.md) has the details).

## Migration

| path | status | comment |
|---|---|---|
| sqlite → mpedb | ✅ `mpedb mirror import` | schema + data + type provenance; the measured conversion matrix is in the [testkit README](crates/mpedb-testkit/README.md) |
| mpedb → sqlite | ✅ `mpedb mirror export` | round-trips are verified (`mirror roundtrip`) |
| live two-way sync with sqlite | ✅ `mirror sync` / daemon | SIGKILL-fuzzed to convergence (`mirror-collide`: writers + a daemon killed at every instant must still converge exactly) |
| PostgreSQL ⇄ mpedb | ✅ `mirror` with a PG source/target | same machinery, `--source-config` DSN handling |
| open an existing `.db` file | 🚧 | two ways today. **Sidecar (read-write)**: `mpedb data.db` works like `sqlite3 data.db` (repl or one-shot) — imports on first open, pulls incrementally on later ones, `mpedb checkpoint data.db` pushes writes back with mirror's conflict rules. **Native (read-only, zero import)**: `mpedb dump data.db` and `mpedb::SqliteAttach` read the sqlite file format directly — no sqlite library in the path, both b-tree layouts, differentially verified row-for-row against the real library; PK probes are b-tree seeks, writes are refused by name. The in-place delta overlay with lock modes is the designed next stage ([design/DESIGN-SQLITE-BACKED.md](design/DESIGN-SQLITE-BACKED.md), 20-finding review folded) |

## Measured speed against sqlite

From the 2026-07-17 head-to-head runs (one run per machine, all engines in the
same run; full tables with latencies and methodology in
[BENCHMARKS.md](BENCHMARKS.md) and the per-machine RESULTS files, the four-way
field including PostgreSQL and Turso in [design/TURSO.md](design/TURSO.md)). Compare within
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
[design/DESIGN-BLOBEXTENT.md](design/DESIGN-BLOBEXTENT.md).

## Extensions beyond SQLite

- `current_setting('key')` and `expr IN (current_setting('key'))` — session
  context for serverless row-level security ([design/DESIGN-MULTIDB.md](design/DESIGN-MULTIDB.md));
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

1. Integer overflow **raises**; sqlite promotes to REAL. (Division / modulo
   by zero, by contrast, yields NULL to match sqlite.)
2. A scalar subquery returning more than one row **errors** (PostgreSQL's
   rule); sqlite silently takes the first row.
3. `ORDER BY` must name something the query outputs; `ORDER BY 1 + 1` is
   refused (sqlite sorts by the constant, i.e. not at all).
4. Text never converts *implicitly* to numbers (not in comparisons, arithmetic,
   or storage) — but an *explicit* `CAST` follows sqlite's permissive affinity
   rules and parses a leading numeric prefix (see the CAST row above).
5. Compound set-ops use sqlite's flat precedence; PostgreSQL binds INTERSECT
   tighter. Documented, matching sqlite here.
