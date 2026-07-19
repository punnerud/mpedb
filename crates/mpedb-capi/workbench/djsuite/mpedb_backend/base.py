"""Django's stock sqlite3 backend with the workbench's ADAPTATIONS applied.

The point of the workbench is to measure what mpedb's libsqlite3 shim can and
cannot do for Django. A gap that stops `migrate` at the very first table hides
every gap behind it, so each adaptation here removes one such wall — and each is
a RECORDED GAP in C-API-COMPAT.md, never a claim that mpedb supports the thing.

**Both arms** of the comparison (stock libsqlite3 and the mpedb shim) run with
this same backend, so the pass/fail diff isolates mpedb-specific behavior rather
than the adaptations. Anything an adaptation breaks breaks in both arms and is
therefore NOT reported as an mpedb gap.
"""

import os
import re
from functools import cached_property

from django.db.backends.sqlite3.base import DatabaseWrapper as SQLiteDatabaseWrapper
from django.db.backends.sqlite3.features import (
    DatabaseFeatures as SQLiteDatabaseFeatures,
)
from django.db.backends.sqlite3.operations import (
    DatabaseOperations as SQLiteDatabaseOperations,
)
from django.db.backends.sqlite3.schema import (
    DatabaseSchemaEditor as SQLiteDatabaseSchemaEditor,
)

# mpedb's 48 reserved words (crates/mpedb-sql/src/token.rs `keyword()`), the set
# that must stay quoted when GAP 4's workaround strips quotes.
MPEDB_KEYWORDS = frozenset(
    """and as asc begin between by case commit conflict delete desc distinct do
    else end explain false from glob group having in inner insert into is join
    like limit match not nothing null offset on or order regexp returning
    rollback select set then true update values when where""".split()
)

_PLAIN_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")


class DatabaseOperations(SQLiteDatabaseOperations):
    def quote_name(self, name):
        # GAP 4 (mpedb-sql, the single biggest one): mpedb's parser accepts a
        # dotted reference only when the QUALIFIER is a bare identifier —
        # `t.id` parses, `"t"."id"` is `SQL parse error: unexpected trailing
        # input Dot`. Django quotes every name, so *every* ORM query fails.
        # Workaround: emit plain identifiers unquoted (keywords and anything
        # needing quoting keep their quotes, so this changes no meaning).
        if name.startswith('"') and name.endswith('"'):
            name = name[1:-1]
        if _PLAIN_IDENT.match(name) and name.lower() not in MPEDB_KEYWORDS:
            return name
        return '"%s"' % name

    @cached_property
    def _references_graph(self):
        # GAP 6 (shim `introspect.rs`): `sql_flush(allow_cascade=True)` walks the
        # FK graph with a RECURSIVE CTE that JOINs `sqlite_master` against itself
        # through a `sql REGEXP …` predicate. The shim answers `sqlite_master`
        # with a hand-rolled mini-evaluator (projection + AND-joined
        # comparisons), which cannot express that, so it refuses — and every
        # `TransactionTestCase` teardown then leaves rows behind, cascading into
        # dozens of unrelated assertion failures.
        #
        # The workbench already runs with `supports_foreign_keys = False`, so no
        # FK constraint exists to cascade through and the graph of a table is
        # just the table. Applied to BOTH arms.
        return lambda table_name: [table_name]


class DatabaseSchemaEditor(SQLiteDatabaseSchemaEditor):
    # GAP 3, second half: the inline-FK templates are class attributes on the
    # schema editor and are used regardless of `supports_foreign_keys`; None
    # makes Django fall through to the (now disabled) feature check.
    sql_create_inline_fk = None
    sql_create_column_inline_fk = None

    # GAP 5 (mpedb-sql): a table constraint may not be NAMED — mpedb's CREATE
    # TABLE takes a bare `UNIQUE (a, b)`, not `CONSTRAINT x UNIQUE (a, b)`.
    # Dropping the name keeps the constraint itself.
    sql_constraint = "%(constraint)s"

    def _iter_column_sql(self, *args, **kwargs):
        # GAP 3, third half: a model field's `db_default` emits a
        # `DEFAULT <literal>` column clause, which CREATE TABLE also refuses.
        # sqlite has `requires_literal_defaults = True`, so the clause carries no
        # bound parameters and dropping the fragment leaves `params` consistent.
        for part in super()._iter_column_sql(*args, **kwargs):
            if isinstance(part, str) and part.upper().startswith("DEFAULT "):
                continue
            yield part


