#!/usr/bin/env python3
"""External-consumer smoke test via ctypes: load the mpedb-capi cdylib and
drive the sqlite3 C-API directly (open :memory:, CREATE, INSERT, SELECT, read a
row). Proves an unmodified libsqlite3 caller — the shape Python's own `sqlite3`
module uses under the hood — runs against mpedb.

Usage:
    python3 tests/smoke.py [path/to/libmpedb_sqlite3.so]
Defaults to target/debug/libmpedb_sqlite3.so relative to the repo root.
"""
import ctypes as C
import os
import sys

SQLITE_OK, SQLITE_ROW, SQLITE_DONE, SQLITE_CONSTRAINT = 0, 100, 101, 19
SQLITE_TRANSIENT = C.c_void_p(-1)


def load(path):
    lib = C.CDLL(path)
    lib.sqlite3_open.argtypes = [C.c_char_p, C.POINTER(C.c_void_p)]
    lib.sqlite3_close.argtypes = [C.c_void_p]
    lib.sqlite3_exec.argtypes = [C.c_void_p, C.c_char_p, C.c_void_p, C.c_void_p, C.c_void_p]
    lib.sqlite3_prepare_v2.argtypes = [C.c_void_p, C.c_char_p, C.c_int,
                                       C.POINTER(C.c_void_p), C.c_void_p]
    lib.sqlite3_step.argtypes = [C.c_void_p]
    lib.sqlite3_finalize.argtypes = [C.c_void_p]
    lib.sqlite3_bind_int.argtypes = [C.c_void_p, C.c_int, C.c_int]
    lib.sqlite3_bind_text.argtypes = [C.c_void_p, C.c_int, C.c_char_p, C.c_int, C.c_void_p]
    lib.sqlite3_column_count.argtypes = [C.c_void_p]
    lib.sqlite3_column_int.argtypes = [C.c_void_p, C.c_int]
    lib.sqlite3_column_text.argtypes = [C.c_void_p, C.c_int]
    lib.sqlite3_column_text.restype = C.c_char_p
    lib.sqlite3_column_name.argtypes = [C.c_void_p, C.c_int]
    lib.sqlite3_column_name.restype = C.c_char_p
    lib.sqlite3_libversion.restype = C.c_char_p
    return lib


def main():
    # The cdylib's suffix is platform-specific: .so on Linux, .dylib on macOS.
    # Hardcoding .so made this test unable to find the library on an M3 at all.
    suffix = "dylib" if sys.platform == "darwin" else "so"
    default = os.path.join(os.path.dirname(__file__), "..", "..", "..",
                           "target", "debug", f"libmpedb_sqlite3.{suffix}")
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.abspath(default)
    lib = load(path)
    print("libversion:", lib.sqlite3_libversion().decode())

    db = C.c_void_p()
    assert lib.sqlite3_open(b":memory:", C.byref(db)) == SQLITE_OK
    assert lib.sqlite3_exec(db, b"CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
                            None, None, None) == SQLITE_OK

    ins = C.c_void_p()
    assert lib.sqlite3_prepare_v2(db, b"INSERT INTO t (id, name) VALUES (?, ?)",
                                  -1, C.byref(ins), None) == SQLITE_OK
    for i, nm in enumerate([b"ada", b"grace", b"linus"], start=1):
        lib.sqlite3_reset(ins)
        lib.sqlite3_clear_bindings(ins)
        lib.sqlite3_bind_int(ins, 1, i)
        lib.sqlite3_bind_text(ins, 2, nm, -1, SQLITE_TRANSIENT)
        assert lib.sqlite3_step(ins) == SQLITE_DONE
    lib.sqlite3_finalize(ins)

    sel = C.c_void_p()
    assert lib.sqlite3_prepare_v2(db, b"SELECT id, name FROM t ORDER BY id",
                                  -1, C.byref(sel), None) == SQLITE_OK
    assert lib.sqlite3_column_count(sel) == 2
    assert lib.sqlite3_column_name(sel, 0) == b"id"
    seen = []
    while lib.sqlite3_step(sel) == SQLITE_ROW:
        seen.append((lib.sqlite3_column_int(sel, 0),
                     lib.sqlite3_column_text(sel, 1).decode()))
    lib.sqlite3_finalize(sel)
    print("rows:", seen)
    assert seen == [(1, "ada"), (2, "grace"), (3, "linus")], seen

    # A duplicate PK is SQLITE_CONSTRAINT.
    rc = lib.sqlite3_exec(db, b"INSERT INTO t (id, name) VALUES (1, 'dup')",
                          None, None, None)
    assert rc == SQLITE_CONSTRAINT, rc

    lib.sqlite3_close(db)
    print("OK")


if __name__ == "__main__":
    main()
