#!/usr/bin/env python3
"""joinstep-profile.py — where does a join step's time go?

The complex-query cell (benchmarks/routing.md) leaves mpedb 2.8x behind
PostgreSQL on a 12-table chain: 5.7 ms against 2.06. That is ~11 000 join
steps at ~520 ns each. Before proposing an operator, decompose the 520:

  slope        per-JOIN-STEP cost, from the chain's wall time vs n (2..12)
  scan_row     per-row cost of a bare full scan (decode + project, no probe)
  keyed_stmt   a whole keyed point statement (probe + all statement overhead)
  count_only   count(*) over one table (no row materialisation at all)

slope - scan_row isolates the probe itself; keyed_stmt - slope says how much
of a statement's cost never reaches the join loop.

Env: MPEDB_PYMOD (dir with _native.so), WORKDIR (scratch).
"""
import os
import statistics
import sys
import time

sys.path.insert(0, os.environ["MPEDB_PYMOD"])
import _native  # noqa: E402

WORKDIR = os.path.abspath(os.environ.get("WORKDIR", "./joinstep-scratch"))
NT, ROWS = 12, 1000
REPS = 7


def build():
    os.makedirs(WORKDIR, exist_ok=True)
    cfg = os.path.join(WORKDIR, "app.toml")
    dbp = os.path.join(WORKDIR, "db.mpedb").replace("\\", "/")
    for suffix in ("", ".wlock"):
        try:
            os.unlink(dbp + suffix)
        except FileNotFoundError:
            pass
    tables = "".join(
        f'\n[[table]]\nname = "t{k}"\nprimary_key = ["id"]\n'
        f'  [[table.column]]\n  name = "id"\n  type = "int64"\n'
        f'  [[table.column]]\n  name = "a"\n  type = "int64"\n'
        for k in range(1, NT + 1))
    with open(cfg, "w") as f:
        f.write(f'[database]\npath = "{dbp}"\nsize_mb = 256\nmax_readers = 8\n{tables}')
    db = _native.Database(cfg)
    for k in range(1, NT + 1):
        rows = [[i, i] for i in range(ROWS)]
        db.query_many(f"INSERT INTO t{k} (id, a) VALUES ($1, $2)", rows) \
            if hasattr(db, "query_many") else [
                db.query(f"INSERT INTO t{k} (id, a) VALUES ($1, $2)", r) for r in rows]
    return db


def chain(n):
    q = "SELECT count(*) FROM t1"
    for k in range(2, n + 1):
        q += f" JOIN t{k} ON t{k-1}.a = t{k}.id"
    return q


def timed(db, sql, params=None, reps=REPS):
    params = params or []
    db.query(sql, params)                      # warm
    out = []
    for _ in range(reps):
        t = time.perf_counter_ns()
        db.query(sql, params)
        out.append(time.perf_counter_ns() - t)
    return statistics.median(out)


def main():
    db = build()
    print("## chain wall time vs n (median of %d, µs)" % REPS)
    ns = [2, 4, 6, 8, 10, 12]
    walls = {}
    for n in ns:
        walls[n] = timed(db, chain(n)) / 1000.0
        print(f"n={n}\t{walls[n]:.1f}")
    # least-squares slope over n (each +1 table = +ROWS join steps)
    xs, ys = ns, [walls[n] for n in ns]
    mx, my = statistics.mean(xs), statistics.mean(ys)
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sum((x - mx) ** 2 for x in xs)
    print(f"\nslope\t{slope:.1f} µs per extra table = {slope * 1000 / ROWS:.0f} ns per join step")

    scan = timed(db, "SELECT a FROM t1") / 1000.0
    cnt = timed(db, "SELECT count(*) FROM t1") / 1000.0
    keyed = timed(db, "SELECT a FROM t2 WHERE id = $1", [500]) / 1000.0
    print(f"scan_row\t{scan * 1000 / ROWS:.0f} ns/row (full scan, {scan:.1f} µs total)")
    print(f"count_only\t{cnt * 1000 / ROWS:.0f} ns/row ({cnt:.1f} µs total)")
    print(f"keyed_stmt\t{keyed * 1000:.0f} ns (whole statement)")
    print(f"\nprobe ≈ slope - scan_row = {slope * 1000 / ROWS - scan * 1000 / ROWS:.0f} ns")


if __name__ == "__main__":
    main()
