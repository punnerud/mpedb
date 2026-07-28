#!/usr/bin/env python3.12
"""Three ways to keep a database in sync, measured on the same world.

Every arm runs against its own copy of the same seeded external system, so
the world evolves identically no matter what the client does. What differs
is only WHEN each call is made:

  naive    full dump of every table, every window. Always converges, and
           ignores the budget by construction — the correctness control arm.
           If IT is wrong, the harness is broken, not the client.
  uniform  every root edge at the same rate under the budget. The theory's
           control arm, and a strong one: uniform beats
           proportional-to-change-rate under every change-rate distribution
           (Cho & García-Molina, TODS'03, Thm 5.1/5.2).
  planned  the rates `db.ingest_advise` computes, re-read every window as
           observations accumulate.

Measured: rows wrong at every window boundary (the mean IS staleness, which
is what the planner optimises; the final value is where it ended up), and
calls and bytes as counted BY THE SOURCE — never self-reported.

    PYTHONPATH=<dir with mpedb package> python3.12 bench.py --ticks 200
"""

import argparse
import os
import shutil
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from system import System  # noqa: E402

import mpedb  # noqa: E402

CASE_PAGE = 100
CONTRACT_PAGE = 200

CONFIG = """\
[database]
path = "{dbpath}"
size_mb = 256
max_readers = 8
durability = "none"

[[table]]
name = "cases"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "subject"
  type = "any"

  [[table.column]]
  name = "state"
  type = "any"

  [[table.column]]
  name = "updated_at"
  type = "any"

[[table]]
name = "contracts"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "case_id"
  type = "any"

  [[table.column]]
  name = "amount"
  type = "any"

  [[table.column]]
  name = "updated_at"
  type = "any"
"""

SOURCE = """
[source]
name = "ext"
policy = "source"

[[source.budget]]
profile = "all"
window_secs = {window}
calls = {calls}

[[source.edge]]
name = "cases_delta"
kind = "root"
table = "cases"
strategy = "delta"
cursor = "updated_at"
overlap_secs = 1
cost_calls = 1
cost_bytes = 1200

[[source.edge]]
name = "cases_full"
kind = "root"
table = "cases"
strategy = "dump"
cost_calls = {case_pages}
cost_bytes = {case_bytes}

[[source.edge]]
name = "contracts_full"
kind = "root"
table = "contracts"
strategy = "dump"
cost_calls = {contract_pages}
cost_bytes = {contract_bytes}

[[source.edge]]
name = "case_contracts"
kind = "derived"
parent = "cases_delta"
table = "contracts"
strategy = "dump"
batch = 20
cost_calls = 1
cost_bytes = 1200
"""


