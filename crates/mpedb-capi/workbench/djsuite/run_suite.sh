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
# database, so the labels are split into groups. G1/G2 are the COMPARABILITY set
# — the same 9 labels / 831 tests every run since run 1, so the arms are
# comparable across runs. They are frozen: add new coverage as a NEW group, in a
# separate invocation (`WB_LABEL_GROUPS`), never by editing these two.
#
# `WB_LABEL_GROUPS` overrides them: newline-separated, one group of
# space-separated labels per line. Group log files are numbered from
# `WB_GROUP_BASE` (default 1) so a second invocation does not overwrite the
# first's logs.
LABEL_GROUPS=(  # NOT `GROUPS`: bash owns that name (the caller's group ids) and
                # silently ignores the assignment — the suite then "runs" one
                # bogus label per group and reports 1 test.
    "basic lookup transactions ordering update delete"
    "aggregation annotations expressions"
)
if [ -n "${WB_LABEL_GROUPS:-}" ]; then
    LABEL_GROUPS=()
    while IFS= read -r line; do
        [ -n "$line" ] && LABEL_GROUPS+=("$line")
    done <<< "$WB_LABEL_GROUPS"
fi

[ -x "$PY" ] || { echo "no venv python at $PY (set WB_PY)"; exit 1; }
[ -d "$DJ/tests" ] || { echo "no Django checkout at $DJ (set WB_DJANGO)"; exit 1; }
( cd "$ROOT" && cargo build --release -p mpedb-capi ) || exit 1
mkdir -p "$OUT"

run_arm() {  # $1 = stock|shim, $2 = group index, $3.. = labels
    local arm=$1 idx=$2; shift 2
    local log="$OUT/${arm}_g${idx}.txt"
    rm -f /dev/shm/mpedb-capi-* /dev/shm/mpedb-*.mpedb
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
    i=${WB_GROUP_BASE:-1}
    for g in "${LABEL_GROUPS[@]}"; do
        # shellcheck disable=SC2086
        run_arm "$arm" "$i" $g
        i=$((i + 1))
    done
done
echo "logs in $OUT"
