# DESIGN-CAPI — a libsqlite3-compatible C-API shim (ABI-level drop-in + broader testing)

**Status: design (2026-07-18). Forward-looking (Phase 6+). The path from SQL-level to **ABI-level**
sqlite drop-in: a cdylib exporting sqlite3's C-API, backed by mpedb, so everything that links
`libsqlite3` — Python's `sqlite3` module, Django, tools, and their test suites — runs against mpedb
unchanged. Complements the existing native `mpedb-py` (PyO3): same engine, two front doors. Prior
art: libSQL (Turso), DuckDB's sqlite3 C-API shim.**

## 0. The idea

A new crate `mpedb-capi` (cdylib) exports the sqlite3 C symbols (`sqlite3_open_v2`, `sqlite3_prepare_v2`,
`sqlite3_step`, `sqlite3_bind_*`, `sqlite3_column_*`, `sqlite3_exec`, `sqlite3_errmsg`, …) as
`extern "C"`, translating each to mpedb's Rust facade. `LD_PRELOAD=libmpedb_sqlite3.so python app.py`
(or linking it as `libsqlite3`) makes an unmodified libsqlite3 consumer talk to mpedb.

## 1. What it unlocks — the testing answer

The point Morten raised: matching the C-API lets us run **far more** tests than the portable
sqllogictest corpus, by borrowing the ecosystem's suites:
- **Python's built-in `sqlite3` module** and its `unittest` suite (CPython links libsqlite3).
- **Django** — its ORM and its own **test suite**, with mpedb as the DB backend (this is what makes
  the retired "a Django test suite will not run against it" line actually false).
- Any ORM / tool / language binding that links libsqlite3 (Rails via the sqlite3 gem, Node
  better-sqlite3, etc.).
- The **SQL-behavior** portions of sqlite's own TCL suite, via a TCL harness linked against the shim.

**Honest scope limit:** sqlite's TCL suite is *heavily* sqlite-internal — it asserts on the file
format, VDBE opcodes, specific `PRAGMA` outputs, byte-exact quirks and error strings that mpedb
deliberately does not reproduce. Those tests stay out of scope. The value is the **SQL-semantics**
tests plus the vast **libsqlite3-consumer** ecosystem, not sqlite's internal regression suite.

## 2. The C-API surface

The sqlite3 C-API is hundreds of functions; a **core ~30 cover the overwhelming majority of usage**:
- open/close: `sqlite3_open[_v2]`, `sqlite3_close[_v2]`, `sqlite3_busy_timeout`.
- prepare/step: `sqlite3_prepare_v2`, `sqlite3_step`, `sqlite3_reset`, `sqlite3_finalize`,
  `sqlite3_exec`.
- bind: `sqlite3_bind_{int64,double,text,blob,null}`, `sqlite3_bind_parameter_count/index`.
- column read: `sqlite3_column_{count,type,int64,double,text,blob,bytes,name,decltype}`.
- status: `sqlite3_errmsg`, `sqlite3_errcode`/`extended_errcode`, `sqlite3_changes`,
  `sqlite3_last_insert_rowid`, `sqlite3_libversion[_number]`.
- **Result codes must match** — `SQLITE_OK/ROW/DONE/BUSY/ERROR/CONSTRAINT/MISUSE/…` (tests check the
  integers). Extended codes where consumers rely on them.

Out of scope (return `SQLITE_ERROR`/a clear "unsupported", documented): loadable extensions
(`sqlite3_load_extension`), VDBE/bytecode introspection, virtual-table modules
(`sqlite3_create_module` — FTS is native, §DESIGN-FTS). Incremental blob
(`sqlite3_blob_open/read/write`) **maps onto mpedb's own #43 incremental blob API** rather than
being stubbed.

⚠ **Three things this list called out of scope have since shipped** — the ecosystem's own test
suites demanded them, which is exactly §1's argument working:

- **User-defined functions** (`crates/mpedb-capi/src/udf.rs`): `sqlite3_create_function[_v2]`
  scalar and aggregate (`xStep`/`xFinal` over a real `sqlite3_aggregate_context`), plus
  `sqlite3_create_window_function` — a genuine sliding window with `xValue`/`xInverse`, not an
  aggregate re-run per frame; supplying only `xStep`/`xFinal` degrades to a plain aggregate.
- **Collations**: `sqlite3_create_collation[_v2]`, including CPython's
  `create_collation(name, None)` deletion form (`xCompare == NULL` removes the entry).
- **The online-backup API** (`crates/mpedb-capi/src/backup.rs`): `sqlite3_backup_init/step/
  finish/remaining/pagecount`, with the page counts mapped honestly onto mpedb's own unit of
  copy rather than faked.

Measured consequences and the remaining gap list live in `C-API-COMPAT.md`.

## 3. Lifecycle mapping

- `sqlite3*` connection handle → an mpedb `Database` + a `Session`. Multi-connection concurrency is
  mpedb's MVCC (a strict improvement over sqlite's single-writer — a libsqlite3 consumer that expected
  `SQLITE_BUSY` under contention gets mpedb's group-commit instead, which is compatible-or-better).
- `sqlite3_stmt*` → a compiled (content-hashed) plan + a bound-parameter set + a row cursor.
  `prepare_v2` = mpedb `prepare`; `step` advances the cursor (`SQLITE_ROW`/`SQLITE_DONE`); `bind_*`
  sets params; `column_*` reads the current row's typed value (mpedb `Value` → sqlite
  type/affinity); `exec` = a prepare→step loop feeding the callback.
- Text is UTF-8 (`sqlite3_column_text`); the shim owns per-statement scratch buffers with sqlite's
  pointer-lifetime rules (valid until the next `step`/`finalize`).

## 4. Fidelity requirements (so the borrowed tests pass)

- `sqlite3_column_type` reports `SQLITE_{INTEGER,FLOAT,TEXT,BLOB,NULL}` — mpedb's typed `Value`s map
  cleanly (and the CAST-affinity work #83 already models sqlite's type rules).
- Error **codes** match; error **strings** match where a test asserts them (else documented
  divergence). `changes()`/`last_insert_rowid()` reflect the last statement (the latter ties to the
  #85 rowid-alias work).
- Statement pointer/lifetime semantics exactly as sqlite documents, or the bindings crash.

## 5. Relationship to `mpedb-py`

mpedb already ships `mpedb-py` (PyO3, abi3) — the **native** path, idiomatic and fast, for code
written *for* mpedb. `mpedb-capi` is the **compat** path: existing sqlite code and test suites run
unchanged. Same engine underneath; the choice is "adopt mpedb's API" vs "keep sqlite's API and swap
the library."

## 6. Staging

1. **Core shim** — open/prepare/step/bind/column/exec + result codes; run a hand-written Python
   `sqlite3` script end to end.
2. **Python `sqlite3` unittest subset** green (the SQL-behavior tests; skip sqlite-internal ones).
3. **Django backend + Django test suite** — the headline "it really is drop-in" milestone.
4. **SQL-behavior TCL tests** via the shim; **incremental-blob** mapped to #43.

Phase 6+, after the SQL-parity sprint. This is the ABI half of the drop-in goal, and the answer to
"can we run more of the tests" — yes, the ecosystem's, by matching the one interface they all speak.
