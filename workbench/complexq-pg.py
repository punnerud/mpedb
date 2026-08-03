#!/usr/bin/env python3
"""complexq-pg.py — the PostgreSQL 16 arm of the complex-query cell
(benchmarks/routing.md, "Mot PostgreSQL: planleggingskost og gjentatte
kjøringer").

Creates a THROWAWAY cluster (initdb -A trust, port 5434, unix socket only,
never the system cluster on 5432), loads the same 17 chain tables as
complexq-mpedb.py, measures, then stops and deletes the cluster.

Planning time = server-side "Planning Time" from EXPLAIN (SUMMARY ON) — no
client overhead in the number. Repeated execution = pgbench -M prepared /
-M simple over the unix socket (the round trip IS PostgreSQL's operational
model; the SELECT-1 floor arm measures it separately).

Env:
  PGBIN    default /usr/lib/postgresql/16/bin
  WORKDIR  scratch dir for the cluster (default: ./complexq-scratch)
  KEEP     set to 1 to leave the cluster running (debugging)
"""
import importlib.util
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile

_spec = importlib.util.spec_from_file_location(
    "complexq_gen",
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "complexq-gen.py"))
gen = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(gen)

PGBIN = os.environ.get("PGBIN", "/usr/lib/postgresql/16/bin")
WORKDIR = os.path.abspath(os.environ.get("WORKDIR", "./complexq-scratch"))
PGDATA = os.path.join(WORKDIR, "pgdata")
# The socket dir must stay SHORT: sun_path caps a unix socket path at ~107
# bytes, and a deep WORKDIR breaks pg_ctl start with "could not create any
# Unix-domain sockets". mkdtemp directly under /tmp; removed in teardown.
PGSOCK = tempfile.mkdtemp(prefix="cqpg-")
PORT = os.environ.get("PGPORT", "5434")
REPS_PLAN = 9   # EXPLAIN reps per point; 2 extra warmups discarded
TXNS = 1000     # pgbench transactions per repeated-execution arm

ARMS = {
    # PostgreSQL as shipped: join_collapse_limit=8, geqo_threshold=12.
    "default": [],
    # The full search space at N=16 — collapse limits lifted, GEQO disabled,
    # so the planner runs exhaustive DP over the whole chain. This is the arm
    # where the space explodes; the default arm shows what ships.
    "exhaustive": ["SET join_collapse_limit = 64;",
                   "SET from_collapse_limit = 64;",
                   "SET geqo = off;"],
}


def psql(*args, db="chain", check=True, input=None):
    cmd = [os.path.join(PGBIN, "psql"), "-h", PGSOCK, "-p", PORT,
           "-U", "bench", "-d", db, "-qX"] + list(args)
    return subprocess.run(cmd, capture_output=True, text=True, check=check,
                          input=input, timeout=600)


def setup():
    if os.path.exists(PGDATA):
        subprocess.run([os.path.join(PGBIN, "pg_ctl"), "-D", PGDATA, "stop",
                        "-m", "immediate"], capture_output=True)
        shutil.rmtree(PGDATA)
    os.makedirs(WORKDIR, exist_ok=True)
    os.makedirs(PGSOCK, exist_ok=True)
    subprocess.run([os.path.join(PGBIN, "initdb"), "-D", PGDATA, "-A", "trust",
                    "-U", "bench", "--no-instructions"], check=True,
                   capture_output=True)
    with open(os.path.join(PGDATA, "postgresql.conf"), "a") as f:
        f.write(f"\nlisten_addresses = ''\nport = {PORT}\n"
                f"unix_socket_directories = '{PGSOCK}'\n"
                f"shared_buffers = 256MB\n")
    subprocess.run([os.path.join(PGBIN, "pg_ctl"), "-D", PGDATA, "-l",
                    os.path.join(WORKDIR, "pg.log"), "-w", "start"],
                   check=True, capture_output=True)
    psql("-c", "CREATE DATABASE chain;", db="postgres")
    ddl = []
    for k in range(1, gen.NT + 1):
        ddl.append(f"CREATE TABLE t{k} (id bigint PRIMARY KEY, a bigint);")
        ddl.append(f"INSERT INTO t{k} SELECT g, g FROM "
                   f"generate_series(0,{gen.ROWS - 1}) g;")
    ddl.append("VACUUM ANALYZE;")
    psql(input="\n".join(ddl))


