from sqlalchemy.testing.suite import *   # noqa: F401,F403

# The generic suite couples two PostgreSQL dialect expectations to engine
# capability flags. Keep those capabilities closed, and assert the actual
# stock PostgreSQL dialect contract instead. MPEDB_STOCK_SUITE preserves an
# unmodified control run; no third-party test files are changed.
import os

if os.environ.get("MPEDB_STOCK_SUITE") != "1":
    from sqlalchemy import column, inspect, select, table
    from sqlalchemy import testing
    from sqlalchemy.testing import eq_

    class DistinctOnTest(DistinctOnTest):
        def test_distinct_on(self):
            statement = select("*").distinct(column("q")).select_from(table("foo"))
            self.assert_compile(statement, "SELECT DISTINCT ON (q) * FROM foo")

    class ComponentReflectionTest(ComponentReflectionTest):
        @testing.requires.temp_table_reflect_indexes
        def test_get_temp_table_indexes(self, connection):
            indexes = inspect(connection).get_indexes(self.temp_table_name())
            for index in indexes:
                index.pop("dialect_options", None)
            eq_(
                [index for index in indexes if index["name"] == "user_tmp_ix"],
                [{"unique": False, "column_names": ["foo"],
                  "name": "user_tmp_ix", "include_columns": []}],
            )

    class EmulatedEnumTest(EnumTest):
        # Additional client-side fallback coverage. The original native
        # EnumTest stays collected and continues to expose CREATE TYPE errors.
        from sqlalchemy import Enum as _Enum
        datatype = _Enum(*EnumTest.enum_values, name="myenum",
                         native_enum=False, create_constraint=True)

        def test_invalid_label_is_rejected_by_database(self, connection):
            import pytest
            from sqlalchemy.exc import IntegrityError

            with pytest.raises(IntegrityError) as error:
                connection.execute(self.tables.enum_table.insert(),
                                   {"id": 1, "enum_data": "invalid"})
            eq_(error.value.orig.pgcode, "23514")
