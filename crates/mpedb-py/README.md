# mpedb-py — Python bindings for mpedb

CPython **3.12+** extension module (PyO3 0.29, `abi3-py312`: one wheel/.so for all
3.12+ versions). Module name: `mpedb`. Free-threading friendly by design: no
module-level mutable state, and the GIL is released around every engine call, so
point reads from multiple Python threads run truly in parallel.

## Building

Manual (no extra tooling):

```sh
cargo build --release -p mpedb-py
mkdir -p /path/to/pymod
cp target/release/libmpedb_py.so /path/to/pymod/mpedb.so   # the rename is required
PYTHONPATH=/path/to/pymod python3.12 -c "import mpedb; print(mpedb.Database)"
```

The cdylib is built as `libmpedb_py.so` but must be importable as `mpedb.so`
(the `#[pymodule]` is named `mpedb`, so its init symbol is `PyInit_mpedb`).

With maturin (builds a proper wheel):

```sh
pip install maturin && maturin build --release -m crates/mpedb-py/Cargo.toml
```

Tests (plain Python, no pytest; run it twice against the same directory to also
exercise persistence across process restarts):

```sh
PYTHONPATH=/path/to/pymod python3.12 crates/mpedb-py/pytest/test_mpedb.py /tmp/mpedb-pytest
PYTHONPATH=/path/to/pymod python3.12 crates/mpedb-py/pytest/test_mpedb.py /tmp/mpedb-pytest
```

## API

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
| `tx.commit()` / `tx.rollback()` | `None` | Explicit finish. A dropped/GC'd transaction rolls back. |
| `with db.begin() as tx:` | | Commits on clean exit, rolls back if an exception propagates (never suppresses it). |

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

Messages carry the engine's `Display` text. Binding-level misuse (bad params
container, non-convertible value) raises the ordinary `TypeError`/`OverflowError`.

## Locking rules (inherited from the Rust facade)

- **Never call `db.prepare(...)`, `db.verify()`, or `db.query(...)` for a
  not-yet-cached statement while a `Transaction` from the same handle is open on
  the same thread.** They may need the single writer lock the transaction
  already holds; the ERRORCHECK mutex turns the relock into an error rather
  than a deadlock. Prepare the statements you need *before* `db.begin()`;
  inside the transaction, `tx.query`/`tx.execute` are always safe.
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
- Use a `Transaction` from the thread that created it; the writer lock is a
  pthread mutex with thread affinity.
