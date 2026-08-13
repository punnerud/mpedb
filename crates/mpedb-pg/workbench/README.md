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

## `satest/` — SQLAlchemy's third-party dialect compliance suite

SQLAlchemy publishes `sqlalchemy.testing.suite` as the conformance bar for a
dialect that is not in the tree, with a `requirements.py` where a dialect
DECLARES what it does not support. That declaration mechanism is the same shape
as mpedb's named refusals, which is why this suite is the better 100 % target
than a corpus written to test PostgreSQL's own internals.

`requirements.py` starts EMPTY on purpose: every property added to it is a
claim, so the baseline is taken against the full bar and each exclusion has to
be argued for.

```sh
cd satest && python -m pytest -q --timeout=20 --timeout-method=thread
```

Two provisioning hooks are neutered in `conftest.py` (scaffolding, not test
behaviour) — see the comment there.
