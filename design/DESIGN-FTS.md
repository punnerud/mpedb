# DESIGN-FTS — full-text search + the `MATCH` operator (sqlite FTS5 equivalence)

**Status: STAGE 1 SHIPPED (2026-07-18, #76, PLAN_FORMAT 25, schema canonical-bytes v4). Stages 2–3
remain design. The wire/index layout (§7) is implemented as described below and carries
truncation-at-every-offset decode tests (`mpedb-types/src/fts.rs`); an adversarial review of that
layout is still warranted before stage 2 builds on it. See §8 for exactly what shipped vs.
deferred.**

## 0. The compat fact this closes

The reference `sqlite3` (3.45.1, the amalgamation build shipped as both the library and the CLI)
is compiled **with FTS3/FTS4/FTS5** (`PRAGMA compile_options` → `ENABLE_FTS5`). So `MATCH` is a
real, supported operator there — but ONLY against a full-text virtual table:

```
'abcdef' MATCH 'cde'                     -- Error: unable to use function MATCH in the requested context
CREATE TABLE t(x); … WHERE x MATCH 'w'   -- SAME error: MATCH on a plain column is not usable
CREATE VIRTUAL TABLE ft USING fts5(body);
  … WHERE ft MATCH 'quick'               -- works: full-text query, whole-row match
  … WHERE body MATCH 'quick'             -- works: column-scoped query
  … ORDER BY rank                        -- works: bm25 relevance, best-first
```

So two things are true at once, and both must be reproduced to be equivalent:
1. `MATCH` on a **non-FTS** column/scalar is an **error** (not FALSE, not a substring test). mpedb
   must raise the identical "unable to use function MATCH in the requested context", never invent a
   fallback — that would answer wrongly, which this repo does not do.
2. `MATCH` against an **FTS table** is full-text search. To have (2) at all, mpedb needs FTS.

The earlier COMPAT.md note ("the bare shell also lacks it") was wrong: the default distribution has
FTS5. This design closes the gap for real. **Non-goal:** sqlite's *loadable-extension virtual-table
plugin API* (`sqlite3_create_module`). mpedb has no extension ABI and wants none (CLAUDE.md: rigid,
in-process). FTS is therefore a **first-class table kind**, not a plugin — see §1.

## 1. mpedb-native shape: FTS as a first-class table kind, not a virtual-table plugin

sqlite's FTS5 is a virtual table backed by ordinary shadow tables (`ft_data`, `ft_idx`,
`ft_content`, `ft_docsize`, `ft_config`). mpedb reuses that *idea* without the vtable indirection:
`CREATE VIRTUAL TABLE ft USING fts5(a, b, …)` is parsed and bound to a new **`TableKind::Fts`** in
the catalog (canonical-bytes schema gains a kind discriminant — additive; per the no-backward-compat
rule we bump the schema-canonical format freely, no migration). An FTS table owns:

- a **content** store — the columns, keyed by an auto-assigned `rowid` (INTEGER PK), exactly like an
  ordinary mpedb table. `contentless` (`content=''`) and `external-content` (`content='base'`) modes
  are stage-3; stage 1 stores content inline.
- an **inverted index** — a B+tree (a normal mpedb index tree, index discipline from CLAUDE.md)
  keyed `(term ‖ colno) → posting list`. This is the whole trick: FTS is *just another secondary
  index* over mpedb's COW B+tree, so it inherits MVCC snapshots, crash-safety (COW + WAL), the
  reader-pin protocol, and multi-process visibility **for free**. No new durability code.
- a **docsize / config** sidecar (small sys-keyspace records) for bm25's per-doc length and the
  average-doclen denominator.

Maintenance is transactional and automatic: an INSERT/UPDATE/DELETE on the FTS table tokenizes the
changed columns and writes the posting deltas **in the same write txn** as the content rows — the
same mutator-level hook used for CDC/trigger capture (write.rs `capture_dirty`), so an FTS table is
never torn from its index even under SIGKILL. There is no "rebuild"; the index is always current.

## 2. Tokenizer

Default **`unicode61`** (sqlite's default): split on Unicode non-alphanumeric, casefold, strip
diacritics per sqlite's table, `remove_diacritics=1`. `ascii` and `porter` (Porter stemmer wrapping
an inner tokenizer) and `trigram` (substring/LIKE-style 3-gram) are selectable via the `tokenize=`
option. Stage 1 ships `unicode61` + `ascii`; `porter` + `trigram` are stage 3. Tokenizer choice is
frozen into the table's schema bytes (content-hashed with the plan, so a query cannot silently
tokenize differently than the index was built with — the rigid-schema advantage over sqlite, where a
mismatched external tokenizer silently corrupts results).

Positions are stored per posting (token offset within the column) so phrase/NEAR queries work.

## 3. The FTS5 query grammar (what `MATCH 'string'` parses)

The right operand of `MATCH` is a **literal string** parsed at plan time into an FTS query tree
(mirrors LIKE/GLOB/REGEXP: literal pattern, content-hashed into the plan). Grammar, staged:

