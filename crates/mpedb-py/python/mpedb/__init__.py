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
# they configure machinery mpedb does not have.
PARSE_DECLTYPES = 1
PARSE_COLNAMES = 2

sqlite_version = "3.45.0-mpedb"
sqlite_version_info = (3, 45, 0)
version = "0.1.0"
version_info = (0, 1, 0)

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
