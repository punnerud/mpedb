"""What mpedb claims of SQLAlchemy's compliance suite.

`SuiteRequirements` is the full bar every third-party dialect is measured
against. Every property here is a DECLARED EXCLUSION, and a declared exclusion
is a claim — so this file started empty, the baseline was taken against the
whole bar, and each entry below has to be argued for in its own docstring.

The test of a good exclusion is that mpedb refuses the feature BY NAME. A
feature that is refused is measured honestly as "not supported"; one that is
silently wrong would be hidden by an exclusion, and that is the line this file
must not cross.
"""

from sqlalchemy.testing import exclusions
from sqlalchemy.testing.requirements import SuiteRequirements


class Requirements(SuiteRequirements):
    @property
    def schemas(self):
        """mpedb has no named schemas at all.

        `CREATE SCHEMA` is refused by name, and there is no catalog object a
        schema could be. The name space mpedb does have is per-FILE (`main`,
        `temp`, and ATTACHed members), which is sqlite's model and is reached
        through `ATTACH`, not through a qualified name inside one database.

        Left in until this was measured: it is the fixture blocker for
        `ComponentReflectionTest`, 766 errors on one `CREATE TABLE
        test_schema.users`.
        """
        return exclusions.closed()

    @property
    def sequences(self):
        """No sequence objects.

        `CREATE SEQUENCE` and `nextval()` are both refused by name. mpedb's
        answer to a generated key is an INTEGER PRIMARY KEY, whose rowid the
        engine allocates — which serves SERIAL but is not a first-class
        sequence a second table could share.
        """
        return exclusions.closed()

    @property
    def server_side_cursors(self):
        """No `DECLARE`/`FETCH`.

        mpedb streams a result set through the wire protocol's own row flow;
        a named cursor is a second, server-held iteration model it does not
        have, and `DECLARE` is refused by name.
        """
        return exclusions.closed()

    @property
    def unique_constraints_reflect_as_index(self):
        """A unique CONSTRAINT also reflects as an INDEX.

        The one OPENING in this file, and it is here because the default is
        `closed()` while both mpedb and PostgreSQL are open: a UNIQUE constraint
        is enforced by an index, that index has a `pg_index` row, and
        `get_indexes` therefore returns it carrying `duplicates_constraint` —
        measured, and byte-for-byte the shape PostgreSQL's own dialect produces
        (which is why SQLAlchemy's postgresql requirements declare it open too).

        Leaving it closed made `test_get_multi_indexes` expect ZERO indexes on a
        table with two unique constraints, so the suite was measuring the
        DEFAULT's assumption rather than the dialect — real PostgreSQL fails that
        comparison identically.
        """
        return exclusions.open()

    @property
    def reflects_pk_names(self):
        """A PRIMARY KEY constraint's DECLARED name reflects back.

        The second OPENING, and it is here because the engine outgrew the
        default: `CONSTRAINT email_ad_pk PRIMARY KEY (…)` used to reflect as the
        derived `<table>_pkey`, and now the declared name is stored
        (`TableDef::pk_name`, canonical bytes v20) and reported by both
        `pg_constraint` and the backing index.

        The suite asserts this INVERTED — `with reflects_pk_names.fail_if()` —
        so leaving it closed turns the fix into an "unexpected success" failure.
        """
        return exclusions.open()
