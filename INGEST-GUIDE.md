# mpedb ingest — pull an external system in, catch every change, call as little as possible

**What this does.** You write the code that talks to the external system —
HTTP, CSV, a database driver, whatever it is. mpedb decides *what to fetch
and when*, receives what you fetched, works out exactly what changed,
records what it could not decide, and hands you the next calls to make.

Think of it as route optimisation. The navigator computes the route from the
map, the traffic and the fuel; you still drive the car. mpedb never makes a
network call.

**This document is self-contained.** Reading only this file is enough to
build a two-way sync. When you want to reshape the data after it lands —
different tables, different column names, values transformed reversibly —
that is `PYSPELL-RRETL.md`, and §8 shows where the two meet.

---

## 1. Setup

```sh
pip install mpedb          # Linux x86-64/aarch64, macOS arm64, Windows x86-64;
                           # CPython 3.12+
```

A database is a four-line TOML; you create tables with ordinary SQL.

```toml
# app.toml
[database]
path = "app.mpedb"
size_mb = 256
max_readers = 8
```

```python
import mpedb
db = mpedb.Database("app.toml")      # the path to the TOML, not the TOML
db.query("CREATE TABLE cases (id TEXT PRIMARY KEY, subject ANY, status ANY, updated_at ANY)")
```

One rule for a table ingest fills: it needs a **single row identity** — a
one-column primary key, or none at all (then the hidden `rowid` is the
identity). The primary key must be the external system's own id, because
that is what makes a re-read harmless.

---

## 2. Declare the source

A source is a set of **calls**, and the calls form a graph: one call's
result drives the next call's parameters. Declare it as a dict (a TOML
string works too):

```python
db.ingest_define({
    "name": "salesforce",
    "budget": [
        {"profile": "work", "window_secs": 300, "calls": 200, "bytes": 10_000_000},
        {"profile": "off",  "window_secs": 300, "calls": 20,  "bytes": 1_000_000},
    ],
    "edges": [
        # A root call: you ask "what changed?", it returns rows.
        {"name": "cases_changed", "kind": "root", "table": "cases",
         "strategy": "delta", "cursor": "updated_at", "overlap_secs": 300,
         "cost_calls": 1, "cost_bytes": 50_000},

        # The reconcile. EVERY source needs one: it is the only way deletes
        # are ever seen, and the only thing that re-checks the cursor.
        {"name": "cases_full", "kind": "root", "table": "cases",
         "strategy": "dump", "cost_calls": 20, "cost_bytes": 2_000_000},

        # A derived call: runs once per key the parent returned.
        # batch = how many keys fit in one call.
        {"name": "case_detail", "kind": "derived", "parent": "cases_changed",
         "table": "case_details", "batch": 200,
         "cost_calls": 1, "cost_bytes": 100_000},

        # And its table needs a reconcile of its own. A derived edge is
        # SCOPED to the keys that drove it, so it never presents the whole
        # table — which means nothing here would ever see a deleted detail.
        {"name": "details_full", "kind": "root", "table": "case_details",
         "strategy": "dump", "cost_calls": 40, "cost_bytes": 4_000_000},
    ],
})
```

**Profiles** are time-of-day budgets. `work` is 08–17 on weekdays, `off` is
everything else; set your own with `"work_from": 6, "work_to": 18` on the
SOURCE (not on the budget entry). Any key mpedb does not know is refused by
name — a setting it silently dropped would be a setting you believe is in
force. Cheap at night, busy
in office hours — declare it once, the plan respects it.

`mpedb ingest show <config> salesforce` prints back the canonical form.

---

## 3. Push what you fetched

Two shapes. Use the simple one until your dumps get big.

```python
rows = my_salesforce_client.query("SELECT ... WHERE LastModifiedDate > :w")

r = db.ingest_delta("salesforce", "cases", rows, calls=1, bytes=51234)
r["inserted"], r["updated"], r["unchanged"]
```

Rows are dicts keyed by column name. Unknown keys are refused by name
rather than silently dropped. Lists work too, but then you must name the
columns yourself — there is nothing else to read the names from:

```python
db.ingest_delta("salesforce", "cases", [[1, "printer", 30]],
                columns=["id", "subject", "updated_at"], calls=1, bytes=99)
```

For a large dump, stream it — memory stays flat no matter how big the dump
is:

