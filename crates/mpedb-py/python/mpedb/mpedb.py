"""``import mpedb.mpedb as db`` — force the NATIVE mpedb engine for any path.

Same DB-API surface as ``import mpedb``, but ``connect("thing.db")`` opens
``thing.db`` as a native mpedb file instead of the sqlite-backed overlay —
no sqlite reading, no checkpoint-sync. Use this when the path spelling is
outside your control but the engine choice is not.
"""

import mpedb as _m
from mpedb import *  # noqa: F401,F403 — the shared DB-API surface
from mpedb import __all__  # noqa: F401


def connect(database, *args, **kwargs):
    """`mpedb.connect` with the engine pinned to native mpedb."""
    kwargs["engine"] = "mpedb"
    return _m.connect(database, *args, **kwargs)
