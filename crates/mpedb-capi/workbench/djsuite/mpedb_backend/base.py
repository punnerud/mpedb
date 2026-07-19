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

Run 3 (2026-07-19) dropped `supports_foreign_keys = False` (D4b) as well: with
it ablated the run is bit-identical, so it was buying nothing. It ADDED D9 —
mpedb refuses an index on a NUMERIC-affinity (`any`) column, and Django indexes
DateTimeFields freely, so migrate died at `django_session.expire_date`.

Every remaining adaptation carries its MEASURED cost, from the ablation
switches below. An adaptation that changes nothing is deleted, not kept.

What is left is only what mpedb still does not do.
"""

import os

from django.db.backends.sqlite3.base import DatabaseWrapper as SQLiteDatabaseWrapper
from django.db.backends.sqlite3.operations import (
    DatabaseOperations as SQLiteDatabaseOperations,
)
from django.db.backends.sqlite3.schema import (
    DatabaseSchemaEditor as SQLiteDatabaseSchemaEditor,
)


# --- adaptation ABLATION switches -------------------------------------------
# "A number measured through a workaround is not the number": each remaining
# adaptation can be turned OFF from the environment so its cost can be MEASURED
# rather than assumed. Set to 1 to run WITHOUT that adaptation (both arms).
#
#   WB_NO_D2=1   keep Django's AUTOINCREMENT suffix
#   WB_NO_D8=1   use Django's own recursive-CTE _references_graph
#   WB_NO_D9=1   index NUMERIC-affinity columns as Django asks
NO_D2 = os.environ.get("WB_NO_D2") == "1"
NO_D8 = os.environ.get("WB_NO_D8") == "1"
NO_D9 = os.environ.get("WB_NO_D9") == "1"


def _sqlite_affinity(decl):
    """sqlite's declared-type → affinity rule (the five ordered clauses)."""
    d = (decl or "").upper()
    if "INT" in d:
        return "INTEGER"
    if "CHAR" in d or "CLOB" in d or "TEXT" in d:
        return "TEXT"
    if "BLOB" in d or not d:
        return "BLOB"
    if "REAL" in d or "FLOA" in d or "DOUB" in d:
        return "REAL"
    return "NUMERIC"


def _is_numeric_affinity(field, connection):
    try:
        return _sqlite_affinity(field.db_type(connection)) == "NUMERIC"
    except Exception:  # noqa: BLE001 — a field with no column (m2m, reverse)
        return False


class DatabaseSchemaEditor(SQLiteDatabaseSchemaEditor):
    # GAP D9 (mpedb-types `schema.rs`): mpedb REFUSES an index — unique or not —
    # on a NUMERIC-affinity column, i.e. `ColumnType::Any`:
    #
    #   CREATE TABLE s (id integer NOT NULL PRIMARY KEY, expire_date datetime)
    #   CREATE INDEX ix ON s (expire_date)
    #     -> schema error: index column `s.expire_date` cannot be `any`: the
    #        index is memcmp-ordered and `any` has no order across types
    #
    # The refusal is PRINCIPLED, not an oversight: index keys are memcmp-ordered
    # and `any` has no order across storage classes, so an IndexRange over one
    # returns wrong rows (and DELETE/UPDATE through it deletes them). It is the
    # cost of the D3 closure — `date`/`datetime`/`time`/`decimal`/`uuid`/`json`
    # all became `any` when Django's own declared types were let through.
    #
    # Django puts `db_index=True` on DateTimeFields freely (`django_session.
    # expire_date` is the first one migrate reaches), so unpatched NOT ONE label
    # gets a database. The adaptation drops exactly those indexes, in BOTH arms.
    def _field_indexes_sql(self, model, field):
        if not NO_D9 and _is_numeric_affinity(field, self.connection):
            return []
        return super()._field_indexes_sql(model, field)

    def _create_index_sql(self, model, *, fields=None, **kwargs):
        if not NO_D9 and fields and any(
            _is_numeric_affinity(f, self.connection) for f in fields
        ):
            return None
        return super()._create_index_sql(model, fields=fields, **kwargs)

    def _model_indexes_sql(self, model):
        return [s for s in super()._model_indexes_sql(model) if s is not None]

    def execute(self, sql, params=()):
        if sql is None:  # a dropped index reached deferred_sql
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
        # mpedb parses `REFERENCES` and drops it — it enforces no foreign key,
        # which is sqlite's own `PRAGMA foreign_keys = OFF` default — so there
        # is no ENFORCED constraint to cascade through and the graph of a table
        # is just the table. Applied to BOTH arms.
        #
        # Measured cost (run 3): without it, G1's shim-only failures go 42 → 74
        # tests (66 F + 123 E vs 0 F + 42 E), so this one is still load-bearing.
        if NO_D8:
            return super()._references_graph
        return lambda table_name: [table_name]


class DatabaseWrapper(SQLiteDatabaseWrapper):
    ops_class = DatabaseOperations
    SchemaEditorClass = DatabaseSchemaEditor

    # GAP D2 (mpedb-sql): `AUTOINCREMENT` is refused BY NAME — mpedb's INTEGER
    # PRIMARY KEY auto-assigns max+1 but REUSES ids after a delete, and the
    # keyword's one added guarantee (never reuse) needs a persisted, crash-safe
    # sequence, so the parser refuses rather than silently weaken it. Django
    # appends the suffix to every AutoField pk, i.e. to essentially every model,
    # so unpatched not one table is created.
    if not NO_D2:
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
