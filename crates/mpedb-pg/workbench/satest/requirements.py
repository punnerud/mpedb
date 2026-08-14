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
