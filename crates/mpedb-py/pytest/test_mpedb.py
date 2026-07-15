#!/usr/bin/env python3.12
"""End-to-end tests for the mpedb Python module.

Plain Python, no pytest. Run with the built module on PYTHONPATH:

    cargo build --release -p mpedb-py
    mkdir -p /tmp/mpedb-pymod && cp target/release/libmpedb_py.so /tmp/mpedb-pymod/mpedb.so
    PYTHONPATH=/tmp/mpedb-pymod python3.12 crates/mpedb-py/pytest/test_mpedb.py /tmp/mpedb-pytest

The working directory argument (default: a fresh temp dir) holds the config
and database file. The suite is re-runnable against the same directory: on a
second run it asserts that the first run's committed data survived, which is
the cross-process persistence check.
"""

import datetime
import os
import random
import sys
import tempfile
import threading
import time

import mpedb

UTC = datetime.timezone.utc

CONFIG_TEMPLATE = """\
[database]
path = "{dbpath}"
size_mb = 64
max_readers = 64

[[table]]
name = "users"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "email"
  type = "text"
  nullable = false
  unique = true

  [[table.column]]
  name = "age"
  type = "int64"
  check = "age >= 0"

[[table]]
name = "vals"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "i"
  type = "int64"

  [[table.column]]
  name = "f"
  type = "float64"

  [[table.column]]
  name = "b"
  type = "bool"

  [[table.column]]
  name = "t"
  type = "text"

  [[table.column]]
  name = "bl"
  type = "blob"

  [[table.column]]
  name = "ts"
  type = "timestamp"

[[table]]
name = "kv"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "n"
  type = "int64"

[[table]]
name = "runs"
primary_key = ["run"]

  [[table.column]]
  name = "run"
  type = "int64"

  [[table.column]]
  name = "users_marker"
  type = "int64"

  [[table.column]]
  name = "kv_count"
  type = "int64"
"""

PASS = []


def ok(name, detail=""):
    PASS.append(name)
    print(f"  PASS {name}" + (f"  ({detail})" if detail else ""))


def expect_raises(exc_type, fn, *args, substring=None):
    try:
        fn(*args)
    except exc_type as e:
        if substring is not None:
            assert substring in str(e), f"expected {substring!r} in {e!r}"
        return e
    raise AssertionError(f"expected {exc_type.__name__}, nothing was raised")


def max_id(db, table):
    rows = db.query(f"SELECT * FROM {table}", [])
    return max((r[0] for r in rows), default=0)


# --------------------------------------------------------------------- tests


def test_open_and_tables(db):
    assert sorted(db.tables()) == ["kv", "runs", "users", "vals"], db.tables()
    ok("open + tables()")


def test_crud(db, base):
    # INSERT / SELECT / UPDATE / DELETE via prepared hashes.
    h_ins = db.prepare("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)")
    h_sel = db.prepare("SELECT * FROM users WHERE id = $1")
    h_upd = db.prepare("UPDATE users SET age = $1 WHERE id = $2")
    h_del = db.prepare("DELETE FROM users WHERE id = $1")
    for h in (h_ins, h_sel, h_upd, h_del):
        assert isinstance(h, str) and len(h) == 64
        int(h, 16)  # 64-hex

    a, b = base + 1, base + 2
    assert db.execute(h_ins, [a, f"crud{a}@x.no", 30]) == 1
    assert db.execute(h_ins, [b, f"crud{b}@x.no", 40]) == 1
    rows = db.execute(h_sel, [a])
    assert rows == [(a, f"crud{a}@x.no", 30)], rows
    assert isinstance(rows[0], tuple)

    assert db.execute(h_upd, [31, a]) == 1
    assert db.execute(h_sel, [a]) == [(a, f"crud{a}@x.no", 31)]

    # one-shot query path, both DML and SELECT
    assert db.query("UPDATE users SET age = $1 WHERE id = $2", [32, a]) == 1
    assert db.query("SELECT * FROM users WHERE id = $1", [a]) == [(a, f"crud{a}@x.no", 32)]

    assert db.execute(h_del, [b]) == 1
    assert db.execute(h_sel, [b]) == []

    # query_full: column names + rows
    cols, rows = db.query_full("SELECT * FROM users WHERE id = $1", [a])
    assert cols == ["id", "email", "age"], cols
    assert rows == [(a, f"crud{a}@x.no", 32)]
    expect_raises(mpedb.ProgrammingError, db.query_full,
                  "DELETE FROM users WHERE id = $1", [base + 999])
    ok("CRUD via execute(hash) / query / query_full")
    return h_sel  # reused by the cross-handle test


