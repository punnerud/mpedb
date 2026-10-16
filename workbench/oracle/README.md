# Oracle reference tests

Run Django and SQLAlchemy's **unmodified upstream tests against real Oracle**.
This is an initial reference environment, not an Oracle dialect/protocol for
MPEdb and not a claim of MPEdb Oracle compatibility. No Oracle implementation
source is needed. There are no feature overrides or disabled provisioning hooks.

Start with the Oracle-specific tests, including stored procedures and functions,
then run both full suites. SQLAlchemy 2.0.44's `OutParamTest` creates a real PL/SQL
procedure and verifies numeric and NULL OUT parameters. Its Oracle tests also
create stored functions. Django's Oracle backend tests include DB-API `callproc`.
These do not establish complete PL/SQL support (packages, cursors, exception
handling, triggers, and transaction semantics need a later coverage audit).

## Environment

Prepared on the extra disk, outside Git:

* `/mnt/HC_Volume_106406886/mpedb-oracle/venv`
* `/mnt/HC_Volume_106406886/mpedb-oracle/sqlalchemy-src` (`rel_2_0_44`)
* Django source: `/mnt/HC_Volume_106406885/codex-pg-tests/django-src`

Install `oracledb`, `pytest<9`, `pytest-xdist`, `greenlet`, both upstream projects
in editable mode, and Django's `tests/requirements/py3.txt`. On this machine
`pylibmc` and `pywatchman` lack system build prerequisites; their absence must
remain visible when interpreting full-suite counts. Python-oracledb uses Thin
mode; Oracle Instant Client is not needed for this initial setup.

Use a **dedicated disposable Oracle PDB**. Upstream tests create/drop users,
tablespaces, tables, procedures, and functions. Django's administrative account
needs the privileges described in its Oracle test documentation. SQLAlchemy
needs a separate test account and its upstream schema/provisioning privileges;
use the configuration in the pinned checkout's `README.unittests.rst` and
`lib/sqlalchemy/dialects/oracle/provision.py`. Do not use a production database
or run SQLAlchemy as SYS/SYSTEM. Django manages two distinct test users:
`mpedb_dj_default` and `mpedb_dj_other`.

Supply credentials through environment variables (do not commit them):

```sh
export ORACLE_DSN='127.0.0.1:1521/FREEPDB1'
export ORACLE_USER='dedicated_test_account'
read -rs -p 'Oracle password: ' ORACLE_PASSWORD; echo
export ORACLE_PASSWORD
read -rs -p 'Django temporary user password: ' ORACLE_TEST_PASSWORD; echo
export ORACLE_TEST_PASSWORD
```

Use the Django administrative account for Django runs and the SQLAlchemy test
account for SQLAlchemy runs. Run sequentially, with one worker.

```sh
ORACLE_PY=/mnt/HC_Volume_106406886/mpedb-oracle/venv/bin/python
ORACLE_ARTIFACTS=/mnt/HC_Volume_106406886/mpedb-oracle/runs

# Oracle-specific tests, including procedure/function cases.
"$ORACLE_PY" workbench/oracle/run.py django \
  --source /mnt/HC_Volume_106406885/codex-pg-tests/django-src \
  --artifacts "$ORACLE_ARTIFACTS" backends.oracle
"$ORACLE_PY" workbench/oracle/run.py sqlalchemy \
  --source /mnt/HC_Volume_106406886/mpedb-oracle/sqlalchemy-src \
  --artifacts "$ORACLE_ARTIFACTS" test/dialect/oracle

# Omit the final test label/path to run the full respective suite.
# --prepare-only verifies imports/source and records versions without a DB.
```

Each run records source revision, package versions, server version (when
connected), exit status, and a suite log. SQLAlchemy also writes JUnit XML.
A timeout is recorded as **incomplete**, never green. Test failures, skips, and
setup errors remain in upstream output. Logs may contain test SQL and data and
are kept in a private artifact directory. No Oracle tests have passed until
they have actually run against an Oracle server.

## Server choice on this Ubuntu host

Oracle 26ai Free's supported native Linux distributions are Oracle Linux and
RHEL, not Ubuntu. Its prebuilt VirtualBox appliance is now retired. With a
preference against VMs, use a local Oracle Linux container (Podman can run
without a VM on Linux) or a separate Oracle server. Keep image/data storage on
the extra disk. Avoid converting RPMs into an Ubuntu installation as the test
baseline, since installation failures obscure database compatibility results.

The inspected host has 3.7 GiB RAM, no `/dev/kvm`, no Docker/Podman, and no
passwordless sudo. A server is **not installed or started** by this workbench.
Local container startup requires installing/configuring a runtime first.

References:

* [Oracle Free requirements](https://docs.oracle.com/en/database/oracle/oracle-database/26/xeinl/requirements.html)
* [Retired VirtualBox appliance](https://www.oracle.com/database/technologies/databaseappdev-vm.html)
* [Django Oracle test privileges](https://docs.djangoproject.com/en/5.2/ref/databases/#oracle-notes)
* [SQLAlchemy upstream test instructions](https://github.com/sqlalchemy/sqlalchemy/blob/rel_2_0_44/README.unittests.rst)
* [Python-oracledb initialization](https://python-oracledb.readthedocs.io/en/latest/user_guide/initialization.html)
