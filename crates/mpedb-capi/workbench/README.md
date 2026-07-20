# C-API ecosystem workbench

Runs real sqlite3 consumers against the mpedb `libsqlite3` shim (`LD_PRELOAD`)
to measure drop-in compatibility beyond mpedb's own tests. Results feed
`C-API-COMPAT.md`. Run: `crates/mpedb-capi/workbench/run.sh`.

## Suites
1. **DB-API 2.0 battery** (`../tests/dbapi_battery.py`) — 23/23 vs stock sqlite.
2. **Django ORM** (`proj/`) — a minimal project (Author/Book models, a FK) whose
   `migrate` + ORM CRUD run under the shim.
3. **Django's OWN test suite** (`djsuite/`) — `run_suite.sh` runs a subset of
   Django's ~19 000-test suite twice, once on stock libsqlite3 and once under the
   shim, and `diff_arms.py` ranks the shim-only failures. Django is not vendored;
   point `WB_DJANGO` at a checkout. Results and the ranked gap list live in the
   "Django's own test suite" section of `C-API-COMPAT.md`.

## Findings (2026-07-19, first Django baseline)
- **Django `migrate` was blocked at connection open**, before any SQL:
  `register_functions(conn)` calls `sqlite3_create_function` ~30 times
  (`django_date_extract`, `django_date_trunc`, `django_time_diff`, `regexp`,
  `django_power`, …), and the shim refused UDF registration → `OperationalError:
  Error creating function`. **This was Django's #1 gate.** Plain Python sqlite3
  (the DB-API battery) is unaffected — it does not register UDFs.

## Findings (2026-07-19, after DESIGN-UDF stage 1: host SCALAR UDFs)
- **Gate 1 is OPEN.** `sqlite3_create_function[_v2]` now stores the callback and
  the engine dispatches to it, so all **26** of Django's scalar registrations
  succeed (`django_date_extract` … `RAND`).
- **The new blocker is one line further on:** `_functions.py:79`
  `connection.create_aggregate("STDDEV_POP", 1, StdDevPop)` →
  `OperationalError: Error creating aggregate`. Django registers four aggregates
  (`STDDEV_POP`, `STDDEV_SAMP`, `VAR_POP`, `VAR_SAMP`); stage 1 refuses
  `xStep`/`xFinal` by design (invoking `xDestroy` so the callable is not leaked).
- **Path to the next step:** DESIGN-UDF **stage 2** — accept `xStep`/`xFinal`,
  back `sqlite3_aggregate_context` with a per-group allocation, and drive them
  from the aggregate executor. That is the whole remaining distance to a Django
  connection opening.

## Findings (2026-07-19, after DESIGN-UDF stage 2: host AGGREGATE UDFs)
- **Gate 2 is OPEN.** All four `connection.create_aggregate(...)` calls
  (`STDDEV_POP`, `STDDEV_SAMP`, `VAR_POP`, `VAR_SAMP`) now succeed, and CPython's
  aggregate bridge works end to end: a differential probe of
  `STDDEV_POP` (bare / `GROUP BY` / empty set / all-NULL) returns **byte-identical
  results to stock sqlite** (`3.0`; `1.0, 3.0`; `None`; `None`).
- **The new blocker is three lines further on**, `_functions.py:85`:
  `select sqlite_compileoption_used('ENABLE_MATH_FUNCTIONS')` →
  `bind error: unknown function sqlite_compileoption_used()`. Django uses the
  answer to decide whether to register its own `ACOS`/`SIN`/`POWER`/… fallbacks.
  The shim needs `sqlite_compileoption_used(name)` (returning 0 is the honest
  answer — it makes Django register the pure-Python fallbacks it already has).
- **Known gap that will bite next:** host UDFs — scalar AND aggregate — are wired
  on the **read path only**. A statement executed inside an OPEN transaction runs
  through the write path, where they are out of scope
  (`host aggregate … is not in scope for this execution`). CPython opens an
  implicit transaction after DML and Django works inside transactions, so
  extending `TxnCtx::host_fns`/`host_aggs` to the write path is the natural
  stage 2.5. Verified: the same probe passes after an explicit `commit()`.

## Findings (2026-07-19, Django's own test suite — see C-API-COMPAT.md)

- Gates 3 and 4 (`sqlite_compileoption_used`, `sqlite_master`'s clause-leading
  `NOT`) are now OPEN, both fixed in the shim. Django's `migrate` completes and
  **506 of 831** Django tests pass under the shim (stock baseline: 826/831).
- The **single highest-leverage remaining fix is in `mpedb-sql`, not the shim**:
  a quoted identifier cannot qualify a dotted reference (`"t"."id"`), and Django
  quotes every name. The workbench works around it by emitting bare identifiers;
  without that, zero ORM queries run.
- ~~The hardest ceiling is **`MAX_TABLES` = 120**: Django's `queries` label alone
  exceeds it, so it cannot be measured at all.~~ **LIFTED** (2026-07-19,
  design/DESIGN-TABLE-CAP.md): footprints and the CDC capture config are sparse
  `TableSet`s instead of per-table bitmaps, so `MAX_TABLES` is 4096 (4088 live
  user tables). The 128-byte identifier limit that independently blocked
  `backends` (a generated m2m name is 134 chars) moved to 255 in the same pass,
  along with the identifier CHARACTER set — a quoted name may now contain spaces
  and punctuation, as sqlite allows. `queries` and `backends` are measurable.

## Findings (2026-07-20, Django run 5 — see C-API-COMPAT.md)

- **811/831 on the frozen G1/G2 labels (97.6 %), `queries` 474/493, `backends`
  314/324 — 1 599 of 1 648 measured tests (97.0 %), +91 over run 4.** Every one
  of the 39 remaining G1/G2/`queries` failures is a REFUSAL; zero wrong answers
  in the SQL surface. The only two shim-only FAILs anywhere are the deliberate
  D11 `PRAGMA foreign_keys` position.
- **The next wall is one parser feature, not a long tail:** `model_fields`
  (528 tests) cannot be measured at all because `migrate` dies on
  `GENERATED ALWAYS AS ("field") STORED`. That single gap outweighs the entire
  remaining measured gap list.
- Both surviving adaptations re-ablated: D2 (AUTOINCREMENT) still gates all
  831; D8 (`_references_graph`) still costs ~147 G1 outcomes, now pinned only
  by the shim's `sqlite_master` mini-evaluator refusing Django's recursive-CTE
  shape (W3's second pin is gone).
