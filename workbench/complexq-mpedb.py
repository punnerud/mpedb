#!/usr/bin/env python3
"""complexq-mpedb.py — the mpedb arm of the complex-query cell
(benchmarks/routing.md, "Mot PostgreSQL: planleggingskost og gjentatte
kjøringer").

Uses the prebuilt PyO3 module IN-PROCESS (a CLI call would bury µs-scale
numbers under ~ms of process spawn + attach). `Database.prepare()` compiles on
EVERY call — only the registry insert is read-first (crates/mpedb/src/lib.rs
`prepare`) — so repeated-prepare medians measure compile, not a memo hit.
Refusal messages are additionally cross-checked against the release CLI
binary, which is the artifact users see.

Env:
  MPEDB_PYMOD  dir containing _native.so (copy of target/release/libmpedb_py.so)
  MPEDB_CLI    path to the release `mpedb` binary (refusal cross-check; optional)
  WORKDIR      scratch dir for the database file (default: ./complexq-scratch)

Durability is irrelevant here — everything measured is planning/compile or
warm read-only execution; no arm ever commits during a timed section.
"""
import importlib.util
import os
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.environ["MPEDB_PYMOD"])
import _native  # noqa: E402

_spec = importlib.util.spec_from_file_location(
    "complexq_gen",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "complexq-gen.py"))
gen = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(gen)

WORKDIR = os.path.abspath(os.environ.get("WORKDIR", "./complexq-scratch"))
CLI = os.environ.get("MPEDB_CLI")

REPS_PLAN = 9      # compile-time reps (median reported, 2 warmups discarded)
REPS_EXEC = 1000   # repeated-execution reps
WARM_EXEC = 100


def med_us(samples_ns):
    return statistics.median(samples_ns) / 1e3


def setup():
    os.makedirs(WORKDIR, exist_ok=True)
    dbfile = os.path.join(WORKDIR, "chain.mpedb")
    for suf in ("", "-shm", "-wal"):
        p = dbfile + suf
        if os.path.exists(p):
            os.unlink(p)
    cfg = os.path.join(WORKDIR, "chain.toml")
    parts = [f'[database]\npath = "{dbfile}"\nsize_mb = 256\nmax_readers = 8\n']
    for k in range(1, gen.NT + 1):
        parts.append(
            f'\n[[table]]\nname = "t{k}"\nprimary_key = ["id"]\n'
            f'\n  [[table.column]]\n  name = "id"\n  type = "int64"\n'
            f'\n  [[table.column]]\n  name = "a"\n  type = "int64"\n')
    with open(cfg, "w") as f:
        f.write("".join(parts))
    db = _native.Database(cfg)
    for k in range(1, gen.NT + 1):
        for lo in range(0, gen.ROWS, 250):
            vals = ",".join(f"({i},{i})" for i in range(lo, lo + 250))
            db.execute(db.prepare(f"INSERT INTO t{k} VALUES {vals}"))
    return db, cfg


def time_prepare(db, sql, reps=REPS_PLAN):
    """Median compile time (µs) over reps, first 2 discarded as warmup."""
    out = []
    for _ in range(reps + 2):
        t0 = time.perf_counter_ns()
        db.prepare(sql)
        out.append(time.perf_counter_ns() - t0)
    return med_us(out[2:])


def time_query_wall(db, sql, reps=7):
    out = []
    for _ in range(reps + 2):
        t0 = time.perf_counter_ns()
        db.query(sql)
        out.append(time.perf_counter_ns() - t0)
    return med_us(out[2:])


def cli_probe(cfg, sql):
    """Run one statement through the release CLI; return (rc, last stderr/stdout line)."""
    if not CLI:
        return None
    r = subprocess.run([CLI, "exec", cfg, sql], capture_output=True, text=True,
                       timeout=600)
    line = (r.stderr.strip() or r.stdout.strip()).splitlines()
    return r.returncode, (line[-1] if line else "")


def main():
    db, cfg = setup()

    print("## claim 1 — compile time vs join count (median of "
          f"{REPS_PLAN}, µs)")
    print("n_tables\tcompile_us")
    # 9..13 included deliberately: the DP_FULL_MAX = 12 cliff
    # (crates/mpedb-sql/src/planner/mpee.rs) lives there.
    for n in (2, 4, 8, 9, 10, 11, 12, 13, 16, 17):
        print(f"{n}\t{time_prepare(db, gen.chain_sql(n)):.1f}")
    for n in (32, 64):
        sql = gen.self_chain_sql(n)
        print(f"{n} (self-alias)\t{time_prepare(db, sql, reps=5):.1f}")

    print("\n## claim 2 — repeated 12-table query, "
          f"{REPS_EXEC} timed runs after {WARM_EXEC} warmups (µs/call)")
    q12 = gen.chain_sql(12)
    q12p = gen.point_chain_sql(12)
    h12 = db.prepare(q12)
    h12p = db.prepare(q12p)
    rows = db.query(q12)
    print(f"# answer: {rows}, point: {db.query(q12p)}")
    for label, fn in (
        ("q12 query(sql) text-memo", lambda: db.query(q12)),
        ("q12 execute(hash)", lambda: db.execute(h12)),
        ("q12-point query(sql)", lambda: db.query(q12p)),
        ("q12-point execute(hash)", lambda: db.execute(h12p)),
        ("query('SELECT 1') floor", lambda: db.query("SELECT 1")),
    ):
        for _ in range(WARM_EXEC):
            fn()
        t = []
        for _ in range(REPS_EXEC):
            t0 = time.perf_counter_ns()
            fn()
            t.append(time.perf_counter_ns() - t0)
        print(f"{label}\tmedian {med_us(t):.2f}\tmean {sum(t)/len(t)/1e3:.2f}"
              f"\tp99 {statistics.quantiles(t, n=100)[98]/1e3:.2f}")

    print("\n## claim 3 — nested subqueries (compile µs, query wall µs, answer)")
    print("shape\tdepth\tcompile_us\twall_us\tanswer")
    for depth in (3, 4, 5):
        for name, q in (("uncorrelated-IN", gen.uncorrelated_sql(depth)),
                        ("correlated-EXISTS", gen.correlated_sql(depth))):
            try:
                c = time_prepare(db, q, reps=7)
                w = time_query_wall(db, q)
                ans = db.query(q)[0][0]
                print(f"{name}\t{depth}\t{c:.1f}\t{w:.1f}\t{ans}")
            except Exception as e:  # refusals are data
                print(f"{name}\t{depth}\tREFUSED\t-\t{e}")

    print("\n## claim 4 — limit probes (py module, then CLI cross-check)")
    for name, sql in gen.LIMIT_PROBES.items():
        try:
            ans = db.query(sql)
            out = f"OK answer={ans[0]}"
        except Exception as e:
            out = f"REFUSED: {e}"
        print(f"{name}\t{out}")
        c = cli_probe(cfg, sql)
        if c is not None:
            rc, line = c
            print(f"{name} [cli rc={rc}]\t{line}")


if __name__ == "__main__":
    main()
