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
dispatch or an Oracle wire/DB-API backend. Scalar functions can also be
registered with `Database::create_function(SpellLang::PlSql, source)` or
`mpedb fn define config.toml routine.pls`, then called from MPEdb SQL and shared
prepared plans. That scalar registration rejects procedures and OUT/IN OUT
signatures before publishing any catalog record; those use `ProcEngine`.
Registration follows `ProcEngine::define`'s existing idempotent name/hash
contract; Oracle CREATE-versus-OR-REPLACE catalog behavior is not implemented
by this host API.

The first compiler supports IN/OUT/IN OUT parameters, local variables and
initialized CONSTANT declarations of BOOLEAN and unconstrained NUMBER,
exact numeric literals and comparisons, IS [NOT] NULL,
assignment, IF/ELSIF/ELSE, WHILE/LOOP, EXIT/CONTINUE [WHEN], and RETURN. Static
type checks cover even skipped short-circuit branches; IN parameters cannot
be assigned. Constants accept `:=` or `DEFAULT` initializers (including NULL),
can reference earlier declarations, and reject assignment even in unreachable
branches. A function that reaches END without RETURN raises. Routine END
names must match, and an optional SQL*Plus slash must occupy its own line.
Parameter lists follow Oracle's grammar: omit parentheses for zero parameters.

NUMBER values remain exact decimals; integers are normalized at parameter
boundaries. New routines use Oracle's twenty base-100 groups (39/40 decimal
digits according to exponent parity), with rounding measured against Oracle
23.26.3. Arithmetic `+`, `-`, `*`, `/` and unary signs use bounded arbitrary-width
integer intermediates, SQL NULL propagation, and proper operator precedence.
Recurring division now works: `1 / 3` retains forty fractional digits. Numeric
overflow raises; literal conversion and multiplication underflow become zero,
whereas division underflow on nonzero operands raises as Oracle does.

The new NUMBER behavior has **distinct IR opcodes**. Previously stored numeric
routines retain their original limited semantics and hashes; redefining source
emits the new instructions and produces a new hash. Older builds reject unknown
opcodes. Existing Python/Rust semantics and stored blobs are unchanged.

The reproducible differential tool compares native compiled/serialized PL/SQL
with real Oracle SELECT results:

```sh
# Build with CARGO_TARGET_DIR on the extra disk as for other Rust tests.
cargo build -p mpedb-spell --example plsql_number_probe
python workbench/oracle/number_differential.py \
  --probe /path/to/target/debug/examples/plsql_number_probe \
  --native-root /mnt/HC_Volume_106406886/mpedb-oracle \
  --output /mnt/HC_Volume_106406886/mpedb-oracle/number-report.json
```

