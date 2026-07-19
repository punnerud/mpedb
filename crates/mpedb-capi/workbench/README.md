# C-API ecosystem workbench

Runs real sqlite3 consumers against the mpedb `libsqlite3` shim (`LD_PRELOAD`)
to measure drop-in compatibility beyond mpedb's own tests. Results feed
`C-API-COMPAT.md`. Run: `crates/mpedb-capi/workbench/run.sh`.

## Suites
1. **DB-API 2.0 battery** (`../tests/dbapi_battery.py`) — 23/23 vs stock sqlite.
2. **Django ORM** (`proj/`) — a minimal project (Author/Book models, a FK) whose
   `migrate` + ORM CRUD run under the shim.

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