def test_value_roundtrips(db, base):
    h_ins = db.prepare(
        "INSERT INTO vals (id, i, f, b, t, bl, ts) VALUES ($1, $2, $3, $4, $5, $6, $7)")
    h_sel = db.prepare("SELECT * FROM vals WHERE id = $1")

    dt = datetime.datetime(2024, 5, 17, 12, 34, 56, 789012, tzinfo=UTC)
    blob = b"\x00\xff\x01binary\x7f"
    text = "blåbær 北京 \U0001f680"
    rid = base + 1
    assert db.execute(h_ins, [rid, -(2**63), -1e300, True, text, blob, dt]) == 1
    (row,) = db.execute(h_sel, [rid])
    assert row == (rid, -(2**63), -1e300, True, text, blob, dt), row
    assert isinstance(row[3], bool) and isinstance(row[1], int)  # bool stays bool
    assert isinstance(row[5], bytes)
    assert isinstance(row[6], datetime.datetime) and row[6].tzinfo is not None
    assert row[6].utcoffset() == datetime.timedelta(0)

    # int64 max, False, empty text, empty blob
    rid2 = base + 2
    assert db.execute(h_ins, [rid2, 2**63 - 1, 3.5, False, "", b"", dt]) == 1
    (row2,) = db.execute(h_sel, [rid2])
    assert row2 == (rid2, 2**63 - 1, 3.5, False, "", b"", dt), row2
    assert isinstance(row2[3], bool) and row2[3] is False

    # NULL round-trip in every nullable column
    rid3 = base + 3
    assert db.execute(h_ins, [rid3, None, None, None, None, None, None]) == 1
    (row3,) = db.execute(h_sel, [rid3])
    assert row3 == (rid3, None, None, None, None, None, None), row3

    # timestamp accepted as raw microseconds (int), returned as datetime
    rid4 = base + 4
    epoch = datetime.datetime(1970, 1, 1, tzinfo=UTC)
    micros = (dt - epoch) // datetime.timedelta(microseconds=1)
    assert db.execute(h_ins, [rid4, 1, 1.0, True, "t", b"b", micros]) == 1
    (row4,) = db.execute(h_sel, [rid4])
    assert row4[6] == dt, row4[6]
    # ... and in a WHERE clause, via the one-shot path
    found = db.query("SELECT * FROM vals WHERE id = $1 AND ts = $2", [rid4, micros])
    assert len(found) == 1
    # naive datetime treated as UTC
    rid5 = base + 5
    naive = datetime.datetime(2001, 2, 3, 4, 5, 6, 7)
    assert db.query(
        "INSERT INTO vals (id, i, f, b, t, bl, ts) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        [rid5, 1, 1.0, True, "t", b"b", naive]) == 1
    (row5,) = db.execute(h_sel, [rid5])
    assert row5[6] == naive.replace(tzinfo=UTC)

    # bytearray accepted for blobs
    rid6 = base + 6
    assert db.execute(h_ins, [rid6, 1, 1.0, True, "t", bytearray(b"ba\x00"), dt]) == 1
    (row6,) = db.execute(h_sel, [rid6])
    assert row6[5] == b"ba\x00" and isinstance(row6[5], bytes)

    # int overflow -> OverflowError, before anything executes
    expect_raises(OverflowError, db.execute, h_ins,
                  [base + 7, 2**63, 1.0, True, "t", b"b", dt])
    ok("value round-trips (int/float/bool/str/bytes/bytearray/None/datetime/µs-int)")


def test_integrity_errors(db, base):
    e = base + 1
    db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
             [e, f"integ{e}@x.no", 1])
    # duplicate primary key
    err = expect_raises(mpedb.IntegrityError, db.query,
                        "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
                        [e, f"other{e}@x.no", 1])
    assert isinstance(err, mpedb.Error)
    # duplicate unique email
    expect_raises(mpedb.IntegrityError, db.query,
                  "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
                  [e + 1, f"integ{e}@x.no", 1])
    # NOT NULL email
    expect_raises(mpedb.IntegrityError, db.query,
                  "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
                  [e + 1, None, 1])
    # CHECK age >= 0
    expect_raises(mpedb.IntegrityError, db.query,
                  "INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
                  [e + 1, f"check{e}@x.no", -5])
    ok("IntegrityError: pk / unique / not-null / check")


