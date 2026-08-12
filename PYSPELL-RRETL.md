# PySpell rRETL — Reversible ETL from Python

**rRETL — Reversible ETL** — is mpedb's name for transforms that can be run
backwards: every apply stores exactly what it destroyed (the *residual*, per
row, in the database), so the reverse is not "hope the transform was
invertible" but a verified reconstruction — and, crucially, the reverse
CARRIES EDITS made to the transformed data. The feature is called rRETL
everywhere: `mpedb rretl` in the CLI, `rretl_*` methods in Python, `rretl_lineage`
and `rretl_residual` in the schema.

The doubled letter is deliberate: **"RETL"/"rETL" already means something
else** — *Reverse ETL* in modern data engineering (warehouse → CRM/SaaS
sync: Hightouch, Census, etc.), and Oracle ships a *Retail Extract,
Transform, and Load* tool called RETL. rRETL is neither: it is not about
sync direction or retail batch jobs, it is about transforms that are
**provably reversible with edits carried**. When searching or citing, use
"rRETL mpedb".

**This document is self-contained.** An agent that reads only this file can
connect to mpedb from Python, define a reversible transform, run it over a
column, let a user edit the transformed data, and then run the reverse — which
**keeps the edits** and re-attaches what the transform threw away. No other
document is required; the design rationale lives in `design/DESIGN-RRETL.md` if
you want it, but nothing there is needed to operate.

The mental model, in one example: a table of pixels is stripped to grayscale.
The colour is not destroyed — the database stores it, per row, as the
**residual**. The user retouches the grayscale (darkens, brightens) and crops
it (deletes rows). Running the reverse re-attaches the colour **to the edited
pixels** — the retouch survives, in colour — and the cropped pixels stay gone.
The same machinery applies to any data: unit conversions, redactions,
normalisations, format changes. Transform → edit → reverse-with-edits.

A machine-learning example of the pattern: a segmentation **color-mask**
(one colour per class) is converted to **one polygon per class** — polygons
are the representation a labeler or an agent can actually work with, no GUI
needed. The vertices get edited, classes get dropped (the crop), and the
reverse produces the EDITED color-mask: the polygon edits carried back into
pixels, the anti-aliasing/edge detail the polygonisation lost re-attached from
the residual. Same triple as everything else: `forward` = mask→polygon,
`rex` = what polygonisation loses, `inverse` = polygon+residual→mask.

Transforms also **stack**: apply pair B on top of pair A's output, ten deep if
needed, then unwind strictly LIFO (last applied, first reversed) — with edits
injected at any depth, each carried down through every remaining inverse. The
engine's chain test drives 12 randomly-ordered transforms up and back down
with edits between every unwind step, model-checked at every depth.

```
apply:    x  ──forward──▶  y      (what is lost = rex(x), stored per row)
edit:     y  ──user─────▶  y'     (values changed, rows deleted)
putback:  y' ──inverse───▶ x' = inverse(y', residual)   (edits kept, loss re-attached)
revert:   y  ──inverse───▶ x      (EXACT undo; refuses if anything was edited)
```

---

## 1. Setup

```sh
pip install mpedb          # Linux x86-64/aarch64/armv7, macOS arm64/x86-64, Windows x86-64;
                           # CPython 3.10+
```

or from a checkout:

```sh
cargo build --release -p mpedb-py            # Linux
# macOS needs the extension-module link flags (maturin adds them for wheels):
#   RUSTFLAGS="-C link-arg=-undefined -C link-arg=dynamic_lookup" \
#     cargo build --release -p mpedb-py
mkdir -p /tmp/pymod/mpedb
cp crates/mpedb-py/python/mpedb/*.py /tmp/pymod/mpedb/
cp target/release/libmpedb_py.so /tmp/pymod/mpedb/_native.so   # .dylib on macOS
export PYTHONPATH=/tmp/pymod
```

A database is opened from a TOML config. The config does not need to declare
any tables — the simplest setup is four lines, and you create the tables you
work against with ordinary `CREATE TABLE`:

