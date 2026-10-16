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

## Migration architecture

The required migration path in **both directions** is:

```text
Oracle ⇄ MPEdb source representation ⇄ rRETL ⇄ MPEdb target representation ⇄ PostgreSQL
```

All transformations run in MPEdb through its existing rRETL lens pairs,
residuals, lineage, verified apply/revert/putback, and incremental map machinery.
Source adapters supply typed snapshots, change capture, and transactional
write-back. They do not contain a parallel Oracle-to-PostgreSQL converter or
bypass rRETL for the reverse direction. Identity mappings must also retain
source type/schema provenance rather than infer reversibility from equal values.

Live synchronization must preserve source commit boundaries, resume cursors,
echo suppression, per-row rejection/conflict handling, and epoch-fenced authority
switches. The current mirror engine permits one authoritative source per MPEdb
file; connecting both source systems and rRETL therefore requires an explicit
composition of mirror instances and maps. That composition, the Oracle source
adapter, Oracle SQL execution, and full PL/SQL execution are **not implemented
or verified by this test setup**. Existing PostgreSQL mirror support alone does
not prove the requested topology works.

PL/SQL coverage remains part of the end goal, including procedures/functions,
IN/OUT/IN OUT parameters, packages, exceptions, cursors, dynamic SQL, triggers,
and database/session transaction behavior. Python/Rust procedure semantics must
not substitute for Oracle's NULL logic or Oracle transaction semantics. The
upstream Oracle suites are initial reference coverage, not the completion gate
for full Oracle support.

## Native implementation progress

`mpedb-spell` now has an Oracle lexical frontend (`plsql::lex`) that preserves
exact numeric spellings, national strings, alternative q-quoting, bind names,
identifier case, and byte spans. `plsql::compile` now compiles native CREATE
FUNCTION/PROCEDURE definitions to the shared IR, available through
`ProcEngine::define(source, Lang::PlSql)` and `mpedb proc define config.toml
routine.pls` (`.plsql` also accepted). These routines persist by name/hash and
remain callable after database reopen. This does not implement Oracle SQL DDL
dispatch or an Oracle wire/DB-API backend.
Registration follows `ProcEngine::define`'s existing idempotent name/hash
contract; Oracle CREATE-versus-OR-REPLACE catalog behavior is not implemented
by this host API.

The first compiler supports IN/OUT/IN OUT parameters and local variables of BOOLEAN and
unconstrained NUMBER, exact numeric literals and comparisons, IS [NOT] NULL,
assignment, IF/ELSIF/ELSE, WHILE/LOOP, EXIT/CONTINUE [WHEN], and RETURN. Static
type checks cover even skipped short-circuit branches; IN parameters cannot
be assigned. A function that reaches END without RETURN raises. Routine END
names must match, and an optional SQL*Plus slash must occupy its own line.
Parameter lists follow Oracle's grammar: omit parentheses for zero parameters.

NUMBER values remain exact decimals; integers are normalized to decimals at
parameter boundaries. Values beyond 38 significant digits or the supported
Oracle exponent range currently raise instead of simulating Oracle rounding
or underflow. Floating-point/text-to-NUMBER conversions and NLS settings are
not implemented. **Defaults, constrained types, SQL
statements, calls to other routines, packages, exceptions, cursors, and dynamic
SQL remain unimplemented**, so this is not full PL/SQL compatibility.

Arithmetic `+`, `-`, `*`, `/` and unary signs now use bounded arbitrary-width
integer decimal intermediates, with SQL NULL propagation and proper operator
precedence. Exact terminating division works (e.g. `5 / 2 = 2.5`); zero division
raises. The implementation handles valid exponents such as `1e100` without a
floating-point fallback or i128 overflow. Nonterminating division (`1 / 3`),
results requiring more than the currently supported 38 significant digits,
overflow, and underflow are still refused. Oracle guarantees up to 20 base-100
digits (39 or 40 decimal digits depending on position); this is why copying
PostgreSQL's 16-place division or guessing a 38-digit rounding rule would be
incorrect. Rounded arithmetic and full Oracle NUMBER behavior require a real
reference measurement and remain part of the unfinished scope. See
[Oracle NUMBER precision](https://docs.oracle.com/en/database/oracle/oracle-database/21/sqlrf/Data-Types.html).

The native `ProcEngine` calling convention supplies one argument slot for every
formal parameter, including OUT placeholders. OUT values start NULL; IN OUT
values are initialized and type-checked from the supplied value. Procedures
with output parameters return a scalar-only tuple containing **all parameters
in declaration order**. Functions with output parameters return a flat tuple
containing the function result first, then all parameters. Routines without
output parameters retain their scalar return convention. This is the native
MPEdb host API, not an implemented Oracle DB-API `callproc`/`callfunc` adapter.
Only successful calls copy values out; errors return no partial output tuple.
`NOCOPY` is accepted for OUT/IN OUT and ignored, using the copy semantics that
Oracle permits for this hint. Argument aliasing is not provided by this value
API. See Oracle's [parameter-mode rules](https://docs.oracle.com/en/database/oracle/oracle-database/26/lnpls/subprogram-parameters.html).

The separate SQL BOOLEAN/comparison IR instructions
implement NULL propagation and three-valued AND/OR/NOT, with strict BOOLEAN
conditions and short-circuit jump primitives. Old Python/Rust instructions and
stored blobs retain their semantics. New opcodes extend format 1 additively;
older readers reject them as unknown, as with the existing cursor extension.

Tests execute serialized IR and cover full Boolean truth tables, comparisons,
NULL, exact decimal ordering, invalid condition types, short-circuit error
behavior, budget exhaustion, and malformed stack/jump programs. Lexer tests
include multibyte input truncation and source locations. These are native unit
tests against the public language specification, **not measured Oracle server
conformance**. A real Oracle reference connection is still needed.

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