def test_programming_errors(db):
    expect_raises(mpedb.ProgrammingError, db.query, "SELEC 1", [])
    expect_raises(mpedb.ProgrammingError, db.query,
                  "SELECT * FROM users WHERE id = $1", [])  # wrong param count
    expect_raises(mpedb.ProgrammingError, db.query,
                  "SELECT * FROM users WHERE id = $1", ["nope"])  # type mismatch
    expect_raises(mpedb.ProgrammingError, db.execute, "00" * 32, [])  # unknown plan
    expect_raises(mpedb.ProgrammingError, db.execute, "zz", [])  # not a hash
    exc = expect_raises(mpedb.Error, db.query, "SELEC 1", [])  # hierarchy
    assert isinstance(exc, Exception)
    ok("ProgrammingError: parse / param count / type / unknown plan")


def test_explain(db):
    plan = db.explain("SELECT * FROM users WHERE id = $1")
    assert isinstance(plan, str) and plan.strip(), plan
    # idempotent when the caller already wrote EXPLAIN, and via query()
    plan2 = db.explain("EXPLAIN SELECT * FROM users WHERE id = $1")
    assert plan2 == plan
    plan3 = db.query("EXPLAIN SELECT * FROM users WHERE id = $1", [])
    assert plan3 == plan
    ok("explain", plan.splitlines()[0][:60])


def test_transaction_commit(db, base):
    i = base + 1
    with db.begin() as tx:
        assert tx.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
                        [i, f"tx{i}@x.no", 7]) == 1
        # the session sees its own uncommitted write
        assert tx.query("SELECT * FROM users WHERE id = $1", [i]) == \
            [(i, f"tx{i}@x.no", 7)]
        # execute-by-hash inside the session (plan prepared before the session)
        h = db.prepare("SELECT * FROM users WHERE id = $1")
        assert tx.execute(h, [i]) == [(i, f"tx{i}@x.no", 7)]
    # committed on clean exit
    assert db.query("SELECT * FROM users WHERE id = $1", [i]) == \
        [(i, f"tx{i}@x.no", 7)]
    # closed transaction refuses further use
    expect_raises(mpedb.ProgrammingError, tx.query, "SELECT * FROM users", [])
    ok("context manager commits on clean exit")


def test_transaction_rollback(db, base):
    i = base + 2

    class Boom(RuntimeError):
        pass

    try:
        with db.begin() as tx:
            tx.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
                     [i, f"rb{i}@x.no", 7])
            assert tx.query("SELECT * FROM users WHERE id = $1", [i]) != []
            raise Boom("abort it")
    except Boom:
        pass
    else:
        raise AssertionError("Boom was swallowed")
    assert db.query("SELECT * FROM users WHERE id = $1", [i]) == []

    # explicit rollback inside the with-block is fine too
    with db.begin() as tx:
        tx.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
                 [i, f"rb{i}@x.no", 8])
        tx.rollback()
    assert db.query("SELECT * FROM users WHERE id = $1", [i]) == []
    ok("context manager rolls back on exception")


def test_poisoned_session(db, base):
    i, j = base + 3, base + 4
    db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
             [i, f"p{i}@x.no", 1])
    db.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
             [j, f"p{j}@x.no", 1])
    tx = db.begin()
    try:
        # Multi-row UPDATE to one shared email: the first row is applied, the
        # second violates UNIQUE -> statement partially applied -> poisoned.
        expect_raises(mpedb.IntegrityError, tx.query,
                      "UPDATE users SET email = $1 WHERE id >= $2",
                      [f"same{base}@x.no", i])
        expect_raises(mpedb.OperationalError, tx.query,
                      "SELECT * FROM users WHERE id = $1", [i],
                      substring="poisoned")
        expect_raises(mpedb.OperationalError, tx.commit, substring="poisoned")
    finally:
        # commit() above already rolled back and closed the session; a second
        # explicit rollback must say "closed", not crash.
        expect_raises(mpedb.ProgrammingError, tx.rollback)
    # nothing from the torn statement is visible
    assert db.query("SELECT * FROM users WHERE id = $1", [i]) == \
        [(i, f"p{i}@x.no", 1)]
    assert db.query("SELECT * FROM users WHERE id = $1", [j]) == \
        [(j, f"p{j}@x.no", 1)]
    ok("poisoned session -> OperationalError; commit refused; state intact")


