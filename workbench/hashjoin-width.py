#!/usr/bin/env python3
"""hashjoin-width.py — what decode-time pruning is worth on the hash join's
inner side, as a function of that side's WIDTH.

The hash-join branch used to gather its inner relation at full width because
the build key comes from the ACCESS path and the #125 mask is free to drop
it. Widening the mask by that one slot (`Mask::with_slot`) lets the scan
decode narrowly instead. The saving is whatever the unread columns cost to
materialise, so it is ~nothing on a narrow table and most of the read on a
wide one — this measures the slope.

Shape: `outer` (1000 rows, 2 cols) joined to `wide` (1000 rows, W cols) on
wide's PK, aggregated as count(*) so NOTHING of wide is observable past the
join. W in {2, 8, 20}. Same query, same data, one variable.

Env: MPEDB_PYMOD, WORKDIR. Run twice — once per arm — with
MPEDB_NO_HASH_SWITCH unset; the A/B is between engine builds, not env.
"""
import os
import statistics
import sys
import time

sys.path.insert(0, os.environ["MPEDB_PYMOD"])
import _native  # noqa: E402

WORKDIR = os.path.abspath(os.environ.get("WORKDIR", "./hjw-scratch"))
ROWS = 1000
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
    tables = (
        '\n[[table]]\nname = "outer"\nprimary_key = ["id"]\n'
        '  [[table.column]]\n  name = "id"\n  type = "int64"\n'
        '  [[table.column]]\n  name = "a"\n  type = "int64"\n')
    for w in WIDTHS:
        cols = "".join(
            f'  [[table.column]]\n  name = "c{i}"\n  type = "int64"\n'
            for i in range(1, w))
        tables += (f'\n[[table]]\nname = "w{w}"\nprimary_key = ["id"]\n'
                   f'  [[table.column]]\n  name = "id"\n  type = "int64"\n{cols}')
    with open(cfg, "w") as f:
        f.write(f'[database]\npath = "{dbp}"\nsize_mb = 256\nmax_readers = 8\n{tables}')
    db = _native.Database(cfg)
    for i in range(ROWS):
        db.query("INSERT INTO outer (id, a) VALUES ($1, $2)", [i, i])
    for w in WIDTHS:
        cols = ", ".join(["id"] + [f"c{i}" for i in range(1, w)])
        ph = ", ".join(f"${i+1}" for i in range(w))
        sql = f"INSERT INTO w{w} ({cols}) VALUES ({ph})"
        for i in range(ROWS):
            db.query(sql, [i] * w)
    return db


def main():
    db = build()
    print("## hash-join inner width vs wall time (median of %d, µs)" % REPS)
    print("width\tµs")
    for w in WIDTHS:
        q = f"SELECT count(*) FROM outer JOIN w{w} ON outer.a = w{w}.id"
        db.query(q, [])
        out = []
        for _ in range(REPS):
            t = time.perf_counter_ns()
            db.query(q, [])
            out.append(time.perf_counter_ns() - t)
        print(f"{w}\t{statistics.median(out) / 1000.0:.1f}")


if __name__ == "__main__":
    main()
