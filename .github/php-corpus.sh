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
TEST_PHP_EXECUTABLE="$(which php)" php run-tests.php -q --offline \
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

sed -n '/FAILED TEST SUMMARY/,/^====/p' "$LOG" \
  | grep -oE 'ext/[a-z0-9_]+/tests/[A-Za-z0-9_.-]+\.phpt' | sort -u > "$FAILS"
