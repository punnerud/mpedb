#!/usr/bin/env python3.12
"""Ingest through the Python surface — the INGEST-GUIDE.md contract.

Plain Python, no pytest. Run with the built module on PYTHONPATH:

    cargo build --release -p mpedb-py
    mkdir -p /tmp/mpedb-pymod && cp target/release/libmpedb_py.so /tmp/mpedb-pymod/mpedb.so
    PYTHONPATH=/tmp/mpedb-pymod python3.12 crates/mpedb-py/pytest/test_ingest.py

This is the guide's fetcher, executed against a fake "external system" that
lives in a dict: a delta that lies about `updated_at`, the dump that catches
it, the cascade that turns one root call's keys into per-key calls under a
budget, and the plan that comes out the other end. No HTTP — the shape of the
host loop is the point, and mpedb never makes a call itself.
"""

import os
import sys
import tempfile

import mpedb

CONFIG = """\
[database]
path = "{dbpath}"
size_mb = 32
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
"""

# The call graph: a cheap delta over cases, the dump that reconciles it (and
# tries its cursor), and a derived edge whose PARAMETERS come from the delta.
SOURCE = """
[source]
name = "sf"
policy = "source"

[[source.budget]]
profile = "work"
hours = "6-18"
window_secs = 300
calls = 50

[[source.budget]]
profile = "off"
window_secs = 300
calls = 50

[[source.edge]]
name = "cases_delta"
kind = "root"
table = "cases"
strategy = "delta"
cursor = "updated_at"
cost_calls = 1
cost_bytes = 2000

[[source.edge]]
name = "cases_full"
kind = "root"
table = "cases"
strategy = "dump"
cost_calls = 1
cost_bytes = 40000

[[source.edge]]
name = "case_contracts"
kind = "derived"
parent = "cases_delta"
table = "contracts"
strategy = "dump"
batch = 2
cost_calls = 1
cost_bytes = 3000
"""

# The "external system". `bumped` is what its API would show as updated_at —
# and row 3's update does NOT bump it, which is the whole problem.
EXTERNAL = {
    1: {"subject": "printer", "updated_at": 10},
    2: {"subject": "vpn", "updated_at": 11},
    3: {"subject": "badge", "updated_at": 5},
}
CONTRACTS = {1: [(101, 5000)], 2: [(102, 250)], 3: [(103, 900)]}


def rows_of(d):
    return [{"id": k, **v} for k, v in sorted(d.items())]


def ask_delta(db, edge="cases_delta"):
    """What a real fetcher sends: everything past where this edge stood.

    The watermark comes from mpedb, not from the fetcher's memory — that is
    the whole point of a receipt.
    """
    st = db.ingest_state("sf").get(edge, {})
    wm = st.get("watermark")
    return [r for r in rows_of(EXTERNAL) if wm is None or r["updated_at"] > wm]


