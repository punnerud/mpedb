"""Use VALUES for bulk inserts; keep the remaining PostgreSQL compiler."""

from django.db.backends.postgresql.compiler import (
    SQLAggregateCompiler,
    SQLCompiler,
    SQLDeleteCompiler,
    SQLUpdateCompiler,
)
from django.db.models.sql.compiler import SQLInsertCompiler

__all__ = [
    "SQLAggregateCompiler",
    "SQLCompiler",
    "SQLDeleteCompiler",
    "SQLUpdateCompiler",
    "SQLInsertCompiler",
]
