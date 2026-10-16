# The PostgreSQL-side workbench

`crates/mpedb-capi/workbench` measures mpedb as a **sqlite** drop-in. This one
measures it as a **PostgreSQL** one: real clients over the v3 wire protocol,
against `mpedb-pg`.

The two ask different questions and find different things. The `pg_regress`
differential (`../src/bin/pg_regress_diff/`) asks *what SQL does mpedb refuse*,
and it goes through `psql`, which uses the SIMPLE query protocol. An ORM uses
the EXTENDED protocol on every parameterised statement and asks the CATALOG
before it does anything at all. Neither of those paths is reachable from the
corpus, and both broke on the first real client.

## `forkserve.py` — one process per connection

`mpedb-pg serve --unix` accepts **serially**, on purpose: the crate's model is
one process per connection with systemd providing the parallelism (see the
comment on `accept_loop`). Every ORM holds a connection POOL, so it blocks on
its second connection and the suite hangs. `forkserve.py` is the smallest thing
that runs the server the way it is meant to run — accept, fork, dup onto fd 0,
exec `serve --inherited-fd`. `systemd-socket-activate --accept` was tried first
and waits for the child, which is the same serialisation one layer up.

```sh
python3 forkserve.py /run/mpedb-wb ./target/release/mpedb-pg db.toml &
```

## `run_django.py` — Django's own PostgreSQL suite

Django has a shared suite selected by database settings, including the
PostgreSQL-specific `postgres_tests` label. The default runner uses the stock
`django.db.backends.postgresql` backend. It retains the checkout revision,
exit status, database files, server logs, suite log, and failed test IDs.
An attempt without a `Ran` summary is explicitly incomplete, including setup
failures or whole-suite timeouts. Errors remain errors.

Install Django from a checkout of `stable/5.2.x` into a separate venv, together
with `tests/requirements/py3.txt` and `tests/requirements/postgres.txt`. Some
optional dependencies require Python development headers and libmemcached.
Run using that venv's Python:

```sh
python crates/mpedb-pg/workbench/run_django.py \
  --django /path/to/django --binary /path/to/mpedb-pg \
  --work-dir /path/on/real/filesystem
# No labels selects the full suite; postgres_tests selects the PG-specific group.

# Exercise ORM behavior with explicit MPEdb workbench adaptations:
python crates/mpedb-pg/workbench/run_django.py \
  --django /path/to/django --binary /path/to/mpedb-pg \
  --work-dir /path/on/real/filesystem \
  --provisioned --portable-sql --test-timeout 15 --timeout 120 \
  basic lookup ordering update
```

The runner uses UTC. `--provisioned` replaces test database creation and
destruction: each alias has a dedicated endpoint and independent fresh file.
It does not implement SQL CREATE DATABASE or general database-name routing.
The optional `--portable-sql` backend also:

* uses Django's generic VALUES bulk-insert compiler instead of UNNEST;
* omits extra PostgreSQL LIKE optimization indexes (MPEdb indexes already
  compare text in binary order);
* omits fixture sequence repair because explicit identity inserts advance
  MPEdb's owned generator;
* flushes with DELETE and computes FK cascade closure when requested;
* uses full-column GROUP BY and disables server-side cursors;
* declares missing row locks, sequence reset, transactional DDL, DISTINCT ON,
  covering indexes, and regex backreferences through Django's feature API.

All adaptations are recorded in `run.json`. Django tests and assertions remain
unchanged. These are MPEdb-backend results, not stock PostgreSQL-backend
conformance. DELETE-based flush can fire DELETE triggers; resetting identity
generators remains an explicit refusal. Optional per-test deadlines report
timeouts as errors; the outer timeout also covers discovery, setup and teardown.
Use `--debug-sql` to capture SQL for failed tests.

Measurement on 2026-10-16, Django revision
`cae7247962de54a94023d28d84f05fc9842e9646`, psycopg **3.3.6**:

* `basic`: **80 passed, 2 skipped** with adaptations; stock PostgreSQL 16.15
  passes all 82 as an environment control.
* `basic lookup ordering update`: **229 passed, 2 errors, 15 skipped** over
  246 tests. Errors are case-insensitive regex (`~*`) and `random()`.
* Full discovery: **18,193 tests**, blocked in setup by `inet`.
* `postgres_tests`: **615 tests**, blocked in setup at `pg_extension`; its
  migrations also require extensions such as hstore, pg_trgm and unaccent.
* A combined run including transactions hit its whole-suite timeout and is
  incomplete; it is not included in completed-run counts.

`pylibmc` and `pywatchman` could not be installed because system development
packages are missing; other Python test requirements were installed. Counts
describe this environment. Stock mode still stops at CREATE DATABASE.
Provisioned mode without portable SQL reaches unsupported PostgreSQL pattern
operator-class indexes. SQLAlchemy conformance does not establish full Django
or PostgreSQL-extension compatibility.

## `satest/` — SQLAlchemy's third-party dialect compliance suite

SQLAlchemy publishes `sqlalchemy.testing.suite` as the conformance bar for a
dialect that is not in the tree, with a `requirements.py` where a dialect
DECLARES what it does not support. That declaration mechanism is the same shape
as mpedb's named refusals, which is why this suite is the better 100 % target
than a corpus written to test PostgreSQL's own internals.

`requirements.py` records three exclusions (schemas, sequences, server-side
cursors) and six supported reflection properties. Each declaration has its
reason in the file. Skipped tests are reported separately; a green attempted
suite would not mean support for every PostgreSQL feature.

For a reproducible run, from the repository root:

```sh
python3 -m venv /tmp/mpedb-pg-venv
/tmp/mpedb-pg-venv/bin/pip install -r crates/mpedb-pg/workbench/requirements.txt
cargo build --manifest-path crates/mpedb-pg/Cargo.toml --bin mpedb-pg
/tmp/mpedb-pg-venv/bin/python crates/mpedb-pg/workbench/run_suite.py \
  --binary /path/to/target/debug/mpedb-pg --work-dir /path/on/real/filesystem
```

The runner creates a fresh database and socket, starts and stops the fork
server, and retains logs and `results.xml`. Pass `--compare /previous/results.xml`
to report fixes and regressions by test name. The exit status is pytest's;
failures remain failures. For an already running server, override the URL:

By default, two generic-suite expectations are adapted to the unchanged
PostgreSQL client dialect in `satest/test_suite.py`: DISTINCT ON compilation
expects PostgreSQL syntax, and ordinary index reflection expects an empty
`include_columns` list. Both original assertions also fail against PostgreSQL
16.15 with these requirements. No engine capability flags are opened by these
adaptations. Pass `--stock-suite` to reproduce the original suite, or set
`MPEDB_STOCK_SUITE=1` when invoking pytest directly. The runner prints the mode.

The adapted mode also adds `EmulatedEnumTest`, exercising SQLAlchemy
`Enum(..., native_enum=False, create_constraint=True)` with the same round-trip
cases and a database-side invalid-label CHECK test. Native `EnumTest` remains
collected and now passes with native ENUM definitions and validated labels.
Emulated ENUM is VARCHAR plus CHECK and uses text sorting. Native ENUM also
supports declaration-order comparisons and ordinary ORDER BY; see COMPAT-PG.md
for the explicit limitations of the initial native implementation.

```sh
cd crates/mpedb-pg/workbench/satest
python -m pytest -q --timeout=20 --timeout-method=thread \
  --dburi 'postgresql+psycopg2://morten@/app?host=/tmp/mpedb-pg-sock'
```

The extension provisioning hook is neutered in `conftest.py` (scaffolding, not test
behaviour) — see the comment there.