```python
run = db.ingest_begin("salesforce", "cases", mode="dump")
try:
    for page in client.pages():
        db.ingest_rows(run, page, calls=1, bytes=len(page_bytes))
    r = db.ingest_finish(run)
except Exception:
    db.ingest_abandon(run)      # NOT finish — see below
    raise
r["deleted"]        # rows the dump did NOT contain — the only way deletes appear
```

**A dump you could not finish must be ABANDONED, not finished.** `finish`
reads every key you did not present as a delete, so a fetch that died on
page 3 of 40 deletes the rest of the table. `ingest_abandon` closes the run
and keeps what you already pushed — those rows were real observations.

**A partial row is a row.** Send `{"id": 3, "subject": "c"}` to a
four-column table and it inserts with the other two columns NULL. An
endpoint that omits fields on some rows will blank them, and the next dump
then reads those rows as changed. Send the whole row, or only the columns
you mean to set on an UPDATE.

**`calls` and `bytes` are what the call actually cost you.** mpedb cannot
see the wire, so it trusts your numbers; they are what the next plan is
computed against. Report them and the budget works.

**Your budget is not the API's rate limit — you declare one FROM the
other.** mpedb's budget is a promise you make to yourself; the upstream
limiter is a wall you hit. Read the real numbers off successful responses
(`X-RateLimit-Limit` / `X-RateLimit-Remaining`), declare a little under them
so a burst does not run you into a 429, and honour `Retry-After` when one
comes anyway. Discovering the limit by being refused is the expensive way.

**The budget window is WALL-CLOCK.** `window_secs = 300` means the last 300
seconds of real time, measured from receipt timestamps. A test loop that
runs a hundred simulated ticks in two seconds puts every receipt in one
window, and `ingest_next` (§7) will hand out nothing after the first few
calls. That is the budget working correctly on a clock you compressed —
make the loop wait, or widen the window.

**Argument 2 is an EDGE name.** A table name is accepted and resolves to
that table's edge of the matching mode — convenient with one delta and one
dump per table, ambiguous the moment there are two, so name the edge when
it matters. The receipt tells you which edge it used in `r["edge"]`.

### What `mode` means

| mode | you send | mpedb finds |
|---|---|---|
| `"delta"` | rows changed since the watermark | inserts + updates |
| `"dump"` | **every** row of the table | inserts + updates + **deletes** |

A delta cannot find deletes — nothing in "rows that changed" says a row is
gone. That is why every source declares a `dump` edge too.

---

## 4. Let mpedb work out the cursor

You declared `"cursor": "updated_at"` as a *candidate*. mpedb does not
believe you. Every dump checks, for each row it found changed, whether that
column would have caught it:

```python
st = db.ingest_state("salesforce")
st["cases_changed"]["cursor_state"]   # "unknown" | "safe" | "unsafe"
st["cases_changed"]["missed"]         # rows the cursor would have LOST
```

The watermark lives here too, and it is what your next delta should ask
from — never a timestamp your fetcher remembered, which a crash loses:

```python
st = db.ingest_state("salesforce")["cases_changed"]
wm = st["watermark"]                             # None until a delta has run
since = 0 if wm is None else max(0, wm - st["overlap_secs"])
rows = client.query(f"... WHERE LastModifiedDate >= {since}")
```

The verdict and the watermark live on the edge that OWNS the cursor — the
delta. The dump is the judge: its receipt reports the verdict it just
produced, but its own state row stays empty, because an edge that never
asks "what changed since X" has no position to resume from.

The first time a dump finds a row whose `updated_at` did not move, the
verdict flips to `unsafe` **and names the row**. That is the common case in
the wild: the application sets the timestamp on insert and forgets it on
update. When a cursor is `unsafe`, the plan stops relying on it and raises
the dump cadence instead — you do not have to notice, but you should look:

```sh
mpedb ingest state app.toml salesforce
```

---

`ingest_state` also carries `last_receipt_micros` — when this edge last
reported. A script that cron runs has no memory of its own, so that is how
it answers "am I due?" without keeping a file beside the database.

### Reading it back

Ordinary SQL, and the row shape is tuples:

```python
rows = db.query("SELECT id, subject FROM cases ORDER BY id")   # [(1, 'printer'), …]
cols, rows = db.query_full("SELECT * FROM cases")              # names too
```

---

