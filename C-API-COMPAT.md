# mpedb C-API (libsqlite3) Compatibility

Status of the **`mpedb-capi`** shim (`crates/mpedb-capi`) — a cdylib that
exports sqlite3's C-API and translates each call to mpedb's Rust facade. Built
as `libmpedb_sqlite3.{so,dylib}`, it is the **ABI-level** drop-in: `LD_PRELOAD`
it as `libsqlite3` (or link it) and a libsqlite3 consumer — Python's `sqlite3`,
a language binding, a tool — runs against mpedb. Companion to the SQL-surface
[COMPAT.md](COMPAT.md) and the native PyO3 path (`crates/mpedb-py`). Design:
[design/DESIGN-CAPI.md](design/DESIGN-CAPI.md).

Legend: ✅ implemented · 🚧 partial / with caveats · ❌ out of scope (returns a
clear error). Result-code **integers match sqlite exactly** (`SQLITE_OK=0`,
`SQLITE_ROW=100`, `SQLITE_DONE=101`, `SQLITE_CONSTRAINT=19`, `SQLITE_MISUSE=21`,
`SQLITE_RANGE=25`, …) because consumers `switch` on them.

## The core ~30 (design §2)

### open / close

| Function | Status | Comment |
|---|---|---|
| `sqlite3_open` | ✅ | Always create+read/write. `:memory:`, `""` and `file::memory:` → an ephemeral file on `/dev/shm` (or the temp dir), removed on close |
| `sqlite3_open_v2` | 🚧 | Honors `SQLITE_OPEN_CREATE` (a missing file without it → `SQLITE_CANTOPEN`) and `SQLITE_OPEN_MEMORY`; minimal `file:` URI parsing. `SQLITE_OPEN_READONLY` is **not** enforced (opens read/write); the `zVfs` argument is ignored |
| `sqlite3_close` / `sqlite3_close_v2` | ✅ | Rolls back any open transaction, unmaps the engine, deletes the file if ephemeral. `NULL` → `SQLITE_OK`. Does not track/return `SQLITE_BUSY` for unfinalized statements |
| `sqlite3_busy_timeout` | 🚧 | Value is stored (and honored by the getter), but mpedb's MVCC/group-commit means writers don't return `SQLITE_BUSY` under contention — the timeout has nothing to wait on |

### prepare / step / exec

| Function | Status | Comment |
|---|---|---|
| `sqlite3_prepare_v2` | ✅ | Compiles/validates one statement (surfaces syntax/bind errors here, as sqlite does); sets `pzTail` to the byte past the first statement; blank/comment-only input → `NULL` stmt + `SQLITE_OK` |
| `sqlite3_prepare` | ✅ | Alias for `_v2` |
| `sqlite3_step` | ✅ | Executes on first step (materialized), then yields rows one at a time (`SQLITE_ROW`/`SQLITE_DONE`). Column pointers valid until the next step/reset/finalize |
| `sqlite3_reset` | ✅ | Clears the cursor/result, keeps bindings; a re-step re-executes |
| `sqlite3_finalize` | ✅ | `NULL` → `SQLITE_OK` |
| `sqlite3_exec` | ✅ | Splits a multi-statement script and runs each; invokes the callback with text column values + names per row; writes a `sqlite3_free`-able `errmsg` on failure; callback non-zero → `SQLITE_ABORT` |

### bind (1-based index)

| Function | Status | Comment |
|---|---|---|
| `sqlite3_bind_int` / `_int64` | ✅ | |
| `sqlite3_bind_double` | ✅ | |
| `sqlite3_bind_text` | ✅ | Copies the bytes (UTF-8, lossy on invalid input); honors a custom destructor, ignores `SQLITE_STATIC`/`SQLITE_TRANSIENT` |
| `sqlite3_bind_blob` | ✅ | Copies the bytes; destructor handled as for `_text` |
| `sqlite3_bind_null` | ✅ | |
| `sqlite3_bind_parameter_count` | ✅ | Counts `?`/`$N` placeholders (quote/comment aware) |
| `sqlite3_bind_parameter_index` | 🚧 | Maps `?N`/`$N`/`:N` to its number; alphabetic named params (`:name`) → 0 (mpedb has no named params) |
| `sqlite3_clear_bindings` | ✅ | |
| index out of `1..=count` | ✅ | → `SQLITE_RANGE` |

