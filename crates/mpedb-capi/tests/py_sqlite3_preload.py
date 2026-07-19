#!/usr/bin/env python3
"""CPython's *built-in* ``sqlite3`` module, driven against mpedb via LD_PRELOAD.

Unlike ``smoke.py`` (which loads the cdylib itself with ``ctypes``), this proves
the real milestone: CPython's ``_sqlite3`` C extension links our shim as
``libsqlite3`` and its own ``sqlite3`` module runs unmodified. Run it with the
shim preloaded so the dynamic linker resolves the ``sqlite3_*`` symbols to
mpedb instead of the system libsqlite3::

    LD_PRELOAD=target/debug/libmpedb_sqlite3.so python3 tests/py_sqlite3_preload.py

Exits 0 on success; any assertion failure raises and exits non-zero. The Rust
test ``py_sqlite3_preload`` shells out to exactly this, and skips gracefully
when ``python3`` is unavailable.
"""
import sqlite3


def main():
    # The header milestone: import works (all sqlite3_* symbols resolved), then
    # basic CRUD + lastrowid against mpedb.
    con = sqlite3.connect(":memory:")
    cur = con.cursor()
    cur.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
    cur.execute("INSERT INTO t(b) VALUES('x')")
    assert cur.lastrowid == 1, ("lastrowid", cur.lastrowid)
    print(cur.lastrowid)
    con.commit()
    cur.execute("SELECT a,b FROM t")
    rows = cur.fetchall()
    assert rows == [(1, "x")], ("rows", rows)
    print(rows)

    # A little more, so a regression in binding / description / transactions is
    # caught here too (still all through CPython's sqlite3, not ctypes).
    cur.execute("INSERT INTO t(b) VALUES(?)", ("y",))
    assert cur.lastrowid == 2, ("lastrowid2", cur.lastrowid)
    con.commit()
    cur.execute("SELECT a, b FROM t WHERE a > ? ORDER BY a", (0,))
    assert cur.fetchall() == [(1, "x"), (2, "y")]
    assert [d[0] for d in cur.description] == ["a", "b"]
    cur.execute("SELECT COUNT(*) FROM t")
    assert cur.fetchone()[0] == 2

    # Rollback of an uncommitted write leaves the table unchanged.
    cur.execute("INSERT INTO t(b) VALUES('z')")
    con.rollback()
    cur.execute("SELECT COUNT(*) FROM t")
    assert cur.fetchone()[0] == 2

    con.close()

    udf_in_implicit_transaction()
    print("OK")


def udf_in_implicit_transaction():
    """Host UDFs (create_function / create_aggregate) with NO commit() between
    the first INSERT and the call — CPython opens an implicit transaction on
    the first DML, so this is where almost every real Django UDF call lands.
    It used to fail with "internal error (bug in mpedb)".
    """
    con = sqlite3.connect(":memory:")
    con.create_function("plus1", 1, lambda x: x + 1)

    class SumPlus:
        def __init__(self):
            self.total = 0

        def step(self, value):
            if value is not None:
                self.total += value

        def finalize(self):
            return self.total + 100

    con.create_aggregate("sumplus", 1, SumPlus)
    cur = con.cursor()
    cur.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, n INTEGER)")

    # The first DML opens the implicit transaction. NO commit() from here on.
    cur.execute("INSERT INTO t(a, n) VALUES(1, 10)")
    cur.execute("INSERT INTO t(a, n) VALUES(2, 20)")

    # scalar UDF in a read inside the open transaction
    cur.execute("SELECT plus1(n) FROM t WHERE a = 1")
    assert cur.fetchone()[0] == 11, "scalar UDF inside the implicit transaction"
    # scalar UDF in the WHERE of that read
    cur.execute("SELECT n FROM t WHERE plus1(a) = 2")
    assert cur.fetchone()[0] == 10, "scalar UDF in WHERE inside the transaction"
    # aggregate UDF inside the open transaction
    cur.execute("SELECT sumplus(n) FROM t")
    assert cur.fetchone()[0] == 130, "aggregate UDF inside the implicit transaction"
    # scalar UDF in a WRITE statement: SET + WHERE, then RETURNING
    cur.execute("UPDATE t SET n = plus1(n) WHERE plus1(a) = 2")
    cur.execute("SELECT n FROM t WHERE a = 1")
    assert cur.fetchone()[0] == 11, "UDF in UPDATE ... SET inside the transaction"
    cur.execute("DELETE FROM t WHERE a = 2 RETURNING plus1(n)")
    assert cur.fetchone()[0] == 21, "UDF in RETURNING inside the transaction"

    # and it all survives the commit
    con.commit()
    cur.execute("SELECT plus1(n) FROM t WHERE a = 1")
    assert cur.fetchone()[0] == 12, "UDF after the commit"
    con.close()
    print("UDF-OK")


if __name__ == "__main__":
    main()
