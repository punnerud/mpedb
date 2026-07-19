"""Django's stock sqlite3 backend with the workbench's ADAPTATIONS applied.

The point of the workbench is to measure what mpedb's libsqlite3 shim can and
cannot do for Django. A gap that stops `migrate` at the very first table hides
every gap behind it, so each adaptation here removes one such wall — and each is
a RECORDED GAP in C-API-COMPAT.md, never a claim that mpedb supports the thing.

**Both arms** of the comparison (stock libsqlite3 and the mpedb shim) run with
this same backend, so the pass/fail diff isolates mpedb-specific behavior rather
than the adaptations. Anything an adaptation breaks breaks in both arms and is
therefore NOT reported as an mpedb gap.

Run 2 (2026-07-19, after the CREATE-TABLE-surface work) DELETED four of the six
adaptations, because the gaps behind them closed:

  * `quote_name()` no longer strips quotes — `"t"."id"` parses (gap D1).
  * `data_types` is Django's own again — `varchar(100)`, `bigint`, `datetime`,
    `decimal(10,2)`, `integer unsigned` are taken via the affinity rule (D3).
  * `DEFAULT` / `CHECK` / inline `REFERENCES` column clauses are emitted as
    Django writes them (D4); DEFAULT and CHECK are enforced, REFERENCES is
    parsed and dropped, which is sqlite's own `foreign_keys=OFF` behaviour.
  * `CONSTRAINT n UNIQUE (…)` keeps its name (D5).

What is left is only what mpedb still does not do.
"""

import os
import re

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


# GAP D9 (mpedb-types `schema.rs`): an index key is memcmp-ordered, and
# `ColumnType::Any` has no order ACROSS storage classes, so `Schema::validate`
# refuses an index whose column is `any`. sqlite's NUMERIC affinity maps to
# `any` (it is the one affinity that is not a single storage class), which makes
# every `datetime`/`date`/`time`/`decimal` column unindexable — and Django's
# `Session.expire_date` is a `DateTimeField(db_index=True)`, so `CREATE INDEX`
# fails during `migrate` and NOT ONE test runs. Recorded, not fixed: it is an
# engine decision, and lifting it is issue "`any` columns indexable".
#
# The adaptation drops exactly those `CREATE INDEX` statements, in BOTH arms.
# An index is a pure performance feature — no answer depends on one — so the
# only tests this can perturb are introspection tests, which then fail in both
# arms and are excluded from the shim-only diff by construction.
_NUMERIC_AFFINITY = re.compile(r"^(date|datetime|time|decimal|numeric)\b", re.I)


def _is_any_typed(field, connection):
    try:
        t = field.db_parameters(connection)["type"]
    except Exception:  # pragma: no cover - defensive
        return False
    if not t:
        return False
    # mpedb's own names win over the affinity rule, so `bool`/`timestamp` are
    # real rigid types and stay indexable; only sqlite's NUMERIC bucket becomes
    # `any`.
    return bool(_NUMERIC_AFFINITY.match(t.strip()))


class DatabaseSchemaEditor(SQLiteDatabaseSchemaEditor):
    def _create_index_sql(self, model, *, fields=None, **kwargs):
        if fields and any(_is_any_typed(f, self.connection) for f in fields):
            return ""
        return super()._create_index_sql(model, fields=fields, **kwargs)

    def execute(self, sql, params=()):
        if not str(sql).strip():
            return
        return super().execute(sql, params)


class DatabaseOperations(SQLiteDatabaseOperations):
    @property
    def _references_graph(self):
        # GAP D8 (shim `introspect.rs`): `sql_flush(allow_cascade=True)` walks the
        # FK graph with a RECURSIVE CTE that JOINs `sqlite_master` against itself
        # through a `sql REGEXP …` predicate. The shim answers `sqlite_master`
        # with a hand-rolled mini-evaluator (projection + AND-joined
        # comparisons), which cannot express that, so it refuses — and every
        # `TransactionTestCase` teardown then leaves rows behind, cascading into
        # dozens of unrelated assertion failures.
        #
        # The workbench runs with `supports_foreign_keys = False`, so no
        # ENFORCED FK constraint exists to cascade through and the graph of a
        # table is just the table. Applied to BOTH arms.
        return lambda table_name: [table_name]


class DatabaseFeatures(SQLiteDatabaseFeatures):
    # GAP D4b (mpedb-sql): `REFERENCES` is now PARSED, but the constraint is
    # discarded — mpedb enforces no foreign key. That is sqlite's own default
    # (`PRAGMA foreign_keys = OFF`) but not what Django's sqlite backend asks
    # for. Telling Django there is no FK support keeps it from asserting
    # enforcement it would not get, and skips its own
    # `@skipUnlessDBFeature("supports_foreign_keys")` tests — in BOTH arms.
    # The inline `REFERENCES …` column clause IS still emitted (the schema
    # editor's `sql_create_inline_fk` is untouched), so the new parser surface
    # is exercised on every ForeignKey.
    supports_foreign_keys = False


class DatabaseWrapper(SQLiteDatabaseWrapper):
    features_class = DatabaseFeatures
    ops_class = DatabaseOperations
    SchemaEditorClass = DatabaseSchemaEditor

    # GAP D2 (mpedb-sql): `AUTOINCREMENT` is refused BY NAME — mpedb's INTEGER
    # PRIMARY KEY auto-assigns max+1 but REUSES ids after a delete, and the
    # keyword's one added guarantee (never reuse) needs a persisted, crash-safe
    # sequence, so the parser refuses rather than silently weaken it. Django
    # appends the suffix to every AutoField pk, i.e. to essentially every model,
    # so unpatched not one table is created.
    data_types_suffix = {}


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


# --- workbench measurement aid ------------------------------------------------
# With WB_SOFT_CREATE_INDEX=1, a failing `CREATE INDEX` during `migrate` is
# logged and SWALLOWED instead of taking down the whole label.
#
# Why this exists: an index is a performance structure, never an answer, so a
# missing one changes no query RESULT — but a refused `CREATE INDEX` aborts
# `create_test_db()` and hides every gap behind it. mpedb currently refuses to
# index an `any` column ("the index is memcmp-ordered and `any` has no order
# across types"), and Django's `datetime`/`decimal` columns take NUMERIC
# affinity → `any`, so `django_session.expire_date` alone blocks BOTH label
# groups outright. Off by default: this is a measurement lever for isolating a
# DOWNSTREAM gap, not an adaptation the reported numbers may rest on. Anything
# measured with it on must say so.
if os.environ.get("WB_SOFT_CREATE_INDEX"):
    from django.db.backends.base.schema import BaseDatabaseSchemaEditor

    _orig_se_execute = BaseDatabaseSchemaEditor.execute

    def _se_execute(self, sql, params=()):
        try:
            return _orig_se_execute(self, sql, params)
        except Exception as exc:
            if "CREATE INDEX" in str(sql).upper():
                print(f"\n[WB-SOFT-INDEX] swallowed: {exc}\n  SQL: {sql}", flush=True)
                return None
            raise

    BaseDatabaseSchemaEditor.execute = _se_execute