### column read (0-based, after `SQLITE_ROW`)

| Function | Status | Comment |
|---|---|---|
| `sqlite3_column_count` | ✅ | Available before the first step for read statements (executes lazily to name the output — see Notes) |
| `sqlite3_column_name` | ✅ | mpedb's output column names (an aliased/expression name where applicable) |
| `sqlite3_column_type` | ✅ | `Int`/`Bool`/`Timestamp`→`SQLITE_INTEGER`, `Float`→`FLOAT`, `Text`→`TEXT`, `Blob`→`BLOB`, `Null`→`NULL` |
| `sqlite3_column_int` / `_int64` | ✅ | With sqlite-style coercion (text → leading integer, etc.) |
| `sqlite3_column_double` | ✅ | With coercion |
| `sqlite3_column_text` | ✅ | UTF-8; non-text scalars render to text; `NULL` value → `NULL` pointer |
| `sqlite3_column_blob` | ✅ | Raw bytes; `NULL`/empty → `NULL` pointer |
| `sqlite3_column_bytes` | ✅ | Payload length of the text/blob representation |
| `sqlite3_column_decltype` | 🚧 | Returns `NULL` — mpedb's result metadata carries names, not declared types (a legal sqlite answer, but disables Python's `detect_types`) |
| `sqlite3_data_count` | ✅ | Extra, aids consumers |

### status / misc

| Function | Status | Comment |
|---|---|---|
| `sqlite3_errmsg` | ✅ | mpedb's error text; `"not an error"` when clear |
| `sqlite3_errcode` | ✅ | Primary code of the last failing call on the handle |
| `sqlite3_extended_errcode` | ✅ | Extended constraint codes (`CONSTRAINT_PRIMARYKEY`/`_UNIQUE`/`_NOTNULL`/`_CHECK`) |
| `sqlite3_changes` | ✅ | Rows from the last INSERT/UPDATE/DELETE (DDL leaves it unchanged) |
| `sqlite3_total_changes` | ✅ | Accumulated DML row count |
| `sqlite3_last_insert_rowid` | ❌ | **Returns 0.** The facade's result carries only an affected count, not the assigned rowid. Use `INSERT … RETURNING id` (mpedb auto-assigns a single-column INTEGER PRIMARY KEY like sqlite's rowid). Top blocker for ORMs — see below |
| `sqlite3_libversion` / `_number` | ✅ | Reports `3.45.0-mpedb` / `3045000` |
| `sqlite3_free` / `sqlite3_malloc` / `_malloc64` | ✅ | libc alloc, so an `exec` `errmsg` is `sqlite3_free`-able |
| `sqlite3_extended_result_codes` | ✅ | No-op toggle (extended codes always tracked) |
| `sqlite3_get_autocommit` | ✅ | 1 unless an explicit transaction is open |
| `sqlite3_sourceid` | ✅ | Extra |

### Transactions

`BEGIN` / `COMMIT` / `END` / `ROLLBACK` and `SAVEPOINT` / `RELEASE` / `ROLLBACK
TO` are intercepted by the shim (they error in the autocommit facade path):
`BEGIN` opens an mpedb `WriteSession`, subsequent statements route through it
(reads see uncommitted writes, as sqlite), `COMMIT`/`ROLLBACK` close it,
savepoints map to mpedb's savepoint API. This is Python's implicit-transaction
model, so `sqlite3`-shaped code works. `COMMIT`/`ROLLBACK` with no active
transaction are lenient no-ops.

