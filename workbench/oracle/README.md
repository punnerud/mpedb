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

The optional `oracle-source` feature now exposes `import_oracle`. It initializes
and validates its own authority-state row, excludes that verified control table,
reads the selected tables at one bootstrap SCN, and imports carriers into a new
MPEdb file. Source identity, full original/logical DDL, column metadata and the
initial cursor persist in checksummed 64 KiB JSON chunks (64 MiB metadata limit).
The start cursor, mirror config, provenance and CDC flags publish in ONE final
commit; failed bootstrap reads publish none of them. Import streams rows at the
same SCN into a private staging directory on the destination filesystem. Batches
are bounded by both `batch_rows` and 8 MiB of encoded rows (one larger row may
exceed that byte budget). After closing the completed database, an atomic
no-replace hard link publishes the destination; failures clean up the owned
stage and never overwrite a destination created concurrently. A durable random
file incarnation survives reopen/move and changes on every fresh import, even
when the destination path is reused. This distinguishes outbound generations
across independent imports.
`load_oracle_bootstrap` verifies metadata and the pinned carrier-table scope
without contacting Oracle. Extra rRETL lineage/maps and local tables are allowed;
the original scope ids and full carrier definitions must still match. Native
tests apply/revert an Oracle PL/SQL Boolean lens, cold-reopen the adapter after
lineage creation, and still reject an added column on a mirrored table. Native tests cover cold reopen, multiple metadata chunks, data values,
existing-destination rejection, altered content and truncated chunks. A native
6000-row fixture exceeds the byte budget, includes an empty table, and verifies
that capacity failure leaves no published destination or staging directory.
Individual row/LOB carrier values are still materialized in memory; the import
no longer retains the entire snapshot. CLI import and full live adapter wiring
remain unfinished. SQL execution uses MPEdb's strict grouping mode, which does
not imply full Oracle SQL-dialect compatibility.

A native component test now runs both real engines: Oracle import → MPEdb
PL/SQL Boolean lens/rRETL → PostgreSQL, then PostgreSQL changes → a second
MPEdb mirror/rRETL → receipted Oracle writes → Oracle CDC into the first mirror.
It checks compound-key moves, deletes, inserts, exact decimals, Unicode CLOB
carriers and a subsequent Oracle update sent forward again. Every representation
change executes in MPEdb; the adapters only transport carriers. This isolated
single-writer fixture does not implement a continuous bidirectional controller.

This transport is not a complete mirror push: capture, echo suppression,
conflict checks, authority fencing, CLI import and the two-database topology
remain unfinished. Writes currently refuse identity columns, special NUMBERs and LONGs. CLOB, NCLOB and BLOB writes use native
temporary LOB locators; reads use explicit locators and fixed Unicode-safe
buffers. Native tests cover large Unicode and binary values, empty LOBs distinct
from NULL, and repeated upserts. Scalar text/RAW binds remain bounded at 4000/2000
bytes. Snapshot results still materialize complete carrier values in memory.
Zoned TIMESTAMP and INTERVAL YEAR/DAY carriers now write back with verified
postimages. Zoned writes reconstruct the instant from the explicit offset and
restore the region, then compare the full region/offset/TZD carrier. LOCAL TIME
ZONE reads explicitly cast to a zoned timestamp in the pinned UTC session;
formatting TZR/TZD directly on that Oracle type previously failed with ORA-01821.
OCI national-character binds need an explicit database-character-set cast for
the AT TIME ZONE region. Native tests cover both occurrences of Oslo's repeated
DST hour, fractional seconds, a fractional-hour fixed offset, LOCAL TIME ZONE,
NULL, large positive/negative intervals, narrower interval precision, repeated
upserts, and whole-batch rollback on truncation or inconsistent offset/TZD.
Historical regions with sub-minute offsets are not proven lossless by this
minute-offset carrier and remain subject to exact postimage rejection.
Schema fences and cursor scopes use logical DDL extracted with the documented
DBMS_METADATA SEGMENT_ATTRIBUTES transform disabled. Original physical DDL is
retained separately as provenance. Native tests verify that deferred segment
allocation leaves the fence and cursor valid, while column and CHECK-constraint
changes invalidate stale metadata. Cursor realm hashing uses its v2 domain;
previous development cursors require a fresh bootstrap.
The native source boundary also captures bounded Flashback Versions windows.
It discovers identities from both version start and end SCNs, then reads their
net images at each commit SCN across all selected tables. Tests cover an
uncommitted transaction, later commit, rollback, historical images, compound-PK
changes, deletes and resume/budget rejection. XIDs retain transaction provenance.
Expired undo fails without advancing a cursor. `OracleAdapter` now implements
the SourceAdapter pull boundary over persisted Oracle bootstrap metadata. It
checks the source incarnation, emits whole commit-SCN batches, reads authority
at the same SCN, and resumes from the atomically persisted MPEdb cursor after
reopen. Raw-event overflow bisects the query window; a single commit exceeding
the caller's limits is refused without truncation. SCNs use named SQL binds
for statement reuse. Native tests cover compound-PK changes, multi-table commits,
first inserts into empty tables, stale cursors, epoch mismatches, cold resume and
DDL fences. The shared apply path now enforces source epochs when provided;
legacy adapters that supply None retain their existing behavior.
Oracle mirror push and MPEdb authority cutover remain explicitly refused until
the durable own-write ledger, postimage handling and checked push are connected.

