# mpedb — a drop-in `sqlite3` replacement

A multi-process embedded database engine with PostgreSQL-grade concurrency
(MVCC snapshots, lock-free readers) behind the `sqlite3` API you already use.
Swap one import and existing code runs unchanged:

```python
import mpedb as db          # was: import sqlite3 as db

conn = db.connect("app.db")
conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
conn.execute("INSERT INTO users (id, name) VALUES (?, ?)", (1, "Ada"))
conn.commit()
print(conn.execute("SELECT name FROM users").fetchall())
```

```sh
pip install mpedb
```

CPython **3.12+** (`abi3`: one wheel per platform covers every 3.12+ version).
Wheels for Linux x86-64 and macOS arm64, published automatically when the full
engine test suite is green on CI.

## The path decides the engine

| path | what happens |
|---|---|
| `*.db` | The file is read as a **real sqlite database**. Writes land in an mpedb delta next to it, and `commit()` checkpoints them back into the `.db` — sqlite tools and mpedb see one store, kept in sync. |
| `*.mpedb` | Native mpedb: multi-process shared-memory engine; attaching processes may be SIGKILLed at any instant without corrupting the file. |
| `:memory:` | Native mpedb, process-private memory. |
| `*.toml` | An explicit mpedb config file (declared schema, durability modes, sizing). |

```python
import mpedb.sqlite3 as db   # the default routing, under its explicit name
import mpedb.mpedb  as db    # force the NATIVE engine for any path
```

## Why

- **Multi-process for real.** sqlite's operational model (no server, attach by
  path) with lock-free MVCC readers: readers never block the writer, the
  writer never blocks readers, and a crash mid-write never corrupts the file.
- **Crash-safe on every platform this ships a wheel for**, and by the same
  standard on each: Linux (x86-64, aarch64, armv7l), macOS/Apple Silicon and
  Windows x86-64 all run **all six multi-process crash harnesses** in CI —
  `crash`, `stress`, `powerloss`, `collide`, `queue-collide`, `mirror-collide`
  — with processes SIGKILLed mid-write and the file verified afterwards.
  Windows is not a port that merely compiles: shared `CreateFileMapping`
  views, a `LockFileEx` writer lock with owner-death release, `GetProcessTimes`
  reader identity, `FlushViewOfFile` + `FlushFileBuffers` durability. The first
  thing those harnesses found on Windows was a corruption bug that turned out
  to be ours on *every* platform.
- **Compiled plans.** SQL compiles once to a content-hashed plan shared across
  processes; repeated parameterised statements execute with zero parsing.
- **Keep your `.db` files.** The sqlite-backed mode means adopting mpedb does
  not mean leaving sqlite — your existing tools keep reading the same file.
- **Never a wrong answer.** mpedb's SQL is a differentially tested subset of
  sqlite's: a statement is either answered exactly as sqlite answers it, or
  refused loudly with `ProgrammingError` — never silently misinterpreted.

## Honest status (0.1)

The DB-API core is implemented: `connect`, `Connection`
(`execute`/`commit`/`rollback`/`close`, context manager), `Cursor`
(`execute`/`executemany`/`fetchone`/`fetchmany`/`fetchall`/`description`/
`rowcount`/iteration), `?` parameters, the PEP 249 exception hierarchy, and
live DDL (`CREATE TABLE` / `DROP TABLE` / `ALTER TABLE`).