def teardown():
    if os.environ.get("KEEP") == "1":
        print(f"# KEEP=1 — cluster left running on {PGSOCK}:{PORT}",
              file=sys.stderr)
        return
    subprocess.run([os.path.join(PGBIN, "pg_ctl"), "-D", PGDATA, "stop",
                    "-m", "fast"], capture_output=True)
    shutil.rmtree(PGDATA)
    shutil.rmtree(PGSOCK, ignore_errors=True)


def explain_times(sql, arm, analyze=False, reps=REPS_PLAN):
    """Median server-side Planning Time (and Execution Time if analyze) in ms."""
    kind = "ANALYZE, SUMMARY" if analyze else "SUMMARY ON"
    script = "".join(ARMS[arm]) + \
        "".join(f"EXPLAIN ({kind}) {sql};\n" for _ in range(reps + 2))
    out = psql(input=script).stdout
    plan = [float(x) for x in re.findall(r"Planning Time: ([\d.]+) ms", out)][2:]
    res = {"plan_ms": statistics.median(plan)}
    if analyze:
        ex = [float(x) for x in re.findall(r"Execution Time: ([\d.]+) ms", out)][2:]
        res["exec_ms"] = statistics.median(ex)
    return res


def pgbench(sql, mode):
    """Average latency (ms) over TXNS single-client transactions."""
    script = os.path.join(WORKDIR, "pgbench-q.sql")
    with open(script, "w") as f:
        f.write(sql + ";\n")
    cmd = [os.path.join(PGBIN, "pgbench"), "-h", PGSOCK, "-p", PORT,
           "-U", "bench", "-n", "-c", "1", "-M", mode, "-f", script, "chain"]
    subprocess.run(cmd + ["-t", "100"], capture_output=True, check=True)  # warmup
    out = subprocess.run(cmd + ["-t", str(TXNS)], capture_output=True,
                         text=True, check=True).stdout
    return float(re.search(r"latency average = ([\d.]+) ms", out).group(1))


def probe(sql):
    r = psql("-t", "-c", sql, check=False)
    if r.returncode == 0:
        return "OK answer=" + " ".join(r.stdout.split())
    return "ERROR: " + r.stderr.strip().splitlines()[0]


def main():
    setup()
    try:
        print("## claim 1 — server-side Planning Time vs join count "
              f"(median of {REPS_PLAN}, ms)")
        print("n_tables\tdefault\texhaustive")
        for n in (2, 4, 8, 12, 16, 17):
            sql = gen.chain_sql(n)
            d = explain_times(sql, "default")["plan_ms"]
            e = explain_times(sql, "exhaustive")["plan_ms"]
            print(f"{n}\t{d:.3f}\t{e:.3f}")

        print("\n## claim 2 — repeated 12-table query, pgbench 1 client, "
              f"{TXNS} txns (avg ms/txn incl. unix-socket round trip)")
        q12 = gen.chain_sql(12)
        q12p = gen.point_chain_sql(12)
        print(f"# answer: {psql('-t', '-c', q12).stdout.strip()} / "
              f"point: {psql('-t', '-c', q12p).stdout.strip()}")
        for label, sql in (("q12", q12), ("q12-point", q12p),
                           ("SELECT 1 floor", "SELECT 1")):
            for mode in ("prepared", "simple"):
                print(f"{label} -M {mode}\t{pgbench(sql, mode):.3f}")

        print("\n## claim 3 — nested subqueries, default arm "
              "(EXPLAIN ANALYZE medians, ms)")
        print("shape\tdepth\tplan_ms\texec_ms\tanswer")
        for depth in (3, 4, 5):
            for name, q in (("uncorrelated-IN", gen.uncorrelated_sql(depth)),
                            ("correlated-EXISTS", gen.correlated_sql(depth))):
                t = explain_times(q, "default", analyze=True, reps=7)
                ans = psql("-t", "-c", q).stdout.strip()
                print(f"{name}\t{depth}\t{t['plan_ms']:.3f}\t{t['exec_ms']:.3f}"
                      f"\t{ans}")

        print("\n## claim 4 — the same limit probes, PostgreSQL side")
        for name, sql in gen.LIMIT_PROBES.items():
            if name == "self-65":
                # 65-way self-join: legal in PG; EXPLAIN only (execution is
                # 64 hash joins over 1000 rows — fine, but the point is the
                # planner accepts it).
                r = psql("-c", f"EXPLAIN (SUMMARY ON) {sql}", check=False)
                m = re.search(r"Planning Time: ([\d.]+) ms", r.stdout)
                print(f"{name}\tOK plan_ms={m.group(1) if m else '?'}")
                continue
            print(f"{name}\t{probe(sql)}")
    finally:
        teardown()


if __name__ == "__main__":
    main()
