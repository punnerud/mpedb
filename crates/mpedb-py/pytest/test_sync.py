#!/usr/bin/env python3
"""mpedb ⇄ mpedb sync, from Python (#157).

The point of this file is that the *embedded* case is the real one: an
application holds its own database, works against it with the ordinary API, and
reconciles with a central instance. Nothing here reaches for a server.

Run with the built module on PYTHONPATH::

    cargo build --release -p mpedb-py
    mkdir -p /tmp/mpedb-pymod/mpedb
    cp -r crates/mpedb-py/python/mpedb/* /tmp/mpedb-pymod/mpedb/
    cp target/release/libmpedb_py.so /tmp/mpedb-pymod/mpedb/_native.so
    PYTHONPATH=/tmp/mpedb-pymod python3 crates/mpedb-py/pytest/test_sync.py

Exits non-zero on the first failure; the exit code is the result, never the
output.
"""

import os
import shutil
import sys
import tempfile

import mpedb

SCHEMA = """
[[table]]
name = "note"
primary_key = ["id"]
  [[table.column]]
  name = "id"
  type = "int64"
  [[table.column]]
  name = "body"
  type = "text"
"""


def node(workdir, tag, role):
    """Open one instance in `role`. The role is the ONLY difference."""
    cfg = os.path.join(workdir, f"{tag}.toml")
    db = os.path.join(workdir, f"{tag}.mpedb")
    for p in (db, db + "-wal"):
        if os.path.exists(p):
            os.remove(p)
    with open(cfg, "w") as f:
        f.write(
            f'[database]\npath = "{db}"\nsize_mb = 16\nmax_readers = 8\n\n'
            f'[sync]\nrole = "{role}"\n{SCHEMA}'
        )
    return mpedb.Database(cfg)


def put(db, i, body):
    db.query("INSERT OR REPLACE INTO note (id, body) VALUES ($1, $2)", [i, body])


def body_of(db, i):
    rows = db.query("SELECT body FROM note WHERE id = $1", [i])
    return rows[0][0] if rows else None


def test_roles_are_visible_and_change_nothing(w):
    """Every role runs the same statements and gets the same answers."""
    for role in ("standalone", "replica", "authority"):
        db = node(w, f"role-{role}", role)
        assert db.role == role, (db.role, role)
        put(db, 1, "hello")
        assert body_of(db, 1) == "hello"


def test_two_replicas_converge(w):
    """The load-bearing one: all three end byte-identical."""
    up = node(w, "conv-up", "authority")
    r1 = node(w, "conv-r1", "replica")
    r2 = node(w, "conv-r2", "replica")
    r1.sync_enable(up, ["note"])
    r2.sync_enable(up, ["note"])

    put(r1, 1, "from one")
    put(r2, 2, "from two")
    for _ in range(2):
        r1.sync(up, ["note"], link=1)
        r2.sync(up, ["note"], link=2)

    want = up.fingerprint(["note"])
    assert r1.fingerprint(["note"]) == want, "replica 1 diverged"
    assert r2.fingerprint(["note"]) == want, "replica 2 diverged"
    assert body_of(r2, 1) == "from one"
    assert body_of(r1, 2) == "from two"


def test_long_offline_catch_up_is_per_row(w):
    """100x the editing, the same catch-up: the change log is coalesced."""
    up = node(w, "off-up", "authority")
    r = node(w, "off-r", "replica")
    r.sync_enable(up, ["note"])

    for rnd in range(40):
        for i in range(25):
            put(up, 100 + i, f"v{rnd}-{i}")

    lag = r.sync_lag(up, ["note"], link=1)
    assert lag == 25, f"lag must be CHANGED ROWS, not edits: {lag}"
    r.sync(up, ["note"], link=1)
    assert r.fingerprint(["note"]) == up.fingerprint(["note"])
    assert r.sync_lag(up, ["note"], link=1) == 0


def test_conflict_policy_is_the_callers_choice(w):
    """Both policies, both reported. A conflict is never silent."""
    for policy, winner in (("upstream-wins", "theirs"), ("local-wins", "mine")):
        up = node(w, f"pol-up-{policy}", "authority")
        r = node(w, f"pol-r-{policy}", "replica")
        r.sync_enable(up, ["note"])
        put(up, 1, "seed")
        r.sync(up, ["note"], link=1)

        put(r, 1, "mine")
        put(up, 1, "theirs")
        rep = r.sync(up, ["note"], link=1, resolve=policy)
        assert rep["conflicts"] == 1, f"{policy}: conflict not reported: {rep}"
        assert body_of(up, 1) == winner, f"{policy}: wrong winner: {body_of(up, 1)}"