## 5. Get the plan, put it in cron

```python
plan = db.ingest_advise("salesforce")
for line in plan["cron"]:
    print(line)
```

```sh
mpedb ingest advise app.toml salesforce --emit-cron
```

```
# salesforce — plan under 200 calls / 300s (work), 20 / 300s (off)
*/5  8-16 * * 1-5  myfetch.py salesforce cases_changed   # delta, overlap 300s
17   3    * * *    myfetch.py salesforce cases_full      # dump, reconcile
```

**Derived edges get no cron line.** They are not scheduled: their rate IS
the parent's rate times the observed fan-out, so they run when the parent
puts keys in the queue (§7). The plan still reports what they will cost —
`ingest advise` shows the fan-out and the calls per window it implies.

Your script does the fetching and calls `ingest_delta`/`ingest_dump`. The
plan says when to run it and what to ask for.

**When cron is too coarse, drive the loop from the plan itself.** Cron
cannot go below one minute, and the plan says so in the edge's `reason`
("at cron's one-minute floor — the plan wants it faster, so run a loop
instead"). Everything you need is in the structure:

```python
for profile in plan["profiles"]:
    profile["budget_calls"], profile["used_calls"]      # the window
    profile["uniform_staleness"], profile["solved_staleness"], profile["verdict"]
    for e in profile["edges"]:
        e["edge"], e["table"], e["kind"], e["strategy"]
        e["interval_secs"]        # <- what a loop sleeps between calls
        e["rate_per_window"]      # <- calls per budget window
        e["fanout"], e["cron"], e["reason"]
```

The report also tells you what it could **not** plan and why — a table with
no observations yet, an edge whose parent never ran, a budget too small for
the declared dump. Nothing is silently dropped.

**Run it a while before trusting the plan.** With no receipts, every edge
is priced at the same unobserved floor and the census says so by name — so
the first plan reflects your DECLARED costs, not the source's behaviour.
As receipts accumulate the rates separate. Two things skew the estimate if
you fight it:

- **Space the calls.** The plan gives a rate per window; making all of them
  back to back inside one window teaches the estimator that the source
  rarely changes between polls, and it lowers the rate. One call per edge
  per window, evenly spaced, is what the theory assumes and what the plan
  means.
- **Receipts closer together than a second are not a sample.** The plan
  ignores them and prices the edge at the unobserved floor rather than
  extrapolating a change rate from a loop you happened to run.

---

## 6. Conflicts

A conflict is a row the policy will not decide — most often: the row changed
on *both* sides since you last synced. Set what should happen per source:

```python
db.ingest_define({... "policy": "source", ...})   # the external system wins
db.ingest_define({... "policy": "local", ...})    # mpedb wins, difference stands
```

Either way, anything that could not be decided is **recorded, never
silently overwritten**:

```python
for c in db.ingest_conflicts("salesforce"):
    print(c["tbl"], c["k"], c["kind"], c["detail"])
```

```sh
mpedb ingest conflicts app.toml salesforce   # exits 1 when non-empty → cron mails you
```

Resolve in bulk when you have decided:

```python
db.ingest_resolve("salesforce", take="local")    # the only value there is
```

That is the whole conflict story on purpose: mpedb will not guess. Newest-
wins is deliberately not offered — it depends on a clock you do not control.

---

### `local` is not a merge

`policy = "local"` does NOT mean "merge, preferring mine". mpedb keeps no
last-synced snapshot, so it can only compare the incoming row against the
row you have **now**. Under `local`, every row that differs is a conflict
and **no source-side change is ever applied** — including rows you never
touched. A database left on `local` stops tracking the source and the
divergence piles up in `ingest_conflicts` for you to drain by hand.

Use `source` for anything that must follow the external system. Use `local`
only for a table you own and want protected, and then read the conflicts.

### ingest does not track YOUR edits

The other half of that coin: nothing in ingest records that *you* changed a
row. Under `policy = "source"` a local edit is simply overwritten by the
next receipt that covers it, and the receipt reports `conflicts: 0` —
because from ingest's side nothing was in dispute. If your application
writes to an ingested table, keep your own record of what it wrote (a dirty
flag, an outbox table, a rRETL map — §8) and push it before the next delta
touches those rows.

---

## 7. Calls that come from data (the cascade)

"List cases changed" gives you keys; each key needs a detail call; a detail
may need a contract call; a result may need a write **back** into another
system. Declare those as `derived`/`writeback` edges (§2) and let mpedb
carry the parameters:

```python
run = db.ingest_begin("salesforce", "cases_delta", mode="delta")
db.ingest_rows(run, page, calls=1, bytes=n)
db.ingest_derive(run, "case_detail", [r["id"] for r in page])   # queue the follow-ups
db.ingest_finish(run)
```

`ingest_derive` queues the follow-ups **in the same transaction as the rows
that produced them**, so a crash between "I have the keys" and "the
follow-ups are recorded" cannot happen. Queueing a key that is already
waiting is a no-op, so re-reading with overlap (§9) costs nothing here.

Then, in the worker your cron runs:

```python
while (task := db.ingest_next("salesforce")):
    rows = client.get_details(task["keys"])          # keys come batched for you
    db.ingest_delta("salesforce", task["edge"], rows, calls=1, bytes=n)
    db.ingest_done("salesforce", task["lease"])
```

Three things to know about that loop:

- **`None` means one of two things**: this window's budget is spent, or the
  queue is empty. Ask `db.ingest_pending(source)` to tell them apart — it is
  also how you see a queue that only grows.
- **Drain the cascade BEFORE you fetch roots.** Only `ingest_next` checks
  the budget; `ingest_begin`/`ingest_rows` do not, because mpedb cannot
  refuse a call you have already made. So a fetcher that dumps first can
  spend the whole window on itself and then find `ingest_next` handing out
  nothing — the queue grows, and the diagnostic will look like fan-out when
  the cause was your own roots. Drain first, or hold back calls for it.
- **A derived receipt goes through the `delta` door**, always. It presents
  only the keys it was asked about, so it upserts and never infers a delete
  — a `dump` receipt on a derived edge is refused for exactly that reason.
- **Keys are handed out under a lease** so two workers cannot fetch the
  same ones. If a fetch fails, `db.ingest_release(source, lease)` puts them
  back. If a worker dies holding them, `db.ingest_reap(source,
  older_than_secs=900)` reclaims them — safe to run any time, since the
  worst case is one duplicate fetch.

**Fan-out is measured here, not declared.** Every `ingest_derive` records
how many keys one parent call produced; that is the cardinality the planner
(§5) needs to price the graph, and you never have to estimate it yourself.
It is stored on the CHILD edge — `ingest_state(src)["case_detail"]["fanout"]`
— because that is whose cost it is. The parent's `fanout` stays 0.

**`ingest_derive` queues every key you hand it, not the changed ones.** The
receipt tells you HOW MANY rows changed, not which; if you only want
follow-ups for rows that actually moved, compare against your table before
you push, and derive that subset.

---

## 8. Two-way, and gradual migration

```
external ──ingest──► source tables ──rRETL map──► your working tables ──► users
    ▲                                                     │
    └──────── your writeback code ◄───────────────────────┘
```

Ingest gets rows in and works out what changed. **rRETL** reshapes them into
the tables your application actually wants and keeps the two sets in sync
both ways — different column names, values transformed reversibly, edits on
either side flowing to the other. That is `PYSPELL-RRETL.md`; the short
version:

```python
db.rretl_map_define({"name": "crm", "tables": [{
    "source": "cases", "target": "tickets",
    "columns": [{"source": "subject", "target": "title"}],
}]})
db.rretl_map_run("crm", max_secs=45, runner="server-1")   # the cron form
```

This is what lets you migrate gradually: run the old system and the new one
side by side, both live, both written to, with the sets kept in sync — and
move user groups over when they are ready instead of on a cutover night.

### Writing back without an echo

A `writeback` edge is a derived edge whose call PUSHES instead of fetching:
declare it with `"kind": "writeback"` and a `parent`, queue keys onto it
with `ingest_derive` exactly like a derived edge, and drain it with
`ingest_next` / `ingest_done`. mpedb carries the keys; your code does the
PUT.

The trap is the echo. A write that stamps a new `updated_at` in the source
comes back through the next delta as an update — of your own change:

```python
row = client.update_case(cid, state="escalated")     # the source stamps it
db.query("UPDATE cases SET state=$1, updated_at=$2 WHERE id=$3",
         [row["state"], row["updated_at"], cid])     # apply what it RETURNED
```

Apply whatever the write call returns — the timestamp included — to your
local row. Skip that and every writeback registers as a source change
forever, because the value matches but the timestamp does not.

**One rule when starting a two-way pair**: load one direction fully while
the other side is frozen, note the watermark at the instant you unfreeze
writes, and start the reverse direction from exactly there. Otherwise the
reverse direction has a gap the width of your initial load.

---

## 9. Things that will bite you (they bite everyone)

- **A cursor can miss rows even when it is working.** A transaction that
  stamps `updated_at`, then commits slowly, can commit *after* your
  watermark has moved past it. Nothing in "changed since X" will ever return
  that row again — only the dump finds it. This is why `overlap_secs`
  exists, and why the dump is not optional.
- **Deletes only appear in a dump.** If rows vanish in the source and your
  cadence for `dump` is weekly, your deletes are up to a week late. That is
  a choice; make it knowingly.
- **A bulk update in the source touches every row's timestamp** and your
  next delta pulls the whole table. Budget for it.
- **A dump does not move the delta's watermark.** The dump saw everything,
  but a row committed late with an older stamp would be skipped if the delta
  jumped ahead — so it does not. Practical consequence: the first delta
  after a seeding dump has no watermark and asks for everything once.
- **A PAGED dump against a moving source can lose a row.** Delete something
  on page 1 and every later row shifts back one; the row that crossed the
  boundary while you were reading is never presented, and `finish` reads
  that as a delete. Page by a stable key (`WHERE id > :last`) rather than by
  offset, or take the reconcile in one big page.
- **A dump commits as it goes**, so a reader during a dump sees a partly
  updated table. If that matters, read from the rRETL working set instead —
  it moves in its own transactions.
- **Report `calls` and `bytes` honestly.** They are the only thing the
  budget knows.

---

## 10. API reference

| call | returns | what it does |
|---|---|---|
| `db.ingest_define(spec)` | `None` | store a source (dict or TOML string); tables, edges and parents validated NOW |
| `db.ingest_sources()` / `ingest_show(name)` / `ingest_drop(name)` | names / TOML / bool | manage sources |
| `db.ingest_dump(src, tbl, rows, calls=, bytes=)` | dict | full-table receipt in one call: finds inserts, updates AND deletes |
| `db.ingest_delta(src, tbl, rows, calls=, bytes=)` | dict | changed-rows receipt: inserts and updates |
| `db.ingest_begin(src, tbl, mode=)` | int run id | start a streamed receipt |
| `db.ingest_rows(run, rows, calls=, bytes=)` | dict | push one chunk |
| `db.ingest_finish(run)` | dict | close it; for `dump`, this is where deletes are found |
| `db.ingest_abandon(run)` | `None` | give up on an open receipt — the rows already pushed stay (they were real), the run closes, and a dump's presented-key set is cleared so the next dump starts honest |
| `db.ingest_state(src)` | dict per edge | watermark, cursor verdict (`safe`/`unsafe`/`unknown`), caught/missed, observed change rate and fan-out |
| `db.ingest_advise(src)` | dict incl. `cron` and `profiles` | the plan (see below) |
| `db.ingest_conflicts(src)` / `ingest_resolve(src, take="local")` | list / count | what could not be decided, and clearing it. `take` accepts only `"local"` — taking the source's version means fetching it, which is a call, so run a dump under `policy = "source"` instead |
| `db.ingest_derive(run, edge, keys)` | int | queue derived calls from this receipt's keys, atomically with it |
| `db.ingest_next(src)` | `{lease, edge, table, keys}` \| `None` | the next batch the budget allows; `None` = window spent |
| `db.ingest_done(src, lease)` / `ingest_release(src, lease)` | int | retire a leased batch / give it back after a failed fetch |
| `db.ingest_reap(src, older_than_secs=900)` | int | reclaim leases held by workers that died |
| `db.ingest_pending(src)` | dict per edge | how much derived work is waiting, and whether any is leased |
| `db.ingest_budget_left(src)` | `{profile, calls, bytes, window_secs}` | what is left of this window |

CLI mirrors all of it: `mpedb ingest define|show|list|drop|state|advise|conflicts|resolve <config> …`
plus the worker side, `next|pending|done|release|reap`, so a shell fetcher
(curl + jq in cron) needs no Python at all. `ingest next` prints one
tab-separated `lease  edge  table  key` line per key.