```toml
[database]
path = "pixels.mpedb"
size_mb = 64
max_readers = 8
```

```python
import mpedb
db = mpedb.Database("pixels.toml")
db.query("CREATE TABLE pixels (id INTEGER PRIMARY KEY, px ANY)")
```

Two rules for a table rRETL will transform: it needs a **single row
identity** — a declared one-column primary key, or no primary key at all
(the hidden `rowid` is then the identity; composite PKs are refused by
name) — and a column that will receive type-changing transforms
(int→float etc.) must be `ANY`. `ANY` accepts every scalar type, while
rigid types (`INTEGER`, `TEXT`, …) refuse type-changing pairs early, with
the pair named. (Declaring tables in the TOML with `[[table]]` blocks still
works and gives the same result; it is just more to write.)

---

## 2. PySpell: the function language

Transforms are **stored functions written in a small Python subset**, compiled
at define time and stored *in the database file* by content hash — every
attached process runs the identical definition. The subset is deliberately
deterministic: **no imports, no clock, no randomness, no file or network I/O,
no floats from nowhere** — the same input gives the same output on every
machine, forever. That determinism is what makes verified reversibility
possible at all.

Accepted (everything else is a compile error with a line number):

- exactly one `def name(a, b):` per source — name and arity are taken from
  the definition itself; no defaults, no `*args`, no annotations, no decorators
- literals: int (i64 range), float, str, `True`/`False`/`None`
- operators: `+ - * / // %`, unary `-`, `not`, `== != < <= > >=`,
  `is None` / `is not None`, `and`/`or`, augmented assignment (`+=` …)
- statements: assignment, `if`/`elif`/`else`, `while`, `break`/`continue`,
  `return`, `pass`
- `len(x)` and indexing

Semantics to know: `/` on ints yields a float, `//` floors, int overflow is an
**error** (no bigints), division by zero is an **error** — and lens functions
may NOT run SQL. Execution has a fixed instruction budget (250 000), so a
runaway loop fails identically everywhere.

**Refusing an input** (domain guards) is idiomatic: any runtime error refuses
the value. The conventional guard is `return 1 // 0`:

```python
def f(x):
    if x < 1:
        return 1 // 0     # "x is outside my domain" — a deliberate refusal
    ...
```

Two float traps the verifier WILL catch if you fall into them:

- `0 - x` is **not** negation: it maps both `+0.0` and `-0.0` to `+0.0`
  (a collision). Unary `-x` is bit-negation and is bijective.
- `+0.0` and `-0.0` are different values here (bit comparison), while
  `-0.0 < 0` is False — if your transform cannot tell them apart, exclude 0
  from the domain (`if x < 1: refuse`).

---

## 3. Lens pairs: declaring HOW a transform reverses

A transform registers as a **lens pair** with an explicit class. The class is
declared by you and **verified by the engine** against a probe corpus of edge
values — a declaration that does not hold is *refused with a concrete
counter-example*, never accepted on trust.

| class | contract | functions |
|---|---|---|
| `bijective` | `inverse(forward(x)) == x`, nothing is lost | `forward/1`, `inverse/1` |
| `residual` | `forward(x) → y` loses information; `rex(x)` extracts exactly what is lost; `inverse(y, r) → x` | `forward/1`, `rex/1`, `inverse/2` |
| `lossy` | not invertible — the source must be kept; `rretl_apply` refuses it | any |

```python
db.define_function(SRC_FORWARD)      # -> ("to_gray", "<hex hash>")
db.define_function(SRC_REX)
db.define_function(SRC_INVERSE)

n = db.create_residual_lens("gray", "to_gray", "chroma", "recolor", "any")
#   name      forward     rex       inverse    declared residual type
# n = how many probe values actually round-tripped (statistical evidence,
# reported — never a bare "verified"). A narrow-domain pair exercises few
# probe values; the apply itself then verifies 100% of YOUR rows.
```

The residual type (`int64`, `float64`, `text`, `blob`, `bool`, `timestamp`,
`any`) is a declaration the engine checks against actual `rex` outputs.
Non-injective forwards are the *point* of the residual class: `abs(x)` maps
`5` and `-5` to the same value, and registers fine when `rex` is the sign —
the pair `x ↦ (|x|, sign)` is injective even though `|x|` is not.