def test_threading(db, base, seconds=2.0, n_readers=4):
    h_ins = db.prepare("INSERT INTO kv (id, n) VALUES ($1, $2)")
    h_sel = db.prepare("SELECT * FROM kv WHERE id = $1")
    db.execute(h_ins, [base + 1, (base + 1) * 2])  # ensure ≥1 row to read

    stop = threading.Event()
    errors = []
    written = [base + 1]  # ids inserted so far (only writer appends)
    reads = [0] * n_readers

    def writer():
        i = base + 2
        try:
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                assert db.execute(h_ins, [i, i * 2]) == 1
                written.append(i)
                i += 1
        except Exception as e:  # noqa: BLE001
            errors.append(("writer", repr(e)))
        finally:
            stop.set()

    def reader(k):
        rng = random.Random(k)
        try:
            while not stop.is_set():
                hi = len(written)  # racy length read is fine: ids are appended
                i = written[rng.randrange(hi)]
                rows = db.execute(h_sel, [i])
                assert rows == [(i, i * 2)], rows
                reads[k] += 1
        except Exception as e:  # noqa: BLE001
            errors.append((f"reader{k}", repr(e)))

    threads = [threading.Thread(target=writer)]
    threads += [threading.Thread(target=reader, args=(k,)) for k in range(n_readers)]
    t0 = time.monotonic()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.monotonic() - t0

    assert not errors, errors
    # final count correct: every inserted id is present exactly once
    rows = db.query("SELECT * FROM kv WHERE id >= $1", [base + 1])
    got = sorted(r[0] for r in rows)
    assert got == written, (len(got), len(written))
    assert all(r == (i, i * 2) for i, r in zip(got, sorted(rows))), "values intact"
    total_reads = sum(reads)
    assert total_reads > 0 and min(reads) > 0, reads
    ok("threading (1 writer + 4 parallel readers)",
       f"{elapsed:.2f}s, {len(written) - 1} inserts, {total_reads} point reads "
       f"({total_reads / elapsed:,.0f}/s, per-thread min {min(reads)})")
    return len(written)


def test_second_handle(db, cfg_path, h_sel_users, base):
    i = base + 1  # inserted by test_crud (email crud{i}@x.no, age 32)
    db2 = mpedb.Database(cfg_path)
    # sees the first handle's committed writes
    assert db2.query("SELECT * FROM users WHERE id = $1", [i]) == \
        [(i, f"crud{i}@x.no", 32)]
    # executes a plan it never prepared (shared registry, same process)
    assert db2.execute(h_sel_users, [i]) == [(i, f"crud{i}@x.no", 32)]
    # and writes made through db2 are visible through db
    j = base + 100
    db2.query("INSERT INTO users (id, email, age) VALUES ($1, $2, $3)",
              [j, f"h2-{j}@x.no", 2])
    assert db.query("SELECT * FROM users WHERE id = $1", [j]) == \
        [(j, f"h2-{j}@x.no", 2)]
    db2.verify()
    ok("second Database handle in the same process")