def main(workdir):
    dbpath = os.path.join(workdir, "ingest.mpedb")
    cfg = os.path.join(workdir, "ingest.toml")
    with open(cfg, "w") as f:
        f.write(CONFIG.format(dbpath=dbpath))
    db = mpedb.Database(cfg)
    db.ingest_define(SOURCE)
    assert "sf" in db.ingest_sources()

    # ---------------------------------------------------------------- first pull
    # Nothing is known yet, so the first receipt is a dump: it is the only
    # receipt that can see the whole table.
    r = db.ingest_dump("sf", "cases_full", rows_of(EXTERNAL), calls=1, bytes=900)
    assert (r["inserted"], r["updated"], r["deleted"]) == (3, 0, 0), r

    # The first delta has nowhere to start from, so it asks for everything and
    # takes its position from what came back.
    r = db.ingest_delta("sf", "cases_delta", ask_delta(db), calls=1, bytes=300)
    assert (r["inserted"], r["updated"], r["unchanged"]) == (0, 0, 3), r
    assert r["watermark"] == 11, r

    # ------------------------------------------------------------ the lying cursor
    # Two changes in the external system. Row 2 bumps its updated_at like an
    # honest API; row 3 does not — an update-without-touch, which is the
    # single most common way a sync silently loses data.
    EXTERNAL[2] = {"subject": "vpn (escalated)", "updated_at": 20}
    EXTERNAL[3] = {"subject": "badge (reissued)", "updated_at": 5}

    # So the delta sees row 2 and nothing else. Row 3 is invisible to it —
    # forever, since its stamp never moves again.
    r = db.ingest_delta("sf", "cases_delta", ask_delta(db), calls=1, bytes=300)
    assert (r["inserted"], r["updated"]) == (0, 1), r
    assert r["complete"] is False and r["deleted"] == 0, "a delta cannot see deletes"

    # The dump reconciles — and while it is here, it tries the delta's cursor
    # against where that delta stood. Row 3 changed without moving the cursor.
    r = db.ingest_dump("sf", "cases_full", rows_of(EXTERNAL), calls=1, bytes=900)
    assert r["updated"] == 1, r
    assert r["cursor_state"] == "unsafe", r
    assert r["missed"] == 1, r

    st = db.ingest_state("sf")
    assert st["cases_delta"]["cursor_state"] == "unsafe", st
    assert st["cases_delta"]["missed"] == 1, st

    # ---------------------------------------------------------------- the cascade
    # A delta whose keys DRIVE a second call. The follow-ups are queued in the
    # same transaction as the rows that produced them, so a crash in between
    # cannot lose a branch.
    EXTERNAL[4] = {"subject": "laptop", "updated_at": 30}
    EXTERNAL[5] = {"subject": "door", "updated_at": 31}
    CONTRACTS[4] = [(104, 75)]
    CONTRACTS[5] = [(105, 1200)]
    run = db.ingest_begin("sf", "cases_delta", "delta")
    fresh = ask_delta(db)
    db.ingest_rows(run, fresh, calls=1, bytes=300)
    queued = db.ingest_derive(run, "case_contracts", [r["id"] for r in fresh])
    assert queued == 2, queued
    db.ingest_finish(run)

    assert db.ingest_pending("sf")["case_contracts"]["waiting"] == 2

    # The host loop: take work while the budget allows, make the call, push
    # what came back, retire the batch. `ingest_next` returning None is the
    # budget working — not an error.
    made = 0
    while True:
        t = db.ingest_next("sf")
        if t is None:
            break
        assert t["edge"] == "case_contracts" and t["table"] == "contracts", t
        rows = [
            {"id": cid, "case_id": k, "amount": amt}
            for k in t["keys"]
            for (cid, amt) in CONTRACTS[k]
        ]
        # A derived receipt is SCOPED to the keys it was asked about, so it
        # goes through the delta door: it upserts, and never infers a delete.
        db.ingest_delta("sf", "case_contracts", rows, calls=1, bytes=400)
        db.ingest_done("sf", t["lease"])
        made += 1
        assert made < 10, "the queue is not draining"

    assert db.ingest_pending("sf") == {}, db.ingest_pending("sf")
    got = db.query("SELECT id, case_id FROM contracts ORDER BY id")
    assert [tuple(x) for x in got] == [(104, 4), (105, 5)], got

    # Fan-out is MEASURED, never declared: two keys from one parent call.
    assert db.ingest_state("sf")["case_contracts"]["fanout"] == 2.0

    # The budget is what stopped nothing here — but it is measured, and
    # `ingest_next` returning None when it runs out is the budget working.
    b = db.ingest_budget_left("sf")
    assert b["window_secs"] == 300 and 0 < b["calls"] < 50, b

    # A lease that dies with its worker comes back.
    db.ingest_derive(run, "case_contracts", [1])
    t = db.ingest_next("sf")
    assert t is not None and db.ingest_next("sf") is None, "leased keys go out once"
    assert db.ingest_reap("sf", older_than_secs=0) == 1
    assert db.ingest_next("sf") is not None

    # ------------------------------------------------------------------ the plan
    plan = db.ingest_advise("sf", cmd="./fetch.py")
    assert plan["source"] == "sf"
    assert plan["cron"], "a plan with no cron line is not a plan"
    for p in plan["profiles"]:
        names = {e["edge"] for e in p["edges"]}
        assert "cases_full" in names and "cases_delta" in names, names
        for e in p["edges"]:
            if e["edge"] == "cases_delta":
                assert "UNSAFE" in e["reason"], e
            if e["kind"] == "derived":
                # Derived edges are not scheduled: their rate IS the parent's
                # rate times the observed fan-out.
                assert e["cron"] == "", e
        assert p["verdict"], p

    print("ingest: all assertions passed")


if __name__ == "__main__":
    workdir = sys.argv[1] if len(sys.argv) > 1 else tempfile.mkdtemp(prefix="mpedb-ingest-")
    main(workdir)