class Arm:
    """One client under test: its own mpedb file, its own copy of the world."""

    def __init__(self, name, workdir, args, knobs):
        self.name = name
        self.sys = System(seed=args.seed, initial=args.initial, **knobs)
        self.window = args.window
        self.budget = args.budget
        self.samples = []
        dbpath = os.path.join(workdir, f"{name}.mpedb")
        cfg = os.path.join(workdir, f"{name}.toml")
        with open(cfg, "w") as f:
            f.write(CONFIG.format(dbpath=dbpath))
        self.db = mpedb.Database(cfg)
        cases = max(1, args.initial // CASE_PAGE)
        # window_secs is wall-clock for the planner (and its cron floor);
        # `--window` is how many ticks of the world go by in one. A tick is
        # whatever you want it to be — here, 300/window seconds of it.
        self.db.ingest_define(SOURCE.format(
            window=args.window_secs, calls=args.budget,
            case_pages=cases, case_bytes=cases * 12000,
            contract_pages=max(1, args.initial // CONTRACT_PAGE),
            contract_bytes=max(1, args.initial // CONTRACT_PAGE) * 24000))
        self.spent = 0
        # The budget is an ALLOWANCE that accrues. A dump that costs more
        # than one window's budget is not forbidden — it waits until enough
        # has accrued. Without this an arm can start an 8-call dump against
        # a 4-call budget and quietly spend double, which is how a "cheaper"
        # arm turns out to have been cheating.
        self.allow = 0
        self.declared = {
            "cases_delta": 1,
            "cases_full": cases,
            "contracts_full": max(1, args.initial // CONTRACT_PAGE),
            "case_contracts": 1,
        }

    def afford(self, edge):
        return self.allow >= self.declared[edge]

    # ------------------------------------------------------------ the calls
    #
    # This is the part a user writes: one function per edge, each making the
    # call and handing mpedb what came back. Everything else is bookkeeping
    # mpedb does.

    def pull_delta(self):
        st = self.db.ingest_state("ext").get("cases_delta", {})
        wm = st.get("watermark")
        since = 0 if wm is None else max(0, int(wm) - 1)   # the mandatory overlap
        rows = self.sys.cases_since(since)
        run = self.db.ingest_begin("ext", "cases_delta", "delta")
        self.db.ingest_rows(run, rows, calls=1, bytes=0)
        if rows:
            self.db.ingest_derive(run, "case_contracts", [r["id"] for r in rows])
        self.db.ingest_finish(run)
        self.spent += 1
        return 1

    def pull_dump(self, table):
        edge = "cases_full" if table == "cases" else "contracts_full"
        size = CASE_PAGE if table == "cases" else CONTRACT_PAGE
        run = self.db.ingest_begin("ext", edge, "dump")
        page, calls = 0, 0
        while True:
            rows = (self.sys.cases_page(page, size) if table == "cases"
                    else self.sys.contracts_page(page, size))
            calls += 1
            self.db.ingest_rows(run, rows, calls=1, bytes=0)
            if len(rows) < size:
                break
            page += 1
        self.db.ingest_finish(run)
        self.spent += calls
        return calls

    def drain_cascade(self, max_calls):
        """The derived half: keys from the delta become their own calls."""
        made = 0
        while made < max_calls:
            t = self.db.ingest_next("ext")
            if t is None:
                break
            rows = self.sys.contracts_for(t["keys"])
            self.db.ingest_delta("ext", t["edge"], rows, calls=1, bytes=0)
            self.db.ingest_done("ext", t["lease"])
            made += 1
        self.spent += made
        return made

    def call(self, edge):
        """Make one edge's call(s); returns what it actually cost."""
        if edge == "cases_delta":
            n = self.pull_delta()
            n += self.drain_cascade(int(self.allow) - n)
        else:
            n = self.pull_dump("cases" if edge == "cases_full" else "contracts")
        self.allow -= n
        return n

    # ------------------------------------------------------------ the oracle

    def wrong(self):
        """Rows that differ from the source. The source is the truth; a
        client that reads `truth()` during the run is cheating."""
        bad = 0
        for table, cols in (("cases", ("subject", "state")),
                            ("contracts", ("case_id", "amount"))):
            truth = self.sys.truth(table)
            got = {r[0]: r for r in self.db.query(
                f"SELECT id, {', '.join(cols)} FROM {table} ORDER BY id")}
            for k, row in truth.items():
                if k not in got:
                    bad += 1
                elif any(got[k][i + 1] != row[c] for i, c in enumerate(cols)):
                    bad += 1
            bad += len(set(got) - set(truth))       # rows the source deleted
        return bad

    def sample(self):
        self.samples.append(self.wrong())

    def report(self):
        c = self.sys.cost()
        n = len(self.samples) or 1
        return {"arm": self.name,
                "mean_wrong": sum(self.samples) / n,
                "final_wrong": self.samples[-1] if self.samples else self.wrong(),
                "calls": c["calls"], "bytes": c["bytes"],
                "rows": len(self.sys.truth("cases")),
                "by_endpoint": c["by_endpoint"]}


# ------------------------------------------------------------------- the arms
#
# Each returns nothing; it just spends up to `budget` calls this window.

ROOTS = ["cases_delta", "cases_full", "contracts_full"]


def spend_naive(a, _state):
    """Ignores the allowance on purpose: this arm buys correctness at any
    price, and exists so the harness can prove itself."""
    a.allow = 10**9
    a.call("cases_full")
    a.call("contracts_full")


def spend_uniform(a, state):
    """Round-robin: every root edge at the same rate, inside the allowance."""
    tries = 0
    while tries < len(ROOTS):
        edge = ROOTS[state["at"] % len(ROOTS)]
        if not a.afford(edge):
            tries += 1
            state["at"] += 1
            continue
        a.call(edge)
        state["at"] += 1
        tries = 0


def spend_planned(a, state):
    """What `ingest advise` says, re-read every window."""
    plan = a.db.ingest_advise("ext")
    owed = state.setdefault("owed", {})
    for p in plan["profiles"]:
        for e in p["edges"]:
            if e["kind"] == "root":
                owed[e["edge"]] = owed.get(e["edge"], 0.0) + e["rate_per_window"]
    # Most-owed first: the edge furthest past its interval goes now.
    progress = True
    while progress:
        progress = False
        for edge in sorted(owed, key=lambda k: -owed[k]):
            if owed[edge] >= 1.0 and a.afford(edge):
                a.call(edge)
                owed[edge] -= 1.0
                progress = True
    state["last_plan"] = plan


ARMS = {"naive": spend_naive, "uniform": spend_uniform, "planned": spend_planned}


def run(a, spend, ticks, window_secs):
    """The lab runs in REAL time on purpose.

    mpedb's budget is wall-clock: `ingest_next` hands out derived work only
    while this window's calls remain, and it decides that from receipt
    timestamps. Compressing a hundred simulated windows into two real
    seconds would put every receipt in one window and starve the cascade
    after the first few calls — measuring a fiction. So each window ends by
    waiting for the clock, and every arm meets the same gate a cron job
    would.
    """
    state = {"at": 0}
    started = time.monotonic()
    windows = 0
    for t in range(ticks):
        a.sys.tick()
        if (t + 1) % a.window == 0:
            a.allow += a.budget
            spend(a, state)
            a.sample()
            windows += 1
            left = started + windows * window_secs - time.monotonic()
            if left > 0:
                time.sleep(left)
    return state


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ticks", type=int, default=120)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--initial", type=int, default=800, help="rows to start with")
    ap.add_argument("--window", type=int, default=6, help="ticks per budget window")
    ap.add_argument("--window-secs", type=int, default=1,
                    help="REAL seconds per budget window — mpedb's budget is "
                         "wall-clock, so the lab waits for the clock")
    ap.add_argument("--budget", type=int, default=4, help="calls per window")
    ap.add_argument("--lying", type=int, default=25,
                    help="%% of updates that do NOT bump updated_at")
    ap.add_argument("--update-pct", type=int, default=3)
    ap.add_argument("--delete-pct", type=int, default=1)
    ap.add_argument("--insert-per-tick", type=int, default=4)
    ap.add_argument("--arms", default="naive,uniform,planned")
    ap.add_argument("--workdir", default=None)
    ap.add_argument("--plan", action="store_true", help="print the last plan")
    args = ap.parse_args()

    workdir = args.workdir or tempfile.mkdtemp(prefix="ingest-lab-")
    os.makedirs(workdir, exist_ok=True)
    knobs = {"lying_pct": args.lying, "update_pct": args.update_pct,
             "delete_pct": args.delete_pct, "insert_per_tick": args.insert_per_tick}
    out, last = [], None
    try:
        for name in args.arms.split(","):
            a = Arm(name, workdir, args, knobs)
            st = run(a, ARMS[name], args.ticks, args.window_secs)
            out.append(a.report())
            if name == "planned":
                last = st.get("last_plan")
    finally:
        if not args.workdir:
            shutil.rmtree(workdir, ignore_errors=True)

    w = max(len(r["arm"]) for r in out)
    print(f"{'arm':<{w}}  {'mean wrong':>10}  {'final':>6}  {'calls':>7}  {'bytes':>10}")
    for r in out:
        print(f"{r['arm']:<{w}}  {r['mean_wrong']:>10.1f}  {r['final_wrong']:>6}  "
              f"{r['calls']:>7}  {r['bytes']:>10}")
    print(f"\n{out[0]['rows']} rows in the source at the end; "
          f"{args.ticks} ticks, window {args.window}, budget {args.budget} call(s), "
          f"{args.lying}% of updates lie about updated_at")
    base = next((r for r in out if r["arm"] == "naive"), None)
    if base and base["final_wrong"]:
        print(f"the control arm is WRONG by {base['final_wrong']} row(s) — the "
              f"harness is broken, not the client")
    for r in out:
        if base and r is not base:
            print(f"{r['arm']}: {r['calls'] / max(1, base['calls']):.2f}x the calls "
                  f"of a full dump every window, mean {r['mean_wrong']:.1f} row(s) "
                  f"behind")
    if args.plan and last:
        for p in last["profiles"]:
            for e in p["edges"]:
                print(f"  {e['edge']:<16} {e['rate_per_window']:>6.2f}/window  "
                      f"{e['reason']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