def test_detached_and_session(db, cfg_path, base):
    d = base + 900_000  # disjoint kv id range

    # A projection no other test publishes: `SELECT *` expands to (id, n), so
    # `SELECT n` is a genuinely distinct plan that is never in the registry.
    sel_sql = "SELECT n FROM kv WHERE id = $1"

    # --- caching Session: same SQL many times is compiled exactly once ---
    sess = db.session()
    assert sess.cached_plans() == 0
    i0 = d + 1
    assert sess.run("INSERT INTO kv (id, n) VALUES ($1, $2)", [i0, i0 * 3]) == 1
    for _ in range(500):
        rows = sess.run(sel_sql, [i0])
        assert rows == [(i0 * 3,)], rows
    # INSERT + SELECT = two distinct statements, each compiled once.
    assert sess.cached_plans() == 2, sess.cached_plans()

    # --- prepare_detached surface: (hash, blob, sql) ---
    h, blob, sql = db.prepare_detached(sel_sql)
    assert isinstance(h, str) and len(h) == 64
    int(h, 16)
    assert isinstance(blob, (bytes, bytearray)) and len(blob) > 0
    assert sql == sel_sql

    # execute_detached runs it AND it is not in the shared registry: a plain
    # execute(hash) for the same hash cannot find it.
    assert db.execute_detached(blob, [i0]) == [(i0 * 3,)]
    expect_raises(mpedb.ProgrammingError, db.execute, h, [i0])

    # DML through a detached plan, then read it back via the same blob.
    _, ins_blob, _ = db.prepare_detached("INSERT INTO kv (id, n) VALUES ($1, $2)")
    i1 = d + 2
    assert db.execute_detached(ins_blob, [i1, i1 * 3]) == 1
    assert db.execute_detached(blob, [i1]) == [(i1 * 3,)]

    # --- a SECOND Database (same file) executes the received blob, no registry ---
    db2 = mpedb.Database(cfg_path)
    assert db2.execute_detached(blob, [i0]) == [(i0 * 3,)]
    assert db2.execute_detached(blob, [i1]) == [(i1 * 3,)]
    expect_raises(mpedb.ProgrammingError, db2.execute, h, [i0])  # still not registered
    db2.verify()

    # --- integrity: a tampered blob raises, never returns wrong data ---
    # Flip a byte inside the carried 32-byte hash (offset 1..33 of the envelope):
    # the plan blob then no longer matches its hash -> corrupt.
    bad = bytearray(blob)
    bad[1] ^= 0x01
    expect_raises(mpedb.OperationalError, db.execute_detached, bytes(bad), [i0])

    ok("detached plans + caching Session (compiled once, no registry writes)")


def check_persistence(db, prev_runs):
    """Assert data committed by earlier runs of this script is still there."""
    for run, users_marker, kv_count in prev_runs:
        rows = db.query("SELECT * FROM users WHERE id = $1", [users_marker])
        assert rows and rows[0][0] == users_marker, \
            f"run {run}: users marker {users_marker} lost"
        kv = db.query("SELECT * FROM kv", [])
        assert len(kv) >= kv_count, f"run {run}: kv rows lost ({len(kv)} < {kv_count})"
    ok(f"persistence: data from {len(prev_runs)} previous run(s) intact")


