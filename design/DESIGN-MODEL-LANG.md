# DESIGN-MODEL-LANG — the workload model, at any resolution

**Status: v1 SHIPPED 2026-07-23 (stage M1 of the post-A–E program).** The
language lives in `mpedb-types/src/model.rs`; storage/validation/synthesis in
`mpedb/src/model.rs`; consumers via `WorkloadSource::Model` and the CLI
(`mpedb model set|show`, `mpedb advise --model`). Preset models in
[`models/`](../models) — one per benchmark, because the benches ARE the
language's test corpus.

Reads with: [DESIGN-WORKLOAD-INDEXES.md](DESIGN-WORKLOAD-INDEXES.md) (#118 —
the advisor this feeds), [DESIGN-MPEE-GENERAL.md](DESIGN-MPEE-GENERAL.md)
(the cost seam archetypes will parameterize), [DESIGN-MPEE-COST.md](DESIGN-MPEE-COST.md)
(the catalog the model complements).

---

## 0. The idea

Morten's framing, which this document makes mechanical: **most people choose a
database after planning their application — but what they actually produced in
that planning is a MODEL, and the database switch is really a model switch.**
"This will be ordinary sqlite3-style usage" is a model. A Django `models.py`
is a model. A list of the exact SQL statements with frequencies is a model.
They differ only in **resolution** — and every one of them is information an
engine can optimize with, if only there were somewhere to put it.

The workload model is that place: a TOML document, stored IN the database,
describing how the application uses its data. Three resolutions, one language:

- **Level 0 — archetype.** One line: `archetype = "sqlite3-general"`. The
  vaguest useful claim, and still a claim: it tells the engine what NOT to
  prepare for (no traversal machinery, no vector statistics).
- **Level 1 — shapes.** Django-model altitude: per-table **roles** (`fact`,
  `dimension`, `edge`, `embedding`, …) and **access declarations**
  (`filter-eq` on these columns, `traverse` from this one, `knn` over that
  one) with relative weights.
- **Level 2 — statements.** The exact SQL with execution weights — what the
  #118 advisor already ingests.

**Refining never changes what a consumer means, only how sharply it can act.**
A level-0 model selects presets; adding level-1 shapes makes index advice
concrete; adding level-2 statements makes it exact. The same document grows in
place — planning output becomes an artifact that ships with the schema instead
of dying in a wiki.

## 1. The language (v1)

```toml
[model]
name = "star-olap"                # optional identifier
archetype = "star-olap"           # optional; the level-0 claim
description = "…"                 # optional, for humans

[[model.table]]                   # level 1, zero or more
name = "fact"                     # must exist in the schema (validated)
role = "fact"                     # fact|dimension|edge|embedding|document|log|queue|generic
read_write = "read-heavy"         # read-heavy|write-heavy|balanced

  [[model.table.access]]
  kind = "filter-eq"              # filter-eq|filter-range|join-key|order-by|point|traverse|knn
  columns = ["product_id"]        # must exist in the table (validated)
  weight = 0.4                    # relative, positive; only ratios matter

[[model.statement]]               # level 2, zero or more
sql = "SELECT … WHERE customer = $1"
weight = 120                      # executions per unit of workload
```

Parsing is strict the way the config is strict: unknown fields are errors (a
typo must not silently describe a different workload), every enum names its
valid values in the refusal, and an empty model is refused — a model that says
nothing is a mistake, not a model.

**Roles are declarations other features consume.** `role = "edge"` is what
will let stage M3's `a :->: b` know WHICH table joins without being told at
every call site; `role = "embedding"` is what points `:~k:` at its column.
The model is the noun the operator sugar refers to.

## 2. Storage and validation

- `Database::set_model(toml)` validates structurally (the language) and
  against the live schema (every named table and column must exist — refusals
  name the offender), then stores the SOURCE verbatim in the sys-keyspace
  (`model/current`), shared by every attached process.
- The model is **advisory metadata**: it never enters plan bytes, plan hashes,
  or the schema hash, so storing one cannot change any query's meaning — only
  what the advisory layer recommends. Hence no `schema_gen` bump: there is
  nothing cached to invalidate, and other processes see the record on their
  next snapshot like any other row.
