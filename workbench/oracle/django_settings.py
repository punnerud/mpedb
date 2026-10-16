"""Stock Oracle backend settings for Django's upstream tests."""

import os


def database(alias):
    return {
        "ENGINE": "django.db.backends.oracle",
        "NAME": os.environ["ORACLE_DSN"],
        "USER": os.environ["ORACLE_USER"],
        "PASSWORD": os.environ["ORACLE_PASSWORD"],
        "TEST": {
            # Separate users and tablespaces for Django's two database aliases.
            "USER": f"mpedb_dj_{alias}",
            "PASSWORD": os.environ["ORACLE_TEST_PASSWORD"],
            "TBLSPACE": f"mpedb_dj_{alias}",
            "TBLSPACE_TMP": f"mpedb_dj_{alias}_tmp",
        },
    }


DATABASES = {alias: database(alias) for alias in ("default", "other")}
SECRET_KEY = "oracle-upstream-tests-only"
PASSWORD_HASHERS = ["django.contrib.auth.hashers.MD5PasswordHasher"]
DEFAULT_AUTO_FIELD = "django.db.models.AutoField"
USE_TZ = False
