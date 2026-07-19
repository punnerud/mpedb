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

## Scope — what "100%" means

sqlite's C reference lists ~300 functions and ~250 constants. This shim exports
the **~50 the drop-in consumer path actually calls** — Python's `sqlite3`, language
bindings, common tools — validated end-to-end by a DB-API 2.0 battery that matches
stock sqlite **23/23**. It does *not* enumerate every symbol, because most are
deliberate non-goals for an in-process, rigid-schema engine (each a clean refusal
or safe no-op, never a wrong answer):

- **UDF registration is REAL — scalar AND aggregate** (`sqlite3_create_function
  [_v2]`, design/DESIGN-UDF.md stages 1 + 2): the callbacks are stored per
  connection and SQL that calls the function dispatches to them, including a real
  `sqlite3_aggregate_context`. `_create_window_function` (`xValue`/`xInverse`) and
  `_create_collation*` still refuse cleanly (invoking the caller's
  `xDestroy(pApp)`, so CPython does not leak the wrapped callable) — stage 3.
- **VFS / virtual-table module ABI** (`sqlite3_vfs_*`, `sqlite3_create_module*`):
  mpedb has its own storage engine, not sqlite's pager — a named VFS is refused
  (see `open_v2`); the one module that matters, **FTS5, is native**, not a plugin.
- **Hooks & authorizer** (`_commit_hook`/`_rollback_hook`/`_update_hook`/`_wal_hook`/
  `_set_authorizer`/`_trace_v2`/`_progress_handler`): safe no-ops.
- **`sqlite3_config`, loadable extensions, serialize/backup internals, and
  incremental blob I/O** beyond the listed set: out of scope.

So "100%" is the **consumer / DB-API surface**, not every symbol in the reference.
The tables below list, by category, exactly what the shim implements.

## The core ~30 (design §2)

### open / close