- **stage 1**: bare terms, `AND`/`OR`/`NOT` (and implicit-AND juxtaposition), parentheses, prefix
  `term*`, column filter `col:term` and `{a b}:term`. Initial-token `^term`.
- **stage 2**: phrases `"a b c"`, `NEAR(a b, N)`, prefix inside phrases.
- Deviations refused by name (never wrong): the `fts5vocab` shadow tables, the `INSERT INTO
  ft(ft) VALUES('optimize'|'rebuild'|'merge')` maintenance verbs (accepted as **no-ops** since our
  index is always merged/current — documented), FTS3/4 legacy `MATCH` enhanced-query quirks, and
  custom tokenizers via the C API.

## 4. Execution — posting-list streaming (MPEE fit)

A `MATCH` predicate compiles to an **FtsScan** access path (planner/access.rs sibling of
PkPoint/IndexRange). Evaluation is posting-list set algebra over the inverted-index B+tree:

- Each term → its posting list (a cursor over the index tree; length is **known exactly** from the
  index — mpedb has transactional row/entry counts). `AND` = intersect, `OR` = merge, `NOT` =
  difference, phrase = positional intersection.
- **This is exactly the MPEE streaming-N×N story** (design/DESIGN-MPEE-OPT.md, the punnerud/mpee
  "one exit collapses the region" heuristic): evaluate the **rarest term first** (smallest posting
  list), and let it drive — every other term is probed only against that candidate set, never
  materialized in full. The classic FTS "sort by document frequency, intersect ascending" is the
  same move as MPEE collapsing all addresses in a region to its single exit. Posting lists are
  streamed and short-circuited, never fully realized, so an `AND` of a rare term with a common one
  costs the rare term's length, not the common term's.
- The exact posting-list lengths feed **#74's prepare-time risk estimate** directly: an FTS `AND`
  can only produce ≤ min(list lengths) rows, so the runtime-budget layer-1 pass can bound an FTS
  query tightly (much tighter than a blind join), and the layer-2 work counter charges one unit per
  posting entry visited.

## 5. Ranking: `rank` / `bm25()`

sqlite exposes relevance as the special column `rank` and the auxiliary `bm25(ft [, w1, w2, …])`.
mpedb reproduces sqlite's exact bm25: k1 = 1.2, b = 0.75, per-column weights default 1.0, and — the
detail that bites — sqlite returns bm25 **negated** so that `ORDER BY rank` ascending yields
best-match-first. We store the same convention and verify `ORDER BY rank` and `ORDER BY bm25(ft)`
row-for-row against sqlite 3.45. Per-doc length and the corpus average come from the docsize sidecar
(§1). Ranking is stage 2 (stage 1 returns matches in rowid order, which sqlite also does when no
`ORDER BY rank` is given).

## 6. Auxiliary functions (stage 3)

`highlight(ft, col, open, close)`, `snippet(ft, col, open, close, ellipsis, tokens)`, and the
positional `offsets()` — all derivable from the stored positions. Deferred to stage 3; refused by
name until then (clean error, never a wrong string).

## 7. Wire/index layout — the part that needs adversarial review before stage 1

- Posting-list value encoding: sqlite uses delta-varint doclists (docid-delta, then position-delta
  varints, column separators). mpedb should use the **same delta-varint discipline** but as an
  mpedb index value (≤ inline-cap bytes stay inline, overflow chains beyond — btree.rs rules), keyed
  `(term ‖ colno)`. Decoder is bounds-checked, `Corrupt`-never-panic (mpedb rule, doubly so for a
  new on-disk structure), with truncation-at-every-offset tests.
- Update deltas must keep index-tree topology invariant under the commit fixpoint (freelist rules,
  CLAUDE.md) — an FTS update rewrites posting values; confirm values stay ≤ 960 B inline or take the
  overflow path cleanly, never a rewrite that changes tree shape mid-fixpoint.
- Membership under NULL: a row with a NULL FTS column contributes no postings for that column
  (mirrors the "any NULL indexed column → no entry" rule, adapted per-column).
- Crash story: index deltas ride the base row's write txn; a torn write leaves neither → prove via
  the CLI `crash`/`powerloss` harness with an FTS workload (new `--fts` mode) before calling it done.

## 8. Staging & format

