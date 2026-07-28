"""mpedb — a drop-in `sqlite3` replacement backed by the mpedb engine.

Usage, exactly like the standard library::

    import mpedb as db          # or: import mpedb.sqlite3 as db
    conn = db.connect("app.db")
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
    conn.execute("INSERT INTO t (id, name) VALUES (?, ?)", (1, "a"))
    conn.commit()
    print(conn.execute("SELECT * FROM t").fetchall())

Routing by path (the whole point of the package):

- ``*.db``      — the file is read as a **sqlite** database; writes land in an
  mpedb delta next to it and ``commit()`` checkpoints them back into the
  ``.db``, so sqlite tools and mpedb see one store, kept in sync.
- ``*.mpedb``   — native mpedb (multi-process shared-memory engine).
- ``:memory:``  — native mpedb, process-private memory.
- ``*.toml``    — an explicit mpedb config file (schema/durability up front).

``import mpedb.mpedb as db`` forces the native engine for any path;
``import mpedb.sqlite3 as db`` is this module's default routing under its
explicit name, so more engines can sit alongside later.

The SQL surface is a real subset of sqlite's: an unsupported statement raises
``ProgrammingError`` loudly rather than doing something else quietly. Answers
that ARE given match sqlite (the engine is differentially tested against it).
"""

from mpedb._native import (
    Connection,
    Cursor,
    Database,
    Error,
    IntegrityError,
    OperationalError,
    ProgrammingError,
    Session,
    Transaction,
    apilevel,
    paramstyle,
    threadsafety,
)
from mpedb._native import connect as _connect

# sqlite3-module compatibility aliases (PEP 249 hierarchy: everything mpedb
# raises is one of the four above; the rest alias the closest parent so
# `except sqlite3.DatabaseError` keeps catching).
DatabaseError = Error
DataError = ProgrammingError
InterfaceError = ProgrammingError
InternalError = OperationalError
NotSupportedError = ProgrammingError
Warning = Error

# Constants sqlite3-shaped code passes around; accepted and ignored where
# they configure machinery mpedb does not have. The SQLITE_LIMIT_* /
# LEGACY_TRANSACTION_CONTROL set matters beyond its face value: two missing
# constants import-blocked 7 of CPython's 10 test_sqlite3 files.
PARSE_DECLTYPES = 1
PARSE_COLNAMES = 2
LEGACY_TRANSACTION_CONTROL = -1
SQLITE_OK = 0
SQLITE_DENY = 1
SQLITE_IGNORE = 2
SQLITE_LIMIT_LENGTH = 0
SQLITE_LIMIT_SQL_LENGTH = 1
SQLITE_LIMIT_COLUMN = 2
SQLITE_LIMIT_EXPR_DEPTH = 3
SQLITE_LIMIT_COMPOUND_SELECT = 4
SQLITE_LIMIT_VDBE_OP = 5
SQLITE_LIMIT_FUNCTION_ARG = 6
SQLITE_LIMIT_ATTACHED = 7
SQLITE_LIMIT_LIKE_PATTERN_LENGTH = 8
SQLITE_LIMIT_VARIABLE_NUMBER = 9
SQLITE_LIMIT_TRIGGER_DEPTH = 10
SQLITE_LIMIT_WORKER_THREADS = 11

sqlite_version = "3.45.0-mpedb"
sqlite_version_info = (3, 45, 0)
version = "0.1.5"
version_info = (0, 1, 5)

# ---------------------------------------------------------------------------
# The PEP 249 type layer, as sqlite3 defines it. Adapters/converters are
# REGISTERED faithfully (Django's sqlite backend registers a set at import
# time) — application is part of the 0.2 detect_types work; registration must
# not crash the import.
import datetime as _dt

Date = _dt.date
Time = _dt.time
Timestamp = _dt.datetime


def DateFromTicks(ticks):
    return Date.fromtimestamp(ticks)


def TimeFromTicks(ticks):
    return Time(*_dt.datetime.fromtimestamp(ticks).timetuple()[3:6])


def TimestampFromTicks(ticks):
    return Timestamp.fromtimestamp(ticks)


Binary = memoryview

adapters = {}
converters = {}


def register_adapter(type_, adapter):
    """Store the adapter (applied in 0.2's detect_types work)."""
    adapters[(type_, PrepareProtocol)] = adapter


def register_converter(name, converter):
    """Store the converter (applied in 0.2's detect_types work)."""
    converters[name.upper()] = converter


class PrepareProtocol:
    """sqlite3's adapter-protocol marker type."""

    def __init__(self, *args, **kwargs):
        pass


def enable_callback_tracebacks(flag):
    """Accepted and ignored — mpedb has no C callbacks to swallow tracebacks in."""


def complete_statement(statement):
    """sqlite3's heuristic: does the string end a complete SQL statement?"""
    s = statement.strip()
    return s.endswith(";")

__all__ = [
    "Connection",
    "Cursor",
    "Database",
    "DataError",
    "DatabaseError",
    "Error",
    "IntegrityError",
    "InterfaceError",
    "InternalError",
    "NotSupportedError",
    "OperationalError",
    "PARSE_COLNAMES",
    "PARSE_DECLTYPES",
    "ProgrammingError",
    "Session",
    "Transaction",
    "Warning",
    "apilevel",
    "connect",
    "paramstyle",
    "sqlite_version",
    "sqlite_version_info",
    "threadsafety",
    "version",
    "version_info",
    "Binary",
    "Date",
    "DateFromTicks",
    "LEGACY_TRANSACTION_CONTROL",
    "PrepareProtocol",
    "SQLITE_LIMIT_SQL_LENGTH",
    "SQLITE_OK",
    "Time",
    "TimeFromTicks",
    "Timestamp",
    "TimestampFromTicks",
    "adapters",
    "complete_statement",
    "converters",
    "enable_callback_tracebacks",
    "register_adapter",
    "register_converter",
]


def connect(
    database,
    timeout=None,
    detect_types=0,
    isolation_level="",
    check_same_thread=True,
    factory=None,
    cached_statements=128,
    uri=False,
    *,
    engine=None,
):
    """PEP 249 ``connect()`` with ``sqlite3.connect``'s signature.

    ``database`` routes by spelling (see the module docstring). The remaining
    positional/keyword arguments exist so sqlite3-shaped call sites run
    unchanged; the ones that configure machinery mpedb does not have
    (``detect_types``, ``cached_statements``, ``factory``) are accepted and
    ignored. ``engine="mpedb"`` forces the native engine — the programmatic
    form of ``import mpedb.mpedb``.
    """
    del timeout, detect_types, isolation_level, check_same_thread
    del factory, cached_statements
    path = str(database)
    if uri and path.startswith("file:"):
        path = path[5:].split("?", 1)[0]
    return _connect(path, engine=engine)


# `from sqlite3 import dbapi2` must resolve as an attribute after a
# sys.modules swap — the submodule import binds it.
from mpedb import dbapi2  # noqa: E402,F401