def test_sub_edits_merge_instead_of_clobbering(w):
    """The cell plane: many editors inside ONE value, ordered by `seq`."""
    up = node(w, "cell-up", "authority")
    put(up, 9, "AAAA....BBBB....CCCC")
    verdicts = up.submit_batch(
        "note",
        "body",
        [
            {"editor": 1, "seq": 10, "key": 9, "at": 0, "remove": 4, "insert": "aa"},
            {"editor": 2, "seq": 11, "key": 9, "at": 8, "remove": 4, "insert": "bb"},
            {"editor": 3, "seq": 12, "key": 9, "at": 16, "remove": 4, "insert": "cc"},
        ],
    )
    assert verdicts == ["committed"] * 3, verdicts
    assert body_of(up, 9) == "aa....bb....cc", body_of(up, 9)


def test_arrival_order_does_not_change_the_result(w):
    """The same edits, delivered in the opposite order, give the same text."""
    edits = [
        {"editor": 1, "seq": 10, "key": 9, "at": 0, "remove": 4, "insert": "aa"},
        {"editor": 2, "seq": 11, "key": 9, "at": 8, "remove": 4, "insert": "bb"},
        {"editor": 3, "seq": 12, "key": 9, "at": 16, "remove": 4, "insert": "cc"},
    ]
    outs = []
    for k, order in enumerate((edits, list(reversed(edits)))):
        up = node(w, f"ord-{k}", "authority")
        put(up, 9, "AAAA....BBBB....CCCC")
        up.submit_batch("note", "body", order)
        outs.append(body_of(up, 9))
    assert outs[0] == outs[1], f"arrival order changed the document: {outs}"


def test_a_replica_commits_provisionally(w):
    """`provisional` = it stands locally, no authority has confirmed it."""
    r = node(w, "prov-r", "replica")
    s = node(w, "prov-s", "standalone")
    for db in (r, s):
        put(db, 1, "0123456789")
    edit = [{"editor": 5, "seq": 1, "key": 1, "at": 0, "remove": 4, "insert": "XXXX"}]
    assert r.submit_batch("note", "body", edit) == ["provisional"]
    assert s.submit_batch("note", "body", edit) == ["committed"]
    # Either way the edit landed: the verdict is about who confirmed it.
    for db in (r, s):
        assert body_of(db, 1) == "XXXX456789"


def test_seed_bootstraps_an_existing_database(w):
    """`sync_enable` is not retroactive; `sync_seed` is how you attach to data
    that already exists. Without it the replica stays empty and nothing says so."""
    up = node(w, "seed-up", "authority")
    r = node(w, "seed-r", "replica")
    for i in range(20):
        put(up, i, f"pre-existing-{i}")

    r.sync_enable(up, ["note"])
    assert r.sync_lag(up, ["note"], link=1) == 0, "nothing to find, and no warning"
    assert r.sync(up, ["note"], link=1)["pulled"] == 0

    assert r.sync_seed(up, ["note"], link=1) == 20
    assert r.fingerprint(["note"]) == up.fingerprint(["note"])

    # And ordinary sync carries on without replaying the copy.
    put(up, 999, "after the seed")
    assert r.sync(up, ["note"], link=1)["pulled"] == 1
    assert r.fingerprint(["note"]) == up.fingerprint(["note"])


def test_lag_is_about_content_not_cursors(w):
    """`lag` answers "is there anything I do not have", not "has my cursor moved".

    So it does not count the replica's own push back at it — a write-heavy
    replica showing a permanent "syncing 7…" was the bug this pins — and a
    second link that has never run reports 0 once the data already matches."""
    up = node(w, "lag-up", "authority")
    r = node(w, "lag-r", "replica")
    r.sync_enable(up, ["note"])

    for i in range(7):
        put(r, i, f"local {i}")
    assert r.sync_lag(up, ["note"], link=1) == 0, "local work is not lag"

    r.sync(up, ["note"], link=1)
    assert r.sync_lag(up, ["note"], link=1) == 0, "our own push was reported as lag"
    # A fresh link over data that already agrees is also not behind.
    assert r.sync_lag(up, ["note"], link=2) == 0

    put(up, 99, "genuinely new")
    assert r.sync_lag(up, ["note"], link=1) == 1, "a real upstream change must show"


def test_an_unknown_role_is_refused(w):
    """A typo in a deployment knob must not silently mean `standalone`."""
    try:
        node(w, "bad-role", "primary")
    except Exception as e:  # noqa: BLE001 - the type is the binding's business
        msg = str(e)
        assert "primary" in msg, msg
        assert "standalone" in msg and "replica" in msg, msg
        return
    raise AssertionError("an unknown sync role opened silently")


def main():
    w = tempfile.mkdtemp(prefix="mpedb-pysync-")
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    failed = 0
    try:
        for t in tests:
            try:
                t(w)
                print(f"ok   {t.__name__}")
            except Exception as e:  # noqa: BLE001
                failed += 1
                print(f"FAIL {t.__name__}: {e}")
    finally:
        shutil.rmtree(w, ignore_errors=True)
    print(f"\n{len(tests) - failed}/{len(tests)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
