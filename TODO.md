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

## 1. Ecosystem parity (P7 residual)

Measured at `03ff5ea`/`d83c21d` on the M3, `C-API-COMPAT.md` is the live sheet.
Django **826/831 + 490/493 + 314/324 + 514/528**, CPython **450/474** (Linux
runner: 447/467 — different interpreter, different denominator, do not subtract).

- [ ] **Derived-table placement** — `test_qs_with_subcompound_qs`,
      `test_distinct_ordered_sliced_subquery`. Both need `exec/`:
      `CompoundPlan.arms: Vec<ArmPlan>` with a recursive variant, and
      `SelectPlan.derived` per `DESIGN-DERIVED-TABLES` §5.7. The cheap
      alternative was checked and does not work (subplan slots fill before
      `exec_derived` runs).
- [ ] **`InsertSource::Expr`** — multi-row `VALUES` carrying an expression
      (`test_bulk_insert`). Plan-format + ~10 lines in `build_insert_row_impl`.
- [ ] **Correlated subquery in an aggregate query / in HAVING** — the oldest
      surviving family; per-group positions landed (`37622a6`), these did not.
- [ ] **Partial-index access** — `IndexDef.predicate` is stored (P1, canonical
      bytes v10); the planner does not yet *pick* a partial, so `SELECT` stays
      correct via FullScan. Needs the implication check. Also blocks
      `UNIQUE … WHERE` and parameterized predicates.
- [ ] **DDL inside SAVEPOINT** (2 `backends` tests) — engine.
- [ ] **Output alias referenced in WHERE** (CPython trace test) — binder.
- [ ] Smaller shim items: `ON CONFLICT ROLLBACK`, `deserialize`,
      `sqlite_master` rows for indexes/views/triggers (a dump that loses
      indexes replays into a different schema).

**Will not be closed, and the ceiling should say so.** `PRAGMA foreign_keys`
×2 + `test_unsaved_fk` (mpedb parses `REFERENCES` and discards it),
`PRAGMA synchronous` (D10), CPython `test_backup.test_progress`. Closing any of
them means *claiming* enforcement or durability semantics mpedb does not have.
**Reachable ceiling: Django 2 171/2 176, CPython 467/474 = parity with stock.**
State that as a line, do not round it away.

---

## 2. Performance (P8 — the active phase)

Corpus, Linux, 621 files, `bc45e69`/`bd420e8` (`minisqlite-vs-mpedb.md` §11):
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
- [ ] **`ALTER TABLE … ADD COLUMN` on an implicit-rowid table** — appends after
      the hidden rowid and fails the schema validator. Both halves are things
      ORMs do constantly; migrations are impossible on exactly the tables the
      shim creates by default.
- [ ] **Shim introspection is stale** — `sqlite_master` does not see a table
      until the statement *after* the COMMIT that created it. Read-your-own-
      writes at the metadata level; ORMs depend on it.
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
