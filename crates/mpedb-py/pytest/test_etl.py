#!/usr/bin/env python3.12
"""Reversible ETL through the Python surface — the PYSPELL-ETL.md contract.

Plain Python, no pytest. Run with the built module on PYTHONPATH:

    cargo build --release -p mpedb-py
    mkdir -p /tmp/mpedb-pymod && cp target/release/libmpedb_py.so /tmp/mpedb-pymod/mpedb.so
    PYTHONPATH=/tmp/mpedb-pymod python3.12 crates/mpedb-py/pytest/test_etl.py

This is the guide's image walkthrough, executed: strip colour from packed-RGB
pixels (the chroma offsets are the residual), let the "user" retouch and crop
the grayscale, and put the colour back ONTO THE EDITS. Plus the refusals that
make the guarantees real: a fake bijection is refused with a counter-example,
revert refuses an edited column, putback refuses an edit outside the pair's
image.
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
name = "pixels"
primary_key = ["id"]

  [[table.column]]
  name = "id"
  type = "int64"

  [[table.column]]
  name = "px"
  type = "any"
"""

# The grayscale pair from PYSPELL-ETL.md, verbatim. The domain guards are the
# verifier's demands, one refusal at a time: huge ints overflow the chroma
# packing, fractional floats break exact recovery, and 0 is excluded because
# +0.0 and -0.0 collide there (a GENUINE non-injectivity the verifier found).
GUARD = """\
    if px < 1:
        return 1 // 0
    if px > 16777215:
        return 1 // 0
    if px % 1 != 0:
        return 1 // 0
"""

TO_GRAY = f"""\
def to_gray(px):
{GUARD}    r = px // 65536
    g = (px // 256) % 256
    b = px % 256
    return (r + g + b) // 3
"""

CHROMA = f"""\
def chroma(px):
{GUARD}    r = px // 65536
    g = (px // 256) % 256
    b = px % 256
    y = (r + g + b) // 3
    return ((r - y + 255) * 512 + (g - y + 255)) * 512 + (b - y + 255)
"""

RECOLOR = """\
def recolor(y, c):
    b = c % 512 - 255 + y
    g = (c // 512) % 512 - 255 + y
    r = (c // 512 // 512) - 255 + y
    return r * 65536 + g * 256 + b
"""


def px(r, g, b):
    return r * 65536 + g * 256 + b


def column(db):
    return [row[0] for row in db.query("SELECT px FROM pixels ORDER BY id")]


def main(workdir):
    dbpath = os.path.join(workdir, "etl.mpedb")
    cfg = os.path.join(workdir, "etl.toml")
    with open(cfg, "w") as f:
        f.write(CONFIG.format(dbpath=dbpath))
    db = mpedb.Database(cfg)

    # 1. Store the pair's three functions. Name and arity come from the defs.
    assert db.define_function(TO_GRAY)[0] == "to_gray"
    assert db.define_function(CHROMA)[0] == "chroma"
    assert db.define_function(RECOLOR)[0] == "recolor"

    # 2. A fake bijection is REFUSED with a counter-example — the verifier is
    # not a rubber stamp, and an agent should see what that looks like.
    try:
        db.create_lens("gray_wrong", "to_gray", "recolor")
        raise AssertionError("a lossy forward must not register as bijective")
    except mpedb.Error as e:
        assert "forward/1 and inverse/1" in str(e) or "not bijective" in str(e), e

    # 3. The residual triple registers, and the sample count is REPORTED
    # (statistical evidence, never a bare "verified").
    samples = db.create_residual_lens("gray", "to_gray", "chroma", "recolor", "any")
    # A narrow-domain pair exercises FEW probe values (the mixed corpus is
    # mostly outside 1..=0xFFFFFF) — and the count SAYS so instead of hiding
    # it. The apply itself then verifies 100% of the real rows, which is the
    # verification that matters.
    assert samples >= 5, samples
    listed = db.lenses()
    assert listed[0]["name"] == "gray" and listed[0]["class"] == "residual"

    # 4. Seed four pixels and strip the colour.
    original = [px(200, 40, 40), px(30, 180, 60), px(20, 60, 200), px(90, 90, 90)]
    with db.begin() as tx:
        for i, p in enumerate(original):
            tx.query("INSERT INTO pixels (id, px) VALUES ($1, $2)", [i, p])
    report = db.etl_apply("gray", "pixels", "px")
    assert report["rows"] == 4 and report["residuals"] == 4, report
    assert column(db) == [93, 90, 93, 90], "luma only — the colour is in etl_residual now"

    # 5. The user works in grayscale: darken pixel 0, brighten pixel 1, CROP
    # pixel 3.
    with db.begin() as tx:
        tx.query("UPDATE pixels SET px = $1 WHERE id = 0", [93 - 20])
        tx.query("UPDATE pixels SET px = $1 WHERE id = 1", [90 + 10])
        tx.query("DELETE FROM pixels WHERE id = 3")

    # 6. revert refuses the edited column — putback is the operation for it.
    try:
        db.etl_revert(report["run_id"])
        raise AssertionError("revert must refuse a column edited outside the pipeline")
    except mpedb.Error as e:
        assert "changed outside the pipeline" in str(e), e

    back = db.etl_putback(report["run_id"])
    assert back["rows"] == 3, back
    after = column(db)
    assert after[0] == px(180, 20, 20), "darkened by 20 per channel WITH its colour back"
    assert after[1] == px(40, 190, 70), "brightened by 10 per channel WITH its colour back"
    assert after[2] == original[2], "the unedited pixel round-trips exactly"
    assert len(after) == 3, "the cropped pixel stays gone"

    outcomes = [l["outcome"] for l in db.etl_log()]
    assert outcomes == ["putback"], outcomes

    # 7. An edit the pair cannot carry back is refused with the row named.
    # Pixel 0 is now (180,20,20): chroma offsets +107/-53/-53, so its LEGAL
    # luma range is 53..=148 — darker and a channel goes negative, which the
    # packing cannot represent. PutRes catches BOTH bad edits: the fractional
    # luma 12.5 (no integer pixel) and the integer luma 12 (channel
    # underflow: forward(inverse(12, r)) comes back 182, not 12).
    report2 = db.etl_apply("gray", "pixels", "px")
    for bad in (12.5, 12):
        with db.begin() as tx:
            tx.query("UPDATE pixels SET px = $1 WHERE id = 0", [bad])
        try:
            db.etl_putback(report2["run_id"])
            raise AssertionError(f"luma {bad} has no pixel preimage and must be refused")
        except mpedb.Error as e:
            assert "putback" in str(e), e
    # The failed putbacks rolled back atomically. A legal darken (73 -> 60,
    # inside 53..=148) carries back.
    with db.begin() as tx:
        tx.query("UPDATE pixels SET px = $1 WHERE id = 0", [60])
    db.etl_putback(report2["run_id"])
    after2 = column(db)
    assert after2[0] == px(60 + 107, 60 - 53, 60 - 53), "legal edit carried back"
    assert len(after2) == 3

    print("etl: all assertions passed")


if __name__ == "__main__":
    workdir = sys.argv[1] if len(sys.argv) > 1 else tempfile.mkdtemp(prefix="mpedb-etl-")
    main(workdir)
