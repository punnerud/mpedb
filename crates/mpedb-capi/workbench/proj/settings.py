SECRET_KEY = "wb"
INSTALLED_APPS = ["django.contrib.contenttypes", "django.contrib.auth", "app"]
DATABASES = {"default": {"ENGINE": "django.db.backends.sqlite3", "NAME": "/tmp/wb-django.db"}}
DEFAULT_AUTO_FIELD = "django.db.models.BigAutoField"
USE_TZ = True

# Adaptation D2 (C-API-COMPAT.md): drop Django's `AUTOINCREMENT` suffix on
# auto-primary-keys. mpedb refuses the keyword BY NAME and by design — it keeps
# no persisted rowid high-water counter, so it cannot promise an id is never
# reused, which is the entirety of what AUTOINCREMENT adds over a plain
# `INTEGER PRIMARY KEY`. Without this, `migrate` dies on the very first
# `CREATE TABLE` and the whole workbench measures nothing.
#
# This is the same adaptation the measured Django runs use, and the one the
# compat table records as KEEP; it was simply never applied to the in-repo
# workbench project, so step 2 here had been dying at `django_migrations`.
# Set WB_NO_D2=1 to leave the keyword in and see that refusal on purpose.
import os

if not os.environ.get("WB_NO_D2"):
    from django.db.backends.sqlite3.base import DatabaseWrapper

    DatabaseWrapper.data_types_suffix = {}