`OracleSource::prepare_write_ledger` and `write_batch_receipted` now provide the
low-level durable write primitive. Application writes and a BLAKE3-checked BLOB
of their actual MPEdb-carrier postimages commit in ONE Oracle transaction. A
committed token replay returns those original images without executing DML,
even after reconnect and intervening foreign changes. Changed payloads, stale
application/ledger DDL, authority changes and corrupt receipts are refused.
Requests serialize on the authority row; failed whole batches leave neither
partial data changes nor a receipt. Native tests cover concurrent retries,
generated postimages, large Unicode/BLOB images, forged frame lengths, business
name collisions, tracking-table exclusion during default import and ledger
recreation. The import verifies tracking ownership before excluding receipts,
and holds its DDL fence while taking the application snapshot.
`recover_write_receipt` separately resolves an uncertain earlier commit without
executing DML, even when the original epoch/authority has changed. It serializes
with writes/cutover on the authority row and still validates the database realm,
control-row incarnation, ledger, application schema, payload and image hashes.
A missing receipt does not authorize retry or local acknowledgement. Native
tests cover old-epoch recovery, role changes, absent/corrupt/mismatched receipts,
and recreation/DDL fences. Durable local outbox/controller recovery remains to
be connected to this primitive.
The shared push protocol now supplies `PushContext` from the SAME local
snapshot as rows, dirty generations, cursor and authority. `PushReply` can return
actual postimages. Matching generations update those rows and clear dirty/park
records atomically; newer local updates/deletes are left untouched. Authority,
configuration, mirrored-table DDL and untracked row changes abort acknowledgement.
New unrelated tables are allowed. Rejections only park the generation actually
sent, and later accepted generations clear stale rejection records. Legacy
adapters retain their default bool-result behavior.
Deterministic protocol tests cover races, malformed images, NaN snapshots and
rollback of the entire local ACK when a later row fails. An isolated Oracle
transport fixture verifies a real generated column through receipted DML and
this common ACK bridge; it deliberately has no foreign writer and does not
implement production conflict or echo policy.

`write_batch_receipted_checked` adds a conservative first-attempt conflict
check against a realm-validated Oracle cursor. It takes SHARE ROW EXCLUSIVE
NOWAIT locks on touched tables before reading the change window, closing both
existing-row and absent-key races with foreign DML. Any changed target rejects
the entire new batch without data/receipt changes; unrelated keys are allowed.
Active foreign writers, expired history and event-budget failures stop safely.
A committed retry still returns its original receipt without DML. Own prior
writes count as changes too, so callers must pull/reconcile before a new request.
This coarse table fence is a transport correctness baseline, not fine-grained
continuous mirroring. Native tests cover foreign commits, compound-key moves,
whole-batch rejection, busy writers, budgets, cursor realms and old-epoch
receipt recovery. Production outbox/echo/controller wiring remains unfinished.

Tokens must include a durable local FILE incarnation and dirty generation;
a path/transaction number or payload hash alone can collide after reimport.
Receipt images are bounded to 64 MiB per batch. This primitive never garbage
collects receipts. Wiring durable prepared requests, per-row rejection, guarded postimages,
conflict checks and key-level echo suppression into the production Oracle push remains
unfinished; adapter write-back stays refused.

