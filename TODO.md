# TODO — the P-plan, reconstructed from the tree

The phase plan lived in commit messages and conversation; this file makes it
explicit and checkable. Status is read from the code and the measured docs, not
from memory. Anything listed as done has a commit; anything open names its
blocker.

**Last reconciled: 2026-07-22 at `bc45e69`.**

---

## Where the P-plan actually stands

| phase | what it was | status |
|---|---|---|
| P1–P4 | storage, MVCC/COW, multi-process writers, group commit, plans-as-content, SQL surface to parity-ish | **done** — the engine these rest on has survived crash/powerloss/corpus |
| P5 | schema v2 (dense stable ids, `IndexDef` as the single source), composite access paths | **done** (`#47` stage 0 + `#55`) |
| P6 | DDL (`CREATE`/`DROP`/`ALTER TABLE`), live and multi-process | **done** — the remaining DDL items are individual gaps below, not the phase |
| P7 | C-API shim + ecosystem suites (Django, CPython `test_sqlite3`) | **substantially done**, residual tracked in `C-API-COMPAT.md`; see §1 |
| P8 | performance parity with sqlite3 | **open — the active phase.** §2 is the whole of it |
| P9+ | the designed-not-built set (`INNOVATIONS.md` §8) | deliberately not started |

---

## 0. The corpus: two records, and what they cost to close

Measured 2026-07-29: **5 938 276 of 5 938 278 (99,9999663 %), 0 wrong answers,
0 error mismatches.** Both remaining records are in
`random/groupby/slt_good_12.test`, both are the bare-column rule under two or
more `min()`/`max()`, and BOTH return an EMPTY result set anyway (`WHERE 37 IS
NULL` in one, a `HAVING` that can never be true in the other) — mpedb refuses at
COMPILE time a query whose answer is not in doubt.

* **`:47732`** — `max(DISTINCT -95)` … `min(col0)`. The tempting fix is to
  discount a min/max over a non-NULL CONSTANT from the count: it improves only
  on the group's first row, so "surely" it can never be the aggregate the
  witness follows. **Tried and REFUTED by the differential oracle in one
  query** (`tests/group_by_dialect.rs`): on a group whose column is all NULL the
  real min/max never improves either, and sqlite then ends on the group's LAST
  row — not the constant's first row, not any extremum. The refusal is correct.
  Closing this needs sqlite's actual witness rule for ≥2, probed rather than
  reasoned.

* **`:61639`** — `MAX(-col0)` inside `COALESCE(+54, …)`, a DEAD arm constant
  folding removes. mpedb lifts aggregates BEFORE folding, and deliberately
  over-collects so the grouped-tuple slot positions the projection was bound
  against stay put. Closing it means either folding before the lift or teaching
  the lift the same pruning rule — a structural change, for one record.

## 1. Ecosystem parity (P7 residual)

**Re-measured 2026-07-29 on the Linux runner, and the count that matters is now
SHIM-ONLY — the arms are diffed by test NAME, not by pass count.** A test that
fails on both arms is the runner's problem and is not mpedb's; counting it
against us is how the old numbers drifted.

| group | tests | shim-only failures |
|---|---|---|
| Django A (`basic lookup transactions ordering update delete` + `aggregation annotations expressions`) | 831 | **0** |
| Django `queries` | 493 | **0** |
| Django `backends` | 324 | 5 |
| Django `model_fields` | 528 | 2 |
| CPython `test_sqlite3` (3.12.3) | 467 | 8 |

Django's whole residual is **seven named tests**, and three of them are one
thing (FK enforcement):

- `SchemaTests.test_constraint_checks_disabled_atomic_allowed`, `…test_disable_constraint_checking_failure_disallowed`, `test_uuid…test_unsaved_fk` — FOREIGN KEY enforcement
- `IntrospectionTests.test_get_primary_key_column`, `…_pk_constraint` — DDL inside a SAVEPOINT
- `backends.sqlite.tests.Tests.test_init_command` — `PRAGMA` (D10)
- `test_jsonfield…test_lookups_special_chars` — JSON-path escapes

Two earlier entries in this section were wrong about their own cause, and the
correction is worth keeping:

