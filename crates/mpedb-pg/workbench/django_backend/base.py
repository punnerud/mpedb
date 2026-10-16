"""External test files, with explicit optional SQL portability adaptations.

run_django.py starts a dedicated server and fresh file for each test alias.
This is not CREATE DATABASE support and must never be used for ordinary apps.
Portable SQL is opt-in; its feature declarations and SQL choices are recorded
in run.json. Test cases and assertions remain unchanged.
"""

from django.db.backends.postgresql.base import DatabaseWrapper as PostgreSQLWrapper
from django.db.backends.postgresql.creation import DatabaseCreation
from django.db.backends.postgresql.schema import DatabaseSchemaEditor
from django.db.backends.postgresql.operations import DatabaseOperations
from django.db.backends.postgresql.features import DatabaseFeatures
from django.db import NotSupportedError
import os


class PortableSchemaEditor(DatabaseSchemaEditor):
    def _create_like_index_sql(self, model, field):
        # Ordinary indexes already use binary text ordering in MPEdb. Omit
        # PostgreSQL's extra locale-independent LIKE optimization index.
        return None


class PortableOperations(DatabaseOperations):
    compiler_module = "django_backend.compiler"

    def sequence_reset_sql(self, style, model_list):
        # MPEdb's owned identity generator advances on explicit primary keys
        # too. Fixture inserts already establish the high-water mark; a
        # PostgreSQL setval(max(pk)) repair is unnecessary for this generator.
        return []

    def sql_flush(self, style, tables, *, reset_sequences=False, allow_cascade=False):
        if reset_sequences:
            raise NotSupportedError(
                "MPEdb does not support resetting owned identity generators"
            )
        tables = list(tables)
        if allow_cascade and tables:
            with self.connection.cursor() as cursor:
                cursor.execute("""
                    SELECT child.relname, parent.relname
                    FROM pg_catalog.pg_constraint fk
                    JOIN pg_catalog.pg_class child ON child.oid = fk.conrelid
                    JOIN pg_catalog.pg_class parent ON parent.oid = fk.confrelid
                    WHERE fk.contype = 'f'
                """)
                edges = cursor.fetchall()
            pending = set(tables)
            while True:
                children = {child for child, parent in edges if parent in pending}
                if children <= pending:
                    break
                pending.update(children)
            tables.extend(sorted(pending - set(tables)))
        return [f"DELETE FROM {self.quote_name(table)};" for table in tables]


class PortableFeatures(DatabaseFeatures):
    allows_group_by_selected_pks = False
    can_distinct_on_fields = False
    supports_covering_indexes = False
    supports_regex_backreferencing = False
    has_select_for_update = False
    has_select_for_update_nowait = False
    has_select_for_update_of = False
    has_select_for_update_skip_locked = False
    supports_sequence_reset = False
    can_rollback_ddl = False


class ProvisionedCreation(DatabaseCreation):
    def _create_test_db(self, verbosity, autoclobber, keepdb=False):
        expected = self.connection.settings_dict["TEST"].get("NAME")
        if not expected or expected != self.connection.settings_dict["NAME"]:
            raise RuntimeError("MPEdb test files must be provisioned by run_django.py")
        return expected

    def _destroy_test_db(self, test_database_name, verbosity):
        # The runner terminates the dedicated servers and retains the files.
        pass

    def _clone_test_db(self, suffix, verbosity, keepdb=False):
        raise NotImplementedError("Provisioned MPEdb tests require --parallel=1")


class DatabaseWrapper(PostgreSQLWrapper):
    creation_class = ProvisionedCreation
    if os.environ.get("MPEDB_DJANGO_PORTABLE_SQL") == "1":
        SchemaEditorClass = PortableSchemaEditor
        ops_class = PortableOperations
        features_class = PortableFeatures
