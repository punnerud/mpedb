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

Measured full Django reference run (5.2 stable, revision recorded in the
artifacts): 18,193 tests in 1973 seconds, 1512 skipped, 6 expected failures,
and 6 failure entries. Four failures came from the harness's test-only
SECRET_KEY; the two affected test methods pass after using upstream's expected
key. Two HTML tests reflect Ubuntu's security-backported Python 3.12.3 reporting
an older version than their version-based expectations. A private stock
[CPython 3.12.15](https://www.python.org/downloads/release/python-31215/) was built
on the extra disk. All three affected test methods now pass with the corrected
settings and complete dependency environment, without changing assertions.
The private interpreter ran 17,587 tests successfully; the missing 606 cache
cases also passed separately. Their combined coverage is 18,193 tests,
1512 skips and 6 expected failures. The harness now starts in upstream's
`tests` directory: a cache directory created in the source root had shadowed
`cache.tests` during discovery. The corrected single full run completed successfully: **18,193 tests in
1984.367 seconds, zero failures, 1512 skipped and 6 expected failures**. See
`reference/framework-baseline.json` for measured counts.

The full broad SQLAlchemy 2.0.44 reference run completed with early failure
stopping disabled: **22,260 passed, 259 failed, 497 setup/teardown errors,
814 skipped**, in 2092.55 seconds. Some ORM fixtures omit Oracle sequences for
generated primary keys; cleanup errors also cascade. These failures still need
classification. Upstream assertions and requirement flags remain unchanged.
These reference results do not describe MPEdb's implementation.

A separate **clean SQLAlchemy backend conformance suite** (`test/dialect/test_suite.py`)
completed with **1355 passed, 87 skipped, zero failures/errors**, in 162.91 seconds
on private Python 3.12.15. Broad ORM fixture failures had left a `joined_base`
table which polluted subsequent reflection assertions. Only idle disposable
SQLAlchemy schemas were reset, with original grants/passwords preserved;
upstream assertions and capability requirements remained unchanged. This clean
backend result does not erase failures in the broad ORM run.

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

## Oracle source boundary

The optional `mpedb-mirror/oracle-source` feature uses the public OCI client to
read application-owned tables and routine definitions. It retains original DDL,
NUMBER precision/scale, character units, generated/identity flags and compound
primary keys. Snapshot rows use an explicit SCN shared by the caller across
tables; snapshot failure never falls back to current data. Exact NUMBER strings,
LOBs and nanosecond/timezone temporal text enter MPEdb losslessly. Unsupported
object types and unvalidated identities are refused.

A live native Oracle test reads a compound-key snapshot, loads its carrier into
MPEdb, compiles a function extracted from that application's `ALL_SOURCE`, and
verifies rRETL apply, lineage and revert. In-place rRETL now uses every primary
key component while retaining single-key residual compatibility. Incremental
maps still require single-key identities. A low-level Oracle write batch now
transports MPEdb carrier rows in one source transaction. It fences source DDL,
checks object identity and original DDL, reads actual postimages, and rolls back
the entire batch if Oracle rounds, truncates or substitutes a writable value.
Generated columns return their actual source values. The live test writes the
rRETL-transformed Boolean back to Oracle and restores it through rRETL; another
test verifies rounding rejection, whole-batch rollback, inserts/deletes,
generated values and stale-schema rejection.

This transport is not a complete mirror push: capture, echo suppression,
conflict checks, authority fencing, CLI import and the two-database topology
remain unfinished. Writes currently refuse identity columns, special NUMBERs,
zoned timestamps, intervals and LONGs, and accept only small LOB binds (4000
text bytes / 2000 binary bytes). Larger LOBs need streaming OCI bindings.
The native source boundary also captures bounded Flashback Versions windows.
It discovers identities from both version start and end SCNs, then reads their
net images at each commit SCN across all selected tables. Tests cover an
uncommitted transaction, later commit, rollback, historical images, compound-PK
changes, deletes and resume/budget rejection. XIDs retain transaction provenance.
Expired undo or an oversized window fails without advancing a cursor. This is
not yet a live `SourceAdapter`: epoch handling and own-write echo suppression
still need their source-side state/ledger.

An importer must establish its initial cursor only after a complete successful
snapshot. On this native Oracle version, an immediate post-DDL SCN can map to a
timestamp preceding that DDL and fail with ORA-01466. Tests retry only that
initial bootstrap; established capture never skips ahead on such an error.
See [Oracle Flashback documentation](https://docs.oracle.com/en/database/oracle/oracle-database/26/adfns/flashback.html).

Metadata inspection currently precedes snapshot capture; a production import
must also fence concurrent source DDL.

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
initialized CONSTANT declarations of BOOLEAN, unconstrained NUMBER, PLS_INTEGER /
BINARY_INTEGER, and VARCHAR2,
exact numeric literals and comparisons, IS [NOT] NULL,
assignment, nested DECLARE/BEGIN blocks, IF/ELSIF/ELSE, WHILE/LOOP, numeric FOR /
FOR REVERSE,
EXIT/CONTINUE [WHEN], and RETURN. Static
type checks cover even skipped short-circuit branches; IN parameters cannot
be assigned. Constants accept `:=` or `DEFAULT` initializers (including NULL),
can reference earlier declarations, and reject assignment even in unreachable
branches. A function that reaches END without RETURN raises. Routine END
names must match, and an optional SQL*Plus slash must occupy its own line.
Parameter lists follow Oracle's grammar: omit parentheses for zero parameters.
Nested blocks provide lexical scope, shadow outer names, and initialize local
variables on every entry. Self/forward references in initializers are rejected,
including references that would otherwise resolve to an outer variable.
Block labels, qualified local names, and exception handlers are not yet supported.

VARCHAR2 follows the measured Oracle 26ai reference profile: AL32UTF8, binary
comparison, and BYTE default length semantics. Local declarations accept
`VARCHAR2(n BYTE)` or `VARCHAR2(n CHAR)` with n from 1 to 32767; unconstrained
VARCHAR2 accepts up to 32767 bytes. Formal parameters and return types cannot
specify a length. CHAR length constraints still have the absolute 32767-byte
ceiling. Empty text becomes NULL. Concatenation `||` treats NULL as empty text,
returning NULL if both operands are empty. Literal-to-literal comparisons use
blank padding; comparisons involving VARCHAR2 and concatenation results do not.
Ordinary and q-quoted strings are supported; national strings, NVARCHAR2, CHAR
variables, session-dependent NLS behavior, and implicit number/text conversion
are not implemented. The new instructions preserve existing stored IR semantics.

The reproducible VARCHAR2 differential test measured 3825 cases with no
mismatches, including Unicode, control characters, quotes, NULL, comparisons,
concatenation, and length errors:

```bash
python workbench/oracle/varchar_differential.py \
  --probe /path/to/target/debug/examples/plsql_number_probe \
  --native-root /path/to/private-oracle-root \
  --output /path/to/artifacts/varchar-differential.json
```

VARCHAR2 normalization is not a bijection over arbitrary MPEdb text values:
empty text and NULL share an Oracle representation. rRETL verification still
requires an appropriate domain or retained provenance; this normalization does
not authorize an unchecked bidirectional mirror.

PLS_INTEGER and its BINARY_INTEGER alias use signed 32-bit values, round decimal
inputs half away from zero, preserve NULL, and raise on overflow. Integer
literals in range retain this type. Integer `+`, `-`, `*`, and unary minus check
32-bit overflow even when assigned to NUMBER; a decimal/exponent literal or
NUMBER operand promotes the operation to NUMBER. Division returns NUMBER.
Numeric FOR evaluates rounded bounds once, includes both endpoints, supports
REVERSE, EXIT and CONTINUE, and guards increment at the integer endpoints.
The index is read-only and scoped to the loop. It shadows outer names even
while evaluating bounds; referencing it in a bound encounters NULL and raises.
An inverted range executes zero iterations. Labels and extended iteration
controls are not yet supported.

The reproducible FOR/PLS_INTEGER differential test measured 1760 cases against
real anonymous PL/SQL blocks with no mismatches:

```bash
python workbench/oracle/for_differential.py \
  --probe /path/to/target/debug/examples/plsql_number_probe \
  --native-root /path/to/private-oracle-root \
  --output /path/to/artifacts/for-differential.json
```

Host compilation uses a dedicated 8 MiB stack so the supported nesting bound
of 128 does not depend on an embedding application's thread stack. Tests also
compile valid nested source and reject excessive nesting from a 64 KiB caller.
Runtime execution remains governed by the validated IR and instruction budgets.

NUMBER values remain exact decimals; integer arguments are normalized at NUMBER
parameter boundaries. Finite arithmetic uses twenty base-100 groups (39/40
decimal digits according to exponent parity), measured against Oracle 23.26.3.
Arithmetic uses bounded arbitrary-width integer intermediates, NULL propagation,
and proper precedence. Recurring division works: `1 / 3` retains forty fractional
digits. New PL/SQL division returns zero on underflow; already stored SQL-style
division still raises. Literal and multiplication underflow also become zero.

New versioned instructions preserve Oracle's overflow specials as MPEdb
`Numeric("Infinity")` / `Numeric("-Infinity")`, including parameter/return checks,
serialization, and shared SQL plans. They are identified in the reference by
public SQL `DUMP` bytes `ff,65` / `0`, not by TO_CHAR's ambiguous `#######` or a
driver's finite +/-1e126 sentinel. NaN is not a supported Oracle NUMBER value.
Source literals beyond the finite range fail during native compilation. Special
values converted to PLS_INTEGER or numeric FOR bounds raise overflow.

**Further arithmetic on these special values is experimental.** The initial
anonymous-block profile yielded rules that differ from PostgreSQL/IEEE, so
those operators have their own instructions. The broader function-wrapper
matrix currently has 29 mismatches in 154 cases. Results differ between measured
block/query shapes in the reference environment; this is a compatibility gap,
not a verified universal infinity arithmetic contract. Both profiles and their
counterexamples are retained. Generic MPEdb/PostgreSQL numeric operators are
unchanged. Special-value identity support does not certify arithmetic on it.

All new NUMBER/integer behavior has distinct IR opcodes. Previously stored
routines retain their original semantics and hashes; redefining source emits
new instructions. Older builds reject unknown instructions. Existing Python/
Rust semantics and stored blobs are unchanged.

The original NUMBER differential run matched Oracle SELECT results in 2016
cases. That was SQL reference evidence, not proof of matching PL/SQL boundaries.
The tool now uses real anonymous PL/SQL functions in the same context as the
native frontend and inspects DUMP to retain special-value identity:

```bash
cargo build -p mpedb-spell --example plsql_number_probe
python workbench/oracle/number_differential.py \
  --probe /path/to/target/debug/examples/plsql_number_probe \
  --native-root /path/to/private-oracle-root \
  --output /path/to/artifacts/number-report.json
# Add --runtime-specials for the extended special-value matrix.
```

The corrected 2016-case NUMBER run now has **one mismatch**, reduced from 155:
all 44 division underflows and 110 special overflow results match. The remaining
out-of-range literal is rejected at compile time by both implementations, but
native located errors do not yet carry Oracle's ORA-06550 code. The expanded
154-case special-arithmetic test still has **29 mismatches** and exits with a
failure status; those failures are not suppressed or counted as passing.
See `reference/plsql-number-gaps.json`, `reference/plsql-number-specials.json`,
and [Oracle NUMBER precision](https://docs.oracle.com/en/database/oracle/oracle-database/21/sqlrf/Data-Types.html).

Floating-point/text-to-NUMBER conversions and NLS settings remain unimplemented.
**Parameter defaults, NUMBER precision/scale constraints, SQL statements, calls to other routines, packages,
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