* **The "7 delete-batching errors" were not a delete problem and not a
  contamination cascade from a leaked signal receiver.** Every one was
  `database is out of space`. The shim gave a `mode=memory` database **1 MiB**,
  chosen so CPython's `test_backup.test_progress` would report a small page
  count — a global default tuned to flatter one test. Measured: such a database
  dies after **7 749** one-row inserts, the six labels in G1 share one test
  database, and running `delete` alone passes 59/59. The default is now 64 MiB
  and G1 is 392/392. `test_progress` failed before the change and fails after
  it, so the tuning had bought nothing even for its own test.
* **The IN-list injection test was not "failing on both arms".** Stock is clean;
  it was ours. It is now declared through Django's own `django_test_skips`
  (adaptation D12), because the test is `skipUnless(vendor == "sqlite")` and its
  docstring says it only applies to databases that do NOT validate parameter
  types — mpedb does, which is COMPAT.md deviation 4 and the product. The
  security property it checks holds here more strongly than in sqlite: sqlite
  answers, we refuse.

- [ ] **Derived-table placement** — `test_qs_with_subcompound_qs`,
      `test_distinct_ordered_sliced_subquery`. Both need `exec/`:
      `CompoundPlan.arms: Vec<ArmPlan>` with a recursive variant, and
      `SelectPlan.derived` per `DESIGN-DERIVED-TABLES` §5.7. The cheap
      alternative was checked and does not work (subplan slots fill before
      `exec_derived` runs).
- [x] **`InsertSource::Expr`** — multi-row `VALUES` carrying an expression
      (`test_bulk_insert`). **Already shipped in `21cd819`** (PLAN_FORMAT 57):
      plan/binder/validate/explain/footprint *and* the `build_insert_row_impl`
      arm. This list was written from a stale `C-API-COMPAT.md` row; re-verified
      2026-07-22 against the bundled sqlite 3.45 on VALUE **and** `typeof()`.
      The only surviving refusal is a HOST-registered UDF in a multi-row cell
      (no connection scope there — `INSERT … SELECT` is the route), by name.
- [x] **Correlated subquery in an aggregate query / in HAVING** — **already
      shipped in `37622a6`**; the two error strings this row quoted are gone
      from the tree. Re-verified 2026-07-22: a correlated scalar subquery in a
      grouped SELECT list (with and without a LEFT JOIN) and a correlated
      subquery in `HAVING` are byte-identical to sqlite 3.45, `typeof()`
      included. Coverage lives in `tests/agg_correlated{,_perrow}.rs`.
- [x] **Partial-index access** — the §5.5 implication test, v1 (rows 1–3 of the
      entailment lattice: exact atom match plus the `IS NOT NULL` weakenings),
      in `planner/partial.rs`. Range subsumption and parameterized predicates
      are refused by name; `UNIQUE … WHERE` is still refused at CREATE, because
      that one needs **engine-side membership** (`index_row_key`), which is a
      `mpedb-core` change and not landed. Until it is, a partial index is built
      FULL — a superset of its members, which every §5.5-approved probe reads
      correctly, since the query's own conjuncts are either the probe's key
      parts or its residual.
- [ ] **DDL inside SAVEPOINT** (2 `backends` tests) — engine.
- [x] **Output alias referenced in WHERE** (CPython trace test) — **already
      shipped in `9e4878b`** (`planner/select.rs::rewrite_where_select_aliases`,
      SLT coverage in `slt/fromless.test`). Re-verified 2026-07-22 against the
      oracle on the shapes that decide the rule: a base column always beats an
      alias (`SELECT y AS x FROM t WHERE x = 10` filters the COLUMN), the first
      of two same-named aliases wins, matching is case-insensitive, a qualified
      `q.z` never resolves, and an aggregate alias raises rather than resolving.
- [x] `sqlite_master` rows for indexes (`ce0caf7`) — views and triggers were
      already there. Same commit fixed two bugs found on the way: `sqlite_master`
      **refused every bound parameter**, so Django's `get_constraints` (which
      reaches the catalog only via `WHERE type='table' and name=%s`) raised on
      every table rather than reading anything; and the `CREATE TABLE`
      reconstruction **invented a `UNIQUE` constraint** for any column carrying a
      separately-created unique index.
- [x] `ON CONFLICT ROLLBACK` — already closed at `c4d1a90`; this entry was stale.
- [ ] `deserialize` — decide scope rather than half-implementing.
- [ ] **`UNIQUE INDEX … WHERE` in `WriteTxn::create_index`** — refused, and it
      blocks Django's `backends` and `introspection` labels at `migrate`, so
      **those two labels have never run on either arm**. Unblocking them is worth
      more than it looks: it converts two unmeasured labels into measured ones.
      **The planner half is now done** — the §5.5 implication test ships, so a
      partial index that a query provably implies IS picked for access. What
      still blocks `UNIQUE … WHERE` is only the ENGINE half: membership
      evaluation in `index_row_key`, so a non-member cannot collide.