- `Database::model()` / `model_source()`; CLI `mpedb model set|show`.

## 3. Consumers (v1 wired, and the ladder ahead)

**Wired now:**
- **The advisor.** `WorkloadSource::Model` — level-1 shapes are *synthesized*
  into exactly the statement forms the #118 extraction understands (the
  reverse of extraction: `filter-eq [a,b]` → `SELECT * FROM t WHERE a = $1
  AND b = $2`, `traverse [src]` → the per-level probe `WHERE src = $1`,
  `order-by` → the sort-tail shape, `point` → the PK probe so SERVED counts
  reflect declared point traffic), level-2 statements pass through, and every
  statement carries its **weight** into the candidate counts — the advisor
  compares counts, and only their ratios mean anything. `knn` synthesizes no
  B-tree candidate (a vector index is a different structure); the declaration
  exists for the cost/analyze layer.
- **`mpedb advise <target> --model <file|stored>`.**

**M3 landed 2026-07-23: `:sym:` custom operators** (SQL-EXTENSIONS.md) — the
role declarations became load-bearing: `mpedb op install-model` derives `:->:`
from `role = "edge"` + a two-column traverse declaration and `:~:` from
`role = "embedding"` + knn. The model is the noun; the operator is the verb.

**M2 landed 2026-07-23: stored SQL functions.** `Database::create_function` /
`mpedb fn define` compiles a PySpell body (full procedure subset — loops) to
content-hashed IR in the sys-keyspace (`func/` name bindings + `funch/`
content-addressed blobs, schema_gen-gated like views), and the binder resolves
the name into `Instr::SpellCall(hash)` — so a plan calling one is
deterministic across every attached process and rides the shared plan
registry, the exact shareability host UDFs are denied. Execution runs under a
fixed instruction budget with no database bridge (SQL-in-function refused at
define, re-checked at load against forged blobs).

**Next rungs (designed, not built):** archetype → MPEE cost presets and
`analyze()` cadence; roles → operator-sugar resolution (M3); the PySpell
cost-policy hook reads the model as one of its inputs (M5) — the model is the
top of the cost-input ladder, statistics are the bottom, and both flow through
the same CostSource seam.

**The maintenance rung (user, mid-M1): model-declared DERIVED structures.**
The graph the graph bench hand-built (an edge table with src/dst indexes and
a composite) could instead be *declared*: a future `[[model.derived]]` block
names a structure ("edge table derived from orders.customer → orders.referrer",
"closure cache to depth 4"), and the ENGINE generates the maintenance — 
PySpell trigger bodies fired on writes to the source tables, so the derived
data is built and kept current inside mpedb rather than by application code.
The declaration stays in the model because a declaration is more robust and
more generic than hand-written triggers: the engine can regenerate the
maintenance when the schema moves, price it (a derived structure is a write
tax the advisor can weigh, exactly like an index), and drop it when the model
no longer claims it. Blocked on DESIGN-TRIGGERS landing (design-only today);
the model language reserves the concept now so M3's operators and this rung
refer to the same nouns.

## 4. The presets are the benches

[`models/`](../models) ships one model per benchmark: `star-olap.toml`
(BENCHMARKS-OLAP), `graph.toml` (BENCHMARKS-GRAPH), `rag.toml`
(BENCHMARKS-VECTOR), `routing.toml` (M4, archetype-level until its schema
lands), and `sqlite3-general.toml` — the founding example: *"this is plain
sqlite3 usage, not a graph database"* as a one-line model. Each is validated
against its bench's schema in `cargo test`, so the language cannot drift from
the workloads it describes — suites-are-the-spec, applied to the new language.

## 5. What failure looks like

- A model that silently tolerates a misspelled table/column — it would then
  describe a different application. Validation refuses by name.
- Synthesis producing statement shapes the advisor cannot extract candidates
  from — the equivalence test (level-1 advice ⊇ level-2 advice on the same
  workload) is the regression net.
- The model leaking into plan identity — it must never; a model change that
  altered a plan hash would violate the determinism law for a document that
  is supposed to be advisory.
