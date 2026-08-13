"""What mpedb claims of SQLAlchemy's compliance suite.

Deliberately EMPTY to start. `SuiteRequirements` is the full bar every
third-party dialect is measured against; every `@property` here would be a
declared exclusion, and a declared exclusion is a claim — so the baseline is
taken with none of them, and each one added later has to be argued for.
"""

from sqlalchemy.testing.requirements import SuiteRequirements


class Requirements(SuiteRequirements):
    pass