> **Read this list against the tree, not against `C-API-COMPAT.md`.** Four of
> the rows above were open in the compat sheet and closed in the code — the
> sheet's residual tables are dated per measurement run and go stale the moment
> a fix lands without a re-measure. When a row here quotes an error string, the
> cheapest check is `grep` for that string first.

**~~Will not be closed~~ — REOPENED 2026-07-29.** This used to read: *"`PRAGMA
foreign_keys` ×2 + `test_unsaved_fk`, `PRAGMA synchronous` (D10), CPython
`test_backup.test_progress`. Closing any of them means claiming enforcement or
durability semantics mpedb does not have."*

That was a **scope decision, not an impossibility**, taken when the goal was
"never answer wrongly" — and then an honest refusal beats a fake semantic. The
goal is now 100 % against sqlite3, so the decision is taken again, and the
answer is that we build the semantics rather than decline to have them:

* **FK enforcement** is unbuilt, not unbuildable. `skip_references_clause`
  already parses the whole grammar — actions, `MATCH`, `DEFERRABLE` — and
  discards it. Triggers with `RAISE(ABORT)`, index point-probes on the write
  path and a commit-time hook next to the optimistic guard are all shipped.
  Closes three Django tests and one CPython test, and un-skips four more.
* **`PRAGMA synchronous`** (D10): the getter can report the mode the config
  actually chose — `none`/`async`/`wal` already mean OFF/NORMAL/FULL, and
  `DESIGN.md` §durability draws the mapping itself. That is not a lie.
* **`test_progress`** is the one genuinely hard position: matching means
  reporting a page count that is not mpedb's geometry. It stays last, and if it
  does not land it is the single honest residual — stated, not rounded away.

The old ceiling line (`Django 2 171/2 176, CPython 467/474`) is retired: it was
computed from the set above being permanent.

---

## 2. Performance (P8 — the active phase)

Corpus, Linux, 621 files, `bc45e69`/`bd420e8` (`benchmarks/minisqlite.md` §11):
**mpedb 239.2 s · minisqlite 153.0 s · sqlite 67.6 s** — mpedb/sqlite ≈ **3.5×**
(was 3.9×), mpedb/minisqlite ≈ **1.56×** (was 1.72×).

### 2a. The serial per-row constant — this is what moves the corpus

Parallelism does **not** move this benchmark: corpus statements sit far below the
~50–100 k rows where `DESIGN-PARALLEL-READ` §8 engages. The gap is the row
pipeline — mpedb validates every decoded row where sqlite memcpy's a VDBE record.

- [x] Fold fusion, borrowed cells, one-pass decode (`407e63b`, `4471128`) —
      `count(*)` 293.5 → 0.7 ns/row, `sum` 347 → 150.7, GROUP BY-10 now beats
      serial sqlite.
- [x] Aggregate access paths over index trees (`4e67ef0`) — min/max as O(log n)
      boundary probes (162 ms → 10–12 µs), `count(a)` 151 ms → 0.45 ms.
- [x] Join candidate-buffer reuse (`d16c666`) — select4 ~9 %, byte-identical.
- [ ] **`eval_filter_host` / ON evaluation** — 16 % inclusive in the select4
      profile, untouched. Next lever.
- [ ] **Cursor/fold fusion for `sum`** — the named floor: the per-row
      `Vec<Value>` plus cursor walk against sqlite's decode-in-VDBE. ~1.6×
      remains on `sum`.
- [ ] **Row-format cost** — the structural residual. We validate; sqlite copies.
      A design question, not constant-shaving; do not start it as one.

### 2b. Parallelism — a different axis, for large single queries

sqlite has **no** parallel mode at all, so this is differentiating where it
applies (and DuckDB, not sqlite, is the real incumbent — `LANDSCAPE.md`).

- [x] Substrate measured (`8cb5098`): hand-partitioned reads **6.8× on 11 cores**
      (M3), flattening is silicon not coordination; 10 k rows is 0.56× — the
      threshold is real.
- [x] Adaptive morsel design (`bd420e8`, §8): calling thread = worker 0, no
      compile-time gate, work-stealing tail, budgeted fan-out because a greedy
      query must not starve the **other processes** sharing the file.
- [ ] **Implement §8 for the order-independent fold** — `count`/`min`/`max` and
      integer `sum` via i128 partials (raises strictly less often than serial,
      never differently; RLS carve-out per the join-reorder precedent).
      *(in flight)*
- [ ] **Row-producing queries** — needs §8's ordered-buffer assembly (later
      partitions buffer until earlier ones are emitted, for byte-identity).
- [ ] **Plan-level parallelism** — compound arms and MPEE barrier segments on
      separate cores. Inherits `DESIGN-MPEE-SOLVER` §7.1's independence proof;
      the cleanest cut in the tree and the one that fits Django's big compounds.

### 2c. Durable writes

- [x] `commit` flush count fixed (`4.05 → 2.02` msyncs), Linux-gated because
      Darwin `msync` costs range width.
- [x] `wal` measured **at parity with PostgreSQL** (0.96× p50 paired); the
      published 3.7×-behind cell was comparing our slowest durable mode to their
      fastest.
- [ ] `#110` — the shim's busy policy forfeits group commit (measured 2.3–2.5×
      under 4 contended writers). **Refused for now**: withdrawing a published
      intent is unsound, and the overshoot is the lock-holder's transaction
      length, so no threshold is defensible. Needs a ring-protocol change
      (`ST_CLAIMED` + `try_withdraw`) and a shared-memory format bump → full
      adversarial review.

