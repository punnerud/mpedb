# PY-COMPAT — the `pip install mpedb` drop-in, measured against reality

How far `import mpedb as sqlite3` carries real code, measured 2026-07-24
(package 0.1.1) three ways: two real projects' full test suites swapped via
`sys.modules`, and CPython's own `test_sqlite3` (the API's specification).
Method and per-test artifacts: agents' runs under /mnt/ext4 (dropin-test/,
cpython-test/); every suite was first run against stdlib sqlite3 on the same
box, so only NEW failures count.

## Scoreboard

| suite | stdlib baseline | mpedb 0.1.1 | first blocker |
|---|---|---|---|
| sqlitedict (89 tests) | 89 pass | 2 pass | `PRAGMA journal_mode` refused at connection bootstrap |
| diskcache core (185) | 185 pass | 4 pass | `unknown table` raises ProgrammingError; diskcache catches OperationalError |
| CPython test_sqlite3 (424 run) | — | 76 pass / 32 fail / 310 error | two missing `SQLITE_*` constants import-blocked 7 of 10 files |

Reference: the C-API shim (a different artifact — the real `_sqlite3` C module
over mpedb's libsqlite3 ABI) scores 440/466 on the same CPython suite.

**The headline finding is GOOD:** zero silent wrong answers across all three
suites. Every divergence was a loud exception. The never-wrong-answer contract
held in the wild; what fails is a small fixed *connection bootstrap ritual*
real consumers run before any query — and everything queued BEHIND that ritual
already worked in statement replays (parameterized CRUD, REPLACE / OR IGNORE,
aggregates, and on the `.db` overlay even triggers and partial indexes).

## 0.2 roadmap, ranked by measured leverage

### Tier 1 — the bootstrap ritual (small surface, flips real projects)
1. **PRAGMA, accept-and-answer.** Both forms: `PRAGMA x = v` (accept, ignore
   or honor) and `PRAGMA x` (must RETURN a row — diskcache destructures
   `PRAGMA page_size`). 85 of sqlitedict's 87 failures are this one statement.
2. **sqlite's exception taxonomy.** Unknown table/column → `OperationalError`
   (mpedb says `ProgrammingError`); real code catches per sqlite's classes
   (diskcache: 100 % of core failures). Add `sqlite_errorcode`/`sqlite_errorname`.
3. **Transaction control through `execute()`**: `BEGIN` / `BEGIN IMMEDIATE` /
   `COMMIT` / `ROLLBACK` as statements (diskcache `_transact`, CPython).
4. **Module/connection attrs**: `mpedb.dbapi2` (satisfies
   `from sqlite3 import dbapi2`), `register_adapter`/`register_converter`
   (Django's backend imports them at collection), settable `text_factory`,
   the full `SQLITE_LIMIT_*`/`SQLITE_*` constant set (two missing constants
   import-blocked 7 of 10 CPython files).
5. **The 1 GiB fallocate landmine.** The generated native config preallocates
   `size_mb = 1024` PER database; a suite creating throwaway DBs left 32 GB
   of temp files and ENOSPC'd /tmp. Shrink the drop-in default sharply and
   document; same review for the ~128 MiB `.db.overlay.mpedb` sidecar.

### Tier 2 — the CPython pass-count jump (76 → ~280 estimated)
6. **sqlite3 transaction semantics** (the one DANGEROUS divergence): native
   backend must read its own uncommitted writes, and file DBs must not
   autocommit per statement; `isolation_level`/`autocommit`/`in_transaction`.
   ~55 tests + the entire wrong-behavior surface trace here.
7. `Connection.executemany` + `executescript` (Connection and Cursor) — ~52
   direct errors, and un-hides all of test_backup/test_dump.
8. `Row` + `row_factory` + `#[pyclass(subclass)]` on Connection/Cursor — ~40
   (all of test_factory is blocked on subclassability).
9. The type layer: `detect_types`, adapters/converters, PEP-249
   `Date/Time/Timestamp/Binary`, memoryview/bytearray binding — ~60.
10. `create_function`/`create_aggregate` — ~35 (the engine already has host
    UDFs, #98/#108; this is plumbing).

### Tier 3 — headline features after the above
- `blobopen` (38 tests; engine has the incremental blob API, #43),
  `Connection.backup`, `set_trace_callback` (17), `create_collation` (9),
  `interrupt`, limits API, `sqlite3.__main__` (CLI), named `:param` style,
  `VACUUM`/`ANALYZE` acceptance, `IF NOT EXISTS` + implicit-ROWID references
  on the NATIVE path (both exist in the engine/capi; the native SQL surface
  must route them).

## Divergences that stay (documented, deliberate)
- mpedb's SQL is a subset: unsupported statements refuse loudly rather than
  misbehave. The suites confirmed the refusals are loud and the answers given
  are right.
- The `.db` overlay checkpoint-on-commit is a forced sync point (sqlite3's
  DDL-in-transaction is transactional; ours checkpoints first).
