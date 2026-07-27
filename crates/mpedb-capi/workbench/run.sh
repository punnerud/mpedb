#!/usr/bin/env bash
# C-API ecosystem workbench: run real sqlite3 consumers (Python DB-API battery +
# a Django ORM project) against the mpedb libsqlite3 shim via LD_PRELOAD, to
# measure drop-in compatibility beyond mpedb's own tests. Feeds C-API-COMPAT.md.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
# `mpedb-capi` is its OWN cargo workspace — it exports `sqlite3_*` and cannot
# co-link the bundled sqlite the parent's feature unification pulls in (see
# CLAUDE.md). So it builds by manifest path, and its artifacts land in ITS
# target dir, not the parent's. This script used to say `-p mpedb-capi` from
# the root, which stopped resolving the day that split happened.
CAPI="$ROOT/crates/mpedb-capi"
VENV="${WB_VENV:-/tmp/wb-venv}"
cargo build --release --manifest-path "$CAPI/Cargo.toml" || exit 1
# Ask cargo where the artifact went rather than guessing: a `target-dir` in
# ~/.cargo/config.toml (this project's dev boxes use one) moves it out of the
# crate tree entirely, and the old hard-coded `$ROOT/target/...` then pointed at
# a path that does not exist.
SO="$(cargo metadata --format-version 1 --no-deps \
        --manifest-path "$CAPI/Cargo.toml" \
      | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/libmpedb_sqlite3.so"
[ -f "$SO" ] || { echo "no shim at $SO" >&2; exit 1; }
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
