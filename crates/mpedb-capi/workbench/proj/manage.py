import os, sys
os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings")
sys.path.insert(0, os.path.dirname(__file__))
from django.core.management import execute_from_command_line
execute_from_command_line(sys.argv)
