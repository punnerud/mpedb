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

Run 4 (2026-07-19, after the typeless-key merge) DELETED two more:

  * The D9 index dropper (and its `WB_SOFT_CREATE_INDEX` lever): an `any`
    column may now be a PRIMARY KEY / index key, so `CREATE INDEX` on
    `datetime`/`decimal` columns succeeds and Django's own indexes are created.
  * `supports_foreign_keys = False` (D4b): run 3's ablation measured it as
    bit-identical in both arms (A labels and `backends`); run 3 recorded it as
    deleted but the `backends`-triage merge reintroduced it by accident.

What is left is only what mpedb still does not do. The two survivors have
ablation switches (`WB_NO_D2`, `WB_NO_D8`) so their cost can be re-measured.
"""

import os

from django.db.backends.sqlite3.base import DatabaseWrapper as SQLiteDatabaseWrapper
from django.db.backends.sqlite3.operations import (
    DatabaseOperations as SQLiteDatabaseOperations,
)


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
        # mpedb enforces no FK constraint (`REFERENCES` is parsed and
        # discarded, sqlite's own `foreign_keys=OFF` behaviour), so no ENFORCED
        # cascade exists and the graph of a table is just the table. Applied to
        # BOTH arms. Ablation: `WB_NO_D8=1`.
        if os.environ.get("WB_NO_D8"):
            return super()._references_graph
        return lambda table_name: [table_name]


class DatabaseWrapper(SQLiteDatabaseWrapper):
    ops_class = DatabaseOperations

    # GAP D2 (mpedb-sql): `AUTOINCREMENT` is refused BY NAME — mpedb's INTEGER
    # PRIMARY KEY auto-assigns max+1 but REUSES ids after a delete, and the
    # keyword's one added guarantee (never reuse) needs a persisted, crash-safe
    # sequence, so the parser refuses rather than silently weaken it. Django
    # appends the suffix to every AutoField pk, i.e. to essentially every model,
    # so unpatched not one table is created. Ablation: `WB_NO_D2=1`.
    if not os.environ.get("WB_NO_D2"):
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
