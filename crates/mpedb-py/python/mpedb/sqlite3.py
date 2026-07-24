"""``import mpedb.sqlite3 as db`` — the sqlite3-compatible surface, explicitly.

Identical to ``import mpedb`` today; the explicit name exists so future
engine-specific submodules (``mpedb.mpedb``, others later) can sit alongside
without the default moving under anyone.
"""

from mpedb import *  # noqa: F401,F403 — re-export the whole DB-API surface
from mpedb import __all__  # noqa: F401