The measured seed **20261017** covers **2,016 cases with zero mismatches** for
arithmetic over valid finite literals plus the boundary corpus. An exploratory
run that also generated invalid/out-of-range source literals had 96 parsing
error-code differences: both implementations rejected those expressions, but
Oracle SQL reported ORA-00923 where the native PL/SQL host path reported numeric
overflow. Exact Oracle SQL parsing/error compatibility remains unfinished.
See [Oracle NUMBER precision](https://docs.oracle.com/en/database/oracle/oracle-database/21/sqlrf/Data-Types.html).

Floating-point/text-to-NUMBER conversions and NLS settings remain unimplemented.
**Defaults, constrained types, SQL statements, calls to other routines, packages,
exceptions, cursors, and dynamic SQL remain unimplemented**; measured arithmetic
agreement is not full PL/SQL compatibility.

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

PL/SQL scalar functions use the **existing rRETL function/lens registry**, not
a separate migration converter. An end-to-end native test registers a Boolean
PL/SQL inverse pair, verifies it, applies it to stored rows through rRETL,
records lineage, and reverts to the original values including NULL. Numeric
domains need additional care: NUMBER legitimately coerces Int to Numeric,
whereas rRETL's type-preserving round-trip law must restore the original Int
representation too. The existing verifier correctly refuses to label that
coercion bijective over its mixed-type corpus. Declared typed domains and/or
residual provenance for such conversions remain required work; the Boolean
test does not establish a full Oracle-to-PostgreSQL migration pipeline.

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
conformance**. The real reference server below is now available for differential
measurements; its framework results do not establish MPEdb compatibility.

## Environment

Prepared on the extra disk, outside Git:

* `/mnt/HC_Volume_106406886/mpedb-oracle/venv`
* `/mnt/HC_Volume_106406886/mpedb-oracle/sqlalchemy-src` (`rel_2_0_44`)
* Django source: `/mnt/HC_Volume_106406885/codex-pg-tests/django-src`

Install `pytest<9`, `pytest-xdist`, `greenlet`, both upstream projects in editable
mode, Django's `tests/requirements/py3.txt`, and its `requirements/oracle.txt`.
The latter requires `oracledb >= 2.3.0, < 26.0.0`; the measured environment now
uses **4.0.2**. All Python requirements, including `pylibmc` and `pywatchman`,
are installed. Missing development headers/libraries were extracted into
`native-build-deps` on the extra disk. Thin mode works without Oracle Client;
the full Oracle-specific SQLAlchemy suite also tests Thick mode and uses the
client libraries bundled with the reference server.

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

For the configured local reference instance, add
`--native-root /mnt/HC_Volume_106406886/mpedb-oracle` to either command. This
loads credentials from the private external directory and selects the correct
suite account and client-library paths; passwords never enter command-line
arguments. Native-root configuration is optional: remote servers still use
the environment variables above.

Each run records source revision, package versions, server version (when
connected), exit status, and a suite log. SQLAlchemy also writes JUnit XML.
A timeout is recorded as **incomplete**, never green. Test failures, skips, and
setup errors remain in upstream output. Logs may contain test SQL and data and
are kept in a private artifact directory. No Oracle tests have passed until
they have actually run against an Oracle server.

## Local Oracle reference on Ubuntu

An **unofficial Ubuntu user-directory installation** now runs Oracle AI
Database 26ai Free **23.26.3.0.0**, without VM, Docker, Podman, sudo, or changes
to host accounts. Oracle officially supports Oracle Linux/RHEL rather than
Ubuntu; the retired VirtualBox appliance is not used.

The original Oracle Linux 9 RPM was downloaded from Oracle and SHA-256 checked:
`895fc9df794685b515c4ac83f104dbdfb657eb0e92c41e45c58302fb8fd53e44`.
It is extracted under `native-root` on the extra disk. Oracle's database
binaries are unchanged. Launcher/base-path configuration is relocated locally;
a private compatibility-library path supplies Ubuntu's libaio. Local admin
processes use nss_wrapper account/group files in `native-accounts`, not changes
to `/etc/passwd` or `/etc/group`. Normal framework connections authenticate
with real database passwords and do not load this NSS fixture.

DBCA could not complete its privileged helper checks. Oracle RMAN successfully
restored the bundled seed controlfile/datafiles, recovered without redo, and
opened FREE with RESETLOGS. FREEPDB1 is created, open read/write, and its open
state saved. Datafiles are under `native-data`; the database uses an SPFILE.
Start/stop/reopen have been tested. The listener binds **127.0.0.1:1521** only.
The SGA is 1 GiB, including a 128 MiB vector area; open_cursors is 1000.

SQLA_TEST, TEST_SCHEMA, TEST_SCHEMA_2, and DJANGO_ADMIN are dedicated test users
inside FREEPDB1. SQLAlchemy's public test links use explicit localhost connect
descriptors. Generated passwords and response files live in `native-private`
(directory 0700, credential files 0600), outside Git.

```sh
python workbench/oracle/native_control.py status --root /mnt/HC_Volume_106406886/mpedb-oracle
python workbench/oracle/native_control.py stop --root /mnt/HC_Volume_106406886/mpedb-oracle
python workbench/oracle/native_control.py start --root /mnt/HC_Volume_106406886/mpedb-oracle
```

Reference measurement on 2026-10-16: SQLAlchemy **2.0.44**'s full
`test/dialect/oracle` directory: **476 passed, no failures or skips**, with
python-oracledb **4.0.2**, covering procedures/functions, OUT parameters,
database links, Thin/Thick modes, reflection, numeric types, and vector indexes.
Artifacts: `runs/sqlalchemy-dv5l1f43`. The initial run with driver 26.0.1 had
467 passes and 9 environment failures; corrected client discovery, links,
vector allocation, and the supported driver version produced the green run.
This is a **real Oracle control**, not a MPEdb Oracle conformance result or a
completed full SQLAlchemy/Django run.

Django 5.2's unmodified `backends.oracle basic` control also completed:
**104 passed, 5 skipped, 109 total**, in `runs/django-b7tvprbl`. Full Django and
full SQLAlchemy measurements are separate and must not be inferred from these
subsets.

`reference/number-boundaries.json` records direct SELECT results from Oracle
23.26.3 with decimal fetching enabled. It includes recurring division, 39/40
digit boundaries, underflow (`1e-131` becomes zero), and overflow (`1e126`
raises ORA-01426). These measured values are inputs to the next NUMBER changes;
they are not a claim that MPEdb already reproduces them.

References:

* [Oracle Free requirements](https://docs.oracle.com/en/database/oracle/oracle-database/26/xeinl/requirements.html)
* [Retired VirtualBox appliance](https://www.oracle.com/database/technologies/databaseappdev-vm.html)
* [Django Oracle test privileges](https://docs.djangoproject.com/en/5.2/ref/databases/#oracle-notes)
* [SQLAlchemy upstream test instructions](https://github.com/sqlalchemy/sqlalchemy/blob/rel_2_0_44/README.unittests.rst)
* [Python-oracledb initialization](https://python-oracledb.readthedocs.io/en/latest/user_guide/initialization.html)
