SECRET_KEY = "wb"
INSTALLED_APPS = ["django.contrib.contenttypes", "django.contrib.auth", "app"]
DATABASES = {"default": {"ENGINE": "django.db.backends.sqlite3", "NAME": "/tmp/wb-django.db"}}
DEFAULT_AUTO_FIELD = "django.db.models.BigAutoField"
USE_TZ = True