If the verifier refuses you, **believe it**. It has been right against its own
authors every time: it caught `celsius⇄fahrenheit` (float64 precision), caught
`0 - x` (signed zeros), caught chroma packing overflow, and caught a genuine
±0.0 collision. The error names the exact input, what it mapped to, and what
came back — fix the domain guard or the maths, not the declaration.

---

## 4. Running ETL: apply, edit, putback / revert

```python
report = db.rretl_apply("gray", "pixels", "px")
# -> {"run_id": 1, "rows": 4, "residuals": 4}
```

`rretl_apply` transforms the column **in place, in one transaction**: it stores
one residual per row in the `rretl_residual` table (keyed `run_id, pk`), records
the run in `rretl_lineage` (including a hash of the residual set itself), and —
before the commit that destroys the source — re-reads every transformed row
and verifies that `inverse(y, r)` reproduces the source, hash-exactly, for
**100% of rows**. Any failure aborts the whole transaction; the column is
never half-transformed. Failed runs are recorded in the lineage too
(`outcome = "failed"`), so `rretl_log()` answers "why is this column stale".
Every pass streams in bounded chunks, so memory stays flat whatever the table
size — there is **no row cap**; the practical bound is database file space
(refused with a named `DbFull`, rolled back whole). Apply holds the single
writer lock for the whole run — treat it as an offline operation.

Now the data is free to be edited by anyone, with plain SQL:

```python
with db.begin() as tx:
    tx.query("UPDATE pixels SET px = $1 WHERE id = 0", [73 - 20])  # darken
    tx.query("DELETE FROM pixels WHERE id = 3")                    # crop
```

Two ways back, and the difference is the whole design:

```python
db.rretl_revert(report["run_id"])    # EXACT undo. Hash-gated: refuses if the
                                   # column changed at all since the apply.

db.rretl_putback(report["run_id"])   # Undo THROUGH the edits: each surviving
                                   # row becomes inverse(edited_value, residual)
                                   # -> the edit is kept, the loss re-attached.
                                   # Deleted rows STAY deleted (the crop).
```

| situation | use |
|---|---|
| nothing was edited; you want the source back exactly | `rretl_revert` |
| the transformed data was edited and the edits must survive the reverse | `rretl_putback` |
| rows were deleted and must stay deleted | `rretl_putback` |
| rows were **inserted** after the apply | residual pairs: putback refuses them by name (there is nothing true to re-attach — delete them first); bijective pairs: they invert like any row |

Putback verification: because the source is no longer the oracle, every row is
checked **per row before commit**: `forward(x') == y'` and `rex(x') == r`. An
edit outside the pair's image — a value no source could produce with that
residual — is refused with the row and both values named, and the whole
putback rolls back. Example from the pixel pair: a pixel with chroma offsets
`+107/-53/-53` can only carry lumas `53..=148` back; luma 12 would need
negative colour channels, and the refusal says exactly that.

```python
db.rretl_log()
# [{"run_id": 1, "lens": "gray", "table": "pixels", "column": "px",
#   "rows": 4, "outcome": "putback", "error": ""}]
# outcomes: applied | reverted | putback | failed
```

Rules enforced for you (each is a named refusal, not a surprise):
- `lossy` pairs cannot `rretl_apply` — in-place transform deletes the source,
  and a lossy pair declares it cannot bring it back. Keep the source instead.
- Runs STACK on a column, and unwind strictly LIFO: reverting or putting back
  a buried run is refused with the topmost run named. Run ids are a counter
  and never reused.
- A row the pair refuses aborts the **whole run** with the row named — a
  half-transformed column is worse than none.
- `rretl_revert`/`rretl_putback` of an unknown run id, a double revert, a missing
  residual row, or a tampered column (revert only) are all named errors.
- The bookkeeping tables (`rretl_lineage`, `rretl_residual`, `rretl_versions`,
  `rretl_archives`, `rretl_archive_members`) are refused as ETL targets, and
  they are ordinary tables — query them with SQL freely.

