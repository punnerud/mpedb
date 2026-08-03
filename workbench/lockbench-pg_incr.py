#!/usr/bin/env python3
"""PostgreSQL arm of the writer-contention curve — the same shape as
`mpedb stress --mode incr` and sqlite_incr.py: N processes, each looping an
autocommit `UPDATE ctr SET v = v + 1 WHERE id = ?` over a 64-key space for
S seconds, synchronous_commit=on (default — WAL fsync per commit, the
durable contract rule #122 requires). No client library on the box, so
each child clocks ONE psql session by write-statement/read-ack round trips
— the ack read is what makes the deadline exact (nothing queues in the
pipe past it), and the conservation invariant is checked at the end.

Usage: pg_incr.py <socket-dir> <port> <workers> <secs>
"""
import os
import random
import subprocess
import sys
import time

KEYSPACE = 64


def child(sockdir, port, secs, wid):
    random.seed(wid * 1_000_003 + secs * 97 + os.getpid())
    p = subprocess.Popen(
        ["psql", "-h", sockdir, "-p", port, "-U", "bench", "-d", "postgres",
         "-qAtX", "--no-psqlrc", "-f", "-"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
    )
    # -q suppresses command tags; make every statement produce exactly one
    # ack line via RETURNING, so the round trip is self-clocking.
    deadline = time.monotonic() + secs
    ops = ok = 0
    while time.monotonic() < deadline:
        key = random.randrange(KEYSPACE)
        p.stdin.write(f"UPDATE ctr SET v = v + 1 WHERE id = {key} RETURNING 1;\n")
        p.stdin.flush()
        line = p.stdout.readline()
        ops += 1
        if line.strip() == "1":
            ok += 1
    p.stdin.close()
    p.wait()
    print(f"{ops} {ok}")


def main():
    if sys.argv[1] == "--child":
        child(sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]))
        return
    sockdir, port, workers, secs = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])

    def psql(sql):
        return subprocess.run(
            ["psql", "-h", sockdir, "-p", port, "-U", "bench", "-d", "postgres",
             "-qAtX", "--no-psqlrc", "-c", sql],
            capture_output=True, text=True, check=True,
        ).stdout.strip()

    psql("DROP TABLE IF EXISTS ctr")
    psql("CREATE TABLE ctr (id integer PRIMARY KEY, v bigint NOT NULL)")
    psql(f"INSERT INTO ctr SELECT g, 0 FROM generate_series(0, {KEYSPACE - 1}) g")
    assert psql("SHOW synchronous_commit") == "on", "synchronous_commit must be on"
    assert psql("SHOW fsync") == "on", "fsync must be on"

    t0 = time.monotonic()
    procs = [
        subprocess.Popen(
            [sys.executable, os.path.abspath(__file__), "--child", sockdir, port,
             str(secs), str(wid)],
            stdout=subprocess.PIPE, text=True,
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

    total = int(psql("SELECT sum(v) FROM ctr"))
    verdict = "ok" if (total == tot_ok and failed == 0) else \
        f"FAILED sum={total} ok={tot_ok} dead_children={failed}"
    print(
        f"postgres incr: workers={workers} secs={secs} ops={tot_ops} ok={tot_ok} "
        f"throughput={tot_ops / elapsed:.0f} ops/s"
    )
    print(f"verify: {verdict}")


if __name__ == "__main__":
    main()