class DatabaseFeatures(SQLiteDatabaseFeatures):
    # GAP 3 (mpedb-sql): `REFERENCES` / `CHECK` / `DEFAULT` inside CREATE TABLE
    # are a named refusal ("declare them in the config schema"), and Django
    # writes an inline FK for every ForeignKey. Turning the features off makes
    # Django emit plain integer columns and skip its own
    # `@skipUnlessDBFeature("supports_foreign_keys")` tests — in BOTH arms.
    supports_foreign_keys = False
    can_create_inline_fk = False
    supports_column_check_constraints = False
    supports_table_check_constraints = False


class DatabaseWrapper(SQLiteDatabaseWrapper):
    features_class = DatabaseFeatures
    ops_class = DatabaseOperations
    SchemaEditorClass = DatabaseSchemaEditor

    # GAP 1 (mpedb-sql): `AUTOINCREMENT` is refused — mpedb's INTEGER PRIMARY KEY
    # auto-assigns max+1 but REUSES ids after a delete, which is not what the
    # keyword promises, so the parser refuses rather than silently weaken it.
    # Django appends the suffix to every AutoField pk, i.e. to essentially every
    # model, so unpatched not one table is created.
    data_types_suffix = {}

    # GAP 3, fourth half: Django's sqlite backend gives every Positive*Field a
    # `CHECK ("col" >= 0)` column constraint, refused by the same parser rule.
    data_type_check_constraints = {}

    # GAP 2 (mpedb-sql): mpedb has a RIGID type vocabulary
    # (`int64`/`int`/`integer`, `text`, `real`, `bool`, `blob`, `timestamp`,
    # `any`) and no sqlite type affinity, so a parameterized or free-form sqlite
    # type name — `varchar(100)`, `char(32)`, `decimal`, `datetime`, `bigint`,
    # `integer unsigned` — is a parse error. Django's `data_types` is written in
    # exactly that vocabulary.
    #
    # The mapping below picks, per Django field, the mpedb type matching what
    # sqlite actually STORES: text for the string/date/time spellings (sqlite
    # keeps ISO strings), integer for the int family, real for floats, blob for
    # binary. Two deliberate choices:
    #   * BooleanField -> integer, not bool: CPython's sqlite3 binds `True` as
    #     the integer 1, and mpedb's `bool` column REFUSES an int64 ("value of
    #     type int64 cannot be inserted into column of type bool"). Django's own
    #     `convert_booleanfield_value` turns the integer back into a bool.
    #   * DecimalField -> any: Django hands sqlite a `str` on write and expects a
    #     number-ish value back; `any` is mpedb's dynamic-typing escape hatch and
    #     the only honest home for a column sqlite gives NUMERIC affinity.
    data_types = {
        "AutoField": "integer",
        "BigAutoField": "integer",
        "BinaryField": "blob",
        "BooleanField": "integer",
        "CharField": "text",
        "DateField": "text",
        "DateTimeField": "text",
        "DecimalField": "any",
        "DurationField": "integer",
        "FileField": "text",
        "FilePathField": "text",
        "FloatField": "real",
        "IntegerField": "integer",
        "BigIntegerField": "integer",
        "IPAddressField": "text",
        "GenericIPAddressField": "text",
        "JSONField": "text",
        "OneToOneField": "integer",
        "PositiveBigIntegerField": "integer",
        "PositiveIntegerField": "integer",
        "PositiveSmallIntegerField": "integer",
        "SlugField": "text",
        "SmallAutoField": "integer",
        "SmallIntegerField": "integer",
        "TextField": "text",
        "TimeField": "text",
        "UUIDField": "text",
    }


# --- workbench debugging aid -------------------------------------------------
# With WB_TRACE_SQL_ERRORS=1, print the exact SQL (and params) of any statement
# the driver rejects. Reducing a Django failure to a minimal repro is the whole
# job here, and Django otherwise reports only the driver's message.
if os.environ.get("WB_TRACE_SQL_ERRORS"):
    from django.db.backends.utils import CursorWrapper

    _orig_execute = CursorWrapper._execute

    def _execute(self, sql, params, *args, **kwargs):
        try:
            return _orig_execute(self, sql, params, *args, **kwargs)
        except Exception as exc:
            print(
                f"\n[WB-SQL-ERROR] {type(exc).__name__}: {exc}"
                f"\n  SQL: {sql!r}\n  PARAMS: {params!r}",
                flush=True,
            )
            raise

    CursorWrapper._execute = _execute
