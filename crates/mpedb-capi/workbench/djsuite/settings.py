"""Settings for running Django's own test suite against the mpedb C-API shim.

Mirrors Django's `tests/test_sqlite.py` (in-memory sqlite, fast hasher) but
points ENGINE at the workbench backend, whose adaptations are documented in
`mpedb_backend/base.py`.
"""

DATABASES = {
    "default": {"ENGINE": "mpedb_backend"},
    "other": {"ENGINE": "mpedb_backend"},
}

SECRET_KEY = "django_tests_secret_key"

PASSWORD_HASHERS = ["django.contrib.auth.hashers.MD5PasswordHasher"]

DEFAULT_AUTO_FIELD = "django.db.models.AutoField"

USE_TZ = False
