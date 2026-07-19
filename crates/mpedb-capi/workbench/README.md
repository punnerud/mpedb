# C-API ecosystem workbench

Runs real sqlite3 consumers against the mpedb `libsqlite3` shim (`LD_PRELOAD`)
to measure drop-in compatibility beyond mpedb's own tests. Results feed
`C-API-COMPAT.md`. Run: `crates/mpedb-capi/workbench/run.sh`.

## Suites
1. **DB-API 2.0 battery** (`../tests/dbapi_battery.py`) — 23/23 vs stock sqlite.
2. **Django ORM** (`proj/`) — a minimal project (Author/Book models, a FK) whose
   `migrate` + ORM CRUD run under the shim.

## Findings (2026-07-19, first Django baseline)
- **Django `migrate` is blocked at connection open**, before any SQL:
  `register_functions(conn)` calls `sqlite3_create_function` ~30 times
  (`django_date_extract`, `django_date_trunc`, `django_time_diff`, `regexp`,
  `django_power`, …), and the shim refuses UDF registration → `OperationalError:
  Error creating function`. **This is Django's #1 gate.** Plain Python sqlite3
  (the DB-API battery) is unaffected — it does not register UDFs.
- **Path to Django support:** the shim's `sqlite3_create_function[_v2]` must
  ACCEPT the registration (store the callback) and the engine must dispatch to it
  when a query calls the function — a real scalar-UDF mechanism (the exec expr IR
  gains an "call host function" path, or the shim rewrites known Django functions
  to native equivalents). Until then, Django cannot open a connection.
