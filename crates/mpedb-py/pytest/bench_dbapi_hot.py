#!/usr/bin/env python3.12
"""N2 instrument: the consumer's microbench cells, mpedb dbapi vs stdlib
sqlite3 side by side. Named bench_* so pytest never collects it; run
directly, with the built module on PYTHONPATH:

    cargo build --release -p mpedb-py
    mkdir -p /tmp/mpedb-pymod && cp target/release/libmpedb_py.so /tmp/mpedb-pymod/mpedb.so
    PYTHONPATH=/tmp/mpedb-pymod python3.12 crates/mpedb-py/pytest/bench_dbapi_hot.py

Cells (mirrors mpedb2/bench shapes; ratio > 1 = sqlite3 wins):
    insert_txn      BEGIN; 10k parameterized INSERTs; ROLLBACK
    point_select    10k point SELECTs (autocommit)
    executemany     one executemany of 20k rows in a txn
    like_scan       50 x SELECT ... LIKE '%x%' over 20k rows
    connect         200 x connect+close
    savepoint_disk  2000 x SAVEPOINT/INSERT/ROLLBACK TO/RELEASE in one txn (disk only)

The INSERT-autocommit cell is deliberately absent: it is durability-unfair
(#122) until measured with matched durability.

Every cell prints µs/op for both engines and the ratio. The (python − Rust)
delta against hot_stmt_latency.rs's cell (a) isolates the pyo3/dispatch
layer's share.
"""

import os
import sqlite3
import sys
import tempfile
import time

import mpedb

N_INSERT = 10_000
N_SELECT = 10_000
N_MANY = 20_000
N_LIKE = 50
N_CONNECT = 200
N_SPCYCLE = 2_000
ROUNDS = 3

DDL = "CREATE TABLE bench (id INTEGER PRIMARY KEY, email TEXT, age INTEGER)"


def connect(engine, target):
    if engine == "mpedb":
        return mpedb.connect(target, isolation_level=None)
    return sqlite3.connect(target, isolation_level=None)


def seeded(engine, target, rows=20_000):
    c = connect(engine, target)
    c.execute(DDL)
    c.execute("BEGIN")
    c.executemany(
        "INSERT INTO bench VALUES (?, ?, ?)",
        [(i, f"u{i}@x.no", i % 90) for i in range(rows)],
    )
    c.execute("COMMIT")
    return c


def cell_insert_txn(c):
    cur = c.cursor()
    c.execute("BEGIN")
    cur.execute("INSERT INTO bench VALUES (?, ?, ?)", (10_000_000, "w@x.no", 1))
    t = time.perf_counter_ns()
    for i in range(N_INSERT):
        cur.execute("INSERT INTO bench VALUES (?, ?, ?)", (11_000_000 + i, "v@x.no", 7))
    dt = time.perf_counter_ns() - t
    c.execute("ROLLBACK")
    return dt / N_INSERT


def cell_point_select(c):
    cur = c.cursor()
    cur.execute("SELECT email FROM bench WHERE id = ?", (1,)).fetchone()
    t = time.perf_counter_ns()
    for i in range(N_SELECT):
        cur.execute("SELECT email FROM bench WHERE id = ?", (i % 1000,)).fetchone()
    dt = time.perf_counter_ns() - t
    return dt / N_SELECT


def cell_executemany(c):
    rows = [(12_000_000 + i, f"m{i}@x.no", i % 90) for i in range(N_MANY)]
    c.execute("BEGIN")
    t = time.perf_counter_ns()
    c.executemany("INSERT INTO bench VALUES (?, ?, ?)", rows)
    dt = time.perf_counter_ns() - t
    c.execute("ROLLBACK")
    return dt / N_MANY


def cell_like_scan(c):
    cur = c.cursor()
    cur.execute("SELECT count(*) FROM bench WHERE email LIKE '%77%'").fetchone()
    t = time.perf_counter_ns()
    for _ in range(N_LIKE):
        cur.execute("SELECT count(*) FROM bench WHERE email LIKE '%77%'").fetchone()
    dt = time.perf_counter_ns() - t
    return dt / N_LIKE


def cell_connect(engine, target):
    connect(engine, target).close()
    t = time.perf_counter_ns()
    for _ in range(N_CONNECT):
        connect(engine, target).close()
    dt = time.perf_counter_ns() - t
    return dt / N_CONNECT


def cell_savepoint(c):
    cur = c.cursor()
    c.execute("BEGIN")
    t = time.perf_counter_ns()
    for k in range(N_SPCYCLE):
        cur.execute("SAVEPOINT sp")
        cur.execute("INSERT INTO bench VALUES (?, ?, ?)", (13_000_000 + k, "s@x.no", 3))
        cur.execute("ROLLBACK TO SAVEPOINT sp")
        cur.execute("RELEASE SAVEPOINT sp")
    dt = time.perf_counter_ns() - t
    c.execute("ROLLBACK")
    return dt / N_SPCYCLE


def target_for(engine, storage, tmp):
    if storage == "memory":
        return ":memory:"
    ext = "mpedb" if engine == "mpedb" else "sqlite3"
    return os.path.join(tmp, f"bench-{engine}.{ext}")


def run(storage):
    print(f"\n=== storage: {storage} (µs/op, median of {ROUNDS}; ratio>1 = sqlite3 wins) ===")
    results = {}
    with tempfile.TemporaryDirectory() as tmp:
        for engine in ("sqlite3", "mpedb"):
            target = target_for(engine, storage, tmp)
            per = {}
            conn_target = target_for(engine, storage, tmp) if storage == "disk" else ":memory:"
            samples = {k: [] for k in
                       ("insert_txn", "point_select", "executemany", "like_scan", "connect", "savepoint")}
            c = seeded(engine, target)
            for _ in range(ROUNDS):
                samples["insert_txn"].append(cell_insert_txn(c))
                samples["point_select"].append(cell_point_select(c))
                samples["executemany"].append(cell_executemany(c))
                samples["like_scan"].append(cell_like_scan(c))
                # connect cell: disk connects to the EXISTING seeded file
                # (schema present — the realistic Django shape); memory
                # connects fresh.
                samples["connect"].append(cell_connect(engine, conn_target))
                if storage == "disk":
                    samples["savepoint"].append(cell_savepoint(c))
            c.close()
            for k, v in samples.items():
                if v:
                    per[k] = sorted(v)[len(v) // 2] / 1000.0  # ns -> µs
            results[engine] = per

    for k in results["sqlite3"]:
        if k not in results["mpedb"]:
            continue
        s, m = results["sqlite3"][k], results["mpedb"][k]
        print(f"  {k:<14} sqlite3 {s:9.1f}   mpedb {m:9.1f}   ratio {m / s:5.2f}x")
    return results


if __name__ == "__main__":
    print(f"python {sys.version.split()[0]}  sqlite3 {sqlite3.sqlite_version}  "
          f"mpedb {getattr(mpedb, '__version__', '?')}")
    run("memory")
    run("disk")