## Out of scope (design §2) — return a clear error / documented no-op

| Function(s) | Status | Comment |
|---|---|---|
| `sqlite3_create_function[_v2]` | ❌ | Not exported — user-defined SQL functions in C are unsupported (FTS/scalars are native to mpedb) |
| `sqlite3_create_collation[_v2]` | ❌ | Not exported |
| `sqlite3_load_extension` | ❌ | Not exported — loadable extensions unsupported |
| `sqlite3_create_module` (virtual tables) | ❌ | Not exported — FTS is native (design/DESIGN-FTS) |
| Online-backup API (`sqlite3_backup_*`) | ❌ | Not exported — use `mpedb mirror` |
| Incremental blob (`sqlite3_blob_*`) | ❌ | Not yet — will map onto mpedb's #43 incremental-blob API |

## Notes, divergences, and design decisions

- **Schema-less open.** sqlite infers structure per file; mpedb refuses a schema
  with no live tables. A fresh `sqlite3_open("new.db")` therefore seeds the file
  with one inert bootstrap table `_mpedb_capi_bootstrap(id)`; user tables are
  created live with `CREATE TABLE`. It is not dropped (mpedb has no
  `sqlite_master` for a consumer to trip over it yet). An **existing** file is
  attached config-free and reads its stored schema.
- **Fixed size.** An mpedb file has a fixed maximum size (16 MiB ephemeral,
  64 MiB file-backed here); it is not currently configurable through the C-API.
  Exceeding it is `SQLITE_FULL`, not silent growth.
- **`column_count`/`_name` before `step`.** mpedb names a result only by running
  it. For read statements the shim executes lazily when column metadata is first
  requested (Python builds `description` this way); it materializes the whole
  result at that point (no server-side streaming cursor).
- **`decltype` is `NULL`.** Disables `sqlite3.PARSE_DECLTYPES`/`PARSE_COLNAMES`
  type detection.
- **Concurrency is better, not bug-for-bug.** mpedb has MVCC readers and
  group-commit; a consumer expecting `SQLITE_BUSY` under contention gets progress
  instead (compatible-or-better).

## Known blockers for the next milestones (Python `sqlite3` unittest / Django)

1. **`last_insert_rowid()` → 0** (both the SQL function and this C function). ORMs
   read `cursor.lastrowid` after INSERT; Django's autofield PKs depend on it.
   Needs a facade hook that surfaces the assigned rowid (or a shim-side
   `RETURNING` rewrite for single-INTEGER-PK inserts).
2. **No `sqlite_master` / `PRAGMA`.** Introspection (`PRAGMA table_info`,
   `SELECT … FROM sqlite_master`), `PRAGMA foreign_keys`, `journal_mode`, etc. are
   unsupported. Django's schema editor and many test fixtures use them heavily.
3. **DDL inside an explicit transaction is rejected.** mpedb's `CREATE`/`DROP`/
   `ALTER` run only in the autocommit path, not inside a `WriteSession`; a
   `BEGIN; CREATE TABLE …; COMMIT` fails. Django migrations wrap DDL in
   transactions.
4. **No user-defined functions/collations** — Django registers a few (e.g.
   `django_date_extract`) through the C-API.
5. **Fixed database size** vs. sqlite's unbounded growth (see above).
6. **Named parameters** (`:name`) are unsupported by mpedb's SQL; only `?`/`$N`.

## Verification

- `cargo test -p mpedb-capi` — 9 Rust FFI tests (open/create/prepare/bind/step/
  column/exec/errmsg/constraint/transactions/persistence/tail) + `sql`-scanner
  unit tests + a **C smoke test** (`tests/smoke.c` compiled against `sqlite3.h`
  and linked to the cdylib).
- `python3 crates/mpedb-capi/tests/smoke.py <cdylib>` — a `ctypes` consumer
  drives the same flow (the shape Python's `sqlite3` uses).
