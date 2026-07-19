#!/usr/bin/env bash
# Run DJANGO'S OWN test suite against the mpedb libsqlite3 shim, twice: once on
# the stock system sqlite (BASELINE) and once with the shim LD_PRELOADed. Both
# arms use `settings.py` here, whose backend applies the documented adaptations
# (see mpedb_backend/base.py), so the pass/fail DIFF isolates mpedb.
#
#   ./run_suite.sh                 # both arms, default label groups
#   WB_ARM=shim ./run_suite.sh     # one arm only (stock|shim)
#
# Django is NOT vendored. Point WB_DJANGO at a checkout (git clone --depth 1
# -b stable/5.2.x https://github.com/django/django) with a venv holding
# asgiref+sqlparse+tzdata. Keep it off `/` — this box's root fills up.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../../.." && pwd)"
SO="$ROOT/target/release/libmpedb_sqlite3.so"
DJ="${WB_DJANGO:-/mnt/xfs/django-workbench/django}"
PY="${WB_PY:-/mnt/xfs/django-workbench/venv/bin/python}"
OUT="${WB_OUT:-/tmp/wb-django-suite}"

# The label GROUPS. Django's suite installs every label's models into ONE
# database, and mpedb caps a schema at 120 user tables (MAX_TABLES = 128 minus 8
# system slots), so the labels must be split into groups that each fit. `queries`
# is absent because it alone exceeds the cap — see C-API-COMPAT.md gap D6.
LABEL_GROUPS=(  # NOT `GROUPS`: bash owns that name (the caller's group ids) and
                # silently ignores the assignment — the suite then "runs" one
                # bogus label per group and reports 1 test.
    "basic lookup transactions ordering update delete"
    "aggregation annotations expressions"
)

[ -x "$PY" ] || { echo "no venv python at $PY (set WB_PY)"; exit 1; }
[ -d "$DJ/tests" ] || { echo "no Django checkout at $DJ (set WB_DJANGO)"; exit 1; }
( cd "$ROOT" && cargo build --release -p mpedb-capi ) || exit 1
mkdir -p "$OUT"

run_arm() {  # $1 = stock|shim, $2 = group index, $3.. = labels
    local arm=$1 idx=$2; shift 2
    local log="$OUT/${arm}_g${idx}.txt"
    rm -f /dev/shm/mpedb-capi-*
    (
        cd "$DJ/tests" || exit 1
        export PYTHONPATH="$DJ:$HERE"
        [ "$arm" = shim ] && export LD_PRELOAD="$SO"
        # 3 GB address-space guard: an unbounded run has OOM-killed this box.
        ulimit -v 3000000
        timeout 1800 "$PY" runtests.py --settings=settings --parallel=1 -v1 "$@"
    ) > "$log" 2>&1
    printf '%-6s g%d  %s | %s\n' "$arm" "$idx" \
        "$(grep -E '^Ran ' "$log")" "$(grep -E '^(OK|FAILED)' "$log")"
}

for arm in ${WB_ARM:-stock shim}; do
    i=1
    for g in "${LABEL_GROUPS[@]}"; do
        # shellcheck disable=SC2086
        run_arm "$arm" "$i" $g
        i=$((i + 1))
    done
done
echo "logs in $OUT"
