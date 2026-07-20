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
stock sqlite **23/23**, and measured against **CPython's own `test_sqlite3`
suite** (the authoritative consumer test of that surface: 344 of the 461
tests stock passes — see the section at the end; the suite hammers contract
details — destructor rules, trace, limits, error codes — that "the ~50
functions exist" does not capture, so it is the scope's honest yardstick).
It does *not* enumerate every symbol, because most are
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
| `sqlite3_busy_timeout` | ✅ | **Honored end-to-end (#109).** The knob is mirrored into `Database::set_busy_timeout`, so the ENGINE's writer-lock wait itself is bounded: cross-process (or cross-thread) writer contention answers `SQLITE_BUSY` / "database is locked" *at the deadline* — measured 300 ms timeout → Busy at 300.1 ms — instead of blocking forever (was engine gap E1). Timeout 0 (default) = one immediate attempt, immediate BUSY, as sqlite. A sibling connection on the SAME thread is an unwinnable wait (the owner is the caller's thread), so it answers BUSY immediately rather than burning the timeout. On top of that, the shim still retries RETRYABLE contention errors (optimistic-mode `WriteConflict`, full reader table, evicted snapshot) with sqlite's own busy-handler backoff table; the engine's `Busy` is terminal for that loop (the timeout was already honored in full — no double wait). No `sqlite3_busy_handler` is exported (CPython never calls it; the timeout is the only mechanism) |
Coverage note: the busy budget bounds every `Database` facade write entry; the sqlite-backed overlay (`SqliteOverlay`) still uses blocking acquisition — its lock discipline is sqlite's own file locks, a separate mechanism.

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
| `sqlite3_column_type` | ✅ | `Int`/`Bool`/`Timestamp`→`SQLITE_INTEGER`, `Float`→`FLOAT`, `Text`→`TEXT`, `Blob`→`BLOB`, `Null`→`NULL`. **`typeof()` agrees with this for every value** — see the `typeof()` note below |
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
| `PRAGMA busy_timeout` / `= N` | ✅ | **Real.** The same milliseconds `sqlite3_busy_timeout()` sets and the BUSY retry loop honours — the one setter pragma the shim can implement truthfully. Both forms answer one row named `timeout` holding the value now in force (sqlite's shape, including for the setter); a negative clamps to 0 |
| `PRAGMA foreign_keys` (getter) | 🚧 | Always `0`, which is BOTH sqlite's own default and the literal truth: mpedb parses `REFERENCES` and discards it. The setter is a no-op, so `= ON` then a read still reports `0` — a deliberate divergence (gap D11): reporting `1` would tell a consumer its FK violations will be caught when they will not |
| `PRAGMA journal_mode` / `user_version` / `schema_version` (getters) | 🚧 | Return a conventional value (`journal_mode` = `memory`, which is what mpedb actually does) |
| `PRAGMA <x> = <v>` and other pragmas (setters) | 🚧 | Accepted as a no-op (the common DB-setup pragmas never error), and **their getters are not stored-and-echoed** — `PRAGMA synchronous` / `cache_size` return no row rather than replay a value mpedb does not honour (gap D10). Echoing would answer a durability probe differently rather than erroring, which is the one thing this shim must not do |
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
  from the unified workspace build because it exports `sqlite3_*`) — 40 Rust FFI
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

> **⚠️ The shim arm's numbers in this section are NOT VALID** — see "The
> contamination" under run 3. The shim read `file:…?mode=memory` as a path, so
> Django's test database survived as a file between runs, and `migrate` then
> skipped every table that already existed. Run 2's shim arm therefore issued no
> DDL and ran against run 1's schema. The stock arm and the two WRONG-ANSWER
> findings (W1, W2) stand; the pass counts and the "D2 costs nothing" finding do
> not.

Run 2 re-measures both arms after the CREATE-TABLE-surface commits
`7066a35 d45ad77 35358c6 05bf406 2097f18 fae9e73`, with four of the six
workbench adaptations DELETED — a number measured through a workaround is not
the number.

### ⚠️ Two WRONG ANSWERS (mpedb answers differently, without erroring)

Both were silent: no error, a different result set. They outrank every gap below.
**W2 is FIXED (`b190bde`); W1 is still open.**

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
subquery matched no row at all. FIXED (`b190bde`).**

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

Adjacent shapes were all CORRECT, which localized it: an UNcorrelated `EXISTS` in
`FILTER` (stock 2 / mpedb 2), an `IN` subquery in `FILTER` (1 / 1), the same
correlated `EXISTS` in the SELECT list (`(1,1),(2,0)` / same), and the same
correlated `EXISTS` in `WHERE` (4.5 / 4.5). Only `FILTER` + correlated subquery
was wrong. Failed `aggregation.test_filter_argument.FilteredAggregateTests.
test_filtered_aggregate_on_exists` (`{'max_rating': None}` vs `{'max_rating': 4.5}`).

**Root cause.** A correlated subplan's result slot is filled PER ROW, after the
gather, into a scratch `[user ‖ subplan results]` vector. `exec_aggregate` threw
that scratch away and evaluated `AggCall::filter` against the pre-fill `params`,
where the slot is still NULL — and a NULL filter REJECTS the row under 3VL. Hence
both polarities counting 0: the row was dropped, never tested. The shape was not
refused either, because the planner's `reject_correlated_in_aggregate` does not
inspect `AggCall::filter` (correctly — `FILTER` is per-row, like `post_filter`),
while `validate`'s mirror of it did, so the two paths disagreed about the same
plan. Fixed by keeping the scratch per surviving row and evaluating the per-row
programs against it; `validate` drops its `a.filter` check to match. The GROUPED
programs (HAVING, projection, ORDER BY) still read `params` and a correlated slot
in them is still refused — there is no per-row correlation once rows are grouped.
No PLAN_FORMAT change. All four statements above now match stock exactly
(`1`, `1`, `(1,4.5),(2,NULL)`, `max_rating = 4.5`); the differential coverage is
`crates/mpedb/tests/agg_filter.rs`.

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

  **This was a trap worth 439 tests, not 2 — and it is now closed.** Both
  JSONField models in these labels (`tests/annotations/models.py::JsonModel`,
  `tests/expressions/models.py::JSONFieldModel`) declare
  `Meta.required_db_features = {"supports_json_field"}`, so at run 2 Django
  simply did not create them. Make the probe succeed and Django DOES create them
  — with Django's own `data_type_check_constraints["JSONField"]`, i.e.
  `CHECK ((JSON_VALID("data") OR "data" IS NULL))`. That CHECK failed to compile
  for **two independent reasons**, both verified directly:

  ```
  CREATE TABLE k (… data text NOT NULL CHECK ((json_valid(data) OR data IS NULL)))
    -> CHECK on `data` failed to compile: unknown function `json_valid()`
  CREATE TABLE j (… data text NOT NULL CHECK ((length(data) OR data IS NULL)))
    -> CHECK on `data` failed to compile: AND/OR requires boolean operands, got int64
  ```

  A CREATE TABLE failure during `migrate` aborts `create_test_db()`, which takes
  down the whole label. Closing this needed **two** things together, and both
  are now in:

  * **DONE (PLAN_FORMAT 46): the JSON function set**, not just `json()`. `json`,
    `json_valid`, `json_type`, `json_quote`, `json_array_length`,
    `json_extract`, the `->` and `->>` operators, `json_array`, `json_object`,
    `json_patch`, `json_remove`, `json_replace`, `json_set`, `json_insert` —
    fifteen `ScalarFn` tags, 44 (the one this note reserved) through 58,
    differential-tested against `sqlite3` 3.45.1 in
    `crates/mpedb/tests/json_fn.rs`. `json_valid()` returns sqlite's INTEGER 0/1,
    which is what the CHECK needs.
  * **DONE: gap 5, the int↔bool bridge**, landed separately (an integer in a
    boolean context is truthy-tested). That was the other half: it is what makes
    `JSON_VALID()`'s INTEGER 0/1 usable as the left operand of the `OR`.

  With both halves in, `django_jsonfield_check_compiles_and_enforces` in
  `json_fn.rs` no longer takes its early-return path — it now compiles Django's
  CHECK verbatim, accepts a valid document and `NULL`, **rejects** `'not json'`,
  and resolves the `data__k` / `data__k__0` lookups Django compiles
  (`JSON_EXTRACT`, `->>`, `JSON_TYPE`). `supports_json_field` can be turned on;
  the 439 gated tests are unblocked and want a run 3 to be scored.

  On the JSON5 subtlety this note flagged: sqlite 3.45's `json()` and
  `json_valid()` **deliberately disagree** — `json('{a:1}')` is `{"a":1}` (it
  accepts JSON5 and REWRITES it) while `json_valid('{a:1}')` is `0` (strict
  RFC 8259). The resolution shipped is to refuse JSON5 in `json()` too, so the
  two AGREE: `json_valid` answers `0` there, matching sqlite, and `json()`
  raises a named error instead of rewriting — a refusal, not a wrong answer. The
  same reasoning drives the depth bound: mpedb parses 128 levels where sqlite
  parses 1000, and `json_valid()` **raises** past that instead of answering `0`,
  because sqlite answers `1`. See COMPAT.md's JSON section for the full list of
  refusals, including the JSON-subtype shapes.

### Deployment-blocking gaps: what closed

| # | Gap | Status |
|---|---|---|
| **D1** | quoted identifier as the qualifier of a dotted reference (`"t"."id"`) | **CLOSED** (`7066a35`). The `quote_name()` quote-stripper is deleted; Django now quotes every name as it likes and 831 tests still run. |
| **D2** | `AUTOINCREMENT` refused by name | **OPEN by design** — but it now **costs nothing measurable**. With the adaptation disabled (`WB_KEEP_AUTOINCREMENT=1`) G1 is bit-identical: 392 ran, 2 F / 111 E, migrate included. Django 5.2 in this configuration does not put the keyword in front of mpedb. NOT root-caused (see the caveat in the run report); the direct `CREATE TABLE t (id integer PRIMARY KEY AUTOINCREMENT)` probe through the shim is still owed. |
| **D3** | sqlite's declared-type vocabulary | **CLOSED** (`d45ad77`). The hand-written `data_types` table is deleted; Django's own `varchar(100)`/`bigint`/`datetime`/`decimal(10,2)` go straight in. **But see W1** — for the NUMERIC-affinity family this converted a loud refusal into a silent wrong answer. |
| **D4** | `DEFAULT` / `CHECK` / `REFERENCES` in CREATE TABLE | **CLOSED for DEFAULT and CHECK** (`05bf406`, `fae9e73` — parsed AND enforced). The `DEFAULT`-stripping `_iter_column_sql` and `data_type_check_constraints = {}` are deleted, and `supports_*_check_constraints` are back on. `REFERENCES` is **parsed and dropped**: the inline FK clause is now emitted on every ForeignKey (`sql_create_inline_fk` untouched), but nothing enforces it, so `supports_foreign_keys = False` stays as **D4b**. |
| **D5** | table/column constraint may not be NAMED | **CLOSED** (`2097f18`). The `sql_constraint = "%(constraint)s"` override is deleted. |
| **D6** | 120-table ceiling | **CLOSED** (design/DESIGN-TABLE-CAP.md, PLAN_FORMAT 42). Footprints and the CDC capture config stopped being per-table bitmaps and became sparse `TableSet`s, so the id space is no longer an integer width: `MAX_TABLES` = 4096, **4088 live user tables**. Not re-measured against Django yet — the ceiling is gone from the code; whether `queries` then runs clean is the next measurement. |
| **D7** | 128-byte identifier limit | **CLOSED** (same pass). Limit is 255 bytes, and the identifier CHARACTER set moved with it: a quoted name may contain spaces, punctuation, `"` (doubled), a leading digit and non-ASCII, matching sqlite 3.45.1. Only control characters (NUL above all — the C-API hands names out as NUL-terminated `const char*`), the empty name and the `__mpedb` prefix are refused. Django's 134-char generated m2m through-table name now fits. |
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
| 3 [4] | **45** | **Subquery / derived-table restrictions.** ⚠️ **This row's sub-breakdown was MIS-ATTRIBUTED — see "Subquery family, re-derived" below.** Two of its lines are CLOSED (#97: unlifted `IN` subquery, correlated `IN`) and the rest collapses to two root causes, neither of which is "a JOIN in a derived table". | `mpedb-sql` planner |
| 4 [3] | ~~44~~ | ✅ **CLOSED.** `quote()` 40 statements (Django's `last_executed_query`) and `strftime()` 8 closed at PLAN_FORMAT 41; the **whole JSON function set** closed at PLAN_FORMAT 46 (`ScalarFn` 44–58 plus the `->`/`->>` operators). All sqlite-differential-tested. `quote()` refuses only the reals whose sqlite rendering is build-dependent, `strftime()` only the modifier/`'now'`/Julian-day forms, JSON only JSON5/JSONB/>128-deep and the three JSON-subtype-undecidable argument shapes — each a named error, never a wrong answer. | `mpedb-sql` builtins |
| 5 [5] | ~~39~~ | ✅ **FIXED after this run — sqlite truthiness + the int/bool value bridge.** A non-boolean in a boolean position (WHERE/HAVING/ON/FILTER, `NOT`, `AND`/`OR`, `CASE WHEN`, `CHECK`, `ON CONFLICT … WHERE`) is truthy-tested exactly as `sqlite3VdbeBooleanValue` does — NULL unknown, integer `!= 0`, everything else `RealValue(x) != 0.0` (the leading-float-prefix parse, over text AND a blob's raw bytes). It desugars in the binder to `x <> 0` / `CAST(x AS REAL) <> 0.0`, so **no new opcode and no `PLAN_FORMAT` bump**. The VALUE bridge is deliberately narrower than "int and bool are interchangeable": in a comparison an int const 0/1 folds into the bool domain (`flag = 1` → `flag = TRUE`, keeping the index-probe shape) and anything else casts the bool side up to its integer, so `flag = 2` is FALSE — sqlite's answer; a bool assigned to an int column is exact (`TRUE` → 1); an int into a bool column converts **only** 0/1; a parameter bound as 0/1 into a bool slot converts (CPython binds `True` via `sqlite3_bind_int64`). Arithmetic on a bool is still rigid. 24 values × 8 boolean positions diffed against the 3.45.1 binary in `crates/mpedb/tests/bool_truthiness.rs`; the C-API path in `capi.rs::django_boolean_field_through_the_c_api`. **Not re-measured** — 39 is run-2's number. Together with the JSON row above this also unblocks the **439-test JSONField label**: both halves of `CHECK ((JSON_VALID("data") OR "data" IS NULL))` — the unknown function AND the int64 boolean operands — are now closed, so Django should create those tables and RUN those tests. That is a prediction to be measured, not a claim. | `mpedb-sql` binder |
| 6 [6] | **20 (+6)** | **`LIKE … ESCAPE` (44 statements) and `ORDER BY … NULLS FIRST/LAST` (6)** are not parsed. **Absorbs rows 10 and 11** — those 6 tests are this gap seen through an enclosing `FILTER`/`IN`/`EXISTS` paren, so the true weight is ~26. | `mpedb-sql` parser |
| 7 [7] | **10** | **Rigid numeric parameter typing**: `$N is int64, statement requires float64` 6, `$N is float64, requires int64` 2, `cannot assign float64 to column of type int64` 2. | `mpedb-sql` |
| 8 [8] | **8** | **Bitwise operators absent** (`\|`, `&`, `<<`, `>>`, `^`) — including 3 tests that surface as `cannot assign any to column of type int64` on Django's XOR emulation. | `mpedb-sql` tokenizer |
| 9 [9] | **5** | **`REGEXP` requires a literal pattern**; Django always binds it. | `mpedb-sql` |
| ~~10~~ | ~~3~~ | **MISATTRIBUTED — these are gap 6, and are folded into it.** `expected ) closing FILTER (WHERE …)` is `LIKE … ESCAPE` inside the FILTER predicate: `ESCAPE` is not a reserved word, so the parser walked past it and blamed the paren. `FILTER` itself parses. (`25b3633` makes the refusal name ESCAPE.) | — |
| ~~11~~ | ~~3~~ | **MISATTRIBUTED — also gap 6.** `expected ) after IN/EXISTS subquery` is `ORDER BY … NULLS FIRST/LAST` or `LIKE … ESCAPE` *inside* the subquery, surfacing at the subquery's closing paren. `IN (SELECT …)` / `EXISTS (SELECT …)` themselves parse, `LIMIT`/`OFFSET`/`ORDER BY`/`GROUP BY`/`HAVING`/`DISTINCT`/`UNION` bodies included. Repros pinned in `crates/mpedb/tests/django_parse_gaps.rs`. | — |
| 12 [10] | **2** | **2-argument `MAX(a,b)` / `MIN(a,b)`** (sqlite's scalar form). | `mpedb-sql` |
| 13 | **2** | `INSERT values must be literals or parameters`. | `mpedb-sql` |
| 14 [11] | **1** | **PANIC in the binder** (`binder.rs:235`, `Scope::only()` on a 2-table scope), surfaced as `internal error (panic) in engine`. | `mpedb-sql` |

One-offs: `unknown column` 1, `expected parameter number after $` (an identifier
containing `$`) 1, `expected X` 2.

### Subquery family, re-derived (2026-07-19, #97)

Gap 3's sub-breakdown was re-measured from a fresh `WB_TRACE_SQL_ERRORS=1` run
and **root-caused**, statement by statement. The published line items were an
artifact of the order in which `view.rs::check_simple` tests the derived-table
grammar: it reported only the FIRST failing check, so a body that had a JOIN
*and* a `GROUP BY` was filed under "JOIN". Fixing only what the label named
would have closed nothing. `check_simple` now names EVERY blocking reason.

Two caveats on the run itself. (a) The suite could not run at all on `main`
that day — `CREATE INDEX … ON django_session(expire_date)` fails because
Django's `datetime` takes NUMERIC affinity → `any` and mpedb cannot index an
`any` column, which aborts `create_test_db()` for BOTH label groups. The run
therefore used `WB_SOFT_CREATE_INDEX=1` (an index is never an answer, so no
result depends on it). (b) These are STATEMENT counts, as the original was.

| category | before | after #97 | root cause |
|---|---|---|---|
| derived body uses a JOIN | 14 | 14 | **NOT a join gap.** All 13 distinct statements ALSO have `GROUP BY` (11) or `DISTINCT` (2) — Django's `.aggregate()` over `.annotate()`. Flattening the join closes **zero**; only MATERIALIZING the body does. |
| correlated subquery in an aggregate query, outside `WHERE` | 10 | **1** ✅ | All 10 are `SELECT (corr subq) AS x, count(*) … GROUP BY <that expr>`. CLOSED by #97: a GROUP BY key and an aggregate ARGUMENT are per-ROW positions — `exec_aggregate`'s row loop already evaluated them against that row's FILLED scratch, so only the planner refusal and its `validate` mirror had to move from "WHERE only" to "per-row positions only". The 1 left is a genuine per-GROUP read (a grouped SELECT-list expression that is not itself a key) and stays refused. |
| unlifted `IN` subquery (position) | 9 | **0** ✅ | **7 of the 9 were `DELETE`/`UPDATE … WHERE pk IN (SELECT …)`** — the write planners simply never ran the lift. CLOSED by #97, no format bump. The 2 left are an `INSERT … VALUES ((SELECT …))` and a subquery in a `GROUP BY` key, both still refused (and both counted under "unsupported position" below). |
| derived body has an aliased/renamed column | 7 | 7 | **Flattening these closes zero too.** Every one projects a correlated scalar/`EXISTS`/window under an alias and is consumed by an outer aggregate `FILTER`/argument, so a projection remap converts it into "correlated subquery in an aggregate argument" — the row above. |
| derived body has `GROUP BY`/`HAVING` | 5 | 5 | Materialization. |
| correlated `IN` ("rewrite as EXISTS") | 4 | **0** ✅ | CLOSED by #97. The refusal predated the per-row correlation fill and needed nothing new; `List` differs from `Exists` only in what `subplan_value` reduces the rows to. |
| subquery in `HAVING` | 2 | 2 | Per-GROUP position. |
| unsupported position | 2 | **1** | `INSERT … VALUES ((SELECT …))` remains. A subquery as a `GROUP BY` key is CLOSED — the lift now descends into `GROUP BY`, which is a per-row program. |
| compound (`UNION`) derived body | 1 | 1 | Materialization. |
| derived body has `ORDER BY` | 1 | 1 | Dropping it would change an unspecified output order — refused. |
| **total** | **55** | **32** | statements. Test ERRORS over the same two label groups: 137 → 114. |

**After #97 the remaining gap is ONE thing, not nine: materializing a derived
table.** 28 of the 32 remaining statements are Django's
`SELECT <agg> FROM (<body that groups, aggregates or DISTINCTs>) subquery`
(`.aggregate()` over `.annotate()`). There is no flattening rewrite for them —
the body changes cardinality, which is exactly what a splice cannot express.
The primitive already exists in the engine: the recursive-CTE working table
(`CTE_TABLE`, `exec/recursive.rs`) answers a scan of a sentinel table id from an
in-memory row set, which is what a materialized derived table needs. Cost: plan
the body once, resolve the derived alias to a synthetic `TableDef` carrying the
body's output columns and types, and a `PLAN_FORMAT` bump for the new node.

The four stragglers: 2 × subquery in `HAVING`, 1 × `INSERT … VALUES ((SELECT
…))`, 1 × a correlated subquery in a grouped SELECT-list expression that is not
itself a GROUP BY key. All three positions are genuinely per-GROUP or per-VALUE
holes and are refused by name.

**Not counted against mpedb:** the two `delete` FAILs (`test_fast_delete_all`,
`test_fast_delete_instance_set_pk_none`) are contamination, not behaviour —
`delete.tests.DeletionTests.test_only_referenced_fields_selected` errors out
between `signal.connect(receiver, sender=Referrer)` and its `disconnect`, so the
leaked receiver makes `Collector.can_fast_delete()` return False for every later
test in the class. Fix any earlier gap and they go away.

### Coverage — re-checked, unchanged

* **`queries` (493 tests) — the ceiling that blocked it is GONE** (D6 closed,
  2026-07-19: `MAX_TABLES` 128 → 4096). Not yet re-measured: what the label does
  once migrate completes is the next run's question, not a claim made here.
* **`backends` — both of its blockers are closed** (D6 and D7). The 128-byte
  identifier limit is now 255 and no longer rejects spaces or punctuation, so
  the 134-char generated m2m name fits. Also unmeasured as yet.
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

## Django's own test suite — run 3 (2026-07-19)

Run 3 re-measures both arms after the six merges of 2026-07-19: type affinity
(canonical-bytes v7), host UDFs on the WRITE path, `quote()`+`strftime()`
(PLAN_FORMAT 41), the int↔bool bridge, `FILTER` + correlated subquery (W2), and
the table cap 120 → 4088 with a 255-byte identifier limit (PLAN_FORMAT 42).

**Read this first: run 2's shim numbers were measured against a database it did
not create.** See "The contamination" below. Run 3's numbers are the first
clean ones since run 1.

### ⚠️ One ANSWER divergence (no error, a different answer)

**`typeof()` of a BOOLEAN column answers `'boolean'`; sqlite answers
`'integer'`.** sqlite's `typeof()` has exactly five possible answers
(`null`/`integer`/`real`/`text`/`blob`); mpedb has first-class `Bool` and
`Timestamp`, and `sqlite_typeof` (`mpedb-types/src/expr/scalar.rs`) names them
honestly. That is defensible for NATIVE mpedb and is documented there as
deliberate — but through a **libsqlite3 shim** it is a value sqlite can never
return, so a consumer switching on `typeof(x)` takes the wrong branch.

```python
c.execute("CREATE TABLE bt (id integer PRIMARY KEY, flag bool NOT NULL)")
c.execute("INSERT INTO bt VALUES (1, ?)", (True,))
c.execute("SELECT flag, typeof(flag) FROM bt")   # stock (1,'integer')
                                                 # mpedb (1,'boolean')   DIVERGES
```

Not fixed here: `scalar.rs` is `mpedb-types`, outside this workbench's remit,
and the right fix is a decision about the shim's contract (map mpedb's extra
type names to sqlite's five at the C-API boundary, or accept the divergence and
document it as one). No Django test in the 1 155 measured hits it.

**Nothing else.** Both W1 and W2 are gone as wrong answers: W2 is fixed, and W1
(`decimal(10,2)` compared as text) is now a REFUSAL — affinity converts on
store, the comparison half is still missing, so `price < '40.0'` errors instead
of answering wrongly. And the strongest evidence is structural: of the 141
failing test outcomes under the shim in group A, **141 are ERRORs and 0 are
FAILs** — every one is a refusal, not a different answer. (Run 2 had 4 FAILs.)
The differential probe behind this section covers affinity storage/ordering/
aggregation, the bool bridge, `quote`/`strftime`, W2's shapes and the CAST/
arithmetic/`IN` surface.

### The contamination — why run 2's numbers were not real

The shim read `file:<name>?mode=memory` as a PATH. Django's test runner names
every test database exactly that way (`file:memorydb_default?mode=memory&
cache=shared`), so the shim created a 64 MiB FILE called `memorydb_default` in
Django's `tests/` directory — and because Django treats an in-memory database as
never-closing, it never deleted it. **The file survived the process.**

Django's `migrate --run-syncdb` skips any model whose table already exists
(`sync_apps()` filters the manifest through `connection.introspection.
table_names()`). So on every run after the first, the shim arm created **no
tables at all** and ran against the schema left by the previous run. Run 2's
headline claim — that it measured the new CREATE TABLE surface (D1–D5) without
workarounds — was measuring a migrate that issued no DDL. Its "D2 costs
nothing, migrate included" finding has the same explanation, and is false: D2
now costs all 831 tests (below).

Run 3 only noticed because canonical-bytes v7 made the stale file unopenable;
before that it opened fine and the contamination was silent. Fixed in
`b5b7405`: `mode=memory` now resolves to a per-process tmpfs file, refcounted
per name — first open in a process starts empty, later opens attach to the same
database (sqlite's shared-cache in-memory semantics), last close destroys it.
Nothing in the CWD, nothing outliving the process. A second shim fix landed with
it: a failed open reported `InterfaceError: out of memory` for every cause,
because CPython reads `sqlite3_errmsg(NULL)` and sqlite's fixed answer there is
that constant; the real reason is now recorded and answered.

### What was run

Django `5.2.17.dev20260714173342` (`stable/5.2.x`, commit `3e389b7`), CPython
3.12.3, system libsqlite3 3.45.1, `--parallel=1`, driven by
`crates/mpedb-capi/workbench/djsuite/run_suite.sh`.

Reported as **two separate measurements**, because the table-cap closure changed
the test population and a single total would conflate "the fixes worked" with
"more tests now run".

### A — comparability: the SAME 9 labels, 831 tests

Frozen since run 1, so the arms are comparable across runs.

| | stock 3.45.1 | mpedb shim run 2 | mpedb shim run 3 |
|---|---|---|---|
| G1 `basic lookup transactions ordering update delete` | 392 ran, 0 failed | 392 ran, 113 failed | **392 ran, 42 failed** (0 F / 42 E) |
| G2 `aggregation annotations expressions` | 439 ran, 0 failed | 439 ran, 209 failed | **439 ran, 99 failed** (0 F / 99 E) |
| **total** | **831/831 (100 %)** | 509/831 (61.3 %) | **690/831 (83.0 %)** |
| delta vs run 2 | — | — | **+181** |

Adjusted for the two tests the shim SKIPS rather than fails (the
`supports_json_field` probe needs `json()`, so Django does not create either
JSONField model — re-verified, same two tests as run 2), the honest comparable
figure is **688/831**.

**Which gaps closed, by test movement:**

| Fix | Run-2 weight | Run-3 weight | Verdict |
|---|---|---|---|
| Host UDFs on the WRITE path | 47 | **0** | CLOSED — the bucket is gone entirely; no `internal error (bug in mpedb)` and no UDF-shaped failure remains. |
| int↔bool bridge | 39 | **0** | CLOSED — bucket gone. |
| `quote()` + `strftime()` | 44 | **3** | CLOSED for `quote()` (40 statements, bucket gone). `strftime()` remains only for `'now'`, which mpedb refuses by name. |
| Type affinity | 68 | **18** | LARGELY CLOSED as a storage problem (and W1 downgraded from wrong answer to refusal). What is left is the COMPARISON half: `arithmetic … got any` 10, `cannot compare int64 with text` 3, `coalesce` type mixes 3, `floor() … got any` 1, `IN` list 1. |
| `FILTER` + correlated subquery (W2) | wrong answer | **fixed** | CLOSED — all four shapes match stock. |
| Table cap 120 → 4088 | — | — | See B. |

### B — new coverage: `queries` and `backends`

| Label | Tests | stock 3.45.1 | mpedb shim |
|---|---|---|---|
| `queries` | 493 | 493 ran, 0 failed | **BLOCKED at migrate — 0 tests run** |
| `backends` | 324 | 324 ran, 5 failed (319/324) | **324 ran, 13 failed (311/324)**; 5 shared with stock, **8 shim-only** |

`backends` runs for the first time — both of its blockers (D6 table cap, D7
identifier limit) are genuinely closed. The cap is closed at the C-API level
too, verified directly: 400 sequential `CREATE TABLE`s through the shim in one
database all succeed, where the old ceiling was 120.

`queries` is **still blocked, by a NEW gap, not by the cap** — it dies at the
7th of its 95 models:

```
queries/models.py:69   class DateTimePK(models.Model):
                           date = models.DateTimeField(primary_key=True, …)

CREATE TABLE "queries_datetimepk" ("date" datetime NOT NULL PRIMARY KEY)
  -> schema error: primary key column `queries_datetimepk.date` cannot be `any`:
     a key is memcmp-ordered, and ordering across types would mean inventing
     whether 5 sorts before "a" — declare the column's real type
```

This is gap D9 (below) in its PRIMARY KEY form, and unlike the index form it has
no honest workaround — changing a model's pk type changes the model. It is the
FIRST blocker, not provably the only one.

The 8 shim-only `backends` failures: `DDL inside a SAVEPOINT` 2, unknown
function `date()` 1, `strftime('now')` 1, `REGEXP` needs a literal 1, and three
introspection/PRAGMA behaviours — `PRAGMA synchronous` returns NO ROW where
sqlite returns a value (`test_init_command` gets `TypeError: 'NoneType' object
is not subscriptable`), and `PRAGMA foreign_keys` always reads 0, so Django's
two `constraint_checks_enabled()` assertions fail.

### Workbench adaptations — re-measured, not assumed

Each adaptation is now an ABLATION SWITCH (`WB_NO_D2`/`WB_NO_D8`/`WB_NO_D9`),
and each was removed in turn and measured, both arms, on the A labels. The stock
arm was 831/831 in all four ablations — every cost below is the shim's.

| Adaptation | Ablated result (shim) | Cost | Kept? |
|---|---|---|---|
| `data_types_suffix = {}` (D2, AUTOINCREMENT) | migrate dies, **0 of 831 run** | **all 831** | KEEP |
| `_references_graph` (D8, `sqlite_master` recursive CTE) | G1 66 F + 123 E; shim-only failing tests 42 → 74 | **32 tests** | KEEP |
| numeric-affinity index dropper (D9, new) | migrate dies at `django_session.expire_date`, **0 of 831 run** | **all 831** | KEEP |
| `supports_foreign_keys = False` (D4b) | **bit-identical**: 392/42, 439/99, same skips; `backends` also bit-identical (5 F/19 E) | **nothing** | **DELETED** |

**D2's cost reversed from run 2's finding, and that is the contamination in one
number**: run 2 measured it as free precisely because migrate created nothing.
On a clean database, `CREATE TABLE … AUTOINCREMENT` is the first statement
Django issues and mpedb refuses it by name, so without the adaptation not one
label gets a database.

Adaptations after run 3: `data_types_suffix = {}` (D2), `_references_graph`
(D8), and the D9 index dropper. Every other one is gone.

### Deployment-blocking gaps: the new one

| # | Gap | Status |
|---|---|---|
| **D9** | **an index or a PRIMARY KEY on a NUMERIC-affinity (`any`) column is refused** | **OPEN, and it is the cost of closing D3.** Letting Django's own declared types through made `date`/`datetime`/`time`/`decimal`/`uuid`/`json` all `ColumnType::Any`, and `mpedb-types/src/schema.rs` refuses `any` as any kind of key: index keys are memcmp-ordered and `any` has no order across storage classes, so an `IndexRange` over one returns wrong rows — and DELETE/UPDATE through it deletes them. The refusal is right; the gap is that a NUMERIC-affinity column has no orderable representation to index. Two faces: the INDEX form (adapted around, costs all 831 without the adaptation) and the PRIMARY KEY form (no workaround — it is what blocks `queries`). |

Minimal repro:

```sql
CREATE TABLE s (id integer NOT NULL PRIMARY KEY, expire_date datetime NULL);
CREATE INDEX ix ON s (expire_date);
-- schema error: index column `s.expire_date` cannot be `any`: the index is
-- memcmp-ordered and `any` has no order across types
CREATE TABLE p (d datetime NOT NULL PRIMARY KEY);
-- schema error: primary key column `p.d` cannot be `any`: a key is memcmp-ordered …
```

Refused for `date`, `datetime`, `time`, `decimal(10,2)`, `uuid`, `json`;
accepted for `integer`, `bigint`, `smallint`, `integer unsigned`, `bool`,
`varchar(100)`, `text`, `real`, `double precision`, `BLOB`, `char(32)`,
`interval`.

### Re-ranked MPEdb-only gaps (group A: 132 tests, 141 outcomes)

Run-2 rank in brackets. The ranking inverted at the top: the affinity and
scalar-function gaps that led it are largely closed, and the planner's subquery
restrictions are now the single biggest item.

| Rank | Tests | Gap | Minimal repro | Where |
|---|---|---|---|---|
| 1 [3] | **53** | **Subquery / derived-table restrictions**: JOIN inside a derived table 14, unlifted `IN` subquery 9, correlated subquery outside `WHERE` in an aggregate query 8, aliased/renamed column 7, GROUP BY/HAVING body 5, correlated `IN` ("rewrite as EXISTS") 4, subquery in `HAVING` 2, unsupported position 2, compound (`UNION`) body 1, `ORDER BY` body 1 | `SELECT * FROM (SELECT a.x FROM a JOIN b ON b.id = a.b_id) s` | `mpedb-sql` planner |
| 2 [6] | **29** | **`LIKE … ESCAPE` (26) and `ORDER BY … NULLS FIRST/LAST` (3)** are not parsed | `SELECT 1 WHERE 'a%b' LIKE 'a\%b' ESCAPE '\'` | `mpedb-sql` parser |
| 3 [1] | **18** | **Comparison/arithmetic affinity — the half of sqlite affinity `any` still lacks.** Storage now converts (v7); comparing and computing does not: `arithmetic … got any` 10, `cannot compare int64 with text` 3, `coalesce` mixes 3, `floor() … got any` 1, `IN` list 1 | `SELECT price + 1 FROM t` where `price decimal(10,2)` | `mpedb-sql` |
| 4 [7] | **12** | **Rigid numeric parameter typing**: `$N is int64, requires float64` 8, the reverse 2, `cannot assign float64 to column of type int64` 2 | `SELECT * FROM t WHERE ratio = ?` bound with `1` | `mpedb-sql` |
| 5 [8] | **5** | **Bitwise operators absent** (`\|`, `&`, `<<`, `>>`, `^`) | `SELECT 5 \| 2` | `mpedb-sql` tokenizer |
| 6 [9] | **5** | **`REGEXP` requires a literal pattern**; Django always binds it | `SELECT 1 WHERE 'ab' REGEXP ?` | `mpedb-sql` |
| 7 [4] | **3** | **`strftime('…','now')`** — the only surviving piece of the scalar-function gap | `SELECT strftime('%Y','now')` | `mpedb-sql` builtins |
| 8 [13] | **2** | `INSERT values must be literals or parameters` | `INSERT INTO t (v) VALUES (1 + 1)` | `mpedb-sql` |
| 9 [12] | **2** | **2-argument `MAX(a,b)` / `MIN(a,b)`** (sqlite's scalar form) | `SELECT max(1, 2)` | `mpedb-sql` |
| 10 [14] | **1** | **PANIC in the binder**, surfaced as `internal error (panic) in engine` | `queries`-shaped reverse-relation transform (`test_filter_by_reverse_related_field_transform`) | `mpedb-sql` |
| 11 | **2** | One-offs: `unknown column` 1, `expected parameter number after $` (an identifier containing `$`) 1 | — | `mpedb-sql` |

Gone since run 2: host UDFs on the write path (47), the int↔bool bridge (39),
`quote()` (40 statements), the `json()`-shaped CHECK compile failure, and the
two `delete` FAILs run 2 attributed to signal-receiver contamination — they were
downstream of a gap that has since closed, exactly as predicted.

### Coverage

* 10 of Django's 219 labels now: the frozen 9 (831 tests) plus `backends`
  (324) — 1 155 tests measured, of which the shim passes **1 001**.
* `queries` (493) remains unmeasured, blocked by D9's PRIMARY KEY form.
* Still `--parallel=1`; no concurrency or multi-process behaviour measured.
---

## `typeof()` — the contract (2026-07-19)

`SELECT typeof(flag)` over a `bool` column answered **`'boolean'`** where stock
sqlite answers `'integer'`. No error, a different answer — the one failure mode
this shim does not allow. `timestamp` answered `'timestamp'`, and the param-only
`List` answered `'list'`.

**Decision: `typeof()` reports EXACTLY one of sqlite's five storage classes —
`null`/`integer`/`real`/`text`/`blob` — for every value, always. Natively and
through the shim.** mpedb's `Bool` and `Timestamp` report `'integer'`.

Why here and not in the shim, and why not behind the dialect flag:

1. **Range.** `typeof()` is a borrowed sqlite function and it borrows sqlite's
   contract: its documented range is those five strings, and every consumer
   switches on exactly those five. A sixth string is wrong against the only
   specification the function has. There is no PG reading to preserve either —
   PG spells it `pg_typeof()` and has no `typeof()` — so gating it on the
   sqlite-vs-PG flag would be a knob with one meaningful setting.
2. **Internal consistency** — checked, not assumed. `valconv::sqlite_type`
   already maps `Bool`/`Timestamp` onto `SQLITE_INTEGER`, and `as_i64`/`as_bytes`
   render them `1` / `"1"`. Through *every other* C-API accessor the value
   already IS an integer; `typeof` was the lone dissenter, so the shim disagreed
   with itself about the same value. (`bool_truthiness.rs` had already made this
   exact fold for VALUES — "a bool is sqlite's integer 0/1"; `typeof` was the
   leftover.)
3. **The shim cannot fix it.** `typeof()` is evaluated in the engine and reaches
   the C boundary as an ordinary `Value::Text`, indistinguishable from a text
   column whose content happens to be the word `boolean`. Remapping strings at
   the boundary would corrupt real data — a shim-only mapping is not merely
   inferior, it is unavailable.

`Value::List` is param-only and cannot reach an expression result; it maps to
`'null'`, matching `valconv::sqlite_type`'s defensive choice, which keeps
"`typeof` and `column_type` never disagree" **total** over all eight `Value`
variants rather than true only for the reachable ones.

`timestamp` is declarable through the shim but **no value of it is reachable**:
there is no bind path that produces a `Value::Timestamp`, and `DEFAULT
CURRENT_TIMESTAMP` is refused by name, so `INSERT INTO ts VALUES (1, 1720…)` is
a clean type-mismatch refusal. Its mapping is pinned where it IS reachable
(`mpedb-types` `expr::tests`).

Verified: `crates/mpedb/tests/typeof_storage_class.rs` (every value class, every
mpedb column type, 15 literal/expression forms — diffed against the `sqlite3`
3.45.1 binary) and `capi.rs::typeof_reports_only_sqlite_storage_classes_and_agrees_with_column_type`
(the same through FFI, asserting `typeof()` and `sqlite3_column_type()` agree on
every value). The curated `.slt` corpus needed no changes.

---

## Django `backends` — first triage (2026-07-19)

**stock 319/324 · shim 311/324 · 13 shim failures, of which 5 are shared with
stock (environmental / Django's own) and 8 are shim-only.** Both arms run the
same workbench backend, so the diff isolates mpedb.

**No wrong answers.** All 8 are refusals — mpedb declines a statement or
reports a setting it does not implement. Nothing answered differently.

### Blocker that had to be cleared first — gap D9

`CREATE INDEX … ON django_session (expire_date)` fails: `expire_date` is a
`DateTimeField`, `datetime` has sqlite NUMERIC affinity, NUMERIC maps to
`ColumnType::Any`, and `Schema::validate` refuses an `any` index column ("the
index is memcmp-ordered and `any` has no order across types"). This kills
`migrate` during test-DB setup, so **not one test runs**. New workbench
adaptation (both arms): the schema editor drops `CREATE INDEX` on NUMERIC-
affinity columns. An index is a pure performance feature — no answer depends on
one — so the only tests it can perturb fail in both arms and are excluded from
the shim-only diff by construction. **Engine gap, recorded not fixed**; it is the
subject of the in-flight "`any` columns indexable" work.

### The 8, classified

| # | Test(s) | Repro | Error | Class | Action |
|---|---|---|---|---|---|
| 1 | `test_get_primary_key_column`, `test_get_primary_key_column_pk_constraint` (2) | `BEGIN; SAVEPOINT s1; CREATE TABLE test_primary (id int PRIMARY KEY NOT NULL)` | `unsupported: DDL inside a SAVEPOINT is not supported by mpedb` | **engine** (`crates/mpedb` `apply_ddl`) — a `ROLLBACK TO` would have to revert the txn's captured schema bundle, which the savepoint snapshot does not restore; refused rather than risk a bundle/catalog desync | recorded |
| 2 | `test_parameter_escaping` (`EscapingChecks`, `EscapingChecksDebug`) | `SELECT strftime('%s', date('now'))` | `bind error: unknown function 'date()'` | **mpedb-sql** — `date()`/`time()`/`datetime()`/`julianday()` are not implemented (`strftime()` is) | recorded |
| 3 | `test_no_interpolation` | `SELECT strftime('%Y', 'now')` | `strftime(): unsupported time string "now"` | **mpedb-sql** — `'now'`, the Julian-day/unix-epoch numeric forms and the modifier language are refused by name rather than guessed | recorded |
| 4 | `test_regexp_function` | `SELECT ? REGEXP ?` with both bound | `bind error: REGEXP pattern must be a literal in Phase 1` | **mpedb-sql** — a REGEXP pattern must be a compile-time literal (the plan is content-hashed and the regex compiled once) | recorded |
| 5 | `test_init_command` | `PRAGMA synchronous = 3` then `PRAGMA synchronous` | `fetchone()` is `None` → `TypeError` | **shim**, deliberately NOT fixed — see below | recorded (gap D10) |
| 6 | `test_constraint_checks_disabled_atomic_allowed` | `PRAGMA foreign_keys = ON` then `PRAGMA foreign_keys` → `0` | `AssertionError: False is not true` | **shim**, deliberately NOT fixed — see below | recorded (gap D11) |
| 7 | `test_disable_constraint_checking_failure_disallowed` | same as 6 — Django's schema editor only raises when FK checks read as enabled | `NotSupportedError not raised` | same root cause as 6 | recorded (gap D11) |

### Why 5–7 are recorded rather than fixed

They are the only two of the eight that live inside `crates/mpedb-capi/`, and
both would be "fixed" by having the shim store a setting and echo it back:

* **D10 `synchronous`/`cache_size`.** Passing `test_init_command` requires
  answering `3` to a durability probe. mpedb's durability is config-authoritative
  and the shim does not honour the pragma, so the echo would be a claim about
  fsync behaviour that is not true. That is the `typeof()` mistake one level up:
  answering differently instead of erroring.
* **D11 `foreign_keys`.** Reporting `1` would tell a consumer its FK violations
  will be caught. mpedb parses `REFERENCES` and discards it; `0` is both the
  truth and sqlite's own default. The workbench already declares
  `supports_foreign_keys = False` for exactly this reason.

**Fixed instead: the honest member of the same family** — `PRAGMA busy_timeout`
/ `= N` now round-trips and IS the `sqlite3_busy_timeout()` knob the BUSY retry
loop reads (`capi.rs::pragma_busy_timeout_round_trips_and_is_the_c_api_knob`).
Before this, a consumer that set its lock timeout via the pragma rather than the
C function was silently left at 0. It flips none of the 8 — it closes a real
silent divergence the triage surfaced next to them.

**Net: 0 of the 8 were both small and clearly correct to fix inside
`mpedb-capi`.** Four are `mpedb-sql`/engine work owned elsewhere; the remaining
two would require the shim to promise behaviour mpedb does not have.

---

## Django's own test suite — run 4 (2026-07-19)

Run 4 scores everything that landed after run 3: subqueries in UPDATE/DELETE
WHERE + correlated `IN` + the correlated per-row aggregate positions, typeless
(`any`) PRIMARY KEY / index keys, the #74 tail batch (the lossless int↔float
parameter bridge, bitwise operators, bound REGEXP, scalar `max(a,b)`/
`min(a,b)`, the binder-panic fix), `typeof()`'s five-storage-class contract,
and the JSON function set flipping `supports_json_field` ON. Same Django
(`5.2.17.dev20260714173342`, commit `3e389b7`), same CPython 3.12.3, same
stock libsqlite3 3.45.1, `--parallel=1`; **both arms re-measured**, as every
run since run 1's fake baseline demands. Four separate measurements (A–D),
never one headline number.

### ⚠️ One WRONG ANSWER (W3 — **CLOSED 2026-07-20**, task #108) — REGEXP silently matches nothing outside mpedb's dialect

> **CLOSED in two parts.** Part 1 (`dbdb429`): a pattern outside the native
> dialect is a NAMED error, never a silent no-match. Part 2 (task #108): the
> honest fix below is BUILT — `x REGEXP y` dispatches to a registered host
> `regexp/2` as `regexp(y, x)` (sqlite's contract, argument order included);
> the native dialect only answers when NO host regexp exists, and is recorded
> in COMPAT.md as an extension (stock sqlite errors `no such function:
> regexp`). Host-REGEXP plans inherit the host-call containment (connection
> -local, never published). Measured: the two probe lines below answer
> `[(1,)]` in BOTH arms (the probe's pre/post diff moved exactly those two
> lines and nothing else); the lookup label's 4 regex tests
> (`test_regex`, `test_regex_backreferencing`, `test_regex_non_string`,
> `test_regex_null`) all flip to PASS under the shim — `test_regex_non_string`
> rode along because host dispatch does not pin the pattern's type (the UDF
> str()s the int, as stock+Python does), closing run-4 gap 6 entirely.
> CPython `test_sqlite3` per-test outcomes are unchanged (355/466 pass,
> identical not-pass set). The section below is the run-4 record of the open
> state.

Bound REGEXP (#74 item 3) turned run 3's clean refusal into a **silent wrong
answer** for every pattern outside mpedb's regexp dialect. Two Django tests
FAIL on it (`lookup.tests.LookupTests.test_regex`,
`test_regex_backreferencing`) — the only 2 FAILs among run 4's 1 648 measured
tests; every other failing outcome is an ERROR (refusal).

```python
con.create_function("regexp", 2, lambda p, s: bool(re.search(p, s)))  # as Django does
c.execute("CREATE TABLE a (id integer NOT NULL PRIMARY KEY, h text NOT NULL)")
c.execute("INSERT INTO a VALUES (1, 'hey-Foo'), (2, 'barfoobaz')")
c.execute("SELECT h FROM a WHERE h REGEXP ?", ("(?i)fo+",))   # stock [('hey-Foo',)]   mpedb []  WRONG
c.execute("SELECT h FROM a WHERE h REGEXP ?", (r"b(.).*b\1",)) # stock [('barfoobaz',)] mpedb []  WRONG
```

Two roots, both engine-side (`mpedb-types/src/expr/ops.rs::regexp_match`),
recorded not fixed:

1. **The intercept.** In real sqlite, `x REGEXP y` has NO built-in meaning —
   it is pure sugar for the consumer's registered `regexp(y, x)` UDF (Python's
   `re.search` under Django). mpedb's engine evaluates REGEXP with its own
   hand-rolled Thompson-NFA dialect and never calls the host UDF the consumer
   registered, so every semantic difference between the two is a silent
   divergence. Django's `__iregex` prepends `(?i)` to EVERY pattern, so the
   whole iregex lookup family answers `[]`.
2. **The malformed-pattern policy.** `regexp_match` documents "a pattern this
   engine cannot compile matches NOTHING, and mpedb never errors on a REGEXP
   pattern". For genuinely malformed patterns that is a defensible GLOB-like
   choice, but `(?i)…` and `\1` are VALID patterns in the consumer's regexp
   implementation — compile-failure-as-false converts an unsupported construct
   into a wrong answer instead of a refusal. Run 3 never saw it because the
   pattern had to be a literal and Django always binds; the moment binding
   worked, the tests reached the dialect edge.

The honest fix is dispatch: when the consumer has registered a `regexp` host
UDF, `REGEXP` must call it (that is sqlite's contract); mpedb's own dialect is
only defensible when no UDF exists — and even then, a pattern using a
construct the dialect knows it does not support should refuse by name, not
answer `[]`. Until then W3 also poisons the D8 ablation (below): Django's
`_references_graph` pattern starts with `(?i)`, so an ablated D8 would
"work" only by silently computing an empty recursive arm — the same answer the
adaptation returns, for the wrong reason.

**Nothing else diverges.** The differential probe
(`djsuite/probe_answers.py`, extended with every run-4 surface: typeless-key
ordering across storage classes under a real index, subquery lifts, bitwise,
scalar max/min, the parameter bridge, the JSON set, `typeof()`) answers
byte-identically to stock on every line where both arms answer; the remaining
diffs are refusals and their documented cascades.

### Workbench changes made before measuring

* **D9 DELETED** (the NUMERIC-affinity index dropper and its
  `WB_SOFT_CREATE_INDEX` lever): an `any` column may now be a PRIMARY KEY /
  index key. Verified directly through the shim before the runs: `CREATE INDEX
  ix ON s (expire_date)` and `CREATE TABLE p (d datetime NOT NULL PRIMARY
  KEY)` both succeed, and the full suite below ran with Django's own indexes
  actually created. Run 3's D9 gap is CLOSED, both faces.
* **D4b actually deleted**: run 3 measured `supports_foreign_keys = False` as
  bit-identical and recorded it deleted, but the `backends`-triage merge
  (`f7d65fe`) took the triage branch's `base.py` wholesale and resurrected it.
  Run 4's numbers are with Django's own feature flag (True). Consequence in
  `backends`: 4 FK-violation tests now SELF-skip under the shim ("This backend
  does not support integrity checks.") because mpedb parses and discards
  `REFERENCES` — Django's own graceful degradation reporting a real recorded
  gap, in the arm where it is true.
* Ablation switches restored for the two survivors: `WB_NO_D2`, `WB_NO_D8`.

`supports_json_field` needed no workbench change at all: Django probes with
`SELECT JSON('{"a": "b"}')` inside `transaction.atomic`, and the shim answers
it — the feature flipped ON by itself, in both arms.

### A — comparability: the SAME 9 labels, 831 tests

| | stock 3.45.1 | mpedb shim run 3 | mpedb shim run 4 |
|---|---|---|---|
| G1 `basic lookup transactions ordering update delete` | 392 ran, 0 failed | 392 ran, 42 failed | **392 ran, 29 failed** (2 F / 27 E) |
| G2 `aggregation annotations expressions` | 439 ran, 0 failed | 439 ran, 99 failed | **439 ran, 65 failed** (0 F / 65 E) |
| **total** | **831/831 (100 %)** | 690/831 (83.0 %) | **737/831 (88.7 %)** |
| delta vs run 3 | — | — | **+47** |

Skip parity is now exact (G1 17/17, G2 5/5, expected-failures 1/1), so no
adjustment applies — run 3's two flattering skips (the JSONField pair) now RUN
under the shim and are scored inside the failure counts. The 94 failing
outcomes cover 87 unique tests; 92 outcomes are ERRORs, 2 are the W3 FAILs.

**Which gaps closed, by test movement (run-3 rank → run-4 weight):**

| Run-3 gap (weight) | Run-4 weight | Verdict |
|---|---|---|
| 1. Subquery / derived-table restrictions (53) | **32** | PARTIALLY CLOSED (−21): UPDATE/DELETE WHERE subqueries, correlated `IN`, and correlated per-row aggregate positions all landed. What remains is the named derived-table blockers (aliased/renamed column + JOIN/GROUP BY/DISTINCT combinations, 27) and subquery-position refusals (HAVING 2, compound body 1, grouped-correlation 1, other 1). |
| 2. `LIKE … ESCAPE` not parsed (26) + `NULLS FIRST/LAST` (3) | **27 + 0** | ESCAPE now PARSES and `NULLS FIRST/LAST` is GONE (its 3 tests pass) — but the LIKE pattern must be a LITERAL, and Django always binds it, so the same tests now die one stage later: `LIKE pattern must be a literal in Phase 1`. The new rank 1. |
| 3. Comparison/arithmetic affinity (18) | **13** | Slightly closed; still the `any` comparison half: `cannot compare int64 with text` 6, `coalesce`/`CASE` mixes 4, `arithmetic … (binder should have coerced)` 1, `floor() got any` 1, `IN` list 1. |
| 4. Rigid numeric parameter typing (12) | **4** | LARGELY CLOSED by the lossless bridge (#74 item 1). Left: float64 param where int64 required (not exactly integral) 2, float→int column assignment 2. |
| 5. Bitwise operators (5) | **0** | CLOSED. |
| 6. REGEXP requires a literal (5) | **2 W3 FAILs + 2** | The literal restriction is gone — and its refusal became W3's wrong answer (above). The 2 remaining ERRORs are `REGEXP requires text, got int64` (Django's `test_regex_non_string` binds an int pattern; sqlite+Python str()s it). |
| 7. `strftime('now')` (3) | **3** | Unchanged, refused BY DESIGN. |
| 8. INSERT values must be literals (2) | **2** | Unchanged. |
| 9. Scalar `MAX(a,b)`/`MIN(a,b)` (2) | **0** | CLOSED. |
| 10. Binder PANIC (1) | **0** | CLOSED (`dcdd896`). |
| 11. One-offs (2) | **2** | Unchanged (`unknown column` 1, identifier containing `$` 1). |
| NEW: JSONField CASE mix | **1** | `test_update_jsonfield_case_when_key_is_null` now RUNS (was gated) and refuses: `cannot mix CASE result types: text and any`. |
| NEW: affinity population growth | +1 | `DecimalFieldLookupTests` reach further than run 3, growing bucket 3. |

### B — `queries` (493): FIRST EVER RUN

| | stock 3.45.1 | mpedb shim |
|---|---|---|
| `queries` | 493 ran, 0 failed (15 skips, 2 xfail) | **493 ran, 34 failed (0 F / 34 E)** — 459/493 (93.1 %) |

**The D9 PRIMARY-KEY wall is gone**: `migrate` creates all 95 models including
`queries_datetimepk` (`datetime NOT NULL PRIMARY KEY`), with real indexes, and
93 % of the label passes on first contact. All 34 outcomes (29 unique tests)
are refusals; zero FAILs. The buckets, all `mpedb-sql`:

| Tests | Gap | Minimal repro |
|---|---|---|
| 13 | **Compound (UNION/EXCEPT/INTERSECT) placement**: compound body in a derived table 6, correlated reference from inside a compound subquery arm (`no table named V0 in this statement`) 3, compound arms typed `any` vs `text` 2, subquery inside a compound SELECT 2 | `SELECT COUNT(*) FROM (SELECT id FROM a UNION SELECT id FROM b) u` |
| 6 (1 test) | **`EXPLAIN QUERY PLAN` is not a statement** | `EXPLAIN QUERY PLAN SELECT 1` → `expected a statement` |
| 5 | Derived-table aliased/renamed-column blockers (same family as A rank 2) | — |
| 4 | **`LIMIT -1`** (sqlite's "no limit" idiom; Django emits it for `qs[5:]`) | `SELECT * FROM t LIMIT -1 OFFSET 5` → `LIMIT requires a non-negative integer literal` |
| 3 | INSERT VALUES with an expression (`STRFTIME(…, 'NOW')` default) | `INSERT INTO t (created) VALUES (STRFTIME('%Y-%m-%d %H:%M:%f', 'NOW'))` |
| 1 | CASE result into a bool column: `column X is bool, value is int64` | `UPDATE i SET alive = CASE WHEN id = ? THEN ? … ELSE alive END` binding True |
| 1 | LIKE bound pattern (rank-1 family) | — |
| 1 | `arithmetic on int64 and float64 (binder should have coerced)` | `…WHERE ptr_id IN (SELECT … WHERE F(x) % 1000000 …)` (`test_ticket_23605`) |

### C — `backends` (324)

| | stock 3.45.1 | mpedb shim run 3 | mpedb shim run 4 |
|---|---|---|---|
| `backends` | 324 ran, 5 failed → **319/324** (unchanged) | 13 failed → 311/324 | **12 failed → 312/324** (5 shared + **7 shim-only**) |

`test_regexp_function` closed (bound REGEXP — its pattern is inside mpedb's
dialect, so it answers correctly). The 7 remaining shim-only failures are
run 3's list unchanged: DDL inside a SAVEPOINT 2, `date()` unknown 1,
`strftime('now')` 1, D10 `PRAGMA synchronous` 1, D11 `PRAGMA foreign_keys` 2.
Skips: 128 vs stock's 124 — the 4 extra are the FK self-skips described above,
Django reporting mpedb's real no-FK-enforcement gap in the honest direction.

### D — JSONField: `supports_json_field` is ON

Django's own probe (`SELECT JSON('{"a": "b"}')` in a transaction) succeeds
through the shim, so the feature is True in BOTH arms with no workbench
involvement. What that changed, measured:

* Both gated models (`annotations.JsonModel`, `expressions.JSONFieldModel`)
  are now CREATED during migrate, with Django's own
  `CHECK ((JSON_VALID("data") OR "data" IS NULL))` compiled and enforced —
  the G2 label group ran all 439 tests with them present, and G2's failure
  count still FELL 99 → 65. The run-2 prediction ("a trap worth 439 tests")
  stayed defused at run 4 scale.
* The two tests run 3 could only SKIP now run:
  `test_values_expression_alias_sql_injection_json_field` **passes**;
  `test_update_jsonfield_case_when_key_is_null` errors on a non-JSON gap
  (`cannot mix CASE result types: text and any` — A's bucket 3).
* Skip parity with stock is exact (5/5), so nothing is flattered.

### Workbench adaptations — re-measured, not assumed

| Adaptation | Ablated result (shim) | Cost | Kept? |
|---|---|---|---|
| `data_types_suffix = {}` (D2, AUTOINCREMENT) | `WB_NO_D2`: migrate dies at the FIRST `CREATE TABLE`, 0 tests run | **all 831** | KEEP |
| `_references_graph` (D8, `sqlite_master` recursive CTE) | `WB_NO_D8`: G1 68 F + 108 E (vs 29 outcomes), G2 unchanged; stock arm still 831/831. Driving error: `this sqlite_master query form is not supported by the mpedb C-API shim` ×80 — the shim's `sqlite_master` mini-evaluator refuses Django's exact CTE shape, and every TransactionTestCase teardown then cascades | **~147 outcomes in G1** | KEEP — and it was DOUBLY pinned: even if the mini-evaluator learned the shape, W3 would silently empty the `(?i)…` REGEXP recursive arm and return the seed table alone. W3 is now CLOSED (#108), so the second pin is gone — the mini-evaluator's CTE-shape refusal is the sole remaining blocker; re-ablate when that closes. |
| D9 index dropper + `WB_SOFT_CREATE_INDEX` | removed for the whole run; migrate succeeds with real indexes everywhere (A, B, C) | **0** | **DELETED** |
| `supports_foreign_keys = False` (D4b) | removed for the whole run; only visible change is 4 honest self-skips in `backends` | **0** | **DELETED** (again, this time in the file) |

### Re-ranked MPEdb-only gaps (A+B: 116 tests, 128 outcomes)

| Rank | Tests | Gap | Minimal repro | Where |
|---|---|---|---|---|
| 0 | 2 | **W3 — REGEXP wrong answer** — **CLOSED 2026-07-20** (#108: named error `dbdb429`, then host `regexp/2` dispatch; see the W3 section note) | `h REGEXP ?` bound `(?i)fo+` → `[]` (now: stock's rows) | `mpedb-types` `expr/ops.rs` + host-UDF dispatch |
| 1 | 28 | **LIKE pattern must be a literal** (ESCAPE now parses; Django binds every pattern) | `name LIKE ? ESCAPE '\'` bound `('A\_b',)` | `mpedb-sql` (same lift bound REGEXP got) |
| 2 | 32 | **Derived-table / subquery-position restrictions** (each blocker named by `check_simple`) | `SELECT s.x FROM (SELECT a.x AS x FROM a JOIN b ON b.id = a.b_id GROUP BY a.x) s` | `mpedb-sql` planner |
| 3 | 13 | **Compound (UNION/INTERSECT/EXCEPT) placement**: in a derived table, in a correlated subquery (scoping: `no table named V0`), arm-type `any`/`text` unification, subquery inside a compound | `SELECT COUNT(*) FROM (SELECT id FROM a UNION SELECT id FROM b) u` | `mpedb-sql` |
| 4 | 14 | **Affinity comparison/arithmetic half** (incl. the `coalesce`/`CASE` mixes and both `binder should have coerced` internal-inconsistency errors) | `SELECT price + 1 FROM t` (`price decimal(10,2)`) | `mpedb-sql` |
| 5 | 6 (1 test) | **`EXPLAIN QUERY PLAN`** not a statement | `EXPLAIN QUERY PLAN SELECT 1` | `mpedb-sql` parser |
| 6 | 5 | **INSERT VALUES with expressions** | `INSERT INTO t (v) VALUES (STRFTIME('%Y', '2020-01-01'))` | `mpedb-sql` |
| 7 | 4 | **`LIMIT -1`** (sqlite's no-limit idiom) | `SELECT * FROM t LIMIT -1 OFFSET 5` | `mpedb-sql` parser |
| 8 | 4 | **Residual param strictness**: non-integral float64 → int64 refusals | `WHERE int_col > ?` bound `1.5` | `mpedb-sql` |
| 9 | 4 | `strftime('now')` 3 (BY DESIGN) + `date()` family (`backends`) | `SELECT date('now')` | `mpedb-sql` builtins |
| 10 | 3 | `REGEXP requires text, got int64` 2 (**CLOSED with W3**: host dispatch does not pin the pattern; the UDF str()s it) + CASE-into-bool-column 1 | `h REGEXP ?` bound `123` | `mpedb-sql` |
| 11 | 4 | One-offs: `unknown column` 1, `$` in identifier 1, JSONField CASE mix 1, DDL-in-SAVEPOINT (`backends`) 2 | — | engine / `mpedb-sql` |

### Coverage

* 11 of Django's 219 labels: the frozen 9 (831) + `backends` (324) +
  `queries` (493) = **1 648 tests measured, shim passes 1 508**
  (A 737 + B 459 + C 312, the same per-section arithmetic run 3 used) —
  91.5 %, vs run 3's 1 001/1 155 (86.7 %) over 493 fewer tests.
* Zero wrong answers is VIOLATED for the first time since W1/W2: W3's two
  FAILs, introduced by #74 item 3. Everything else refuses.
* Still `--parallel=1`; no concurrency or multi-process behaviour measured.

## CPython's own `test_sqlite3` — FIRST EVER RUN (2026-07-19)

The stdlib's own suite — the authoritative consumer test of the `sqlite3`
module, exercising the exact C-API surface this shim implements — had never
been pointed at mpedb. Route: the distro strips the `test` package, so
`Lib/test/` was fetched from CPython's GitHub at the EXACT tag of the local
interpreter (v3.12.3) into `/mnt/xfs/cpython-tests/lib` and run with the
system python: `PYTHONPATH=… python3 -m test test_sqlite3`, per-test results
diffed via `--junit-xml`. Engine = `main` @ `acdf180` + the three capi commits
below.

### #112 wave 2 (2026-07-20): decltype verbatim, collations, N-ary aggregates

Three buckets, measured on the same route at `main` @ `976a658`:

| arm | run | pass | fail/err | skip |
|---|---|---|---|---|
| baseline at this head | 466 | **350** | 111 | 5 |
| + bucket A (decltype verbatim) | 466 | **357** | 104 | 5 |
| + buckets B (collations) + C (N-ary aggregates) | 466 | **369** | 92 | 5 |

* **A — declared-type verbatim, +7.** The one SILENT divergence on the list is
  closed: see E3(a) below. All 6 `DeclTypesTests` plus `DateTimeTests.
  test_sqlite_date` (a `d date` column is `Any`, so its decltype was NULL and
  the DATE converter never ran). Schema canonical bytes v7 → v8; the whole
  workspace suite passes unchanged, and the only in-tree edits the bump forced
  were three version-byte assertions.
* **B — custom collations, +6.** See E6. The scope is deliberately partial and
  the rest refuses by name.
* **C — N-ary host aggregates, +6.** See E7.

The ⚠️ divergence below is the one A closed; it is kept for the record.

### ⚠️ Silent divergences (no error, different behavior) — none in SQL answers, one in metadata

No shim-only failure was a wrong SQL answer: every diverging test either
raises an error, or asserts on error/trace/metadata behavior. ONE family is a
silent behavioral divergence a consumer can feel without an error:

**`column_decltype` is canonicalized, so `PARSE_DECLTYPES` converters
silently do not fire for non-canonical declared types.** mpedb stores the
canonical type (`REAL`, `BOOLEAN`), not the verbatim declared text, so a
column declared `f float` reports decltype `REAL` — CPython looks up the
converter under `FLOAT`, finds none, and hands back the RAW value with no
error. 6 tests (`DeclTypesTests`: bool/float/foo/number1/number2/cblob):

```python
sqlite3.register_converter("FLOAT", lambda x: 47.2)
con = sqlite3.connect(":memory:", detect_types=sqlite3.PARSE_DECLTYPES)
con.execute("create table t(f float)"); con.execute("insert into t values (3.14)")
con.execute("select f from t").fetchone()   # stock: (47.2,)   mpedb: (3.14,)  — silently
```

Closing it needs the schema to carry the verbatim declared-type text
(engine-side; the shim's decltype is derived from `ColumnType`). Canonical
types (`INTEGER`/`TEXT`/`REAL`/`BLOB`/`TIMESTAMP`/`BOOLEAN` spelled that way)
convert identically to stock — Django's `PARSE_DECLTYPES` use is unaffected
(measured byte-identical in the Django runs above).

### The crash that gated the whole suite (FIXED)

The first run **segfaulted CPython** at `test_hooks.CollationTests`: the
`sqlite3_create_collation_v2` refusal stub invoked the caller's
`xDestroy(pApp)` — but sqlite's documented contract (unlike
`create_function_v2`!) is that the destructor is NOT called when
`create_collation_v2` fails, and CPython therefore frees the context itself
on a non-OK return. The stub's call made that a double-free; the corrupted
heap took down the interpreter ~200 tests later. The window-function stub's
destructor call is CORRECT (sqlite does invoke it on failure; CPython relies
on that by not freeing). Fixed + FFI-pinned
(`collation_refusal_leaves_destructor_alone_window_refusal_runs_it`).

### The two arms

| | run | pass | fail/error | skip |
|---|---|---|---|---|
| stock libsqlite3 3.45.1 (baseline) | 466 | **461** | 0 | 5 |
| shim, first non-crashing run | 466 | 283 | 178 | 5 |
| **shim, after the 3 commits** | 466 | **344** | **117** | 5 |
| **shim, after #109 (busy timeout end-to-end)** | 466 | **350** | **111** | 5 |

The 117 counts one test the run must EXCLUDE because it deadlocks (worse
than failing — see engine gap E1). The 5 baseline skips are sqlite-version
gates, identical in both arms. 344/461 = 74.6 % of the baseline-passing
suite; the DB-API battery (23/23) and Django numbers elsewhere in this file
measure breadth this suite does not, and vice versa — this suite is the one
that hammers the API's CONTRACTS (destructor rules, trace, limits, error
codes/messages, blob/backup surfaces).

### What the shim fixed (the +61 after the crash fix, grouped)

* **`SQLITE_TRACE_STMT` is REAL** (~30 tests): `sqlite3_trace_v2` dispatches
  as a statement begins running, on the step path AND the exec path
  (CPython's legacy-autocommit COMMIT goes through exec); the callback's P
  argument is the statement handle (CPython re-enters `expanded_sql` /
  `db_handle` on it — fired with no Rust borrows live). CPython's whole
  isolation-level/autocommit family verifies THROUGH the trace, so a no-op
  trace failed all of it.
* **`sqlite3_progress_handler` fires** (4): once per statement execution
  (mpedb has no VM opcode stream to count N against); non-zero return —
  including CPython's -1 on a raising handler — interrupts.
* **Stub honesty — refusals must live ON the handle** (converted 46 bare
  `SystemError`s into proper exceptions): `blob_open`/`backup_init` (on the
  DESTINATION, sqlite's contract)/`deserialize` now set the error state the
  consumer reads. Same class: `db_config`'s toggle ops write 0 (the literal
  truth) to the out-pointer CPython was reading uninitialized.
* **`sqlite3_limit` is real storage** (~6): sqlite's defaults, prior-value
  return, -1 on a bad category; `VARIABLE_NUMBER` enforced at prepare
  ("too many SQL variables"), `LENGTH` in `expanded_sql` (NULL past it);
  `SQL_LENGTH` is enforced by CPython itself reading the stored value.
* **UDF error code/text passthrough** (~7): `sqlite3_result_error_nomem/
  _toobig/_error` now surface their CODE (→ `MemoryError`/`DataError`) and
  exact TEXT instead of the engine's opaque `unsupported: user function
  raised: …` wrapper. Plus: NaN in `bind_double`/`result_double` stores NULL
  (sqlite has no NaN); `create_function` refuses nArg outside -1..=127.
* **Open-path correctness** (~5): a non-UTF-8 filename is an OS byte path
  (it was silently opening an EPHEMERAL database instead!); `file:` URI
  paths percent-decode byte-wise; `mode=ro` never creates and refuses writes
  with `SQLITE_READONLY`; CANTOPEN messages lead with sqlite's canonical
  "unable to open database file".
* **Leading comments + maintenance statements** (4): mpedb's parser does not
  skip LEADING comments — the shim strips them (classification and the text
  the engine sees), so `-- comment\nINSERT …` (CPython suite, iterdump
  scripts) runs; `VACUUM`/`ANALYZE` are accepted as no-ops (freelist page
  reuse, no planner statistics — genuinely nothing to do).
* **Message shapes consumers grep** (2): constraint errors lead with
  sqlite's "… constraint failed:"; the sibling-lock error (below) reads
  "database is locked".

### Remaining 117, grouped by root cause, ranked by tests blocked

**Engine-side, recorded (the shim cannot fix these):**

| # | Tests | Gap | Repro / note |
|---|---|---|---|
| E1 | **FIXED (#109, 2026-07-19)** | ~~Cross-process writer contention BLOCKS forever~~ → `Database::set_busy_timeout` + `Engine::begin_write_deadline` bound every facade writer-lock wait; the shim wires the knob at open (0), `sqlite3_busy_timeout`, and `PRAGMA busy_timeout`. `MultiprocessTests.test_ctx_mgr_rollback_if_commit_failed` passes UN-excluded (0.097 s); the suite no longer hangs. Two-process elapsed evidence in `crates/mpedb/tests/busy_timeout.rs` (timeout 300 ms → Busy at 300.1 ms; timeout 0 → 4.7 µs; SIGKILLed holder → waiter ACQUIRES via EOWNERDEAD adoption in 22.7 µs, never hangs). | |
| E2 | 38 | **Incremental blob I/O** (`sqlite3_blob_*` = mpedb #43). All of `BlobTests`; refusal is honest OperationalError now. | `con.blobopen("t", "b", 1)` |
| E3 | ~~14~~ **7** | **Declared-type family.** (a) ~~6 × decltype-canonicalization~~ **CLOSED (#112 wave 2)**: `ColumnDef` carries `decl: Option<String>` — the declared text VERBATIM, sliced out of the CREATE TABLE source (canonical bytes v7→v8). `sqlite3_column_decltype` returns what the statement said, so `PARSE_DECLTYPES` converters fire. All 6 `DeclTypesTests` + `DateTimeTests.test_sqlite_date` flip. (b) STILL OPEN — 7 × bind rigidity where sqlite coerces: text→`timestamp` (CPython's own adapters bind datetime as an ISO STRING — `insert into t(x) values (?)` with `"2026-07-19 10:00:00"` into `x timestamp` is IntegrityError; stock stores it), int→`text`, blob→`text`. | `crates/…/scratchpad` repros in section text |
| E4 | 9 | **Window functions** (`sqlite3_create_window_function`) — DESIGN-UDF stage 3b; clean refusal. | |
| E5 | 8 | **Authorizer** (`sqlite3_set_authorizer` accepted, never invoked — would need compile-time callbacks). | |
| E6 | ~~6~~ **0** | **Custom collations — CLOSED (#112 wave 2), with a NAMED boundary.** `sqlite3_create_collation[_v2]` registers a real comparator; `ORDER BY <expr> COLLATE <name>` sorts through it (plain table, compound, derived table, grouped, join — all differentially matched against stock). A host collation is a COMPARATOR, so it stops exactly where a KEY ENCODING starts: a column declared `COLLATE <host>`, a `GROUP BY`/`DISTINCT` fold, and a comparison's `COLLATE` all REFUSE with sqlite's own "no such collation sequence: <name>" rather than being answered under BINARY — an index built bytewise cannot answer a host-collated probe, and answering it anyway is the wrong-answer-with-no-error this refuses to be. Enforced structurally: those paths take a built-in `Collation`, which no registration can produce. Plans naming one are connection-local (the host-UDF no-publish rule). | `con.create_collation("x", f)` |
| E7 | ~~6~~ **0** | **Host AGGREGATES are N-ary — CLOSED (#112 wave 2).** `AggCall` grows `extra_args` (PLAN_FORMAT 51) and the AST's `Expr::Agg` a trailing argument list; the parser takes the whole list for a HOST name only (every built-in still falls through to the `min(a,b)`/`max(a,b)` scalar rule, then to the one-argument error), the arity gate matches the CALL's count against the registration (exact or variadic `-1`), and the executor evaluates every argument over the same base row and hands `xStep` `[arg] ‖ extra_args`. `count(*)`'s row-shape stays exclusive to `count`. All 6 `AggregateTests.test_aggr_check_param*` flip. | `create_aggregate("f", 2, C)` then `select f(a, b) …` |
| E8 | **FIXED (#109, 2026-07-19)** | ~~First-compile of a NEW statement text needs the writer lock~~ → under a busy policy, plan-registry publication is OPPORTUNISTIC (one immediate-deadline attempt; a held lock — including a same-thread sibling's — skips the insert and keeps the plan in the local cache, exactly like the host-UDF plan path). All 5 `TransactionTests.*_starts_transaction` tests pass; readers proceed under a writer. | |
| E9 | 4 | **`ON CONFLICT` clause family**: `INSERT OR ROLLBACK` refused — the parser's own error message LISTS ROLLBACK as expected (`OR IGNORE` works), and the table-constraint form `unique(x) on conflict rollback` doesn't parse. | `INSERT OR ROLLBACK INTO t …` → "expected IGNORE, REPLACE, ABORT, FAIL, or ROLLBACK after OR" (sic) |
| E10 | 5 | **iterdump surface**: 2 × AUTOINCREMENT (deliberate refusal), 1 × fts4 (deliberate — fts5 only), 1 × sqlite_master query form, 1 × un-root-caused "one statement at a time" in `test_table_dump`. | |
| E11 | 3 | **`zeroblob()`** absent. | `select zeroblob(100)` |
| E12 | 1+1+1+1 | **Parser one-offs**: `==` (sqlite's alias for `=`); unquoted identifier bytes ≥ 0x80 (`select 1 as \xff`; the QUOTED form works); bare `current_timestamp` keyword; partial index `CREATE INDEX … WHERE`. | `select 1 where 1 == 1` → "expected an expression" |
| E13 | 1 | **Unquoted table names are case-SENSITIVE** (`CREATE TABLE t` then `INSERT INTO T` fails; sqlite matches case-insensitively). One test here, but any consumer can trip it. | |
| E14 | 1 | FROM-less SELECT can't resolve its own output alias in WHERE (`select 1 as a where a=?`). | |

**Deliberate refusals / documented divergences (win by refusing loudly):**
backup (7 — `mpedb mirror` is the answer), serialize/deserialize (2),
`db_config` toggles not stored-and-echoed (1, the D10/D11 stance),
`SQLITE_LIMIT_FUNCTION_ARG` not enforced (1 — the shim can't count a
function call's args without parsing).

### Verification added

9 new FFI tests in `tests/capi.rs` (now 40): the collation/window destructor
contract, trace on step+exec (expanded), limits round-trip + enforcement,
leading comments + VACUUM/ANALYZE, NaN→NULL, `mode=ro`, percent-decoded URI
paths, sibling-connection BUSY + recovery after COMMIT, refusal-stub handle
errors.

### #109 (2026-07-19 late): `busy_timeout` honored end-to-end — the hang is gone

E1 and E8 closed engine-side (`Database::set_busy_timeout` /
`Engine::begin_write_deadline`; opportunistic plan publication under a busy
policy — see the annotated rows above). Both arms re-run **UN-excluded**
(`/mnt/xfs/cpython-tests/{baseline2,shim5}.{xml,log}`):

| | run | pass | fail/error | skip |
|---|---|---|---|---|
| stock libsqlite3 (re-run, un-excluded) | 466 | **461** | 0 | 5 |
| **shim, after #109** | 466 | **350** | **111** | 5 |

The suite terminates in 6.4 s with no exclusion —
`MultiprocessTests.test_ctx_mgr_rollback_if_commit_failed` (the former
deadlock) passes in 0.097 s. +6 vs run 3 (E1's 1 + E8's 5); the failing set
is a strict subset of run 3's — zero regressions. 350/461 = 75.9 % of the
baseline-passing suite. Verification: 2 new FFI tests in `tests/capi.rs`
(now 42) — cross-thread holder with elapsed-time bounds (timeout 200 ms →
BUSY ≥ 200 ms; timeout 0 → immediate; a 5 s timeout ACQUIRES when the holder
commits mid-wait) and same-thread-sibling immediate BUSY — plus the
two-process + SIGKILL-holder suite in `crates/mpedb/tests/busy_timeout.rs`.
