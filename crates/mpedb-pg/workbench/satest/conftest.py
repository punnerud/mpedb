# The mpedb arm of SQLAlchemy's third-party-dialect compliance suite.
#
# mpedb speaks the PostgreSQL wire protocol, so the stock `postgresql+psycopg2`
# dialect drives it unchanged. Two provisioning hooks are neutered here — NOT
# test behaviour, just the scaffolding SQLAlchemy runs against a real server:
#
#   * `post_configure_engine` installs the `citext`/`hstore` EXTENSIONS. mpedb
#     has no extension mechanism and refuses `CREATE EXTENSION` by name, which
#     aborts the whole session before a single test runs. Skipping it leaves
#     the citext/hstore tests to fail on their own terms, which is the honest
#     outcome — they are counted as failures, not hidden.
# The dialect's provision module registers its hook when the dialect LOADS,
# which is after this file is imported — so re-registering the hook here loses
# the race. Emptying the list the hook iterates does not.
import sqlalchemy.dialects.postgresql.provision as _pgprov

_pgprov._extensions.clear()


from sqlalchemy.testing.plugin.pytestplugin import *   # noqa: E402,F401,F403
