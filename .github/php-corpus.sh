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
find ext/pdo_sqlite/tests ext/sqlite3/tests -name '*.diff' -delete 2>/dev/null

# run-tests exits non-zero whenever any test fails, which is the expected case
# under the shim. Its exit code carries nothing we do not read from the log.
TEST_PHP_EXECUTABLE="$(which php)" php run-tests.php -q --offline \
  -p "$(which php)" ext/pdo_sqlite/tests ext/sqlite3/tests > "$LOG" 2>&1

# "Number of tests : 133 130" — collected, then actually executed. The second
# number is the one that matters: SKIPIF blocks (a missing extension, most of
# all) retire a test without ever loading a database.
EXEC=$(grep -m1 'Number of tests' "$LOG" | awk '{print $NF}')
COLLECTED=$(ls ext/pdo_sqlite/tests/*.phpt ext/sqlite3/tests/*.phpt | wc -l)
echo "executed ${EXEC:-none} of $COLLECTED collected"
grep -E '^(Tests skipped|Tests failed|Tests passed)' "$LOG" || true

if [ -z "$EXEC" ] || [ "$EXEC" -lt "$MIN" ]; then
  echo "the corpus did not run — refusing to report a failure count from it"
  tail -40 "$LOG"
  exit 1
fi

sed -n '/FAILED TEST SUMMARY/,/^====/p' "$LOG" \
  | grep -oE 'ext/[a-z0-9_]+/tests/[A-Za-z0-9_]+\.phpt' | sort -u > "$FAILS"