### Whole blobs too: versions and archives

The same keep-what-was-lost discipline applies to raw bytes. Two built-in,
engine-coded transforms (no PySpell involved):

```python
v1 = db.rretl_put_version("model.onnx", first_bytes)   # -> 1
v2 = db.rretl_put_version("model.onnx", second_bytes)  # -> 2; v1 silently
                                                      #    became a reverse delta
db.rretl_get_version("model.onnx", 1) == first_bytes   # always True — or a
                                                      # NAMED corruption error
```

The newest version is always stored full; each older one is a delta against
the version above it, **verified byte-identical before the rewrite commits**,
and every 8th version stays full so no reconstruction ever walks more than 7
deltas. Nothing is deleted, ever.

```python
aid = db.rretl_pack_in("dataset.zip", zip_bytes)  # members -> rows you can query
db.rretl_pack_out(aid) == zip_bytes               # byte-identical, hash-gated
```

A zip goes in by *splice*: each member's data segment becomes a row in
`rretl_archive_members`, and the residual keeps every other byte (headers,
ordering quirks, self-extracting stubs, non-UTF-8 names — all of it), so the
reconstruction is byte-identical, not merely "a zip with the same files".
Editing a member row afterwards makes `rretl_pack_out` refuse by name —
re-ingest the edited data as a new archive instead. Both transforms appear in
`rretl_log()` (outcomes `versioned` / `packed`) and are re-verified by
`rretl_fsck()`.

### Table-SET maps: migration mirrored both ways

The migration/sync shape: your tables mirrored into a DIFFERENT schema
(renamed tables, renamed columns, converted values) — for feeding another
system — with edits on either side flowing back through the pairs. You own
getting data in and out of the external system (delta-since-timestamp, full
dump + diff, whatever it supports), applied to the target tables with plain
SQL; the map owns the transformation, both directions, and the loop safety
of repeating it.

Defined straight from Python, no files and no TOML authoring — the same
way you run `CREATE TABLE`:

```python
db.rretl_map_define({
    "name": "crm",
    "tables": [{
        "source": "customers",
        "target": "crm_customers",   # auto-created at first sync if missing
        "columns": [
            {"source": "name",   "target": "full_name"},   # no pair = copy
            {"source": "temp_c", "target": "temp_f", "pair": "celsius"},
        ],
    }],
})
db.rretl_map_sync("crm")   # materializes; later calls sync BOTH ways
```

(A mapping-TOML string is accepted too — `rretl_map_show` always returns
the canonical TOML the definition was stored as, whichever form you gave.)

The rules, each a named refusal when broken:

- **Repeating a sync is a no-op.** After every push the database records
  both sides' hashes (`rretl_map_state`), so a change that arrived BY sync
  never bounces back — no epochs, no origin flags, no loops, regardless of
  how often or from where you call sync.
- **Both sides moved since the last sync = a CONFLICT.** The sync aborts
  whole (one transaction — the set moves together or not at all), the row is
  named. Fix one side, re-sync.
- Residual pairs work both ways WITHOUT stored residuals: the source still
  exists, so `rex(source_value)` is computed live and the target edit comes
  home through `inverse(edit, rex(x))`, PutRes-gated per row.
- Lossy pairs flow forward only; a target-side edit to a lossy column is
  refused by name. Rows created on the target side invert home only when
  every mapped column is bijective (there is no residual to attach
  otherwise), and never when the source identity is a hidden rowid.
- Deletes propagate both ways; delete-vs-edit is a conflict.
- **Only a byte-identical re-define keeps the map's sync state.** A
  changed spec, a first define, a define after `rretl_map_drop` — all
  re-baseline: state is cleared in the same transaction, so the next sync
  re-arbitrates every row (agreement is adopted, disagreement is a named
  conflict) instead of trusting hashes recorded under another spec. An
  identical re-define is idempotent and changes nothing.
