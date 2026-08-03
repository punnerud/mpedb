#!/usr/bin/env python3
"""nestedloop-width.py — the per-outer-row PROBE, as a function of the inner
side's width, with an answer check on every cell.

Forces the index nested loop rather than the hash join by keeping the outer
side under HASH_SWITCH_MIN_OUTER (256 rows): every outer row does its own PK
probe into `w<W>`. `count(*)` means nothing of the inner survives the join,
so decode-time pruning should read one column (the PK the probe already
carries) instead of W.

Every cell asserts the count AND a value-bearing variant (`sum(w.c1)`), so a
mask that dropped a needed column shows up as a wrong answer, not a fast one.

Env: MPEDB_PYMOD, WORKDIR.
"""
import os
import statistics
import sys
import time

sys.path.insert(0, os.environ["MPEDB_PYMOD"])
import _native  # noqa: E402

WORKDIR = os.path.abspath(os.environ.get("WORKDIR", "./nlw-scratch"))
OUTER = 200          # < 256, so the hash switch stays off
INNER = 1000
WIDTHS = [2, 8, 20]
REPS = 9


def build():
    os.makedirs(WORKDIR, exist_ok=True)
    cfg = os.path.join(WORKDIR, "app.toml")
    dbp = os.path.join(WORKDIR, "db.mpedb").replace("\\", "/")
    for suffix in ("", ".wlock"):
        try:
            os.unlink(dbp + suffix)
        except FileNotFoundError:
            pass
    tables = ('\n[[table]]\nname = "outer"\nprimary_key = ["id"]\n'
              '  [[table.column]]\n  name = "id"\n  type = "int64"\n'
              '  [[table.column]]\n  name = "a"\n  type = "int64"\n')
    for w in WIDTHS:
        cols = "".join(f'  [[table.column]]\n  name = "c{i}"\n  type = "int64"\n'
                       for i in range(1, w))
        tables += (f'\n[[table]]\nname = "w{w}"\nprimary_key = ["id"]\n'
                   f'  [[table.column]]\n  name = "id"\n  type = "int64"\n{cols}')
    with open(cfg, "w") as f:
        f.write(f'[database]\npath = "{dbp}"\nsize_mb = 256\nmax_readers = 8\n{tables}')
    db = _native.Database(cfg)
    for i in range(OUTER):
        db.query("INSERT INTO outer (id, a) VALUES ($1, $2)", [i, i])
    for w in WIDTHS:
        names = ", ".join(["id"] + [f"c{i}" for i in range(1, w)])
        ph = ", ".join(f"${i+1}" for i in range(w))
        sql = f"INSERT INTO w{w} ({names}) VALUES ({ph})"
        for i in range(INNER):
            db.query(sql, [i] * w)
    return db


def timed(db, q, reps=REPS):
    got = db.query(q, [])
    out = []
    for _ in range(reps):
        t = time.perf_counter_ns()
        db.query(q, [])
        out.append(time.perf_counter_ns() - t)
    return statistics.median(out) / 1000.0, got


def main():
    db = build()
    print("## nested-loop probe: inner width vs wall time (median of %d, µs)" % REPS)
    print("width\tcount_µs\tsum_µs\tanswers")
    for w in WIDTHS:
        base = f"FROM outer JOIN w{w} ON outer.a = w{w}.id"
        c_us, c_got = timed(db, f"SELECT count(*) {base}")
        s_us, s_got = timed(db, f"SELECT sum(w{w}.c1) {base}")
        # outer.a = 0..199 all match; sum(c1) = sum(0..199) = 19900
        ok = (str(c_got).find(str(OUTER)) >= 0) and (str(s_got).find("19900") >= 0)
        print(f"{w}\t{c_us:.1f}\t{s_us:.1f}\t{'OK' if ok else f'WRONG c={c_got} s={s_got}'}")


if __name__ == "__main__":
    main()