def test_dbapi(cfg_path, base):
    """PEP 249, so sqlite3-shaped code runs unchanged."""
    assert mpedb.apilevel == "2.0", mpedb.apilevel
    assert mpedb.paramstyle == "qmark", mpedb.paramstyle
    assert mpedb.threadsafety == 1, mpedb.threadsafety
    ok("dbapi: module globals")

    conn = mpedb.connect(cfg_path)
    cur = conn.cursor()

    # `?` placeholders, rowcount, commit.
    cur.execute("INSERT INTO users (id, email, age) VALUES (?, ?, ?)",
                [base + 900, f"dbapi{base}@x", 30])
    assert cur.rowcount == 1, cur.rowcount
    conn.commit()
    ok("dbapi: execute/rowcount/commit")

    # description names the output columns; PEP 249 allows the other six
    # fields to be None, and they are.
    cur.execute("SELECT id, email FROM users WHERE id = ?", [base + 900])
    assert [d[0] for d in cur.description] == ["id", "email"], cur.description
    assert all(len(d) == 7 for d in cur.description)
    assert cur.fetchall() == [(base + 900, f"dbapi{base}@x")]
    ok("dbapi: description/fetchall")

    # fetchone / fetchmany / iteration all walk the same cursor.
    cur.execute("SELECT id FROM users WHERE id >= ? ORDER BY id", [base + 900])
    assert cur.fetchone() is not None
    cur.execute("SELECT id FROM users WHERE id >= ? ORDER BY id", [base + 900])
    assert len(cur.fetchmany(1)) == 1
    cur.execute("SELECT id FROM users WHERE id >= ? ORDER BY id", [base + 900])
    assert len(list(cur)) >= 1
    ok("dbapi: fetchone/fetchmany/__iter__")

    cur.executemany("INSERT INTO users (id, email, age) VALUES (?, ?, ?)",
                    [[base + 901, f"m1{base}@x", 1], [base + 902, f"m2{base}@x", 2]])
    conn.commit()
    cur.execute("SELECT count(*) FROM users WHERE id >= ?", [base + 900])
    assert cur.fetchone() == (3,), "executemany"
    ok("dbapi: executemany")

    # rollback drops what was buffered.
    cur.execute("INSERT INTO users (id, email, age) VALUES (?, ?, ?)",
                [base + 903, f"gone{base}@x", 9])
    conn.rollback()
    cur.execute("SELECT count(*) FROM users WHERE id >= ?", [base + 900])
    assert cur.fetchone() == (3,), "rollback"
    ok("dbapi: rollback")

    # THE rewrite test: a `?` inside a string literal is a character, not a
    # parameter. A regex-based driver gets this wrong and corrupts the value
    # silently, because the statement still parses.
    cur.execute("SELECT ?, 'why?' FROM users WHERE id = ?", [42, base + 900])
    assert cur.fetchone() == (42, "why?"), "qmark inside a literal"
    ok("dbapi: ? inside a string literal is not a parameter")

    # Context manager: commit on a clean exit, roll back on an exception.
    with mpedb.connect(cfg_path) as c2:
        c2.execute("INSERT INTO users (id, email, age) VALUES (?, ?, ?)",
                   [base + 904, f"ctx{base}@x", 4])
    assert conn.execute("SELECT count(*) FROM users WHERE id = ?",
                        [base + 904]).fetchone() == (1,)
    try:
        with mpedb.connect(cfg_path) as c3:
            c3.execute("INSERT INTO users (id, email, age) VALUES (?, ?, ?)",
                       [base + 905, f"boom{base}@x", 5])
            raise ValueError("boom")
    except ValueError:
        pass
    assert conn.execute("SELECT count(*) FROM users WHERE id = ?",
                        [base + 905]).fetchone() == (0,), "__exit__ must roll back"
    ok("dbapi: context manager commits / rolls back")

    # Errors are DB-API exceptions, and they arrive at execute() rather than
    # waiting for commit() — the caller is still looking at the statement.
    try:
        conn.execute("INSERT INTO users (id, email, age) VALUES (?, ?, ?)",
                     [base + 900, f"dup{base}@x", 1])
        raise AssertionError("a duplicate PK must not be accepted")
    except mpedb.IntegrityError:
        pass
    ok("dbapi: IntegrityError at execute()")

    # The honest boundary: mpedb has no DDL, and a sqlite3 program that runs
    # some fails HERE rather than being told it is "100% compatible".
    try:
        conn.execute("CREATE TABLE nope (id INTEGER)")
        raise AssertionError("DDL must be refused")
    except mpedb.ProgrammingError:
        pass
    ok("dbapi: DDL is refused, loudly")

    conn.close()
    try:
        conn.execute("SELECT 1 FROM users")
        raise AssertionError("a closed connection must refuse")
    except mpedb.Error:
        pass
    ok("dbapi: close()")


def main():
    workdir = sys.argv[1] if len(sys.argv) > 1 else tempfile.mkdtemp(prefix="mpedb-py-")
    os.makedirs(workdir, exist_ok=True)
    cfg_path = os.path.join(workdir, "app.toml")
    if not os.path.exists(cfg_path):
        with open(cfg_path, "w") as f:
            f.write(CONFIG_TEMPLATE.format(dbpath=os.path.join(workdir, "app.mpedb")))
    print(f"mpedb python test suite  (workdir: {workdir})")

    db = mpedb.Database(cfg_path)

    prev_runs = sorted(db.query("SELECT * FROM runs", []))
    run_no = (prev_runs[-1][0] + 1) if prev_runs else 1
    if prev_runs:
        check_persistence(db, prev_runs)
    print(f"run #{run_no}")

    # Disjoint id ranges per run keep every test re-runnable.
    base = run_no * 1_000_000

    test_open_and_tables(db)
    h_sel_users = test_crud(db, base)
    test_value_roundtrips(db, base)
    test_integrity_errors(db, base + 200)
    test_programming_errors(db)
    test_explain(db)
    test_transaction_commit(db, base + 300)
    test_transaction_rollback(db, base + 300)
    test_poisoned_session(db, base + 300)
    test_threading(db, base + 400)
    test_second_handle(db, cfg_path, h_sel_users, base)
    test_detached_and_session(db, cfg_path, base)
    test_dbapi(cfg_path, base)

    db.verify()
    ok("verify()")

    # Record this run for the next invocation's persistence check.
    kv_count = len(db.query("SELECT * FROM kv", []))
    db.query("INSERT INTO runs (run, users_marker, kv_count) VALUES ($1, $2, $3)",
             [run_no, base + 1, kv_count])

    print(f"OK: {len(PASS)} checks passed (run #{run_no})")


if __name__ == "__main__":
    main()