- **A table is a map source or a map target — never both, and never a
  target twice.** Two maps sharing a target would merge two unrelated
  masters into each other; a reverse map (`a→b` plus `b→a`) makes every
  ordinary edit a permanent conflict; a chain inside one map breaks the
  agreement between `rretl_map_check` and the sync. Refused at define
  time, by name. Staging chains are separate maps, synced in the order
  you choose.
- **If the target table is DROPPED, the next sync refuses.** A missing
  target means "materialize it" only before the map has state; after
  that it means the table was dropped or renamed away, and treating that
  as a target-side delete would empty the source. Restore the table, or
  `rretl_map_drop` + define again to re-materialize.
- **Do not write to `rretl_map_state` by hand.** It is the sync's sole
  arbiter — the one thing that decides which side moved. Its hashes are
  bound to (map, target, key) so they cannot be copied between rows, the
  key value is cross-checked against its reference on every path that
  trusts it, and state belonging to no defined map is an fsck finding.
  But a doctored arbiter can still make a sync do the wrong thing with
  no oracle able to see it afterwards: fsck verifies data against state,
  not state against the world.
- **Re-registering a pair name changes what every map using it means.**
  `create_lens` rebinds the name, maps resolve pairs by name at each
  sync, and both sides still read clean — so the sync does nothing while
  every target row holds the old pair's output. This is the standing
  reason to run `rretl_map_check`: `diverged` is what sees it.

**Running it from cron** — `rretl_map_sync` is one transaction (the set
moves together or not at all), which is right for a migration and wrong
for a scheduled job on a large set. `rretl_map_run(name, ...)` is the
daemon form: it commits as it goes, so a bounded run makes real progress
and the next one RESUMES from where it stopped.

```python
db.rretl_map_set_runner("crm", "server-1")     # only this runner may run it
r = db.rretl_map_run("crm", max_secs=45, runner="server-1")
r["round_complete"]   # False = budget spent; call again to continue
r["conflicts"]        # counted and skipped, never fatal — see map_check
```

```
* * * * * mpedb rretl map run /etc/app/db.toml crm --max-secs 45 --runner server-1
```

### Making it react instead of scan

A round walks every row on both sides, so on a big table a change waits for
the cursor to reach it. Declare `stream = true` and mpedb puts triggers on
the mapped tables: every write appends the key it touched to a journal, and
the daemon drains that FIRST.

```python
db.rretl_map_define({"name": "crm", "stream": True, "tables": [...]})
db.rretl_map_backlog("crm")     # {"crm_customers": 3} — what is waiting
r = db.rretl_map_run("crm", max_secs=45)
r["streamed"]                   # journal entries consumed this run
```

Measured: at the same per-tick budget, a change at the far end of an
8 000-row table is mirrored after **1** invocation instead of 40. The
latency stops following the table's size.

Three things that are true and matter:

- **It costs the WRITE path.** Every insert, update and delete on a mapped
  table now also writes a journal row. That is why it is opt-in.
- **The scan does not go away, and must not.** Triggers fire on the SQL path
  only — a `mirror import`, a restored file, or a `DROP TRIGGER` leaves rows
  that differ with nothing recording it. The rounds are what find those, so
  keep running them; the journal only decides what gets synced first.
- **`map sync` clears the journal** when it succeeds: it did the whole set
  in one transaction, so nothing is outstanding by definition.

Four things to know:

- **Every commit advances the whole set** — a chunk from EACH table per
  transaction, so an interrupted run never leaves one table mirrored and
  another untouched.
- **A round is a full pass**; rows that change behind the cursor are
  picked up by the next round. `rretl_map_status(name)` shows the round
  and which tables are mid-pass.
- **Conflicts do not stop it.** A daemon that aborted on the first would
  let one unresolvable row block every other row forever.
- **The runner restriction is a guard against mistakes, not auth.**
  Anything that can write the file can claim any runner name; the real
  fence is that only the server has the file and the crontab line.

