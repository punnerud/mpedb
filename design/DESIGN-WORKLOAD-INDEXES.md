# DESIGN-WORKLOAD-INDEXES — indexes derived from the workload, identified by content

**Status: design + measurement, 2026-07-20 (task #118). No engine change.** The
measurement half is a new census mode on the #117 harness
(`sqlite_corpus --index-census`), and it is the thing that decides whether the rest is
worth building.

Morten's premise: *"Vanlig er å måtte designe masse indekser, men hvorfor ikke ta inn
modell/SQL — eks modellen brukt i Django — så kjøre test av SQLene og lage indeks basert på
det. Ikke sikkert hele tabellen skal ha indeks heller, men deler av dataene på tvers. …
Nye versjoner bør ikke rebygge indekser."* And the objection to the obvious alternative:
*"Alternativet er å bygge indeks under MPEE-bruk, men da blir det forsinkelse basert på
bruk."*

Reads with: [DESIGN-MPEE-COST.md](DESIGN-MPEE-COST.md) (#88 — the cost catalog and the
recommend-only staging this doc inherits), [FOOTPRINT-INDEX-MEASURED.md](FOOTPRINT-INDEX-MEASURED.md)
(#117 — the corpus census this one extends, and the verdict *cost history keys on the PLAN
HASH, not on shape*, which applies unchanged here), [DESIGN-MPEE-SOLVER.md](DESIGN-MPEE-SOLVER.md)
§9 (the measurement seam mode (B) attaches to), [DESIGN-SCHEMA-V2.md](DESIGN-SCHEMA-V2.md)
(index numbering), [DESIGN-SERVICE.md](DESIGN-SERVICE.md) (#77, the durable queue).

---

## 0. The one-paragraph answer

The workload is **enumerable, not sampled**: every statement mpedb has ever compiled is a
record in the plan registry carrying its full SQL text *and* its full `CompiledPlan` blob,
so "which columns does this application filter on" is a `SELECT`, not a guess (§1). From
those plans a candidate index set is derivable mechanically (§4), and the measurement says
the set is **small** (§3) — small enough to enumerate and cost exhaustively, not search.
An index is identified by **what it is** (§2) — a content hash over the table name, the key
column names, their collations, and the predicate — so an app upgrade that still implies an
index finds it already built. Partial indexes (§5) are designed, and the honest measured
verdict is narrower than the pitch: **42 % of the candidate set carries a predicate but only
3.3 % of occurrences do, and every predicate the corpus produced is an `IS NULL` shape** —
which is a statement about sqllogictest (no soft-delete, no status enum, no tenant key) far more
than about applications, so §3.3 names the one missing measurement rather than extrapolating.
The whole thing runs **ahead of time from a test-suite run** (§6 mode A), with adaptive
refinement (§6 mode B) attached to #88's per-plan-hash cost history, and every build is
background and droppable (§7). Nothing here reaches the write hot path.

Six things mpedb cannot do today are named as **prerequisites** (§10), not designed around.

---

## 1. What the registry actually retains — verified

`crates/mpedb/src/registry.rs`. A registry record under sys-key `plan/<32-byte hash>` is:

```text
u32 sql_len ‖ sql ‖ u32 blob_len ‖ blob ‖ u64 last_used_txn      (registry.rs:5-9)
```

So the registry retains **the original SQL text** (`Record::sql`, kept "for tooling … and as
the re-prepare fallback") **and** the full `CompiledPlan::encode()` blob. From the blob,
`CompiledPlan::decode` reconstructs everything a candidate generator needs:

| what | where | recoverable? |
|---|---|---|
| the table(s) touched | `SelectPlan::table`, `Join::table`, `PlanStmt::Update/Delete::table` | **yes** |
| the chosen access path | `AccessPath::{PkPoint,PkRange,IndexPoint,IndexRange,FullScan,FtsScan}` | **yes**, with `index_no` |
| the **residual WHERE predicate** | `SelectPlan::filter: Option<ExprProgram>` (also `joined_filter`, `post_filter`, `Update/Delete::filter`) | **yes** — full stack IR, column ordinals intact |
| which literal vs which parameter | `Instr::PushConst(k)` into `ExprProgram::consts` vs `Instr::PushParam(k)`; `k >= n_user_params()` is a subplan/context slot | **yes** |
| collation of a comparison | `Instr::CmpColl(CmpKind, Collation)` / `CmpClass` | **yes** |
| ORDER BY, GROUP BY, DISTINCT keys | `order_by` + `order_over: OrderOver`, `Aggregation::group_by`, `distinct` | **yes** |
| execution frequency | — | **no** (see below) |
| selectivity / rows returned | — | **no** (see below) |

> **Verdict: the WHERE predicate is fully recoverable, so partial-index derivation is nearly
> free.** This is the fact the rest of the design rests on, and it is the fact that
> distinguishes mpedb from sqlite (no plan store at all) and from PostgreSQL
> (`pg_stat_statements` keeps a normalized *text*, sampled, with the constants stripped —
> exactly the information a partial-index predicate is made of).

**One correction to the naive reading, and it matters.** The predicate is *not* all in
`filter`. `planner::access::extract_access` **consumes** the conjuncts it turns into an
access path — a `WHERE id = 3 AND status IS NULL` over a PK becomes
`AccessPath::PkPoint([Const])` plus a residual `filter` holding only `status IS NULL`. A
candidate generator that reads only `filter` misses precisely the columns that already
matter. It must union the access path's pinned columns with the residual conjuncts. The
census in §3 does exactly that.

### 1.1 Three limits of the registry, stated up front

1. **It is a bounded window, not a history.** `MAX_REGISTRY_PLANS = 4096`, and an insert
   into a full registry evicts `EVICT_BATCH = 256` by `last_used_txn`
   (`registry.rs:27,31,162`). #117 counted **81,036 distinct plans** in one corpus run.
   A registry-only mode (A) therefore sees at most the most recent 4,096 plans. **Design
   consequence: mode (A) must accept an offline statement stream as an alternative input,
   not only the live registry** (§6.1). A test-suite run piped through the compiler is not
   subject to the cap.
2. **`last_used_txn` is not a frequency counter.** It is a *recency* stamp, and it is
   deliberately not bumped on read-only loads (`registry.rs:146-161`: bumping it would take
   the writer lock on a read path). So the registry can say *what* runs but not *how often*.
   The frequency number the cost model needs (§8) is #88's "execution count and total work
   per plan hash" (DESIGN-MPEE-SOLVER §9.1 item 4), which does not exist yet — **prerequisite
   P5**.
3. **A plan carrying a host UDF never enters the registry** (design/DESIGN-UDF.md;
   `ExprProgram::has_host_call`). Such statements are invisible to mode (A). Accepted: an
   index derived from a host-UDF predicate would not be probeable anyway (§5.4).

---

## 2. Index identity — settle this first

### 2.1 What mpedb does today, and why it is 80 % of the answer already

`IndexDef` is, in full (`crates/mpedb-types/src/schema.rs:245-250`):

```rust
pub struct IndexDef {
    pub columns: Vec<u16>,   // ordinals into TableDef::columns, in key order
    pub unique: bool,
}
```

No name. No predicate. No collation (it is read off `ColumnDef.collation`). No state.
`index_no = 1 + position in TableDef::indexes`; index 0 is the PK tree. The index tree's
root lives in the catalog under `[0x01, table_id BE, index_no BE] -> (root u64, rowcount u64)`
(`engine/mod.rs:226-232`).

Two consequences that are already exactly what #118 asks for:

- **The index NAME is not persisted at all** (`crates/mpedb-sql/src/ddl.rs:181-191`; COMPAT.md:
  "The index name is not persisted (indexes are positional)"). A rename is a no-op by
  construction.
- **`CREATE INDEX` is already idempotent BY SHAPE, not by name.** `Database::apply_create_index`
  compares `(columns, unique)` against the live `TableDef.indexes` and returns a silent no-op
  on a match — *including for a differently-named index with the same shape*
  (`crates/mpedb/src/ddl_apply.rs:751-753`). So a v2 migration that re-issues
  `CREATE INDEX ix_order_customer ON orders(customer_id)` under a new name **does not rebuild**
  today.

So "nye versjoner bør ikke rebygge indekser" is already true for whole-table indexes. What is
missing is that the shape is (a) **ordinal-based**, not name-based, and (b) has no room for a
predicate or a collation. Both break the moment §5 lands or DROP INDEX lands.

### 2.2 The rule

> **An index is named by a content hash over its definition, and the definition is written in
> NAMES, not ordinals.**
>
> ```text
> IndexId = blake3( canonical_index_bytes )
>
> canonical_index_bytes :=
>     u8   version
>   ‖ str  table_name                      -- NOT table_id
>   ‖ u8   unique
>   ‖ u16  n_key
>   ‖ n_key × ( str column_name            -- NOT column ordinal
>               ‖ u8  collation            -- Binary | NoCase | Rtrim
>               ‖ u8  direction )          -- reserved; always Asc today
>   ‖ u32  predicate_len ‖ predicate_canonical_bytes   -- empty = whole-table
> ```
>
> `str` is `u32 len ‖ UTF-8`, exactly as `Schema::canonical_bytes` writes identifiers.
> `predicate_canonical_bytes` is the partial predicate in the **canonical conjunct form** of
> §5.2 — a sorted list of atoms over column NAMES — not the raw `ExprProgram`, whose
> instruction encoding is an ordinal-based artefact of one compile.

**Names, not ordinals, is the load-bearing part**, and it goes against mpedb's grain
everywhere else (table ids, column ordinals and `index_no` are positional by design). The
justification is specific: a positional identity survives an *append*, which is all mpedb does
today, but not a *reorder* or a *drop*, which is what an app-schema evolution does. A name
identity survives both. And it costs nothing at runtime: names are resolved to ordinals once
at attach, exactly as `resolve_index_columns` already does (`ddl_apply.rs:524-543`).

**What identity is NOT.** Not the index name (not stored, and an app renames freely). Not the
app or schema version (that is the whole point). Not the position in `TableDef.indexes` (an
implementation detail; §2.4). Not the statistics — the #88 rule *"statistics inform costing,
never the plan identity"* (DESIGN-MPEE-SOLVER §9.2) applies verbatim: **an index's measured
usefulness must never enter its identity**, or a recount rebuilds the tree.

### 2.3 What the identity buys, concretely

`CREATE INDEX` (and the §6 advisor's build) becomes:

1. Canonicalize the definition, compute `IndexId`.
2. Look up sys-key `ixid/<IndexId>`. **Present ⇒ the index exists and is built; return.**
   No scan, no write, no `schema_gen` bump, no plan invalidation.
3. Absent ⇒ allocate an `index_no`, build, publish, record `ixid/<IndexId> -> index_no`.

An app upgrade re-declaring its whole index set therefore costs one hash and one sys-lookup
per index. And it is *stronger* than today's shape check, because it survives a column
reorder, an index reorder, a rename, and (once §5 lands) tolerates two indexes on the same
columns with different predicates — which today's `(columns, unique)` comparison would
collapse into a false no-op.

### 2.4 Interaction with #47 (live DDL) and #81 (table-id reclamation)

- **#47.** Every DDL commit bumps `schema_gen`, and `CREATE INDEX` changes
  `Schema::canonical_bytes` ⇒ `Schema::hash` ⇒ every persisted plan's embedded `schema_hash`
  mismatches ⇒ `PlanInvalidated` ⇒ re-prepare. That is correct and desirable — a new index
  *should* re-plan the statements that can use it — but it is a **global** invalidation of
  every plan in the registry, not just those reading the table. #117 already named the right
  refinement: *"the footprint is a legitimate coarse index over plans ('invalidate everything
  that reads T after `CREATE INDEX`')"*. Not required for correctness; noted so the advisor's
  cost model (§8) charges the re-prepare storm honestly, since an advisor that adds ten
  indexes triggers ten full invalidations.
- **#47 step 2 (the `IndexId` record must move in the same commit as the tree).** The catalog
  already hard-errors `Corrupt("missing catalog entry for table T index i")` if a schema claims
  an index whose tree root was never seeded (`engine/mod.rs:1367-1372`). The `ixid/` record
  joins that set: schema, tree root and `IndexId` record are published in one commit or none.
- **#81 / table-id reclamation.** Not built; `design/DESIGN-TABLE-ID-GEN.md` supersedes it and
  table ids are a monotone high-water today (`schema.rs:299-305`), with `DROP TABLE` leaving a
  tombstone whose id is never reused. So `IndexId` is unaffected *today*. But a **name-based**
  identity introduces a hazard that an id-based one does not: `DROP TABLE t; CREATE TABLE t(…)`
  reproduces the same `IndexId` for a *different* table. **Rule: `DROP TABLE` must delete every
  `ixid/*` record belonging to that table**, in the same commit that frees its index trees
  (`write.rs:1101-1135` already walks them). Then a resurrected `t` re-derives the same
  `IndexId`, finds no record, and builds — correct. If generation-tagged ids ever land, the
  generation joins `canonical_index_bytes` after `table_name`.

---

## 3. The measurement — how many candidates does a real workload generate?

**Harness**: `sqlite_corpus --index-census[=out.tsv]`, a sibling of #117's
`--footprint-census`, in `crates/mpedb-testkit/src/bin/sqlite_corpus.rs`. It hooks the same
statement stream at the same place, recompiles each statement, and folds the resulting
`CompiledPlan` into candidate keys. It reads the **compiled plan**, not the SQL text, for the
reason in §1: the equality conjuncts that decide the key columns have been consumed into the
`AccessPath` and are not in `filter` any more.

**Candidate extraction**, per single-table statement (`Select` without joins/windows,
`Update`, `Delete`):

1. Equality columns pinned by the access path (`PkPoint` ⇒ all PK columns; `IndexPoint` ⇒ the
   covered prefix of that index) ∪ equality columns from residual conjuncts
   (`col = <const|param>`, `col IN (<consts>)`).
2. Plus at most **one** range column (from `PkRange` / `IndexRange` / a residual `< <= > >=`).
3. Plus the `ORDER BY` tail, but only when `order_over == OrderOver::BaseRow` and the statement
   neither aggregates nor `DISTINCT`s — otherwise the ordinals do not name base columns.
4. Predicate conjuncts = whatever is left that is *not* on a key column.

Because the corpus is essentially unparameterized (#117 §1: 1.17 statements per distinct plan)
while a real ORM binds parameters, counting "predicates" one way would be dishonest either
direction. So **three families** are counted:

| family | predicate admitted | reads as |
|---|---|---|
| **W** | none — `(table, key columns)` only | parameterization-INVARIANT: the classic advisor's search space |
| **Pnull** | `col IS NULL` / `col IS NOT NULL` on non-key columns | the class that **survives** an ORM: Django emits `IS NULL` as literal text, never as a bound parameter |
| **Plit** | Pnull + `col = <const>` / `col IN (<consts>)` | the **upper bound**: in this corpus every literal is inline, so a real parameterized app reaches this only for genuine constants (enums, booleans, tenant sentinels) |

### 3.1 What was run

`sqlite_corpus --index-census=<tsv>` over 14 real sqllogictest files —
`index/{between,commute,delete,in,orderby,orderby_nosort,random,view}/1000/slt_good_0.test`,
`random/{aggregates,expr,groupby,select}/slt_good_0.test`, `evidence/slt_lang_update.test`,
`select1.test` (corpus root `/home/morten/sqllogictest/test`). **115,612 records, 103,564
passing, 0 wrong**, 44 s wall. **Run twice; every count below reproduced exactly.** Nothing in
the engine, the commit path or the plan format changed.

(#117's run used the same 14 directories but did not record which size level per `index/` dir;
this run pins `1000` for all eight, so the totals differ slightly from its 94,689. Every
conclusion below is a ratio inside this run.)

### 3.2 The counts

| | |
|---|---|
| statements compiled | **99,279** |
| uncompilable in the prepare-only pass (views, DDL-dependent surface) | 1,452 |
| skipped — join / compound / recursive / derived / INSERT / txn control | 31,969 |
| single-table but pinning **no** column (bare scans, aggregates) | 45,215 |
| filter too opaque to split into conjuncts (CASE / COALESCE jumps) | **0** |
| **statements yielding a candidate** | **22,095** |

Access path of those statements: `PkPoint` 0, `PkRange` 0, `IndexPoint` 1,693, `IndexRange`
10,747, **`FullScan` 54,870**. (No PK access at all: sqllogictest tables declare no PRIMARY KEY,
so mpedb synthesizes the hidden rowid (#94) and nothing filters on it.) Of the candidate-bearing
statements, **12,465 are already served** by an existing index or PK prefix and **9,630 are
novel**. Key-width histogram: **1 = 20,189, 2 = 1,820, 3 = 86** — narrow, exactly as #117's
table-width histogram predicts.

**The distinct-candidate counts, which is the number #118 asked for:**

| family | distinct candidates | of which **partial** | distinct predicates | occurrences under a partial |
|---|---|---|---|---|
| **W** (table, key cols) | **112** | 0 | — | — |
| **Pnull** (+ `IS [NOT] NULL`) | **188** | **79 (42.0 %)** | **5** | 727 (3.3 %) |
| **Plit** (+ `= const` / `IN`) | **188** | **79 (42.0 %)** | **5** | 727 (3.3 %) |

Concentration (W): the top 8 candidates cover **50.8 %** of occurrences, the top 32 cover
**94.2 %**. Per table: 6 tables carry candidates, the largest carrying **23**. Comparison
right-hand sides: **15,411 constant, 0 parameter**.

### 3.3 Reading it — four findings

**(1) The search space is an enumeration, not a search.** 99,279 statements collapse to **112**
whole-table candidates, at most 23 per table, and 32 of them cover 94 % of all occurrences. That
is small enough to cost *exhaustively* — every candidate against every plan — with no pruning
heuristic, no greedy selection, and no risk of missing the good one. And it grows **linearly in
tables**, not combinatorially, because the key width is 1–3 (#117's statement width is 1–3 for
the same reason): the candidate space of a 200-table Django schema is on the order of a few
thousand, still an enumeration. This is the finding that makes the whole idea tractable.

**(2) `Plit` ≡ `Pnull`, exactly — the count is parameterization-robust.** Admitting literal
equalities as partial predicates added **zero** candidates and **zero** occurrences. The reason
is a rule in the generator, and it is the right rule to make explicit:

> **An equality atom becomes a KEY column, never a predicate.** A predicate is what is left over
> that *cannot* be a key column. `WHERE status = 'active'` is better served by an index *keyed*
> on `status` (which serves every value of `status`) than by an index *restricted* to
> `status = 'active'` (which serves one).

This matters far beyond tidiness. The census is over a corpus that is 100 % unparameterized
(15,411 constant right-hand sides, **0** parameters), and the obvious worry was that its inline
literals would manufacture a partial-index candidate per literal and inflate the count into
garbage. They did not — because every literal equality was consumed as a key column instead. So
the candidate count above is the number a *parameterized* application would produce too, and it
is quotable at a real workload. The surviving predicates are exactly the class that survives an
ORM: all five are `IS NULL` shapes.

**(3) In *this* corpus partial indexes are a footnote — and this corpus cannot answer the
question.** 42 % of the candidate *set* carries a predicate, but those candidates account for
only **3.3 %** of occurrences, and all five distinct predicates are `col IS NULL`. That is not
evidence against partial indexes; it is evidence that sqllogictest cannot exhibit the pattern.
Its schema is `tab0..tab4(col0..col4)` of random numerics with NULLs sprinkled in — there is no
soft-delete column, no status enum, no tenant key, no `deleted_at`. The Django shapes that
motivate §5 are structurally absent from the input.

> **So the measurement bounds the candidate space (that result transfers) and does NOT settle
> whether partial indexes are the main event or a footnote in an application workload (that
> result does not transfer).** What would settle it is a captured Django statement stream. There
> is none on this machine and none in this repo — the `crates/mpedb-capi/workbench/` harness runs
> Django's suite under the C-API shim and emits **test-failure diffs, not SQL**. Capturing one
> (a statement log from a workbench run) is the cheapest next measurement and it is small; it is
> named here rather than guessed at.

What the corpus *does* establish about §5 is the mechanism: the predicates it produced are
`IS NULL` conjunctions, i.e. exactly the atom vocabulary §5.2 canonicalizes and §5.5's v1
implication test decides — including one two-atom conjunction (`col1 IS NULL AND col4 IS NULL`),
so multi-atom predicates are real even here.

**(4) The workload is nowhere near index-served, so the advisor has room.** 54,870 full scans
against 12,440 index accesses, and 9,630 of the 22,095 candidate-bearing statements have no
index covering their candidate — this despite the `index/` corpus files creating indexes
explicitly. An advisor run over this stream would have real work to do.

### 3.4 What the census does not measure

- **Joins.** 31,969 statements were skipped as not single-table; a join's `joined_filter`
  indexes the concatenated tuple, and attributing its conjuncts back to base columns is a
  separate (and easy, but unwritten) mapping. #117 measured the fan as 1–3 tables, so this is a
  bounded gap, but the inner side of an index nested-loop is exactly where an FK index pays and
  it is not counted here.
- **Frequency.** Occurrences are corpus occurrences, not production frequency (§1.1, P5).
- **Parameterized predicates.** Zero in this corpus, so §5.5's parameter limit and P6's reach
  are untested by measurement.
- **Selectivity.** No candidate's member fraction `m` was measured; §8's cost model is
  unexercised.

---

## 4. The (A) pipeline — registry in, recommendations out

```text
   statement source                candidate generation             costing              action
   ────────────────                ────────────────────             ───────              ──────
 ┌────────────────────┐
 │ live registry      │──┐
 │ (plan/<hash>, ≤4096)│  │       ┌──────────────────────┐   ┌──────────────────┐   ┌─────────────┐
 └────────────────────┘  ├──────▶ │ per plan, per table: │──▶│ hypothetical     │──▶│ recommend   │
 ┌────────────────────┐  │        │  eq cols ∪ access    │   │ re-plan under I  │   │ (default)   │
 │ offline stream     │──┘        │  + 1 range col       │   │ Δ MPEE cost      │   ├─────────────┤
 │ (test-suite .sql)  │           │  + ORDER BY tail     │   │ × freq(plan hash)│   │ auto-create │
 └────────────────────┘           │  + residual preds    │   │ − write tax      │   │ (opt-in)    │
                                  └──────────────────────┘   └──────────────────┘   └─────────────┘
                                            │                          ▲
                                            │ dedup by IndexId (§2)    │ freq + measured selectivity
                                            ▼                          │ from #88 cost history
                                     candidate set (small, §3) ────────┘
```

Four steps, each of which already has its machinery except where §10 says otherwise.

**4.1 Enumerate.** `sys_scan()` the `plan/` prefix, `decode_registry_plan` each record. Every
plan that fails to decode is skipped, exactly as the load path does (registry records are
untrusted). Or: read an offline statement stream and `mpedb_sql::prepare` each line — the same
generator, no database required. Both paths exist today.

**4.2 Generate.** §3's extraction, but keyed by `IndexId` (§2) rather than by a display string,
and with two additions the census does not need:

- **Prefix subsumption.** A candidate whose key is a prefix of another candidate's key, with an
  implied-or-equal predicate, is dropped in favour of the longer one — one index serves both.
  This is what keeps the set from being one candidate per statement.
- **Existing-index filter.** A candidate already covered by an existing index or the PK
  (prefix-covered, same predicate or a weaker one) is dropped. `covers` in the census is the
  same test.

**4.3 Cost.** §8.

**4.4 Act.** Recommend-only first, per DESIGN-MPEE-COST §6 staging. The entry point:

```rust
/// Derive index candidates from the workload and cost them. Reads only;
/// creates nothing. `IndexAdvice::ddl()` renders the exact CREATE INDEX.
pub fn recommend_indexes(&self, source: WorkloadSource) -> Result<Vec<IndexAdvice>>;

pub enum WorkloadSource {
    /// The live plan registry (bounded: MAX_REGISTRY_PLANS).
    Registry,
    /// An offline statement stream — a test-suite capture. Not capped.
    Statements(Vec<String>),
}
```

That name is load-bearing for §9: it is the symbol an error message points at.

---

## 5. Partial indexes

mpedb has `CREATE [UNIQUE] INDEX [IF NOT EXISTS] n ON t (col [ASC|DESC], …)` and **no**
`WHERE` clause: `parse_create_index` (`crates/mpedb-sql/src/parser/ddl.rs:988-1010`) returns
right after `)` and `parse_ddl` then calls `expect_eof`, so a trailing `WHERE` is a hard parse
error. `DdlStmt::CreateIndex` has no slot for a predicate and `IndexDef` has no field for one.

### 5.1 Syntax

```sql
CREATE [UNIQUE] INDEX [IF NOT EXISTS] <name> ON <table> ( <col> [, …] ) [ WHERE <pred> ]
```

sqlite's and PostgreSQL's spelling, deliberately — the ORM/LLM writing the migration already
knows it. Restrictions on `<pred>`, all checkable at parse/bind time:

- references **only columns of `<table>`**, no subquery, no `EXISTS`, no aggregate;
- **no parameters** — a predicate is a property of the index, not of a query;
- **deterministic only** — no host UDF (`ExprProgram::has_host_call`), no `'now'` / the
  `@statement_instant` context slot, nothing that reads a clock or a session. An index whose
  membership drifts with wall time is a silently corrupt index.
- the same collation rules as a key column: an explicit `COLLATE` is allowed and is part of the
  identity (§2.2).

`UNIQUE … WHERE p` means unique **among members**. That is the Django soft-delete pattern
(`UNIQUE(email) WHERE deleted_at IS NULL`) and it is a genuine expressiveness gain, not just a
size win.

### 5.2 Canonical predicate form

The predicate must have exactly one representation, because it is hashed into `IndexId` and
because §5.5's implication test compares representations.

> `predicate_canonical_bytes` := the top-level `AND` conjuncts, each rendered as a tagged atom
> over **column names**, sorted by (column name, atom tag, operand bytes), deduplicated.
>
> Atoms in v1: `IsNull(col)`, `IsNotNull(col)`, `Cmp(col, op, const)` where `op ∈ {=,≠,<,≤,>,≥}`
> and `const` is a `Value` in its canonical encoding, `In(col, [const…])` with the constant list
> sorted and deduped, and `Not(atom)`. Anything else — a disjunction at the top level, an
> expression on the left of a comparison, a `LIKE`/`GLOB`/`REGEXP` — is **refused at CREATE
> time**, not silently canonicalized. A predicate mpedb cannot canonicalize is a predicate it
> cannot reason about in §5.5, and an index it cannot reason about is an index it must not
> build.

This is stricter than sqlite (which accepts almost any deterministic expression) and that is the
right trade for mpedb: the whole value of the feature is the planner's ability to *prove* a
query may use the index, and that proof runs over this normal form.

### 5.3 Storage and membership

`IndexDef` grows one field:

```rust
pub struct IndexDef {
    pub columns: Vec<u16>,
    pub unique: bool,
    /// Partial-index predicate (canonical form, §5.2). `None` = whole-table —
    /// the only shape before canonical-bytes v10.
    pub predicate: Option<IndexPredicate>,
}
```

That is a **canonical-bytes version bump** (v9 → v10) and therefore a `Schema::hash` change and
a one-time invalidation of every persisted plan — acceptable under the standing no-backward-
compat rule, and it is exactly what the format-version window is for.

**The membership rule.** Today there is exactly one, in exactly one function,
`index_row_key` (`crates/mpedb-core/src/engine/mod.rs:345-366`): *a row with ANY NULL indexed
column has no entry* (`if v.is_null() { return None }`). Every call site treats `None` as "skip
this index for this row". The new rule is the conjunction, and it lands in the same function so
that there is still exactly one:

> A row is a member of index `I` iff **(no indexed column of `I` is NULL)** **and**
> **(`I.predicate` evaluates to TRUE on that row — `eval_filter` semantics: NULL and FALSE both
> mean non-member)**.

Keeping it in `index_row_key` matters: `insert_row`, `delete_by_pk`, `update_by_pk`, the
UNIQUE pre-checks, `scan_by_index` and the executor's index probe all funnel through it, so a
single change keeps read and write agreeing about membership. Two things it must NOT do: it
must not allocate (it is on the write path per row per index), and it must not need anything
beyond the row it is given — which the §5.1 restrictions guarantee.

### 5.4 Maintenance on UPDATE — where a row enters or leaves

`update_by_pk` (`crates/mpedb-core/src/engine/write.rs:676`, index work at `:751-779`) already
does delete-old-then-insert-new per index, with both sides `Option`: *"a row that gained a NULL
loses its entry, one that lost a NULL gains one, and the counts move accordingly."* Partial
membership is **the same shape of transition**, so the existing code is already the right code —
what changes is only what fills the two `Option`s:

| `was_member(old)` | `is_member(new)` | action |
|---|---|---|
| false | false | nothing — **the write-tax saving** |
| false | true | insert new entry, `icount += 1` (row **enters**) |
| true | false | delete old entry, `icount -= 1` (row **leaves**) |
| true | true | if the key bytes changed: delete old, insert new; else nothing |

Two details that are easy to get wrong:

- **The "changed?" short-circuit must not skip a partial index.** Today an index whose columns
  did not change is skipped entirely (`index_value_equal` over the *encoded* bytes,
  `write.rs:752-754,1738-1749`). With a predicate, an index must **also** be visited when any
  column *the predicate reads* changed — otherwise `UPDATE t SET deleted_at = now()` silently
  leaves a ghost entry in `idx WHERE deleted_at IS NULL`. So the skip test becomes: skip iff no
  key column changed **and** no predicate column changed. The predicate's column set is static
  and computed once per schema load, next to `sec_indexes` in `SchemaBundle`.
- **The UNIQUE pre-check must be membership-aware.** `write.rs:698-725` pre-checks changed
  unique indexes before mutating. For a partial unique index, a new row that is a *non-member*
  must skip the check entirely — otherwise `UNIQUE(email) WHERE deleted_at IS NULL` rejects a
  second soft-deleted row with a duplicate email, which is the exact case the feature exists for.

**Cost, honestly.** Per updated row per partial index, the added work is at most two
`ExprProgram` evaluations over an already-decoded row — a stack-machine run with no I/O, over a
predicate the §5.2 normal form caps at a handful of atoms. In exchange, the non-member cases
skip a B+tree descent and a page COW. For the Django shapes that motivate this
(`WHERE deleted_at IS NULL` over a table that is mostly live rows) that is roughly a wash on
UPDATE; the win is on the **read** side and on **space**, and on tables where members are a
small minority it is a large win on writes too.

### 5.5 The planner probe — when may a partial index answer a query?

> **A partial index `I` with predicate `p` may serve query `Q` with predicate `q` only if
> `q ⇒ p`.** Getting this wrong does not make a query slow — it makes it return **fewer rows
> than exist**, which is the one class of bug mpedb refuses to ship. So the implication test is
> **sound by construction and deliberately incomplete**: it proves implication or it declines.

The test, over the §5.2 canonical form:

1. Split `q` into conjuncts, including the ones `extract_access` consumed (the access path's
   pinned equalities are conjuncts of `q` — the probe runs *before* the residual is finalized,
   or it must re-inject them).
2. Canonicalize each conjunct into the same atom vocabulary. A conjunct that does not
   canonicalize is simply not available as evidence (it does not disqualify — `q ∧ junk ⇒ p`
   whenever `q ⇒ p`).
3. `I` is usable iff **every** atom of `p` is entailed by **some** atom of `q`, under this
   entailment lattice — and only this one:

   | index atom `p_i` | entailed by query atom `q_j` when |
   |---|---|
   | `IsNull(c)` | `q_j` is `IsNull(c)` |
   | `IsNotNull(c)` | `q_j` is `IsNotNull(c)`, or `Cmp(c, op, v)` with `v` non-NULL and `op ≠ ≠`, or `In(c, S)` with `S` non-empty and NULL-free |
   | `Cmp(c,=,v)` | `q_j` is `Cmp(c,=,v)` (same canonical `Value`) |
   | `In(c,S)` | `q_j` is `Cmp(c,=,v)` with `v ∈ S`, or `In(c,S')` with `S' ⊆ S` |
   | `Cmp(c,>,a)` | `q_j` is `Cmp(c,>,b)` with `b ≥ a`, or `Cmp(c,≥,b)` with `b > a`, or `Cmp(c,=,v)` with `v > a` |
   | `Cmp(c,≥,a)`, `<`, `≤` | symmetric |

   Value comparison uses **the column's collation**, which is why collation is part of `IndexId`:
   `Cmp(c,=,'Bob')` entails `Cmp(c,=,'bob')` under `NOCASE` and does not under `BINARY`, and an
   index built one way must never be probed by the other.

4. **v1 ships rows 1–3 of that table only** (exact atom match plus the `IsNotNull` weakenings).
   The range subsumption rows are a v2 with a differential test per row, because an off-by-one
   there is a wrong answer, not a slow query.

> **SHIPPED 2026-07-22** — `crates/mpedb-sql/src/planner/partial.rs`, wired into
> `extract_access`'s IndexPoint and IndexRange loops. Rows 1–3 exactly as written above.
> Three deviations from this section, all in the narrowing direction:
>
> - **The predicate is re-parsed and re-bound, not stored canonical.** `IndexDef.predicate`
>   is the source `String` (`c4d1a90`, canonical bytes v10), not the §5.2 atom form, so the
>   probe parses + binds it against the table and canonicalizes then. Plan-time only, and only
>   for a table that actually has a partial index whose columns the query covers.
> - **Structural `Value` equality, not collated equality.** §5.5 says compare under the column's
>   collation; v1 compares derived `PartialEq`. That can only *decline* (`'Bob'` vs `'bob'` under
>   `NOCASE`), never over-claim, because both atoms name the same column of the same table — so
>   identical `(column, op, value)` triples are the identical predicate whatever the collation is.
>   Collated entailment is v2, with the `IndexId` collation identity §2.2 already provides.
> - **`ClassCmp` / `CollateCmp` are not canonicalized at all.** They carry an affinity and a
>   collation the atom does not record; admitting them is what would make structural equality
>   unsound. A typeless-column or explicit-`COLLATE` predicate therefore makes its index
>   unusable, and such a conjunct is not evidence.
>
> Two sites that were choosing an index with **no** partial guard were closed with the same
> commit: `extract_join_access` (the nested-loop inner probe — it is handed only the ON
> equalities, so the WHERE that would prove membership is out of scope; it refuses partials
> outright, as `agg_index_choice` already did) and the MPEE solver's `known()`.
>
> **Still open, and it is the engine's half:** membership is not evaluated on write
> (`engine::index_row_key` ignores `predicate`), so a partial index is currently built FULL.
> Every §5.5-approved probe reads a full tree correctly — it is a superset of the members, and
> the query's own conjuncts are either the probe's key parts or its residual — but the write tax
> and the space win of §5.3/§5.4 are not there yet, and `UNIQUE … WHERE` stays refused at CREATE
> until they are.

**The parameter problem, and it is the real limit.** `WHERE status = $1` does **not** imply
`WHERE status = 'active'` — the compiler does not know `$1`. In a Django workload most selective
equalities are bound parameters, and only the NULL-shaped predicates arrive as literal text.
So a partial index on a parameterized column is unreachable by this probe. Two honest options:

- **v1: accept it.** Partial indexes serve `IS NULL`-shaped and true-constant predicates. §3's
  Pnull family is exactly the reachable set, and that is why it is measured separately.
- **v2, and it fits the content-hash contract cleanly:** a guarded access path,
  ```rust
  AccessPath::Guarded {
      when: ExprProgram,          // over PARAMS only; no columns, no clock
      then: Box<AccessPath>,      // the partial index
      otherwise: Box<AccessPath>, // the fallback
  }
  ```
  The guard is compiled into the plan bytes, so the plan hash still determines execution exactly;
  the *choice* is per-execution but the *plan* is not. This is the same "plan = what must be
  agreed across processes; strategy = what one execution decides for itself" split
  DESIGN-MPEE-SOLVER §9.4 draws, landed one level lower. It is **prerequisite P6**, not v1.

---

## 6. The two modes, and how they compose

### 6.1 (A) — ahead of time, from the workload

Morten's primary. Cost at runtime: **zero**. It needs the workload up front, and a test suite
is the workload: running an app's suite against mpedb populates the registry (or, better, is
captured as a statement stream and fed to `recommend_indexes(WorkloadSource::Statements(..))`,
which is not subject to the 4,096 cap of §1.1).

What (A) can get right: **which columns are filtered, joined, ordered, and with which
literal predicates** — all of it exactly, because it reads plans and not samples.
What (A) cannot get right: **how often each statement runs in production** and **how selective
each predicate is on real data**. A test suite's frequency distribution is the suite's, not the
app's, and its data volume is not the app's either. So (A) produces a *ranked* candidate set
under an assumed uniform frequency and a default selectivity, and it is honest about which
number is assumed (§8).

### 6.2 (B) — adaptive, and where it attaches

Morten's objection is exact: *"da blir det forsinkelse basert på bruk"* — the first N queries
pay, and the build itself adds latency. The composition answers both halves separately:

- **The first-N-queries cost is removed by (A)**, for everything the suite covered. (B) never
  starts from zero.
- **The build latency is removed by making the build background** (§7), never synchronous with
  a query. A query that would benefit from a not-yet-built index simply runs its current plan;
  when the index appears, `schema_gen` bumps, plans re-prepare, and the *next* execution is
  faster. No statement ever waits for an index.

(B) needs exactly two inputs, and **both are already specified in DESIGN-MPEE-SOLVER §9.1** as
the measurements #88 should persist first:

| (B) needs | §9.1 item | keyed on |
|---|---|---|
| how often this statement runs | 4 — "execution count and total work per plan hash" | **plan hash** |
| how selective this predicate really is | 3 — "actual rows out of a residual filter (per compiled `ExprProgram`)" | **plan hash** |

That is the whole attachment. **(B) is not a new subsystem**; it is (A)'s cost step re-run with
measured numbers in place of assumed ones, triggered when the measured selectivity of a
candidate's predicate diverges from the assumption by more than a configured factor. And #117's
verdict carries over unchanged: *key on the plan hash, not on the shape* — a candidate's
frequency is the sum over the plan hashes that would use it, never a per-footprint estimate,
because the across-plan-within-footprint cost spread is a median 217×.

### 6.3 Composition, stated as a rule

> **(A) is the cold-start set; (B) only ever refines what (A) got wrong.** (A) may propose and
> (with auto-create opted in) build. (B) may (i) raise a candidate (A) ranked below the line,
> (ii) demote or drop an index (A) built that measurement shows unused, (iii) never propose a
> shape (A)'s generator cannot express. (iii) is what keeps the two from disagreeing: both read
> the same candidate generator; only the cost inputs differ.

---

## 7. Building the index — background, droppable, never on the write path

The hard constraint, standing since #88: *index maintenance/building is cheap / background /
opt-in — NEVER on the write hot path.* Today `CREATE INDEX` violates the spirit of it:
`WriteTxn::create_index` (`write.rs:1336-1395`) holds the single writer lock while it scans the
entire PK tree, decodes every row, **materializes the whole entry set in an in-memory `Vec`**
(`:1362-1372`), builds the tree, and publishes — one commit, all or nothing. On a large table
that is an unbounded memory spike and a total write stall. It is fine as a DDL operation a human
issues; it is not fine as something an advisor does on its own.

**Does the #77 queue fit?** Its *semantics* fit perfectly and its *plumbing* does not.

Fits: the queue is a durable ordinary mpedb table `mq_task` (`crates/mpedb-cli/src/queue.rs:92-107`),
so it inherits COW/MVCC crash safety; claiming is one atomic autocommit `UPDATE … RETURNING`
under the single writer lock, so concurrent runners claim disjoint tasks with no `SKIP LOCKED`;
a SIGKILLed runner's claim expires by lease and is reclaimed to `pending`; there is backoff, a
`failed` and a `dead` state, and `mpedb queue run` drains and exits with no resident daemon.
A background index build wants exactly those properties, and the resume cursor (a PK key) fits
in the task's `payload`/`result`.

Does not fit, and both are real:

1. **The queue lives in `mpedb-cli`, above the facade.** The engine has no queue. A build job
   driven from there would be a new CLI subcommand or proc, not something `Database` can schedule.
2. **A queued task can only run a stored procedure** (`mpedb_proc::ProcEngine`), and
   `mpedb-proc` has **no DDL path at all** — nothing in it references `parse_ddl` or `DdlStmt`.
   So today a queued task literally cannot run `CREATE INDEX`.

And underneath both, the blocking one:

3. **There is no way to express a half-built index.** `IndexDef` has no state field, and the
   planner will choose an index the moment it appears in `TableDef.indexes`
   (`planner/access.rs:102`). A background build must publish the index *before* it is filled —
   which today produces an index the planner uses and the data is not in. That is a wrong-answer
   bug, and it is **prerequisite P2**.

So: **the #77 queue is the right home, and it is not ready to be the home.** The honest staging
is (i) P2 (an index state bit), (ii) P4 (a chunked, resumable build), (iii) then attach to #77.
Until then, an advisor **recommends** and a human or a migration issues the synchronous
`CREATE INDEX` — which is DESIGN-MPEE-COST's recommend-only stage anyway, so nothing is blocked
that was supposed to ship first.

**Droppable.** DESIGN-MPEE-COST §4 promises "every auto-action is droppable". **mpedb has no
`DROP INDEX`** — `DROP INDEX t1i1` currently errors with *"expected `POLICY`"*, and nothing
anywhere removes an element from `TableDef.indexes`. So the "drop the unused" half of the
advisor is unimplementable today (**prerequisite P3**), and — worse — auto-create without
auto-drop is a ratchet. **Rule: auto-create must not be enabled before DROP INDEX exists.**
Recommend-only has no such dependency and is the correct first stage regardless.

---

## 8. The honest cost model

> An index that helps one query and slows every write is a loss. The advisor's job is not to
> find indexes that help; it is to find indexes whose help exceeds their tax.

For a candidate `I` on table `T` with member fraction `m ∈ (0,1]` (m = 1 for a whole-table index):

```text
benefit(I) = Σ over read plans p:   freq(p) · ( cost(p | schema)  −  cost(p | schema + I) )
tax(I)     = Σ over write plans w:  freq(w) · rows(w) · m · c_write
space(I)   = |T| · m · (key_width + pk_width) · (1 + btree_slack)
verdict:   build  iff  benefit(I)  >  w_write · tax(I)  +  w_space · space(I)  +  floor
```

Each term, and where the number actually comes from:

- **`cost(p | schema + I)`** — the *hypothetical* index, `hypopg`-style. mpedb can do this
  honestly and cheaply, which is unusual: `prepare` is a pure function of (SQL, schema,
  row counts), so the advisor clones the `Schema`, calls `with_added_index`, re-prepares, and
  reads MPEE's cost for both. No file is touched. This is the single strongest argument for
  doing the advisor in mpedb rather than as an external tool.
- **`freq(p)`** — #88's per-plan-hash execution count (DESIGN-MPEE-SOLVER §9.1 item 4).
  **Does not exist yet (P5).** Until it does, mode (A) assumes uniform frequency over distinct
  plans, and the recommendation output must **say so**, because uniform-over-distinct-plans is
  wrong in a specific direction: it over-weights the long tail of one-off statements a corpus or
  test suite is full of. #117's caveat is the same one — corpus SQL is literal-heavy, so distinct
  plans over-count relative to a parameterized app.
- **`m`, the member fraction** — the selectivity of the partial predicate, and the one number
  that decides whether a partial index is worth it. Sources, best first:
  1. **Measured**: #88 §9.1 item 3, "actual rows out of a residual filter", for a plan whose
     residual *is* that predicate. This closes a nice loop — the candidate was *derived* from
     such a plan, so by construction the plan exists.
  2. **Sampled**: a background counting pass. Cheap (one predicate eval per row, no writes) and
     it is exactly the "background-sampled ANALYZE-like pass" DESIGN-MPEE-COST §1 already
     specifies.
  3. **Assumed**: a default. `IS NULL` on a nullable column with no other information is a coin
     flip, and a recommendation resting on an assumed `m` must be labelled as such.
- **`c_write`** — the per-entry write cost: one B+tree descent + insert/delete, plus (partial
  only) one `ExprProgram` evaluation per candidate index per written row, paid *whether or not*
  the row is a member. That last part is the partial index's honest overhead and the model must
  charge it: a partial index with `m = 0.01` still costs a predicate evaluation on 100 % of
  writes.
- **`rows(w)`** — from the write plan's own `KeyAccess`: `Point` ⇒ 1, `Range`/`Full` ⇒ the
  table's exact `row_count` scaled by whatever selectivity is known. mpedb's exact transactional
  `row_count` is the ground truth here and is already available.
- **`w_write`, `w_space`, `floor`** — the tunables DESIGN-MPEE-COST §4 calls "the crucial knob".
  `floor` is what stops a candidate that wins by 3 % from churning the schema.

**Two costs the naive model forgets, and both belong here:**

- **The re-prepare storm.** Every `CREATE INDEX` invalidates every persisted plan (§2.4).
  Adding *k* indexes one at a time costs *k* full re-prepares of the working set. Charge it, and
  prefer to apply a batch of recommendations in one commit.
- **The build itself.** A full scan + tree build of `|T| · m` entries, once. Amortized over the
  index's expected life, this is usually noise — but for a candidate that helps a statement
  running twice a day on a 10⁸-row table it is not, and it is the difference between "helps" and
  "worth it".

**Staging** (inherited verbatim from DESIGN-MPEE-COST §6, and it is not negotiable):
**recommend-only → opt-in auto-create → auto-drop of the unused.** Auto-create additionally
gated on P3 (DROP INDEX) per §7.

---

## 9. Responses that name the method — writing for the reader who is a model

Morten: *"Fordi de fleste (alle?) nå bruker LLMer kan vi bygge inn feedback som peker på
… smartere indeks-bygging, enn klassisk indeks."* Then, correcting the first instinct:
*"Eks bare peke på interne APIer og funksjoner, dokumentasjon finner som regel LLMer selv?"*

The second version is the right one, and the reason is worth writing down so it does not get
"improved" back into URLs later.

**The rule: name the SYMBOL, not a URL.**

1. **A URL in an error message is an API you must keep alive** across every doc reorganisation,
   forever. A symbol name is already load-bearing — if it moves, the code moved, and the message
   moves with it in the same commit. There is no such thing as a dead symbol reference that
   compiles.
2. **The reader is usually inside the repo.** The consumer of an mpedb error is increasingly an
   agent working on this codebase, with grep and the file tree. `Database::recommend_indexes`
   and `design/DESIGN-WORKLOAD-INDEXES.md §5.1` are *more* actionable to it than a link, because
   it can read the real thing rather than a doc that may lag.
3. It helps humans identically: an IDE resolves a symbol, `rg` resolves a path.
4. It is mpedb's existing discipline, not a new one — errors already name the manual fix. Naming
   the API is that move, one step further.

**Which responses carry a pointer, and to what.** The restraint is the point: a pointer on a
routine syntax error is noise, and noise trains the reader to ignore pointers. Only where the
right move is **non-obvious** — i.e. where an LLM trained on twenty years of "add an index on
the FK column" would otherwise emit exactly that:

| situation | pointer | why non-obvious |
|---|---|---|
| `CREATE INDEX` on a whole table where the observed workload implies a **partial** one | `CREATE INDEX … WHERE <pred>` (the syntax) + `Database::recommend_indexes` | the reader does not know mpedb has partial indexes, nor that the workload already implies the predicate |
| a plan fell back to `FullScan` where a candidate index would have served | `Database::recommend_indexes` | the reader knows *an* index is wanted; it does not know mpedb can derive *which* |
| an index refused on an `ColumnType::Any` (typeless) column | `crates/mpedb-sql/src/planner/access.rs` `typeless` | a legal-looking index that the planner can never choose is worse than a refusal |
| a partial-index predicate refused as non-canonicalizable (§5.2) | `design/DESIGN-WORKLOAD-INDEXES.md §5.2` | the accepted atom vocabulary is mpedb-specific and not guessable |
| a surfaced recommendation | `Database::recommend_indexes` + the exact `CREATE INDEX` DDL | so the caller can act, and re-derive |

**Not** on: parse errors, type mismatches, constraint violations, `PlanInvalidated`, or anything
in the RLS-redacted family (`Error::WriteRejected`, `Error::PolicyViolation`) — those withhold
detail deliberately (DESIGN-MULTIDB §6.5) and a pointer must not become the side channel that
gives it back.

**Honesty rule.** A recommendation must never read as if it happened. The wording is
*"recommended, not created"*, always, for as long as the staging is recommend-only:

```text
index recommended (NOT created): CREATE INDEX ON orders (customer_id) WHERE status = 'open'
  derived from 1,204 statements in the plan registry; estimated 3 % of rows indexed
  frequency assumed uniform (no execution-count history — see DESIGN-MPEE-COST.md §1)
  see Database::recommend_indexes
```

Three properties of that message are deliberate: it says **NOT created**; it says where the
recommendation came from and how many statements back it; and it **names the assumption**
(`frequency assumed uniform`) rather than presenting an estimate as a measurement. A model that
reads it can act correctly, and — the point — can also tell that the estimate is soft.

**Mechanically**: this is a `hint: Option<String>` (or a `doc: &'static str`) alongside the
existing message, not string concatenation into `Error::Unsupported(String)`, so a caller that
renders errors machine-readably can separate them. Small, additive, and out of scope for this
task.

---

## 10. Prerequisites — named, not designed around

| # | prerequisite | blocks | today |
|---|---|---|---|
| **P1** | `IndexDef.predicate` field + canonical-bytes **v10** + `CREATE INDEX … WHERE` in the parser | all of §5 | **SHIPPED (storage + parse):** predicate text on `IndexDef`, v10 wire, CREATE accepts `WHERE`; planner skips partials for access; UNIQUE partial refused until membership eval; non-deterministic UDF gate on C-API |
| **P2** | an index **state bit** (`Building`/`Ready`) that the planner honours | any background build (§7); `access.rs:102` picks an index the instant it appears | no state field exists; the planner cannot be told to skip one |
| **P3** | **`DROP INDEX`** | the "drop the unused" half of the advisor, and therefore auto-create (§7) | does not exist; `DROP INDEX x` errors *"expected `POLICY`"*. **Positional `index_no` is the landmine**: removing element *k* renumbers every later index and silently repoints every catalog key and every persisted `AccessPath::IndexPoint`. §2's `IndexId` is what makes a tombstoning DROP safe |
| **P4** | a **chunked, resumable** index build | §7 | `create_index` materializes every entry in one in-memory `Vec` and holds the writer lock throughout (`write.rs:1362-1372`) |
| **P5** | **per-plan-hash execution count** (DESIGN-MPEE-SOLVER §9.1 item 4) and **measured residual selectivity** (item 3) | the `freq(p)` and `m` terms of §8; all of mode (B) | neither is persisted; `last_used_txn` is recency, not frequency (§1.1) |
| **P6** | `AccessPath::Guarded` — a param-guarded access path | partial indexes on **parameterized** predicates (§5.5) | the access path is fixed in the plan bytes; `WHERE c = $1` can never prove `c = 'active'` |

P1 and P5 are the two that gate anything shipping. P2+P4 gate the background build. P3 gates
auto-create. P6 is the difference between partial indexes being useful for `IS NULL` shapes and
being useful for everything.

## 11. What would make the whole idea unworkable, and does not

Three candidates were checked; none of them lands, and the reasons are worth recording:

1. **"The registry only keeps plan bytes, so predicates are lost."** False — it keeps the SQL
   text *and* the blob, and the blob's `filter` is the full expression IR (§1). This was the
   single largest risk and it is retired.
2. **"The candidate space is a combinatorial search."** False, measured — §3.
3. **"Content-addressed indexes conflict with mpedb's positional everything."** They coexist:
   `IndexId` is an *identity*, `index_no` stays the *addressing*, and the catalog gains one
   record mapping the first to the second (§2.3). Positional numbering is already never reused
   because there is no DROP INDEX; `IndexId` is what makes it safe to add one.

The real limits are P6 (parameterized predicates) and P5 (no frequency history) — both of which
narrow the feature rather than invalidate it.

**The one open risk, stated plainly.** The *partial*-index half of the pitch rests on a class of
predicate this corpus cannot exhibit (§3.3), and the measurement that would confirm it — a
captured Django statement stream — does not exist yet. If that capture comes back showing that
an ORM's selective predicates are overwhelmingly **bound parameters** rather than literal text,
then partial indexes are unreachable without **P6**, and P6 moves from "v2 nicety" to "the
feature". The candidate-space result (§3.3 finding 1) and the identity rule (§2) are unaffected
either way — they are what make the *whole-table* derivation work, and that half is measured.
**Capture the stream before building P1.**
