#!/usr/bin/env python3
"""sqlite control arm for the writer-contention curve — the EXACT shape of
`mpedb stress --mode incr`: N processes, each looping an autocommit
`UPDATE ctr SET v = v + 1 WHERE id = ?` on a random key in a 64-key space
for S seconds. WAL + synchronous=FULL (like-for-like against mpedb
--durability commit, rule #122), busy_timeout so contention waits instead
of erroring. Verifies the conservation invariant (sum(v) == total ok).

Children are spawned via re-exec (not fork): macOS kills forked Python
children silently in this shape, which read as a 0 ops/s control arm —
an instrument failure the A/B rules exist to catch.

Usage: sqlite_incr.py <db-path> <workers> <secs>
"""
import os
import random
import sqlite3
import subprocess
import sys
import time

KEYSPACE = 64


def child(path, secs, wid):
    random.seed(wid * 1_000_003 + secs * 97 + os.getpid())
    c = sqlite3.connect(path, isolation_level=None, timeout=60.0)
    c.execute("PRAGMA journal_mode=WAL")
    c.execute("PRAGMA synchronous=FULL")
    # Rule #122, the macOS half: plain fsync() does not reach the platter on
    # Darwin, and mpedb's commit durability pays F_FULLFSYNC — so the fair
    # control arm must too. No-op on Linux.
    c.execute("PRAGMA fullfsync=ON")
    c.execute("PRAGMA busy_timeout=60000")
    deadline = time.monotonic() + secs
    ops = ok = 0
    while time.monotonic() < deadline:
        key = random.randrange(KEYSPACE)
        ops += 1
        cur = c.execute("UPDATE ctr SET v = v + 1 WHERE id = ?", (key,))
        if cur.rowcount == 1:
            ok += 1
    c.close()
    print(f"{ops} {ok}")


def main():
    if sys.argv[1] == "--child":
        child(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]))
        return
    path, workers, secs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    for suffix in ("", "-wal", "-shm"):
        try:
            os.unlink(path + suffix)
        except FileNotFoundError:
            pass
    c = sqlite3.connect(path)
    c.execute("PRAGMA journal_mode=WAL")
    c.execute("CREATE TABLE ctr (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)")
    c.executemany("INSERT INTO ctr VALUES (?, 0)", [(i,) for i in range(KEYSPACE)])
    c.commit()
    c.close()

    t0 = time.monotonic()
    procs = [
        subprocess.Popen(
            [sys.executable, os.path.abspath(__file__), "--child", path, str(secs), str(wid)],
            stdout=subprocess.PIPE,
            text=True,
        )
        for wid in range(workers)
    ]
    tot_ops = tot_ok = failed = 0
    for p in procs:
        out, _ = p.communicate()
        if p.returncode != 0 or not out.strip():
            failed += 1
            continue
        ops, ok = map(int, out.split())
        tot_ops += ops
        tot_ok += ok
    elapsed = time.monotonic() - t0

    c = sqlite3.connect(path)
    total = c.execute("SELECT sum(v) FROM ctr").fetchone()[0]
    c.close()
    verdict = "ok" if (total == tot_ok and failed == 0) else f"FAILED sum={total} ok={tot_ok} dead_children={failed}"
    print(
        f"sqlite incr: workers={workers} secs={secs} ops={tot_ops} ok={tot_ok} "
        f"throughput={tot_ops / elapsed:.0f} ops/s"
    )
    print(f"verify: {verdict}")


if __name__ == "__main__":
    main()