**Auditing a map** — `rretl_map_check(name)` is the read-only dry run of a
sync: what WOULD move in each direction, every conflict named (a sync
aborts on the first; the check lists them all), and `diverged` — rows the
echo guard structurally cannot see, where the state says both sides are
clean yet `forward(source) != target` (tampered state, an old redefine, or
an engine bug). `chk["clean"]` is the post-sync steady state: nothing
pending, nothing wrong. `rretl_fsck()` also walks every stored map at
rest (records parse, specs resolve, pairs load, no diverged rows), so the
same cron that guards residuals guards maps.

---

## 5. The complete worked example (runnable as-is)

This is the image story end to end. A "pixel" is a packed RGB int
(`px = r*65536 + g*256 + b`); forward keeps the luma, `rex` keeps the chroma
offsets exactly, and the domain is `1..=0xFFFFFF` — every guard below was
demanded by the verifier (huge ints overflow the chroma packing; fractional
floats break exact recovery; 0 is excluded because ±0.0 genuinely collide).

```python
import mpedb

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

db = mpedb.Database("pixels.toml")                      # config from §1
for src in (TO_GRAY, CHROMA, RECOLOR):
    db.define_function(src)
db.create_residual_lens("gray", "to_gray", "chroma", "recolor", "any")

px = lambda r, g, b: r * 65536 + g * 256 + b
with db.begin() as tx:
    for i, p in enumerate([px(200,40,40), px(30,180,60), px(20,60,200), px(90,90,90)]):
        tx.query("INSERT INTO pixels (id, px) VALUES ($1, $2)", [i, p])

run = db.rretl_apply("gray", "pixels", "px")              # colour -> residuals
# column is now [93, 90, 93, 90] — pure luma

with db.begin() as tx:                                  # the user's edits
    tx.query("UPDATE pixels SET px = $1 WHERE id = 0", [73])   # darken
    tx.query("UPDATE pixels SET px = $1 WHERE id = 1", [100])  # brighten
    tx.query("DELETE FROM pixels WHERE id = 3")                # crop

db.rretl_putback(run["run_id"])                           # colour back ONTO the edits
# id 0: (180, 20, 20)  — darkened by 20 per channel, in colour
# id 1: (40, 190, 70)  — brightened by 10 per channel, in colour
# id 2: (20, 60, 200)  — untouched, restored exactly
# id 3: gone           — the crop survived
```

The executable version of this walkthrough, with every refusal asserted, is
`crates/mpedb-py/pytest/test_rretl.py`.

*Scaling* an image (resampling) creates rows that never had residuals — that
is a `lossy` operation in this model: keep the pre-scale table (or run the
scale as its own apply with the full source as the residual).

---

## 6. Python API reference

