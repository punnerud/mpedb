"""``sqlite3.dbapi2`` — the stdlib exposes the DB-API under this name too
(`from sqlite3 import dbapi2`), so the drop-in must as well."""

from mpedb import *  # noqa: F401,F403
from mpedb import __all__  # noqa: F401