---

## 3. Correctness debt found by measurement, not by tests

The pattern worth noticing: **the corpus and the benchmarks found what the suite
did not.** Every item here was found by running something, not by a failing test.

- [x] `:memory:` fixpoint regression (`31cb87c`) — in-place writes broke §4.5's
      monotone-lattice precondition; found by the corpus timing run.
- [x] NOCASE min/max compared under BINARY (`bd8768b`) — a live wrong answer,
      8 of 11 differential tests failed pre-fix.
- [x] Non-numeric bound parameter coerced to 0 (`fc088d6`) — a refusal widened
      into a wrong answer to make one Django test pass.
- [ ] **`ALTER TABLE … ADD COLUMN` on an implicit-rowid table** — confirmed
      ENGINE-side, two sites, both the same shape: `Schema::with_added_column`
      (`mpedb-types/src/schema.rs`) pushes past the trailing hidden rowid that
      `validate`'s `implicit_rowid` arm requires to be last — insert at
      `len() - 1` and bump the `primary_key` entry; and
      `WriteTxn::alter_add_column` (`mpedb-core/src/engine/write.rs`) must
      `insert` at the same position in the row rewrite, or every row is
      re-encoded with the new column where the rowid belongs. Migrations are
      impossible on exactly the tables the shim creates by default.
- [x] **Shim introspection staleness** — the SQL surface was already fixed by
      `a48ec1c`; the note outlived it. What survived was `sqlite3_blob_open`,
      which bypasses SQL entirely, and the blob WRITE path where the stale
      bundle disabled the handle's expiry detector. Both fixed in `ce0caf7`.
- [ ] **`access_report` over-claims** — reports `exact_columns: true` while
      `..`-destructuring silently drops `windows`, `returning`, `with_check`,
      `ON CONFLICT DO UPDATE`. It is the C-API authorizer's input.

---

## 4. Testing rigour — where `LANDSCAPE.md` §4 says we are behind

Our harnesses are process-level (`crash` SIGKILLs) or model-level (`powerloss`
replays the engine's own trace). **Neither drops writes the OS believes it
made** — the difference between surviving a kill and surviving a power cut.

- [ ] **LazyFS or `dm-flakey`** — bbolt runs dm-flakey power-failure tests on
      ext4 *and* xfs in CI; DuckDB and WiredTiger use LazyFS. Days of work, and
      it targets exactly the `wal` path's fsync claims.
- [ ] **redb's fuzzer shape** — IO-error injection with *separate* durability-
      class oracles, so the test asserts *which* commits must survive *where*.
      We have the differential-oracle habit already; this is the missing wiring.

---

## 5. Housekeeping

- [ ] `rest.txt` is Norwegian scratch and deliberately untracked; fold anything
      durable from it into this file and drop the rest.
- [ ] Repo language: English going forward, converted a file at a time.
- [ ] `README.md` highlights are behind the aggregate/index work (min/max as
      µs boundary probes, `count(a)` ahead of sqlite, GROUP BY ahead of serial
      sqlite). Update when the parallel fold lands, not before — one measured
      round, one doc update.