| call | returns | notes |
|---|---|---|
| `db.define_function(source)` | `(name, hash)` | PySpell subset (§2); name/arity from the `def` itself; redefining a name re-binds it, but registered pairs keep the hash they were verified against |
| `db.create_lens(name, fwd, inv, class="bijective", probes=None)` | sample count | `bijective` or `lossy`; verified, refusals name a counter-example. `probes` = YOUR domain's edge values, appended to the built-in corpus — a pair that breaks on one is refused at registration instead of aborting the first apply that meets it |
| `db.create_residual_lens(name, fwd, rex, inv, residual_type, probes=None)` | sample count | the triple; residual type is declared AND verified; `probes` as above |
| `db.lenses()` | list of dicts | `name, class, forward_hash, inverse_hash, rex_hash, residual_type, samples, healthy` |
| `db.rretl_apply(pair, table, column)` | `{"run_id", "rows", "residuals"}` | one txn; 100% verified before the source-destroying commit |
| `db.rretl_revert(run_id)` | same dict | exact undo; hash-gated |
| `db.rretl_putback(run_id)` | same dict | undo through edits; PutRes-verified per row |
| `db.rretl_log()` | list of dicts | all runs, oldest first, failures included |
| `db.rretl_fsck()` | list of finding strings | verify-at-rest: every standing run re-checked (top-run column hash, residual coverage, pair loadability, and the residual SET re-hashed against what the apply wrote — buried runs included), every stored version/archive re-materialized against its hash, AND every stored map (record parses, spec resolves, pairs load, no diverged rows); empty = clean; reports, never repairs |
| `db.rretl_put_version(obj, data)` | version number | blob versioning: newest kept full, the previous newest rewritten as a reverse delta (verified byte-identical before commit), every 8th version stays full |
| `db.rretl_get_version(obj, ver)` | `bytes` | materialize ANY version; every reconstruction step hash-verified — corruption is a named error, never wrong bytes |
| `db.rretl_versions(obj)` | list of dicts | `ver, stored_as ("full"/"delta"), bytes, content_hash` |
| `db.rretl_prune_versions(obj, keep)` | deleted count | delete the OLDEST versions, keep the newest `keep`. Chain-safe by construction (every delta bases on the version above it, so a deleted prefix orphans nothing); recorded in the lineage as outcome `pruned`; `keep=0` refused |
| `db.rretl_pack_in(name, data)` | archive id | splice a zip: members become rows in `rretl_archive_members` (queryable!), the residual keeps every other byte; reconstruction verified byte-identical BEFORE the ingest commits. zip64, encrypted and overlapping archives are refused by name |
| `db.rretl_pack_out(archive_id)` | `bytes` | rebuild the zip byte-identically, hash-gated — a member row edited outside the pipeline is a named refusal (re-ingest instead of mutating) |
| `db.rretl_archives()` | list of dicts | `archive_id, name, members, content_hash` |
| `db.rretl_map_define(spec)` | `None` | store a table-SET map from a Python dict (or a mapping-TOML string); sources, identities and pairs validated NOW |
| `db.rretl_map_sync(name)` | dict of counts | both directions, one txn; echo-guarded repeats; conflicts abort whole, named |
| `db.rretl_map_run(name, max_secs=, max_rows=, runner=, lease_secs=)` | dict | the CRON form: commits as it goes, bounded, resumable; every commit advances the whole set; conflicts counted and skipped |
| `db.rretl_map_set_runner(name, runner)` | `None` | restrict who may run it ("" clears) — a mistake guard, not auth |
| `db.rretl_map_status(name)` | dict | runner, round, live lease, tables mid-round |
| `db.rretl_map_check(name)` | dict (`clean`, `pending_total`, `tables`) | READ-ONLY dry run: would-move counts, EVERY conflict named, `diverged` = state-clean rows where forward(source) != target — the audit the echo guard cannot do |
| `db.rretl_maps()` / `rretl_map_show(name)` / `rretl_map_drop(name)` | names / TOML / bool | manage stored maps; drop keeps state, a CHANGED redefine clears it (re-baseline) |

Everything raises `mpedb.Error` subclasses (`ProgrammingError` for refusals,
`OperationalError` for engine trouble) with the engine's full message. These
calls take the writer lock — do not call them while a `db.begin()` transaction
is open on the same thread (you get a named refusal, not a hang).

CLI equivalents (same engine, same rules): `mpedb fn define <target> <f.py>`,
`mpedb lens define <target> <name> <fwd> <inv> --class residual --rex <fn>
--residual-type <ty>`, `mpedb rretl apply <target> <pair> <table>.<col>`,
`mpedb rretl revert|putback <target> <run_id>`, `mpedb rretl fsck <target>`
(exit 1 on findings — cron-able), `mpedb rretl log <target>`.

---

## 7. What the database guarantees (and what it does not)

Guaranteed, enforced by verification rather than promised: registration
refuses classes that do not hold, with counter-examples; apply verifies 100%
of rows against the source before the commit that destroys it; revert is
hash-gated; putback is PutRes-verified per row; every run — including failed
ones — is in `rretl_lineage` with the pair's content hashes; a SIGKILL at any
instant leaves the column either fully transformed or fully untouched, never
mixed (one transaction, crash-safe engine).

Not guaranteed: minimal residual size (the lower bound is information-
theoretic and uncomputable — the residual is whatever your `rex` declares);
corpus verification as *proof* (it is statistical evidence over edge values;
the per-run verification over your actual rows is the strong check); and
nothing about data edited into shapes the pair cannot carry back — those are
refused, loudly, which is the guarantee.