The large-value import test also found and fixed a shared storage bug:
`row::encoded_len` omitted NUMERIC's variable-length body. This caused large
NUMBER/LOB rows to fail extent insertion. A source-independent MPEdb regression
covers rigid and ANY numerics, large Unicode and binary values, verification and
cold reopen with internal extents, external extents and overflow storage.

An importer must establish its initial cursor only after a complete successful
snapshot. On this native Oracle version, an immediate post-DDL SCN can map to a
timestamp preceding that DDL and fail with ORA-01466. Tests retry only that
initial bootstrap; established capture never skips ahead on such an error.
See [Oracle Flashback documentation](https://docs.oracle.com/en/database/oracle/oracle-database/26/adfns/flashback.html).

Metadata inspection precedes snapshot capture; the snapshot fences each table
and verifies the inspected logical DDL and object identity before reading rows.

## Oracle bootstrap and authority foundations

`OracleSource::snapshot` fences table DDL and reads the complete selected table
set at one SCN, checking database identity before and after the read. The mirror
variant also reads its initialized authority row at that same SCN. Snapshot
failure releases the fences and publishes no cursor. Table ids follow MPEdb's
canonical schema order even when the caller supplies reordered Oracle metadata;
native tests verify capture and write-back with reversed metadata order.

Version-1 Oracle positions bind the SCN to CDB DBID/resetlogs, container GUID,
PDB incarnation, application owner, session account and original schema scope.
The fixed 41-byte codec rejects unknown versions, truncation, appended bytes,
zero SCNs and another resume domain. Password changes do not change identity;
connection secrets are never part of the record. Identity reads require:

```sql
GRANT SELECT ON SYS.V_$DATABASE TO application_user;
GRANT SELECT ON SYS.V_$PDB_INCARNATION TO application_user;
```

The source authority table has a validated shape, version marker and primary
key. Mirror ids use RAW so session collation cannot merge different identities.
Initialization is idempotent; CAS advances exactly one epoch, verifies its
actual result and commits atomically. Native tests cover competing connections
(one winner), stale CAS, old-SCN reads and refusing a disabled ownership marker.
Durable Oracle import and the pull adapter use these foundations. Complete
write-back, own-write echo suppression and the two-database controller remain
unfinished. Bootstrap metadata version 3 binds the control table's
OBJECT_ID and DATA_OBJECT_ID plus the authority row's SYS_GUID incarnation. Adapter reconnect, capture and authority CAS
refuse a recreated or truncated control table even when its row was reseeded
with the same epoch. CAS locks the authority row, compares its table generation and row token,
updates the epoch, then verifies the token again before commit. Established
adapters never recreate missing authority rows. A dedicated test owner verifies
TRUNCATE, DROP/CREATE and DELETE/reinsert with the same id and epoch, without
touching shared tracking state. Control-table format V2 adds INCARNATION RAW(16)
DEFAULT SYS_GUID() NOT NULL; new API inserts explicitly generate a fresh token.
Existing V1 tables are refused rather than automatically altered, and older
development bootstrap files require reimport. The dedicated native test table
was explicitly migrated with existing epochs preserved and a private backup.

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
initialized CONSTANT declarations of BOOLEAN, NUMBER, PLS_INTEGER /
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
Local `NUMBER(p,s)` and `NUMBER(p)` variables/constants now round on assignment
using exact decimal arithmetic, then enforce precision. Precision is 1..38 and
scale -84..127, including negative scale and scale greater than precision.
Intermediate expressions remain unconstrained. Overflow raises VALUE_ERROR in
handlers; formal parameter/return constraints and invalid bounds are rejected.
A new additive IR opcode (90) validates its bounds and stack on decode. Unit
checks cover ties, overflow after rounding, NULL, constants and arithmetic;
128 differential calls match the same application functions on native Oracle.
See Oracle's [NUMBER type rules](https://docs.oracle.com/en/database/oracle/oracle-database/26/sqlrf/Data-Types.html).

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
Literal `EXECUTE IMMEDIATE` DML now compiles to MPEdb's shared SQL plans:
INSERT, UPDATE and DELETE with input `USING` expressions. Repeated placeholder
names bind by occurrence, q-quoted literals retain their values, and empty SQL
strings become NULL. A real Oracle application procedure is extracted from
`ALL_SOURCE` and executes unchanged inside MPEdb with exact NUMBER binds.
Procedure errors roll back already-executed DML; stored procedures and their
plans survive reopen. Scalar SQL functions continue to reject database work.

Literal dynamic SELECT now supports typed `INTO` targets too. The linker
checks query column count, and additive IR opcode 82 performs exact fetch.
No rows raises structured ORA-01403; multiple rows raises ORA-01422. Native
Oracle application-source tests verify both errors and exact NUMBER results;
transaction tests verify read-your-writes and rollback on a later fetch error.
The existing Python/Rust opcodes and stored hashes keep their meanings.

This is a limited native dynamic-SQL frontend, not complete dynamic SQL.
Runtime-generated SQL strings, RETURNING/OUT binds, implicit SQL
cursor attributes, DDL and anonymous dynamic blocks remain unsupported. The
embedded statement also must be supported by MPEdb's SQL engine. See
[Oracle's binding rules](https://docs.oracle.com/en/database/oracle/oracle-database/26/lnpls/native-dynamic-sql.html).

Native exception blocks now support `NO_DATA_FOUND`, `TOO_MANY_ROWS`,
`ZERO_DIVIDE`, `VALUE_ERROR`, `DUP_VAL_ON_INDEX`, OR alternatives, `WHEN OTHERS`,
named `RAISE` and re-raise. Additive opcodes 83–86 preserve existing IR meanings.
Protected blocks require an empty evaluation stack; scope ranges unwind on
EXIT/CONTINUE and nested handlers propagate correctly. Declaration initializer
errors bypass their own block's handlers. Engine budgets, I/O, corruption and
unsupported operations cannot be hidden by `WHEN OTHERS`.

For procedures with handlers, SQL statements execute under complete engine
savepoints. A caught multi-row INSERT failure removes every inserted row while
preserving preceding successful DML. Native Oracle tests compare that behavior,
NO_DATA_FOUND fallback, and an exception-bearing Boolean function transported
through rRETL apply/revert and source write-back. Native `SQLCODE`, no-argument `SQLERRM` and
`RAISE_APPLICATION_ERROR(code, message[, FALSE])` are also implemented with
additive opcodes 87–89. Native reference tests cover +100 for NO_DATA_FOUND,
negative codes for other errors, restoration of enclosing handler context,
BINARY_INTEGER rounding of application codes, NULL/invalid-code ORA-21000,
and UTF-8-safe SQLERRM truncation to 512 bytes. The measured native 23.26.3
application-error envelope retains 2034 message bytes; uncaught-message parity
is tested too.

Full exception coverage still needs user-defined declarations/EXCEPTION_INIT,
TRUE error-stack preservation, SQLERRM(error_code), localized/detailed messages
and complete Oracle classification of database errors. See
[Oracle error reporting](https://docs.oracle.com/en/database/oracle/oracle-database/26/lnpls/retrieving-error-code-and-error-message.html)
and [raising exceptions](https://docs.oracle.com/en/database/oracle/oracle-database/26/lnpls/raising-exceptions-explicitly.html).

Static INSERT/UPDATE/DELETE and SELECT INTO now lower into the same MPEdb
plans and exact-fetch/exception bridge as literal dynamic SQL. Input variables
must use `routine.variable` qualification; that binding refers to the routine's
scope even inside a nested block with a shadowing local. SQL numeric literals
become exact NUMBER bindings, never int64/f64 approximations. Native tests use
identical application source for both engines and cover nested shadowing,
large exact numbers, empty-string NULLs, DML, duplicate handling, fetch errors,
INTO precision checks and cold reopen. Bare PL/SQL names are refused until
schema-aware column-priority resolution is available; qualify SQL columns with
a table alias when names overlap. Numeric literals with ORDER/GROUP BY,
qualified/record INTO targets, DML RETURNING and full Oracle SQL syntax remain
gaps. See Oracle's [column precedence rules](https://docs.oracle.com/en/database/oracle/oracle-database/26/lnpls/column-name-precedence.html).

**Parameter defaults, complete static SQL name resolution, calls to other
routines, packages and cursors remain unimplemented**;
measured arithmetic agreement is not full PL/SQL compatibility.

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
