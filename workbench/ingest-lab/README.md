# ingest-lab — an external system that behaves badly, and three ways to sync it

The lab exists to answer one question with numbers: **does planning the
calls beat calling everything at the same rate, at the same budget?** It
also exists to be handed to someone who has read only `INGEST-GUIDE.md`, to
find out where that guide fails them.

Nothing here is a mock of mpedb. The client under test uses the real
library; what is faked is the external system — and only so it can be made
to misbehave on demand.

## What the external system does to you

`system.py` is sqlite plus a deterministic mutator (xorshift64\*, seeded —
same seed, same world). Every knob is a real failure somebody ships:

| knob | what it models |
|---|---|
| `lying_pct` | updates that do NOT bump `updated_at` — the reason deltas silently lose rows |
| `delete_pct` | rows that simply vanish; no endpoint can report them |
| `bulk_every` | a migration that touches every row at once → cursor storm |
| `work_hours`, `quiet_divisor` | diurnal activity: quiet nights, busy days |
| `probe_recall`, `probe_precision` | a "did anything change" signal that is wrong in both directions |

The API is deliberately awkward: a delta by timestamp, a paged full dump, a
per-key detail call, a per-parent-key derived call, a probe, and a
writeback. **Every read charges calls and bytes inside `system.py`**, so
the instrument is the source, not the client — a client cannot flatter
itself by under-reporting.

`truth()` is the oracle and is not part of the API. A client that calls it
during a run is cheating.

## Running it

```bash
cargo build --release -p mpedb-py
mkdir -p /tmp/pymod/mpedb && cp crates/mpedb-py/python/mpedb/*.py /tmp/pymod/mpedb/
cp target/release/libmpedb_py.so /tmp/pymod/mpedb/_native.so

cd workbench/ingest-lab
PYTHONPATH=/tmp/pymod python3.12 bench.py --ticks 120 --initial 400 \
    --update-pct 1 --budget 8 --lying 40
```

The lab runs in **real time on purpose**. mpedb's budget is wall-clock:
`ingest_next` hands out derived work only while this window's calls remain,
and it decides that from receipt timestamps. Compressing twenty simulated
windows into two real seconds would put every receipt in one window, starve
the cascade after a few calls, and measure a fiction. So each window ends
by waiting for the clock — a 120-tick run at `--window-secs 1` takes about
20 seconds per arm.

## The three arms

- **naive** — full dump of every table, every window. Always converges and
  **ignores the budget by construction**. It is the correctness control: if
  `naive` is ever wrong, the harness is broken, not the client.
- **uniform** — every root edge at the same rate, inside the budget. This
  is a strong control arm, not a straw man: uniform beats
  proportional-to-change-rate under *every* change-rate distribution
  (Cho & García-Molina, TODS'03, Thm 5.1/5.2).
- **planned** — the rates `db.ingest_advise` computes, re-read every window
  as observations accumulate.

Measured at every window boundary: **rows wrong** (the mean is staleness,
which is what the planner optimises; the final value is where it ended up),
and calls and bytes as counted by the source.

## Measured, 2026-07-28 (Linux dev box)

`--ticks 120 --initial 400 --update-pct 1 --budget 8 --lying 40`, nine
seeds. Same world per seed, same budget for both non-naive arms:

| arm | mean rows wrong | calls | seeds still wrong at the end |
|---|---|---|---|
| naive | 0.0 | 170.4 | 0 / 9 |
| uniform | 69.4 | 160.3 | 5 / 9 |
| **planned** | **61.1** | 160.9 | **1 / 9** |

**At equal cost the plan is 12 % less stale, and better on 7 of 9 seeds.**
Not a landslide — and single-seed runs swing between −40 % and +25 %, so
anyone quoting one seed is quoting noise. The steadier difference is the
last column: uniform was still carrying wrong rows at the end of five runs,
the plan of one.

Both are far cheaper than correctness-at-any-price: `naive` needs 170 calls
and its cost scales with the TABLE, while the delta's scales with the
CHANGE RATE — so the gap widens as the source grows.

That regime is generous, though: a full dump costs 4 of the 8 calls in a
window, so uniform can afford to dump often and the allocation barely
matters. **Make the table big enough that a dump does not fit in a window
and the picture changes completely** — `--initial 2000`, same budget, three
seeds:

| arm | mean rows wrong | calls |
|---|---|---|
| naive | 0.0 | 578 |
| uniform | 3488 | 160 |
| **planned** | **1300** | **112** |

**63 % less stale on 30 % fewer calls**, and within 2 rows of the same
answer on all three seeds. This is where planning is worth having: a dump
costs the whole TABLE while a delta costs the CHANGE RATE, so once the
table outgrows the window the allocation is the entire game. Uniform keeps
spending its budget on dumps it cannot finish often enough to matter; the
plan buys the cheap delta at a high rate and the dump at the reconcile
floor.

One more honest note: the lying cursor barely moves either arm in the small
regime, because both reconcile often enough that a missed delta row is
caught by the next dump. The cursor verdict earns its keep by telling the
PLAN not to spend on a cursor that cannot deliver — which is worth most
exactly when the budget is tight.

## The agent trial (what this is really for)

Hand someone — a person or an agent — **only** `INGEST-GUIDE.md` and this
directory, and ask for a two-way sync. Then measure:

1. **Does it converge?** `wrong == 0` at the end, against `naive`.
2. **What did it cost?** calls and bytes from `system.cost()`.
3. **Where did the guide fail them?** Every place they had to read the
   Rust, guess an argument, or discover a rule by hitting an error is a doc
   bug. This is the main product of the trial.

The first trial found two: `ingest_rows` refused an empty last page (a
paged fetch whose row count divides evenly ends with one, and the call it
cost was real), and the guide's cascade section documented an API shape
that did not match the built one. Both are fixed; the pattern is what to
look for.