| Function | Status | Comment |
|---|---|---|
| `sqlite3_open` | ✅ | Always create+read/write. `:memory:`, `""` and `file::memory:` → an ephemeral file on `/dev/shm` (or the temp dir), removed on close |
| `sqlite3_open_v2` | 🚧 | Honors `SQLITE_OPEN_CREATE` (a missing file without it → `SQLITE_CANTOPEN`) and `SQLITE_OPEN_MEMORY`; minimal `file:` URI parsing. A named **`zVfs`**: the built-in names (`unix*`/`win32*`/`memdb`, or NULL) denote ordinary file I/O and are honored; a **custom/unknown VFS is REFUSED** with `SQLITE_ERROR` + "no such vfs" — mpedb runs no sqlite VFS modules (it has its own storage engine, not sqlite's pager), and silently ignoring e.g. an encryption VFS would be unsafe. `SQLITE_OPEN_READONLY` is **not** enforced (opens read/write) |
| `sqlite3_close` / `sqlite3_close_v2` | ✅ | Rolls back any open transaction, unmaps the engine, deletes the file if ephemeral. `NULL` → `SQLITE_OK`. Does not track/return `SQLITE_BUSY` for unfinalized statements |
| `sqlite3_busy_timeout` | ✅ | On a BUSY-class contention error — an optimistic-mode `WriteConflict` (loser rolled back), a full reader table, or an evicted snapshot, all mapped to `SQLITE_BUSY` — the shim retries with sqlite's own busy-handler backoff table until the timeout elapses, then returns `SQLITE_BUSY`. Timeout 0 (default) = no retry, immediate BUSY, as sqlite. In the normal serial writer mode the writer lock **blocks** (never returns `SQLITE_BUSY`), so the timeout has nothing to wait on — either way, sqlite-faithful |

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
| `sqlite3_bind_parameter_count` | ✅ | Highest parameter number used, all kinds sharing one numbering space (quote/comment aware) — `?`, `?N`, and named `:a`/`@a`/`$a` |
| `sqlite3_bind_parameter_index` | ✅ | Returns a parameter's number by its spelling (sigil included, e.g. `:name`); unknown/sigil-less → 0. Answered from the prepare-time name map |
| `sqlite3_bind_parameter_name` | ✅ | Returns the `idx`-th parameter's spelling (sigil included) for a named `:a`/`@a`/`$a` or an explicit `?N`, or NULL for an anonymous `?`. The shim rewrites named→numbered before mpedb parses |
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
| `sqlite3_column_decltype` | ✅ | Plan-derived: a bare base-table column reports its declared type (`INTEGER`/`TEXT`/`REAL`/`BLOB`/`BOOLEAN`/`TIMESTAMP`); a computed column (expression, aggregate, function, join/window output, typeless `ANY`) reports `NULL` — exactly what sqlite does. Drives Python's `PARSE_DECLTYPES` byte-identically. Computed lazily; no plan-format change |
| `sqlite3_data_count` | ✅ | Extra, aids consumers |

### status / misc

| Function | Status | Comment |
|---|---|---|
| `sqlite3_errmsg` | ✅ | mpedb's error text; `"not an error"` when clear |
| `sqlite3_errcode` | ✅ | Primary code of the last failing call on the handle |
| `sqlite3_extended_errcode` | ✅ | Extended constraint codes (`CONSTRAINT_PRIMARYKEY`/`_UNIQUE`/`_NOTNULL`/`_CHECK`) |
| `sqlite3_changes` | ✅ | Rows from the last INSERT/UPDATE/DELETE (DDL leaves it unchanged) |
| `sqlite3_total_changes` | ✅ | Accumulated DML row count |
| `sqlite3_last_insert_rowid` | ✅ | **Real value.** A facade hook (`mpedb::take_last_insert_rowid`, thread-local, drained per statement in `exec_one`) surfaces the rowid an INSERT assigned/used on a rowid-alias (INTEGER PRIMARY KEY) table — the last row of a multi-row insert wins; a non-insert leaves it unchanged, as sqlite does. Powers `cursor.lastrowid` |
| `sqlite3_libversion` / `_number` | ✅ | Reports `3.45.0` / `3045000`. **Pure `X.Y.Z`** — CPython's `dbapi2` parses each dotted field as an int, so no suffix. mpedb identity is in `sqlite3_sourceid` |
| `sqlite3_free` / `sqlite3_malloc` / `_malloc64` | ✅ | libc alloc, so an `exec` `errmsg` is `sqlite3_free`-able |
| `sqlite3_extended_result_codes` | ✅ | No-op toggle (extended codes always tracked) |
| `sqlite3_get_autocommit` | ✅ | 1 unless an explicit transaction is open |
| `sqlite3_sourceid` | ✅ | Carries the mpedb identity (`mpedb-capi shim`) |
| `sqlite3_errstr` | ✅ | Static message per primary result code (sqlite-matching strings) |
| `sqlite3_complete` | ✅ | True if the text ends in `;` (quote/comment aware) |
| `sqlite3_threadsafe` | ✅ | Reports `1` (mpedb is internally synchronized) |
| `sqlite3_initialize` / `_shutdown` | ✅ | `SQLITE_OK` no-ops (no global init state) |
| `sqlite3_sleep` | ✅ | Sleeps `ms` and returns it |
| `sqlite3_stricmp` | ✅ | ASCII case-insensitive C-string compare |
| `sqlite3_db_handle` | ✅ | The `sqlite3*` that prepared a statement |
| `sqlite3_stmt_readonly` | ✅ | 1 for SELECT / transaction-control / blank, else 0 |
| `sqlite3_stmt_busy` | ✅ | 1 while a statement is mid-iteration |
| `sqlite3_expanded_sql` | ✅ | Substitutes each bound parameter as a SQL literal (quote/comment aware — a `$K` inside a string or comment is untouched; text `'`-escaped, blobs `X'…'`, NULL/int/float/timestamp rendered); `sqlite3_free`-able |
| `sqlite3_interrupt` | 🚧 | Sets an atomic flag (safe to call from another thread) polled at step entry and during the busy-retry wait → the interrupted statement returns `SQLITE_INTERRUPT` and clears the flag. mpedb materializes a result synchronously, so there is no mid-scan yield point; a runaway scan is bounded instead by the per-statement runtime budget (#74) |

### Introspection (shim-emulated — mpedb has no `PRAGMA`/`sqlite_master`)

Answered entirely inside the shim (`introspect.rs`) as a pure function of the
live schema (`db.schema()`); nothing reaches the engine. `classify` routes a
`PRAGMA` leading keyword to `Kind::Pragma`, and a `SELECT … sqlite_master`/
`sqlite_schema` read is detected by identifier and re-routed.

| Feature | Status | Comment |
|---|---|---|
| `PRAGMA table_info(t)` / `table_xinfo` | 🚧 | `(cid, name, type, notnull, dflt_value, pk)` from the live schema; `dflt_value` is always NULL (defaults not reconstructed); a PK column reports `notnull=1` (mpedb PKs are genuinely NOT NULL, unlike sqlite's nullable rowid alias) |
| `PRAGMA table_list` | ✅ | `(schema, name, type, ncol, wr, strict)` for user tables |
| `PRAGMA index_list(t)` | 🚧 | `(seq, name, unique, origin, partial)`; synthesized index names |
| `PRAGMA foreign_key_list` / `foreign_key_check` | ✅ | Empty (mpedb has no foreign keys) |
| `PRAGMA foreign_keys` / `journal_mode` / `user_version` / … (getters) | 🚧 | Return a conventional value |
| `PRAGMA <x> = <v>` and other pragmas (setters) | ✅ | Accepted as a no-op (the common DB-setup pragmas never error) |
| `SELECT … FROM sqlite_master` / `sqlite_schema` | 🚧 | Emulated from the schema (user tables only; the bootstrap table is hidden). Projects any subset of `type, name, tbl_name, rootpage, sql` (or `*`, `count(*)`); `WHERE` supports AND-joined `col = 'x'` / `<>` / `IN (…)` / `[NOT] LIKE 'p'`; `ORDER BY name [DESC]`. `rootpage` is 0, `sql` is a reconstructed `CREATE TABLE`. Unsupported shapes error clearly. Views/indexes not listed yet — handles Django's `SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'` |

### Transactions

`BEGIN` / `COMMIT` / `END` / `ROLLBACK` and `SAVEPOINT` / `RELEASE` / `ROLLBACK
TO` are intercepted by the shim (they error in the autocommit facade path):
`BEGIN` opens an mpedb `WriteSession`, subsequent statements route through it
(reads see uncommitted writes, as sqlite), `COMMIT`/`ROLLBACK` close it,
savepoints map to mpedb's savepoint API. This is Python's implicit-transaction
model, so `sqlite3`-shaped code works. `COMMIT`/`ROLLBACK` with no active
transaction are lenient no-ops.

## Extended surface — exported so CPython's `_sqlite3` loads

CPython's `_sqlite3` C extension references ~50 `sqlite3_*` symbols at load
time; **any one it cannot resolve is an `undefined symbol` at `LD_PRELOAD`**, so
all of them are now exported. The ones not covered above are **safe stubs**:
they never produce a wrong query answer — they refuse (a documented error code)
or no-op, which is enough for `import sqlite3` + basic CRUD to work. Verified
against `_sqlite3.cpython-312` on Linux/x86-64 (Python 3.12).

| Function(s) | Status | Behaviour |
|---|---|---|
| `sqlite3_create_function` / `_create_function_v2` (SCALAR) | ✅ | Real dispatch (design/DESIGN-UDF.md stage 1). The `xFunc` is stored per connection and a SQL call to that name invokes it with the evaluated arguments. `nArg = -1` is variadic; re-registering the same `(name, nArg)` replaces (running the old `xDestroy`); `xFunc == NULL` deletes. Names are matched case-insensitively. A plan containing a host call is compiled/executed LOCALLY and never published to the shared plan registry (it is valid only for the connection that registered the function) |
| `sqlite3_create_function[_v2]` (AGGREGATE: `xStep`/`xFinal`) | ✅ | Real dispatch (design/DESIGN-UDF.md stage 2). `xFunc == NULL` + both of `xStep`/`xFinal` registers an aggregate; half a pair is `SQLITE_MISUSE`; all-NULL deletes. The executor mints one accumulator per group, steps it per surviving row (after `WHERE`/policy/`FILTER`/DISTINCT) and finalizes at the group's end; an EMPTY group finalizes a fresh, never-stepped context (→ NULL, sqlite's rule). Unlike a built-in, a user aggregate is stepped for NULL arguments too — sqlite's behaviour, which Django relies on. The call shape is one argument. Same local-plan rule as a scalar. Verified against CPython's `create_aggregate` (`STDDEV_POP` bare / `GROUP BY` / empty / all-NULL: identical to stock sqlite) |
| `sqlite3_create_window_function` | ❌ stub | Refuse with `SQLITE_ERROR` (destructor honored) — `xValue`/`xInverse` have no mpedb equivalent, and `myagg(x) OVER (…)` is refused at parse |
| `sqlite3_create_collation_v2` | ❌ stub | Refuse with `SQLITE_ERROR` (destructor honored) — DESIGN-UDF stage 3 |
| `sqlite3_set_authorizer` | ❌ stub | `SQLITE_OK`, callback never invoked (mpedb enforces its own RLS) |
| `sqlite3_trace_v2` / `_progress_handler` | ❌ stub | Registration accepted, callback never fired |
| `sqlite3_enable_load_extension` / `_load_extension` | ❌ stub | Enable is a no-op `SQLITE_OK`; load refuses with `SQLITE_ERROR` + errmsg |
| `sqlite3_db_config` | ❌ stub | Fixed-arg shim (register-compatible with the common `(int,int*)` forms on SysV/x86-64); honors no toggles, returns `SQLITE_OK` |
| `sqlite3_limit` | ❌ stub | Reports "no limit"; set is ignored |
| `sqlite3_value_{type,int,int64,double,text,bytes,blob}` | ✅ | Read a scalar UDF's arguments, with sqlite's cross-type coercion (an integer read via `_text` yields its decimal text, …). `_text`/`_blob` pointers stay valid for the duration of the callback |
| `sqlite3_result_{null,int,int64,double,text,blob,error,error_code,error_nomem,error_toobig}` | ✅ | Write a scalar UDF's result cell; `_text`/`_blob` copy in immediately and honor the caller's destructor (STATIC/TRANSIENT respected). `_error*` aborts the statement with that message instead of yielding a row |
| `sqlite3_user_data` | ✅ | Returns the registration's `pApp` |
| `sqlite3_aggregate_context` | ✅ | First call of an aggregation with `nBytes > 0` allocates that many ZEROED bytes; every later call in the SAME aggregation (`xFinal` included) returns the SAME pointer; `nBytes <= 0` never allocates and returns NULL when the group was never stepped. Freed after `xFinal`. NULL inside a scalar callback, as sqlite does for that misuse |
| `sqlite3_context_db_handle` | ❌ stub | Returns NULL |
| Online-backup API (`sqlite3_backup_*`) | ❌ stub | `_init` → NULL (use `mpedb mirror`); the rest are inert |
| Incremental blob (`sqlite3_blob_*`) | ❌ stub | `_open` → `SQLITE_ERROR`; will map onto mpedb's #43 incremental-blob API |
| `sqlite3_serialize` / `_deserialize` | ❌ stub | NULL / `SQLITE_ERROR` |
| `sqlite3_create_module` (virtual tables) | ❌ | Not referenced by `_sqlite3`; FTS is native (design/DESIGN-FTS) |

## Notes, divergences, and design decisions

- **Schema-less open.** sqlite infers structure per file; mpedb refuses a schema
  with no live tables. A fresh `sqlite3_open("new.db")` therefore seeds the file
  with one inert bootstrap table `_mpedb_capi_bootstrap(id)`; user tables are
  created live with `CREATE TABLE`. It is not dropped (mpedb has no
  `sqlite_master` for a consumer to trip over it yet). An **existing** file is
  attached config-free and reads its stored schema.
- **Fixed size — configurable, reserved not grown.** An mpedb file has a fixed
  maximum size, `fallocate`d at creation (crash-safety: no SIGBUS on a disk-full
  mmap write). Defaults are small (16 MiB ephemeral, 64 MiB file-backed); a
  `file:…?size_mb=N` URI (alias `max_size_mb=N`) pre-reserves exactly N MiB for a
  **new** file — both *smaller* than the default (mpedb does not always take
  "several MB" more than asked) and up to the 16 TiB engine cap, so an 800 GiB
  database is `file:big.mpedb?size_mb=819200`. The size is fixed at creation;
  reopening an existing file keeps its geometry and ignores the parameter.
  Exceeding the reservation is `SQLITE_FULL`, never silent growth.
- **`column_count`/`_name` before `step`.** mpedb names a result only by running
  it. For read statements the shim executes lazily when column metadata is first
  requested (Python builds `description` this way); it materializes the whole
  result at that point (no server-side streaming cursor).
- **`decltype` is plan-derived.** A bare base-table column reports its declared
  type, a computed column reports `NULL` — so `sqlite3.PARSE_DECLTYPES` converts
  the same columns as under stock sqlite. (`PARSE_COLNAMES`, which reads a
  `[type]` hint from the column *label*, is orthogonal and works regardless.)
- **Concurrency is better, not bug-for-bug.** mpedb has MVCC readers and
  group-commit; a consumer expecting `SQLITE_BUSY` under contention gets progress
  instead (compatible-or-better).
- **`prepare` `nByte` is an upper bound.** A positive `nByte` bounds the text but
  the statement ends at the first NUL within it — CPython passes `strlen+1`, so
  the shim must not feed the trailing `\0` to the parser.
- **DDL prepares, then applies at `step`.** mpedb applies `CREATE`/`DROP`/`ALTER`
  through `parse_ddl`/`apply_ddl`, not the plan compiler, so the shim skips
  compile-time validation for DDL and defers it to execution (a syntax error in
  DDL surfaces at `step`, not `prepare` — sqlite surfaces some at `prepare`).
- **`lastrowid` is per-thread, copied per-connection.** The facade hook is a
  thread-local drained into the connection's field right after each statement.
  It is exact for single-connection use; the only theoretical bleed is a
  group-commit *leader* draining another **process's** enqueued INSERT in the
  same `query()` call, which the single-process/`durability=none` shim path does
  not do.

## This iteration (2026-07-18): CPython `sqlite3` loads + CRUD + `lastrowid`

`LD_PRELOAD=libmpedb_sqlite3.so python3` now runs the target script against
mpedb:

```
$ LD_PRELOAD=target/debug/libmpedb_sqlite3.so python3 -c "import sqlite3; \
    con=sqlite3.connect(':memory:'); cur=con.cursor(); \
    cur.execute('CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)'); \
    cur.execute(\"INSERT INTO t(b) VALUES('x')\"); print(cur.lastrowid); \
    con.commit(); cur.execute('SELECT a,b FROM t'); print(cur.fetchall())"
1
[(1, 'x')]
```

What it took beyond the core ~30: (a) exporting every `sqlite3_*` symbol
`_sqlite3` resolves (real where easy, safe stubs otherwise); (b) the
`last_insert_rowid` facade hook; (c) a pure-numeric `libversion` (CPython parses
it); (d) treating a positive `nByte` as an upper bound and stopping at the first
NUL — CPython passes `strlen+1`, so the terminator was reaching the parser; (e)
routing `CREATE`/`DROP`/`ALTER` past the compile-time validation (mpedb applies
DDL via `parse_ddl`/`apply_ddl`, not the plan compiler), deferring them to step.

## Remaining blockers for the next milestone (Django), ranked

Addressed since the import-loads milestone (see the tables above): the facade
`last_insert_rowid`, `PRAGMA table_info`/`table_list`/setup pragmas, and the
common `SELECT … FROM sqlite_master` introspection forms — the single biggest
gap for Django's connection setup and schema editor is now covered.

**Resolved (the three biggest Python/Django blockers):**
- ✅ **Named parameters** (`:name`, `@name`, `$name`) — the shim runs a
  quote/comment-aware scan at prepare that assigns each parameter a number
  exactly as sqlite does (all kinds share one space; a repeated name reuses its
  number; a bare `?` takes the next), rewrites the SQL so mpedb's numbered-`$N`
  binder sees `$K` placeholders, and answers `bind_parameter_count`/`_name`/
  `_index` from the maps. mpedb's native binder stays positional — this is
  shim-only. **DB-API battery now 23/23.** Note (sqlite-faithful, verified
  against sqlite 3.45): the `$` sigil is a *named* parameter, so `$5` is the name
  `$5` assigned the next sequential number, NOT positional slot 5 — matching
  sqlite, not mpedb-native `$N`.
- ✅ **Implicit `rowid`** — a PK-less `CREATE TABLE t(a, b)` now synthesizes a
  hidden auto-increment integer `rowid` as the key, exactly like sqlite;
  `SELECT *` hides it, `rowid`/`_rowid_`/`oid` address it, INSERT auto-assigns it,
  explicit-PK tables unchanged (canonical-bytes v5, differential-verified).
- ✅ **DDL inside a transaction** — `CREATE`/`DROP`/`ALTER`/`CREATE INDEX` now
  apply to the open `WriteSession`'s own transaction (atomic commit/rollback,
  in-session visibility), so CPython's implicit-transaction-on-first-DML no longer
  blocks a `CREATE` after an `INSERT`, and `executescript` works.

- ✅ **Host SCALAR UDFs — the old #1 Django gate, now open** (design/DESIGN-UDF.md
  stage 1). `sqlite3_create_function[_v2]` stores the callback per connection, the
  binder resolves an otherwise-unknown `f(args)` against that registry, and exec
  invokes `xFunc` through the `sqlite3_context`/`sqlite3_value` ABI. Measured:
  Django's `register_functions(conn)` now completes all **26** scalar
  registrations (`django_date_extract`, `django_date_trunc`, `regexp`, `MD5`,
  `SHA256`, `RAND`, …) instead of failing on the first one.

- ✅ **Host AGGREGATE UDFs — the gate right after it, also open**
  (design/DESIGN-UDF.md stage 2). `xStep`/`xFinal` register, the parser resolves
  the name into the AGGREGATE grammar (so `FILTER`/DISTINCT ride along), the plan
  carries it by name (`PLAN_FORMAT` 40), and the executor drives one accumulator
  per group over a real `sqlite3_aggregate_context`. Measured: Django's four
  `create_aggregate` calls now all succeed, and a CPython `STDDEV_POP` probe
  matches stock sqlite exactly (bare / `GROUP BY` / empty set / all-NULL).

Still blocking (ranked by real-app impact):

1. **`sqlite_compileoption_used()` — Django's NEXT gate (measured).** With both
   UDF stages in, the `workbench/` Django 5.2 project completes every
   `create_function` AND every `create_aggregate`, then dies three lines later at
   `django/db/backends/sqlite3/_functions.py:85`:
   `select sqlite_compileoption_used('ENABLE_MATH_FUNCTIONS')` →
   `bind error: unknown function sqlite_compileoption_used()`. Django uses the
   answer to decide whether to register its own pure-Python `ACOS`/`SIN`/`POWER`/…
   fallbacks, so returning **0** is both honest and the path of least resistance.
   Run `crates/mpedb-capi/workbench/run.sh` to reproduce.
2. **Host UDFs in a WRITE statement / open transaction** — dispatch is wired on
   the READ path (autocommit `SELECT`, its `WHERE`/projection/aggregate). A UDF or
   host aggregate in an `UPDATE … SET`, an `INSERT` value, a `RETURNING`
   projection, a window PARTITION/ORDER term, **or any statement run inside an
   open transaction** (`WriteSession`) surfaces a clean "host function/aggregate …
   not in scope" error rather than a wrong answer. This one is sharper than it
   looks for Python: CPython opens an implicit transaction after the first DML, so
   a `SELECT myagg(x) …` without an intervening `commit()` takes the write path.
   Verified: the same CPython probe passes byte-identically to sqlite after an
   explicit `commit()`. Closing it means giving the write context the same
   `host_fns()`/`host_aggs()` the read context has.
3. **No custom collations** (`sqlite3_create_collation_v2`) — DESIGN-UDF stage 3.
4. **`sqlite_master` breadth** — views and indexes are not listed; complex
   `WHERE`/join forms error rather than returning wrong metadata.

(Resolved since: **fixed database size** — a `file:…?size_mb=N` URI now
pre-reserves any size up to 16 TiB, so this is no longer a blocker.)

## Verification

- `cargo test -p mpedb-capi` (build/test **standalone** — the crate is excluded
  from the unified workspace build because it exports `sqlite3_*`) — 15 Rust FFI
  tests (open/create/prepare/bind/step/column/exec/errmsg/constraint/
  transactions/persistence/tail/`last_insert_rowid`/`PRAGMA table_info`/
  `sqlite_master`/named-params-by-index/named+positional-mixed) + `sql`-scanner
  unit tests (incl. sqlite-matching parameter numbering) + a **C smoke test**
  (`tests/smoke.c` compiled against `sqlite3.h` and linked to the cdylib) + the
  **Python preload test** below.
- `tests/py_preload.rs` → `tests/py_sqlite3_preload.py` — runs CPython's own
  `sqlite3` module against the shim under `LD_PRELOAD` (import + CRUD +
  `lastrowid`), skipping gracefully when `python3` is absent.
- `python3 crates/mpedb-capi/tests/smoke.py <cdylib>` — a `ctypes` consumer
  drives the same flow (the shape Python's `sqlite3` uses).
- `tests/dbapi_battery.py` — a **DB-API 2.0 compliance battery** (module/
  connection/cursor/execute/executemany/fetch*/description/type round-trip/
  transactions/executescript/IntegrityError). Run it against the shim
  (`LD_PRELOAD=<cdylib> python3 …/dbapi_battery.py`) and against stock sqlite3
  (no preload) for a baseline. **Current: stock 23/23; shim 23/23** — with named
  `:params` now rewritten to numbered placeholders, the shim matches stock across
  the whole battery. No wrong answers, only refusals.
- `tests/dbapi_extra.py` — companion probes over EXPLICIT-PK tables (row_factory/
  `sqlite3.Row`, cursor-as-iterator, arraysize, connection context manager,
  aliased/aggregate column names, unicode+blob, executescript, error classes).
  **stock 11/11; shim 11/11** — the 3 former gaps were all DDL-in-(implicit)-
  transaction, now resolved (DDL applies to the open `WriteSession`'s txn).

## Django's own test suite — run 2 (2026-07-19)

Run 2 re-measures both arms after the CREATE-TABLE-surface commits
`7066a35 d45ad77 35358c6 05bf406 2097f18 fae9e73`, with four of the six
workbench adaptations DELETED — a number measured through a workaround is not
the number.

### ⚠️ Two WRONG ANSWERS (mpedb answers differently, without erroring)

Both are silent: no error, a different result set. They outrank every gap below.

**W1 — a NUMERIC-affinity column stores the text and compares/orders/aggregates
as text.** `d45ad77` made `decimal(10,2)`/`numeric`/`datetime` legal declared
types mapping to `ColumnType::Any`, but `Any` implements neither half of
sqlite's affinity: no storage-class conversion on write, no numeric comparison
affinity. So a refusal became a wrong answer.

```python
c.execute("CREATE TABLE t (id integer NOT NULL PRIMARY KEY, price decimal(10, 2) NOT NULL)")
c.execute("INSERT INTO t (id, price) VALUES (1, ?)", ("1000",))   # Django binds Decimal as str
c.execute("INSERT INTO t (id, price) VALUES (2, ?)", ("35",))
c.execute("SELECT id, price, typeof(price) FROM t ORDER BY id")   # stock (1,1000,'integer'),(2,35,'integer')
                                                                  # mpedb (1,'1000','text'),(2,'35','text')
c.execute("SELECT id FROM t WHERE price < ? ORDER BY id", ("40.0",))  # stock [(2,)]      mpedb [(1,),(2,)]   WRONG
c.execute("SELECT id FROM t ORDER BY price")                          # stock [(2,),(1,)] mpedb [(1,),(2,)]   WRONG
c.execute("SELECT MAX(price) FROM t")                                 # stock [(1000,)]   mpedb [('35',)]     WRONG
```

This is the one that fails `aggregation.tests.AggregateTestCase.test_filtering`:
`Publisher.objects.filter(book__price__lt=Decimal("40.0"))` returns the three
1000-priced books too, because `'1000' < '40.0'` lexicographically. The loud
faces of the same root (`sum() expects a number, got text`, `cannot compare text
with int64`) are gap 1 below.

**W2 — an aggregate `FILTER (WHERE …)` whose predicate contains a CORRELATED
subquery matches no row at all.**

```python
# b=(1,4.5),(2,3.0)   ba=(1, book_id=1)
"SELECT COUNT(*) FILTER (WHERE EXISTS(SELECT 1 FROM ba U0 WHERE U0.book_id = b.id)) FROM b"
#   stock 1   mpedb 0            WRONG
"SELECT COUNT(*) FILTER (WHERE NOT EXISTS(SELECT 1 FROM ba U0 WHERE U0.book_id = b.id)) FROM b"
#   stock 1   mpedb 0            WRONG — and not the negation of the above, so the
#                                predicate is not "always false", the row is dropped
"SELECT b.id, MAX(b.rating) FILTER (WHERE EXISTS(SELECT 1 FROM ba U0 WHERE U0.book_id = b.id)) FROM b GROUP BY b.id"
#   stock (1,4.5),(2,None)   mpedb (1,None),(2,None)     WRONG
```

Adjacent shapes are all CORRECT, which localizes it: an UNcorrelated `EXISTS` in
`FILTER` (stock 2 / mpedb 2), a correlated `IN` subquery in `FILTER` (1 / 1), the
same correlated `EXISTS` in the SELECT list (`(1,1),(2,0)` / same), and the same
correlated `EXISTS` in `WHERE` (4.5 / 4.5). Only `FILTER` + correlated subquery
is wrong. Fails `aggregation.test_filter_argument.FilteredAggregateTests.
test_filtered_aggregate_on_exists` (`{'max_rating': None}` vs `{'max_rating': 4.5}`).

### What was run

Django `5.2.17.dev20260714173342` (`stable/5.2.x`, commit `3e389b7`), CPython
3.12.3, `--parallel=1`, driven by `crates/mpedb-capi/workbench/djsuite/
run_suite.sh`. Same labels, same two groups as run 1.

| Group | Labels | Tests |
|---|---|---|
| G1 | `basic lookup transactions ordering update delete` | 392 |
| G2 | `aggregation annotations expressions` | 439 |

A harness bug found and fixed on the way: `run_suite.sh` held its label groups in
`GROUPS`, which bash owns (the caller's group ids). The assignment is silently
ignored, so the script actually ran the labels `1000` and `27` — `Ran 1 test`
per group. Renamed to `LABEL_GROUPS`.

### The two arms, run 1 → run 2

| | stock libsqlite3 3.45.1 | mpedb shim |
|---|---|---|
| G1 | 392 ran, 0 failed → **392 ran, 0 failed** | 392 ran, 116 failed → **392 ran, 113 failed** (2 F / 111 E) |
| G2 | 439 ran, 5 failed → **439 ran, 0 failed** | 439 ran, 209 failed → **439 ran, 209 failed** (2 F / 207 E) |
| **total** | 826/831 (99.4 %) → **831/831 (100 %)** | 506/831 (60.9 %) → **509/831 (61.3 %)** |
| delta | **+5** | **+3** |

The honest reading of those two deltas:

* The stock arm's +5 is not sqlite improving — those five G2 errors were caused
  by the workbench's own `data_types` adaptation, which is now gone. Run 1's
  "baseline" was 5 tests worse than a real sqlite baseline.
* The shim's +3 is small **because the four gaps that closed were all
  deployment blockers that run 1 had already worked around**. What changed is
  that run 2 needs no workaround for them: the same 831 tests, the same migrate,
  with Django's own `quote_name`, its own `data_types`, its own `DEFAULT`/
  `CHECK`/`REFERENCES`/`CONSTRAINT` DDL. That is the movement; the pass count
  was never going to show it.
* Flattering the shim by 2: G2 skips 7 under the shim vs 5 under stock.
  `supports_json_field` probes with `SELECT JSON('{"a": "b"}')`, which mpedb has
  no `json()` for, so `test_update_jsonfield_case_when_key_is_null` and
  `test_values_expression_alias_sql_injection_json_field` are skipped rather
  than failed. Adjusted, the shim is 507/831.

### Deployment-blocking gaps: what closed

| # | Gap | Status |
|---|---|---|
| **D1** | quoted identifier as the qualifier of a dotted reference (`"t"."id"`) | **CLOSED** (`7066a35`). The `quote_name()` quote-stripper is deleted; Django now quotes every name as it likes and 831 tests still run. |
| **D2** | `AUTOINCREMENT` refused by name | **OPEN by design** — but it now **costs nothing measurable**. With the adaptation disabled (`WB_KEEP_AUTOINCREMENT=1`) G1 is bit-identical: 392 ran, 2 F / 111 E, migrate included. Django 5.2 in this configuration does not put the keyword in front of mpedb. NOT root-caused (see the caveat in the run report); the direct `CREATE TABLE t (id integer PRIMARY KEY AUTOINCREMENT)` probe through the shim is still owed. |
| **D3** | sqlite's declared-type vocabulary | **CLOSED** (`d45ad77`). The hand-written `data_types` table is deleted; Django's own `varchar(100)`/`bigint`/`datetime`/`decimal(10,2)` go straight in. **But see W1** — for the NUMERIC-affinity family this converted a loud refusal into a silent wrong answer. |
| **D4** | `DEFAULT` / `CHECK` / `REFERENCES` in CREATE TABLE | **CLOSED for DEFAULT and CHECK** (`05bf406`, `fae9e73` — parsed AND enforced). The `DEFAULT`-stripping `_iter_column_sql` and `data_type_check_constraints = {}` are deleted, and `supports_*_check_constraints` are back on. `REFERENCES` is **parsed and dropped**: the inline FK clause is now emitted on every ForeignKey (`sql_create_inline_fk` untouched), but nothing enforces it, so `supports_foreign_keys = False` stays as **D4b**. |
| **D5** | table/column constraint may not be NAMED | **CLOSED** (`2097f18`). The `sql_constraint = "%(constraint)s"` override is deleted. |
| **D6** | 120-table ceiling | **OPEN, re-verified.** `queries` still dies at `schema error: too many tables (121 > 120)`. |
| **D7** | 128-byte identifier limit | **OPEN** (`schema.rs:250`, `s.len() <= 128`). Note `backends` now stops on **D6** first — same `too many tables (121 > 120)` — so D7 is confirmed present in the code but is no longer the label's proximate blocker. |
| **D8** | `sqlite_master` breadth (recursive-CTE FK graph) | **OPEN.** The `_references_graph` adaptation is kept. |

### Re-ranked MPEdb-only gaps

305 shim-only failing tests classified by terminal exception (322 failures total;
the difference is subTest repeats collapsing onto one test id). Run-1 rank in
brackets — the order barely moved, because nothing in this window touched the
query path.

| Rank | Tests | Gap | Where |
|---|---|---|---|
| 1 [1] | **68** | **No sqlite affinity; `any` neither coerces nor computes.** 49 × `TypeError: argument must be int or float` (Django's DecimalField converter gets a `str`), 10 × `arithmetic requires int64/float64, got any`, 3 × `cannot assign any to column of type text`, 2 × `cannot mix coalesce() argument types: any and float64`, 1 each `avg() expects a number, got text` / `floor() expects a number, got any` / `cannot compare int64 with text` / `cannot compare with IN list: text and int64`. **Plus wrong answer W1.** | `mpedb-sql`/`mpedb` |
| 2 [2] | ~~47~~ | ✅ **FIXED after this run — host UDFs resolve on the WRITE path** (`design/DESIGN-UDF.md` §The WRITE path). Scalar and aggregate closures reach the write context via `exec::WriteCtx`, gated by a single `Database::host_tables(plan)` snapshot taken only when the plan contains a host call; `WriteSession`, autocommit DML and the group-commit leader's own statement all carry them. A host-call plan still never enters the shared `plan/<hash>` registry and never rides the intent ring. The out-of-scope contexts that remain refuse cleanly (`Unsupported`) instead of the old `internal error (bug in mpedb)`. **Not re-measured** — the 47 is run-2's number, and the next Django run should show it move. | `mpedb` `ring_exec` / write `TxnCtx` |
| 3 [4] | **45** | **Subquery / derived-table restrictions**: JOIN inside a derived table 14, correlated subquery outside `WHERE` in an aggregate query 8, unlifted `IN` subquery 5, aliased/renamed column 4, GROUP BY/HAVING body 4, correlated `IN` ("rewrite as EXISTS") 4, subquery in `HAVING` 2, unsupported position 2, compound (`UNION`) body 1, `ORDER BY` body 1. | `mpedb-sql` planner |
| 4 [3] | **44** | **Missing sqlite scalar functions**: `quote()` 40 statements (Django's `last_executed_query`), `strftime()` 8, `json()` (the feature probe → the 2 skips above). | `mpedb-sql` builtins |
| 5 [5] | **39** | **No int↔bool bridge**: `predicate must be a boolean expression, got int64` 29, `NOT requires a boolean, got int64` 3, `parameter $N is int64, statement requires bool` 3, `cannot compare int64 and bool` 1, `cannot assign bool to column of type int64` 1, `predicate evaluated to int64, expected bool` 1, `CASE WHEN must be a bool condition, got int64` 1. | `mpedb-sql` binder |
| 6 [6] | **20** | **`LIKE … ESCAPE` (44 statements) and `ORDER BY … NULLS FIRST/LAST` (6)** are not parsed. | `mpedb-sql` parser |
| 7 [7] | **10** | **Rigid numeric parameter typing**: `$N is int64, statement requires float64` 6, `$N is float64, requires int64` 2, `cannot assign float64 to column of type int64` 2. | `mpedb-sql` |
| 8 [8] | **8** | **Bitwise operators absent** (`\|`, `&`, `<<`, `>>`, `^`) — including 3 tests that surface as `cannot assign any to column of type int64` on Django's XOR emulation. | `mpedb-sql` tokenizer |
| 9 [9] | **5** | **`REGEXP` requires a literal pattern**; Django always binds it. | `mpedb-sql` |
| 10 | **3** | **`FILTER (WHERE …)` parse**: `expected ) closing FILTER (WHERE …)`. | `mpedb-sql` parser |
| 11 | **3** | `expected ) after IN subquery` 2, `after EXISTS subquery` 1. | `mpedb-sql` parser |
| 12 [10] | **2** | **2-argument `MAX(a,b)` / `MIN(a,b)`** (sqlite's scalar form). | `mpedb-sql` |
| 13 | **2** | `INSERT values must be literals or parameters`. | `mpedb-sql` |
| 14 [11] | **1** | **PANIC in the binder** (`binder.rs:235`, `Scope::only()` on a 2-table scope), surfaced as `internal error (panic) in engine`. | `mpedb-sql` |

One-offs: `unknown column` 1, `expected parameter number after $` (an identifier
containing `$`) 1, `expected X` 2.

**Not counted against mpedb:** the two `delete` FAILs (`test_fast_delete_all`,
`test_fast_delete_instance_set_pk_none`) are contamination, not behaviour —
`delete.tests.DeletionTests.test_only_referenced_fields_selected` errors out
between `signal.connect(receiver, sender=Referrer)` and its `disconnect`, so the
leaked receiver makes `Collector.can_fast_delete()` return False for every later
test in the class. Fix any earlier gap and they go away.

### Coverage — re-checked, unchanged

* **`queries` (493 tests) — still cannot run.** `django.db.utils.OperationalError:
  schema error: too many tables (121 > 120)` during migrate (D6).
* **`backends` — still cannot run**, now for D6 rather than D7: same
  `too many tables (121 > 120)`. The 128-byte identifier limit is still there
  (`crates/mpedb-types/src/schema.rs:250`), it just is not what stops the label
  first any more.
* Still 9 of Django's 219 labels, 831 of ~19 000 tests, `--parallel=1`, no
  concurrency or multi-process behaviour measured.

### Workbench adaptations after run 2

Removed (gap closed): `quote_name()` quote-stripping, the `data_types` table, the
`DEFAULT`-clause stripper, `data_type_check_constraints = {}`, the two
`supports_*_check_constraints = False`, `sql_create_inline_fk`/
`sql_create_column_inline_fk = None`, `sql_constraint`.

Kept: `data_types_suffix = {}` (D2 — now demonstrably free, see above),
`supports_foreign_keys = False` (D4b — REFERENCES parsed, not enforced),
`_references_graph` (D8).
