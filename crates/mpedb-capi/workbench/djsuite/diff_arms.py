"""Diff two arms of `run_suite.sh`: print the tests that fail ONLY under the
mpedb shim, bucketed by their (normalized) exception line and ranked by how many
tests each bucket unblocks.

    python3 diff_arms.py stock_g1.txt stock_g2.txt shim_g1.txt shim_g2.txt

The first half of the arguments is the baseline arm, the second half the shim
arm; the two halves must be the same length.
"""

import re, sys, collections
SEP = re.compile(r"^={20,}$", re.M)
def parse(path):
    txt = open(path, errors="replace").read()
    out = {}
    for block in SEP.split(txt):
        m = re.match(r"\s*(FAIL|ERROR): (\S+)", block)
        if not m: continue
        kind, tid = m.group(1), m.group(2)
        body = block.split("\n", 1)[1]
        body = re.split(r"^-{20,}\nRan \d+ tests", body, flags=re.M)[0]
        lines = [l for l in body.splitlines() if l.strip() and not re.match(r"^-{20,}$", l)]
        exc = ""
        for l in reversed(lines):
            if not l.startswith((" ", "\t")):
                exc = l.strip(); break
        out[tid] = (kind, exc, body)
    return out
def norm(e):
    e = re.sub(r"`[^`]*`", "`X`", e)
    e = re.sub(r"\d+", "N", e)
    return e[:140]
base, shim = {}, {}
n = (len(sys.argv)-1)//2
for p in sys.argv[1:1+n]: base.update(parse(p))
for p in sys.argv[1+n:]: shim.update(parse(p))
only = {k:v for k,v in shim.items() if k not in base}
print(f"base fails={len(base)}  shim fails={len(shim)}  shim-only={len(only)}  shared={len(set(base)&set(shim))}")
buckets = collections.Counter(); ex = {}
for tid,(kind,e,body) in only.items():
    k = norm(e); buckets[k]+=1; ex.setdefault(k, []).append(tid)
for e,c in buckets.most_common(50):
    print(f"{c:5d}  {e}\n         e.g. {ex[e][0]}")
