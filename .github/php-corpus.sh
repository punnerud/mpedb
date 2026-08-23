#!/usr/bin/env bash
# One run of PHP's own pdo_sqlite + sqlite3 corpus. Writes the failing tests,
# one path per line, to $3.
#
# Which libsqlite3 the interpreter binds is the caller's business — that choice
# IS the experiment. This script's only job is to guarantee the run happened,
# because "0 tests failed" and "0 tests ran" print the same number, and the
# difference between them is the entire measurement. The step that called this
# has already reported a confident zero once while the corpus sat untouched.
set -uo pipefail

CORPUS=${1:?corpus dir}
LOG=$(realpath -m "${2:?log path}")
FAILS=$(realpath -m "${3:?fails path}")
MIN=${MIN_TESTS:-100}

cd "$CORPUS"
# pdo_sqlite's common.phpt is a REDIRECTTEST into ext/pdo/tests, the shared PDO
# suite every driver is run through. Without that directory run-tests does not
# skip the test, it dies — which is exactly how 133 tests became 13.
test -d ext/pdo/tests || { echo "ext/pdo/tests missing; common.phpt will kill the run"; exit 1; }
find ext/pdo_sqlite/tests ext/sqlite3/tests ext/pdo/tests -name '*.diff' -delete 2>/dev/null

# run-tests exits non-zero whenever any test fails, which is the expected case
# under the shim. Its exit code carries nothing we do not read from the log.
# --no-color: show_result() wraps every verdict in an ANSI escape, so a result
# line does not begin with PASS/FAIL/SKIP but with \e[1;33m. Every grep below
# depends on this flag; if a future run-tests drops it the run dies outright,
# and dying is the correct outcome — the alternative is empty lists again.
TEST_PHP_EXECUTABLE="$(which php)" php run-tests.php -q --offline --no-color \
  -p "$(which php)" ext/pdo_sqlite/tests ext/sqlite3/tests > "$LOG" 2>&1
RC=$?
STARTED=$(grep -c '^TEST ' "$LOG")
echo "run-tests exit $RC after starting $STARTED tests"

# "Number of tests : 133 130" — collected, then actually executed. The second
# number is the one that matters: SKIPIF blocks (a missing extension, most of
# all) retire a test without ever loading a database.
EXEC=$(grep -m1 'Number of tests' "$LOG" | awk '{print $NF}')
COLLECTED=$(ls ext/pdo_sqlite/tests/*.phpt ext/sqlite3/tests/*.phpt | wc -l)
# Executed can exceed collected: common.phpt redirects into the shared PDO
# suite, and every one of those counts as a test run against sqlite.
echo "executed ${EXEC:-none}; $COLLECTED sqlite tests collected, plus the PDO suite via redirect"
grep -E '^(Tests skipped|Tests failed|Tests passed)' "$LOG" || true

if [ -z "$EXEC" ] || [ "$EXEC" -lt "$MIN" ]; then
  echo "the corpus did not run to completion — refusing to report a failure count"
  echo "exit $RC; a bare 139 is the runner itself dying, not a test failing"
  tail -40 "$LOG"
  exit 1
fi

# ANCHORED: run-tests prints "EXPECTED FAILED TEST SUMMARY" 25 lines above the
# real one, and an unanchored match takes the earlier section — which is how a
# baseline reporting "Tests failed: 0" produced two failing test names, both of
# them tests that are SUPPOSED to fail.
sed -n '/^FAILED TEST SUMMARY/,/^====/p' "$LOG" \
  | grep -oE 'ext/[a-z0-9_]+/tests/[A-Za-z0-9_.-]+\.phpt' | sort -u > "$FAILS" || true

# A test whose SKIPIF block bails leaves the comparison without a verdict, and
# a skip reads as silence, not as a difference. Record them so the caller can
# see whether the two runs judged the same set of tests.
grep '^SKIP ' "$LOG" \
  | grep -oE 'ext/[a-z0-9_]+/tests/[A-Za-z0-9_.-]+\.phpt' | sort -u > "${FAILS%.txt}-skipped.txt" || true

# Check every list we derived against the count run-tests printed itself. A
# pattern that matches nothing yields an empty file, and an empty file is
# indistinguishable from good news — that mistake has now been made three
# times in this one step, twice by the code written to prevent it. Parsing
# MORE than reported is fine (a redirect parent is named alongside its
# children); parsing fewer means the pattern missed, and the count is a lie.
check() { # name reported parsed sample-pattern
  echo "  $1: run-tests reported $2, parsed $3"
  [ "$3" -ge "$2" ] || {
    echo "parsed fewer $1 than run-tests reported — the pattern missed"
    echo "  how those lines actually look in the log:"
    grep -inE "$4" "$LOG" | head -5 | cut -c1-140
    exit 1
  }
}
check failures "$(grep -m1 '^Tests failed' "$LOG" | awk '{print $4}')" "$(wc -l < "$FAILS")" 'FAIL'
check skips    "$(grep -m1 '^Tests skipped' "$LOG" | awk '{print $4}')" "$(wc -l < "${FAILS%.txt}-skipped.txt")" 'SKIP'

# grep exits 1 on no match, and under `pipefail` that becomes the script's
# status: a perfectly clean run would report itself as a failure. The count
# above is what says whether the run happened; an empty list here means zero
# failures and nothing else.
exit 0