Not yet implemented (the 0.2 roadmap, mapped against CPython's own
`test_sqlite3` suite): `Row`/`row_factory`, `executescript`, `lastrowid`,
adapters/converters (`detect_types`), `create_function`, `blobopen`,
`isolation_level`/`autocommit` control. On the native engine, reads on a
connection do not yet see its own uncommitted writes (they do on the `.db`
overlay backend). Details and progress:
[github.com/punnerud/mpedb](https://github.com/punnerud/mpedb).

---

# Reference

## Advanced API (beyond sqlite3)

The native module also exposes mpedb's own machinery — content-hashed prepared
plans, explicit write sessions, `EXPLAIN`, streaming blob inserts:

```python
import mpedb
db = mpedb.Database("app.toml")   # open/create from a TOML config file
```

| Call | Returns | Notes |
|---|---|---|
| `mpedb.Database(config_path)` | `Database` | Opens/creates the database described by the TOML config. Thread-safe; share one handle. |
| `db.prepare(sql)` | `str` (64-hex plan hash) | Compiles once, publishes to the shared plan registry: any attached process can execute it by hash. |
| `db.execute(hash, params=None)` | SELECT → `list[tuple]`; DML → `int` (affected) | Hot path — no SQL parsing. `params` is a list/tuple. |
| `db.query(sql, params=None)` | as `execute`; `EXPLAIN …` → `str` | One-shot prepare + execute. Use `$1…$n` parameters, never interpolate values into the SQL text (each distinct text becomes a registry plan). |
| `db.query_full(sql, params=None)` | `(columns: list[str], rows: list[tuple])` | For callers who need output column names. Raises `ProgrammingError` for non-SELECT. |
| `db.explain(sql)` | `str` | Plan rendering; nothing is executed (prepends `EXPLAIN` if absent). |
| `db.tables()` | `list[str]` | Table names from the schema. |
| `db.verify()` | `None` | Page-accounting verification; raises on integrity failure. Takes the writer lock briefly. |
| `db.begin()` | `Transaction` | Interactive write transaction; holds the single writer lock until commit/rollback. |
| `tx.execute(hash, params=None)` / `tx.query(sql, params=None)` | as above | Run inside the transaction; SELECTs see the session's own uncommitted writes. `tx.query` plans are cached process-locally, never published. |
| `tx.insert_file(table, values, stream_col, path)` | `None` | INSERT one row, streaming column `stream_col` from the file at `path` a page at a time (never resident — files larger than RAM are fine). `values` is the full row; `values[stream_col]` is a placeholder (`b""`). Path-based on purpose: the engine pulls pages with the writer lock held, so there is no Python `read()`-callback variant. The streamed column must be the table's **last** varlen column; tables with a secondary UNIQUE index are refused. |
| `tx.commit()` / `tx.rollback()` | `None` | Explicit finish. A dropped/GC'd transaction rolls back. |
| `with db.begin() as tx:` | | Commits on clean exit, rolls back if an exception propagates (never suppresses it). |

Free-threading friendly by design: no module-level mutable state, and the GIL
is released around every engine call, so point reads from multiple Python
threads run truly in parallel.

## Value mapping (both directions)

| Python | mpedb column type | Notes |
|---|---|---|
| `None` | NULL | |
| `bool` | `bool` | checked before `int` (Python bool subclasses int) |
| `int` | `int64` | out of range → `OverflowError` |
| `float` | `float64` | |
| `str` | `text` | |
| `bytes` / `bytearray` | `blob` | always returned as `bytes` |
| `datetime.datetime` | `timestamp` | stored as microseconds since epoch, UTC. Aware datetimes are converted to UTC; naive ones are treated as UTC. **Returned** as an aware UTC `datetime`. A plain `int` is also accepted for timestamp parameters and taken as raw microseconds. |

## Exceptions

```
mpedb.Error (Exception)
├── mpedb.IntegrityError     primary-key / UNIQUE / NOT NULL / CHECK violations
├── mpedb.ProgrammingError   parse, bind, type mismatch, wrong param count,
│                            unknown/invalidated plan, unsupported statement
└── mpedb.OperationalError   I/O, corruption, DbFull, ReadersFull, evicted
                             snapshot, config/schema mismatch, poisoned write
                             session, engine internals
```

The sqlite3 aliases (`DatabaseError`, `InterfaceError`, `DataError`,
`InternalError`, `NotSupportedError`, `Warning`) exist and alias the closest
parent, so `except sqlite3.DatabaseError` keeps catching. Messages carry the
engine's `Display` text. Binding-level misuse (bad params container,
non-convertible value) raises the ordinary `TypeError`/`OverflowError`.

## Locking rules (inherited from the Rust facade)

- **`db.prepare(...)`, `db.verify()`, `db.query(...)`, `db.query_full(...)` and
  a second `db.begin()` are REFUSED while a `Transaction` from the same handle
  is open on the same thread** (#161) — a `ProgrammingError` that names the
  method and what to do instead. They may need the single writer lock the
  transaction already holds, and that is a hang, not an error: no traceback, no
  hint which call was the mistake. Prepare the statements you need *before*
  `db.begin()`; inside the transaction, `tx.query`/`tx.execute` are always safe.

  It refuses **unconditionally**, not only when the plan is uncached, and that
  is deliberate: a guard that allows the call when the plan happens to be in
  the registry reproduces the original bug's worst property — it works in
  testing and hangs in production. The first thing this caught was a call in
  mpedb's own test suite, in a test whose comment already said the prepare
  belonged outside the block.

  Another THREAD calling these is ordinary contention and waits its turn; only
  the thread holding the lock is refused.
- **Sessions poison on partially-applied statements.** Statements are not
  internally atomic: if e.g. a multi-row UPDATE fails on its third row, the
  first two are already modified and the session becomes *poisoned* — every
  further `tx.execute`/`tx.query` and `tx.commit()` raises
  `mpedb.OperationalError` ("… poisoned …") and `commit` rolls back instead of
  persisting the torn statement. Only `tx.rollback()` (or leaving the `with`
  block via the exception) is valid. A statement that fails *before* any side
  effect (single-row constraint violation, type error) does **not** poison the
  session.
- One writer at a time, process-wide and machine-wide: `db.begin()` blocks on
  (or errors for re-entry into) the single writer lock. Readers never block.
- **A `Transaction` is refused from any thread but the one that created it**
  (#161). The writer lock is a mutex with thread affinity — releasing it from
  another thread is undefined behaviour in POSIX, not a rule this API invented.
  `__exit__` is the one exception: it runs on whatever thread is unwinding, and
  refusing there would leave a `with` block that cannot be left and a writer
  lock that is never released.

## Building from source

See [github.com/punnerud/mpedb](https://github.com/punnerud/mpedb):
`maturin build --release` in `crates/mpedb-py/` produces the wheel; the test
suite is `crates/mpedb-py/pytest/test_mpedb.py` (plain Python, no pytest —
run it twice against the same directory to also exercise persistence across
process restarts).

License: MIT OR Apache-2.0
