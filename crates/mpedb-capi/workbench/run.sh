#!/usr/bin/env bash
# C-API ecosystem workbench: run real sqlite3 consumers (Python DB-API battery +
# a Django ORM project) against the mpedb libsqlite3 shim via LD_PRELOAD, to
# measure drop-in compatibility beyond mpedb's own tests. Feeds C-API-COMPAT.md.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
SO="$ROOT/target/release/libmpedb_sqlite3.so"
VENV="${WB_VENV:-/tmp/wb-venv}"
( cd "$ROOT" && cargo build --release -p mpedb-capi ) || exit 1
[ -d "$VENV" ] || python3 -m venv "$VENV"
"$VENV/bin/python" -m pip install --quiet --upgrade pip
"$VENV/bin/pip" install --quiet "django>=5,<6"
PY="$VENV/bin/python"

echo "== 1. Python DB-API 2.0 battery (shim vs stock) =="
LD_PRELOAD="$SO" "$PY" "$ROOT/crates/mpedb-capi/tests/dbapi_battery.py" 2>&1 | tail -3

echo "== 2. Django ORM: migrate under the shim =="
DB=/tmp/wb-django.db
rm -f "$DB" "$DB.overlay.mpedb" 2>/dev/null
LD_PRELOAD="$SO" "$PY" "$HERE/proj/manage.py" migrate 2>&1 | tail -8