- **Stage 1** (format bump: `MATCH` instr + FtsScan access path + `TableKind::Fts` schema byte —
  lands AFTER window functions' format 24, so this is the next number): `CREATE VIRTUAL TABLE …
  USING fts5(cols [, tokenize=…])`, unicode61/ascii tokenizers, terms + AND/OR/NOT + prefix +
  column-filter + `^`, whole-row and column-scoped `MATCH`, rowid-order results, transactional
  index maintenance, crash-tested. `MATCH` on non-FTS → the exact sqlite error.
- **Stage 2**: phrases, NEAR, `rank`/`bm25()` with sqlite's negated-score ordering.
- **Stage 3**: highlight/snippet/offsets, porter/trigram tokenizers, contentless/external-content.

Every stage is differential-tested against `sqlite3` 3.45 (FTS5 present) row-for-row, and lands its
own tests (own-code discipline). COMPAT.md `MATCH` row: ❌ → 🚧 (stage 1–2) → ✅ (stage 3); the
"loadable extensions / virtual tables" row stays ❌ with a note that FTS5 specifically is native, the
general vtable plugin ABI is a deliberate non-goal.

## 8.1 What stage 1 actually shipped (2026-07-18)

**Storage / maintenance.** `TableKind::Fts { tokenizer }` in the schema (canonical-bytes v4: a table
gains a kind discriminant after its `dead` byte; the SEED-hash / dense-id / validate invariants all
hold, and an FTS table is validated to be a single INTEGER `rowid` PK + TEXT content columns, no
secondary indexes). `CREATE VIRTUAL TABLE ft USING fts5(cols [, tokenize='unicode61'|'ascii'])`
parses (`parser/ddl.rs`) and applies through the live-DDL path (`ddl_apply.rs`), building an auto
`rowid` INTEGER-PK content table. The inverted index lives at a reserved `index_no`
(`FTS_INDEX_NO`, engine `fts.rs`); it is seeded by `create_table`, torn down by `drop_table`,
discovered by the page-accounting verifier's catalog prefix scan, and maintained **in the row txn**
by `insert_row`/`update_by_pk`/`delete_by_pk` (a NULL column contributes no postings). Optimistic
blind-apply is disabled for FTS tables (they have no `TableDef.indexes`, so the row path is the only
place the index is maintained). **Posting wire layout** (`mpedb-types/src/fts.rs`, §7): key =
`term_utf8 ‖ 0x00 ‖ colno_be_u16`; value = a count-prefixed delta-varint doclist — `n:uvarint`, then
per doc `zigzag(docid-delta), n_pos:uvarint, (uvarint pos-delta)*`. The leading count makes every
proper-prefix truncation a clean `Corrupt` (tested at every offset). Tokenizers: `unicode61`
(Unicode alnum split, casefold, common-Latin diacritic fold) and `ascii` (a-z/0-9 + high bytes kept,
ASCII casefold only).

**Query.** `MATCH` is a `Kw`/AST node (`Expr::Match`) parsed at comparison precedence. It is NOT a
boolean: the binder errors on ANY `Expr::Match` it sees (*unable to use function MATCH in the
requested context*), and the planner intercepts the ONE legal shape first — a top-level WHERE `AND`
conjunct `<col-or-table> MATCH 'literal'` against the single FROM table — turning it into
`AccessPath::FtsScan { query }` (plan format 25; recursive bounds-checked encode/decode/validate,
footprint = table-level read). The literal is parsed (`planner/fts.rs`) into an `FtsQuery` tree with
terms normalized by the table's frozen tokenizer: bare terms, `AND`/`OR`/`NOT` + implicit-AND, parens
(precedence NOT > AND > OR), prefix `term*`, column filter `col:` / `{a b}:`, initial `^`. Execution
(`exec/fts.rs`) is posting-list set algebra — AND intersects (driven by the rarer side), OR merges,
`X NOT Y` differences — over the inverted-index prefix scans (`ReadTxn::fts_prefix`), charging the
#74 work meter per posting entry visited, yielding rowids ascending; `gather_rows` then fetches each
row by PK and applies the residual WHERE / RLS filter.

**Deliberate stage-1 deviations (documented; each is a clean error, never a wrong answer):**
- **Explicit `rowid` required on insert.** mpedb has no auto-increment PK; an FTS `INSERT` must
  supply `rowid`. **Stage 1b:** auto-assign `max(rowid)+1` (a `DefaultExpr::AutoRowid` on the rowid
  column, filled at insert exec + a `validate` relaxation), matching sqlite's omitted-rowid insert.
- **`MATCH` must be a top-level `AND` conjunct against the single FROM table.** `MATCH` inside an
  `OR`, a second `MATCH` conjunct, or `MATCH` with a join is refused (sqlite answers some of these
  via vtable OR-union / multi-constraint). **Stage 1b/2:** multi-`MATCH` intersection and MATCH-in-OR.
- **`SELECT *`** on an fts table returns `rowid` first (it is a real column here); sqlite hides it.
- unicode61 diacritic folding covers the common Latin set, not sqlite's full Unicode table.

**Follow-up (noted, not done):** the multi-process **SIGKILL crash test** for FTS atomicity — a CLI
`crash --fts` / `powerloss --fts` workload proving the index and content commit atomically under a
kill at every instant (§7 crash story). Stage 1 proves atomicity/durability at unit level only
(`fts.rs::index_and_content_persist_across_reopen` — build, close, reopen from the file, MATCH still
finds the rows), since the index rides the base row's ordinary COW+WAL commit. Add the CLI harness
before relying on FTS under power loss.
