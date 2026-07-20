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
# Interposition differs per platform:
#   Linux — LD_PRELOAD the cdylib directly.
#   macOS — there is no LD_PRELOAD. dyld resolves a dependent library by its
#     LEAF NAME through DYLD_LIBRARY_PATH first, so a directory of
#     libsqlite3{,.0,.3}.dylib symlinks pointing at our cdylib makes CPython's
#     _sqlite3 load the shim instead of the system one. DYLD_* survives exec of
#     a Homebrew python (not SIP-protected); it would be stripped for
#     /usr/bin/python3, so the venv MUST be built on a Homebrew interpreter.
case "$(uname -s)" in
  Darwin) SO="$ROOT/target/release/libmpedb_sqlite3.dylib"; WB_OS=darwin ;;
  *)      SO="$ROOT/target/release/libmpedb_sqlite3.so";     WB_OS=linux  ;;
esac
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
[ -f "$SO" ] || { echo "no shim at $SO"; exit 1; }
mkdir -p "$OUT"

# Where the shim puts its "in-memory" database files (see mpedb-capi/src/lib.rs).
SCRATCH=$([ -d /dev/shm ] && echo /dev/shm || echo "${TMPDIR:-/tmp}")
SCRATCH="${SCRATCH%/}"

# The leaf-name symlink directory dyld searches (macOS only; harmless on Linux).
SHIMDIR="$OUT/shim"
mkdir -p "$SHIMDIR"
for leaf in libsqlite3.dylib libsqlite3.0.dylib libsqlite3.3.dylib; do
    ln -sf "$SO" "$SHIMDIR/$leaf"
done

# A wall-clock cap. GNU coreutils `timeout` is not on a stock macOS; perl's
# alarm+exec is, and it is exactly what the corpus runner already uses here.
if command -v timeout  >/dev/null 2>&1; then
    run_capped() { local t=$1; shift; timeout  "$t" "$@"; }
elif command -v gtimeout >/dev/null 2>&1; then
    run_capped() { local t=$1; shift; gtimeout "$t" "$@"; }
else
    run_capped() { local t=$1; shift; perl -e 'alarm shift; exec @ARGV' "$t" "$@"; }
fi

# The launcher prefix that puts a run in the shim arm.
#
# On macOS this may NOT be a plain `export`: /usr/bin/perl (and every other
# SIP-protected binary the wrapper chain might go through) has DYLD_* STRIPPED
# from its environment by dyld, and the stripped environment is what it passes
# on to python. `env DYLD_LIBRARY_PATH=…` re-adds the variable from env's own
# argv, after the stripping, and python is not protected so it keeps it. Getting
# this wrong is silent: the shim arm then runs on the SYSTEM libsqlite3 and
# reports a perfect score identical to stock.
arm_prefix() {  # $1 = stock|shim; prints the prefix words, one per line
    [ "$1" = shim ] || return 0
    if [ "$WB_OS" = darwin ]; then
        printf '%s\n' /usr/bin/env "DYLD_LIBRARY_PATH=$SHIMDIR"
    else
        printf '%s\n' /usr/bin/env "LD_PRELOAD=$SO"
    fi
}

run_arm() {  # $1 = stock|shim, $2 = group index, $3.. = labels
    local arm=$1 idx=$2; shift 2
    local PRE=(); local w
    while IFS= read -r w; do [ -n "$w" ] && PRE+=("$w"); done < <(arm_prefix "$arm")
    local log="$OUT/${arm}_g${idx}.txt"
    rm -f "$SCRATCH"/mpedb-capi-* "$SCRATCH"/mpedb-*.mpedb
    # Django names every test database file:memorydb_<alias>; the shim makes
    # those REAL files in the test cwd, and a stale one silently carries a
    # previous run schema into this one.
    rm -f "$DJ"/tests/memorydb_*
    (
        cd "$DJ/tests" || exit 1
        export PYTHONPATH="$DJ:$HERE"
        # 3 GB address-space guard: an unbounded run has OOM-killed the Linux
        # box. Darwin's ulimit has no -v (RLIMIT_AS is not enforced there), so
        # the guard is simply absent on macOS.
        [ "$WB_OS" = linux ] && ulimit -v 3000000
        run_capped 1800 ${PRE[@]+"${PRE[@]}"} \
            "$PY" runtests.py --settings=settings --parallel=1 -v1 "$@"
    ) > "$log" 2>&1
    printf '%-6s g%d  %s | %s\n' "$arm" "$idx" \
        "$(grep -E '^Ran ' "$log")" "$(grep -E '^(OK|FAILED)' "$log")"
}

# Interposition self-check. A shim arm that silently runs on the SYSTEM
# libsqlite3 produces a beautiful, meaningless 100 %; refuse to measure that.
# It MUST go through the same wrapper chain the measurement uses (run_capped +
# arm_prefix) — checking a shorter path is how a stripped DYLD_LIBRARY_PATH got
# past this gate once already.
verify_interposition() {
    local stock shim w
    local PRE=()
    while IFS= read -r w; do [ -n "$w" ] && PRE+=("$w"); done < <(arm_prefix shim)
    stock=$(run_capped 60 "$PY" -c 'import sqlite3; print(sqlite3.sqlite_version)' 2>&1)
    shim=$(run_capped 60 ${PRE[@]+"${PRE[@]}"} "$PY" -c 'import sqlite3; print(sqlite3.sqlite_version)' 2>&1)
    echo "interposition: stock=$stock shim=$shim"
    [ "$stock" != "$shim" ] || {
        echo "REFUSING TO MEASURE: the shim arm reports the same sqlite_version as stock."
        exit 1
    }
}
verify_interposition

for arm in ${WB_ARM:-stock shim}; do
    i=${WB_GROUP_BASE:-1}
    for g in "${LABEL_GROUPS[@]}"; do
        # shellcheck disable=SC2086
        run_arm "$arm" "$i" $g
        i=$((i + 1))
    done
done
echo "logs in $OUT"
